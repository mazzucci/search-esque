//! The reference evaluator: the profile's semantics written plainly over JSON records.
//!
//! It is the definition of what every construct means, and the oracle the SQL compiler is
//! held to. Nothing here is clever: a loop over records for the query, a map of counts for
//! each aggregation. It answers in Elasticsearch's response shape so a fixture can pin the
//! whole answer.
//!
//! Records are JSON objects with an `_id` (string or number) and one entry per mapped field.
//! A missing or null field is "empty"; an array field holds a list.

use crate::ast::*;
use crate::dsl::num_json;
use crate::error::{Error, Result};
use crate::mapping::{FieldType, Mapping};
use crate::timeutil::{bucket_start, format_iso, next_bucket, parse_bound, parse_iso};
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// A comparable, hashable view of a field value. Ordered within a kind; kinds never mix for
/// one field.
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Bool(bool),
    Num(f64),
    Str(String),
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Key::Bool(a), Key::Bool(b)) => a.cmp(b),
            (Key::Num(a), Key::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Key::Str(a), Key::Str(b)) => a.cmp(b),
            (Key::Bool(_), _) => Ordering::Less,
            (_, Key::Bool(_)) => Ordering::Greater,
            (Key::Num(_), Key::Str(_)) => Ordering::Less,
            (Key::Str(_), Key::Num(_)) => Ordering::Greater,
        }
    }
}

/// Lower-cased alphanumeric runs: the evaluator's stand-in for FTS5's `unicode61` tokenizer.
pub fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

fn raw_values<'a>(doc: &'a Value, field: &str) -> Vec<&'a Value> {
    match doc.get(field) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().filter(|v| !v.is_null()).collect(),
        Some(v) => vec![v],
    }
}

/// The field's values as keys: keyword and text as strings, numbers as numbers, booleans,
/// dates as epoch milliseconds. Values that do not fit the type are skipped.
fn keys(doc: &Value, field: &str, ty: FieldType) -> Vec<Key> {
    raw_values(doc, field)
        .into_iter()
        .filter_map(|v| match ty {
            FieldType::Keyword | FieldType::Text => v.as_str().map(|s| Key::Str(s.to_string())),
            FieldType::Number => v.as_f64().filter(|_| !v.is_boolean()).map(Key::Num),
            FieldType::Boolean => v.as_bool().map(Key::Bool),
            FieldType::Date => v.as_str().and_then(parse_iso).map(|ms| Key::Num(ms as f64)),
        })
        .collect()
}

fn scalar_key(s: &Scalar, ty: FieldType) -> Option<Key> {
    match (ty, s) {
        (FieldType::Date, Scalar::Str(d)) => parse_iso(d).map(|ms| Key::Num(ms as f64)),
        (_, Scalar::Str(x)) => Some(Key::Str(x.clone())),
        (_, Scalar::Num(n)) => Some(Key::Num(*n)),
        (_, Scalar::Bool(b)) => Some(Key::Bool(*b)),
    }
}

fn eq_ignoring_case(a: &Key, b: &Key) -> bool {
    match (a, b) {
        (Key::Str(x), Key::Str(y)) => x.to_lowercase() == y.to_lowercase(),
        _ => a == b,
    }
}

fn text_of(doc: &Value, field: &str) -> Vec<String> {
    raw_values(doc, field)
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// Does the record match the query?
pub fn matches(q: &Query, doc: &Value, m: &Mapping) -> bool {
    match q {
        Query::MatchAll => true,
        Query::Bool {
            must,
            should,
            must_not,
            filter,
            minimum_should_match,
        } => {
            if !must.iter().all(|c| matches(c, doc, m))
                || !filter.iter().all(|c| matches(c, doc, m))
            {
                return false;
            }
            if must_not.iter().any(|c| matches(c, doc, m)) {
                return false;
            }
            if should.is_empty() {
                return true;
            }
            // Lucene: with no required clause, at least one optional clause must match, even
            // when minimum_should_match says 0. Checked against Elasticsearch 9.5.
            let needed = if must.is_empty() && filter.is_empty() {
                minimum_should_match.unwrap_or(1).max(1)
            } else {
                minimum_should_match.unwrap_or(0)
            };
            let matched = should.iter().filter(|c| matches(c, doc, m)).count() as u32;
            matched >= needed
        }
        Query::Term { field, value } => {
            let ty = m.fields[field].ty;
            let want = match scalar_key(value, ty) {
                Some(k) => k,
                None => return false,
            };
            keys(doc, field, ty)
                .iter()
                .any(|k| eq_ignoring_case(k, &want))
        }
        Query::Terms { field, values } => {
            let ty = m.fields[field].ty;
            let wanted: Vec<Key> = values.iter().filter_map(|v| scalar_key(v, ty)).collect();
            keys(doc, field, ty)
                .iter()
                .any(|k| wanted.iter().any(|w| eq_ignoring_case(k, w)))
        }
        Query::Range {
            field,
            gte,
            gt,
            lte,
            lt,
        } => {
            let ty = m.fields[field].ty;
            let b = |s: &Option<Scalar>, round_up: bool| {
                s.as_ref().and_then(|x| match (ty, x) {
                    (FieldType::Date, Scalar::Str(d)) => {
                        parse_bound(d, round_up).map(|ms| Key::Num(ms as f64))
                    }
                    _ => scalar_key(x, ty),
                })
            };
            let (gte, gt, lte, lt) = (b(gte, false), b(gt, true), b(lte, true), b(lt, false));
            keys(doc, field, ty).iter().any(|k| {
                gte.as_ref().is_none_or(|x| k >= x)
                    && gt.as_ref().is_none_or(|x| k > x)
                    && lte.as_ref().is_none_or(|x| k <= x)
                    && lt.as_ref().is_none_or(|x| k < x)
            })
        }
        Query::Exists { field } => !raw_values(doc, field).is_empty(),
        Query::Match {
            field,
            query,
            operator,
        } => {
            let want = tokens(query);
            if want.is_empty() {
                return false;
            }
            let have: Vec<String> = text_of(doc, field).iter().flat_map(|t| tokens(t)).collect();
            match operator {
                Operator::Or => want.iter().any(|w| have.contains(w)),
                Operator::And => want.iter().all(|w| have.contains(w)),
            }
        }
        Query::MatchPhrase { field, query } => {
            let want = tokens(query);
            if want.is_empty() {
                return false;
            }
            text_of(doc, field).iter().any(|t| {
                let have = tokens(t);
                have.len() >= want.len() && have.windows(want.len()).any(|w| w == want.as_slice())
            })
        }
        Query::Prefix { field, value } => {
            let f = &m.fields[field];
            let want = value.to_lowercase();
            match f.ty {
                FieldType::Text => text_of(doc, field)
                    .iter()
                    .flat_map(|t| tokens(t))
                    .any(|t| t.starts_with(&want)),
                _ => raw_values(doc, field)
                    .iter()
                    .filter_map(|v| v.as_str())
                    .any(|s| s.to_lowercase().starts_with(&want)),
            }
        }
    }
}

fn doc_id(doc: &Value) -> Result<String> {
    match doc.get("_id") {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(Error::new(
            "$",
            "every record needs an _id (string or number)",
        )),
    }
}

fn key_json(k: &Key) -> Value {
    match k {
        Key::Bool(b) => Value::Bool(*b),
        Key::Num(n) => num_json(*n),
        Key::Str(s) => Value::String(s.clone()),
    }
}

/// Elasticsearch prints range bucket bounds as doubles: `1.0`, `5.0`.
fn es_double(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1.0e16 {
        format!("{}.0", n as i64)
    } else {
        format!("{}", n)
    }
}

fn sorted_dedup(mut v: Vec<Key>) -> Vec<Key> {
    v.sort();
    v.dedup();
    v
}

fn aggregate(a: &AggKind, docs: &[&Value], m: &Mapping) -> Value {
    let f = &m.fields[a.field()];
    match a {
        AggKind::Terms { field, size, order } => {
            let mut counts: BTreeMap<Key, u64> = BTreeMap::new();
            for d in docs {
                for k in sorted_dedup(keys(d, field, f.ty)) {
                    *counts.entry(k).or_insert(0) += 1;
                }
            }
            let mut buckets: Vec<(Key, u64)> = counts.into_iter().collect();
            buckets.sort_by(|(ka, ca), (kb, cb)| match order {
                TermsOrder::CountDesc => cb.cmp(ca).then_with(|| ka.cmp(kb)),
                TermsOrder::CountAsc => ca.cmp(cb).then_with(|| ka.cmp(kb)),
                TermsOrder::KeyAsc => ka.cmp(kb),
                TermsOrder::KeyDesc => kb.cmp(ka),
            });
            let shown = buckets.len().min(*size);
            let other: u64 = buckets[shown..].iter().map(|(_, c)| c).sum();
            let out: Vec<Value> = buckets[..shown]
                .iter()
                .map(|(k, c)| {
                    let mut b = Map::new();
                    match (f.ty, k) {
                        (FieldType::Boolean, Key::Bool(v)) => {
                            b.insert("key".into(), json!(if *v { 1 } else { 0 }));
                            b.insert("key_as_string".into(), json!(v.to_string()));
                        }
                        (FieldType::Date, Key::Num(ms)) => {
                            b.insert("key".into(), json!(*ms as i64));
                            b.insert("key_as_string".into(), json!(format_iso(*ms as i64)));
                        }
                        _ => {
                            b.insert("key".into(), key_json(k));
                        }
                    }
                    b.insert("doc_count".into(), json!(c));
                    Value::Object(b)
                })
                .collect();
            json!({"doc_count_error_upper_bound": 0, "sum_other_doc_count": other, "buckets": out})
        }
        AggKind::Range { field, ranges } => {
            // Elasticsearch returns range buckets ordered by `from`, then `to`, open ends first
            // and last respectively, whatever order the request gave them in.
            let mut ordered: Vec<&RangeBucket> = ranges.iter().collect();
            ordered.sort_by(|a, b| {
                let key = |r: &RangeBucket| {
                    (
                        r.from.unwrap_or(f64::NEG_INFINITY),
                        r.to.unwrap_or(f64::INFINITY),
                    )
                };
                key(a).partial_cmp(&key(b)).unwrap_or(Ordering::Equal)
            });
            let out: Vec<Value> = ordered
                .iter()
                .map(|r| {
                    let n = docs
                        .iter()
                        .filter(|d| {
                            keys(d, field, f.ty).iter().any(|k| match k {
                                Key::Num(v) => {
                                    r.from.is_none_or(|lo| *v >= lo)
                                        && r.to.is_none_or(|hi| *v < hi)
                                }
                                _ => false,
                            })
                        })
                        .count();
                    let mut b = Map::new();
                    let key = r.key.clone().unwrap_or_else(|| {
                        format!(
                            "{}-{}",
                            r.from.map_or("*".to_string(), es_double),
                            r.to.map_or("*".to_string(), es_double)
                        )
                    });
                    b.insert("key".into(), json!(key));
                    if let Some(lo) = r.from {
                        b.insert("from".into(), json!(lo));
                    }
                    if let Some(hi) = r.to {
                        b.insert("to".into(), json!(hi));
                    }
                    b.insert("doc_count".into(), json!(n));
                    Value::Object(b)
                })
                .collect();
            json!({"buckets": out})
        }
        AggKind::Histogram {
            field,
            interval,
            min_doc_count,
        } => {
            let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
            for d in docs {
                let mut seen: Vec<i64> = keys(d, field, f.ty)
                    .iter()
                    .filter_map(|k| match k {
                        Key::Num(v) => Some((v / interval).floor() as i64),
                        _ => None,
                    })
                    .collect();
                seen.sort();
                seen.dedup();
                for b in seen {
                    *counts.entry(b).or_insert(0) += 1;
                }
            }
            let mut out = Vec::new();
            if let (Some(&lo), Some(&hi)) = (counts.keys().next(), counts.keys().next_back()) {
                for b in lo..=hi {
                    let c = counts.get(&b).copied().unwrap_or(0);
                    if c >= *min_doc_count {
                        out.push(json!({"key": num_json(b as f64 * interval), "doc_count": c}));
                    }
                }
            }
            json!({"buckets": out})
        }
        AggKind::DateHistogram {
            field,
            calendar_interval,
            min_doc_count,
        } => {
            let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
            for d in docs {
                let mut seen: Vec<i64> = keys(d, field, f.ty)
                    .iter()
                    .filter_map(|k| match k {
                        Key::Num(ms) => Some(bucket_start(*ms as i64, *calendar_interval)),
                        _ => None,
                    })
                    .collect();
                seen.sort();
                seen.dedup();
                for b in seen {
                    *counts.entry(b).or_insert(0) += 1;
                }
            }
            let mut out = Vec::new();
            if let (Some(&lo), Some(&hi)) = (counts.keys().next(), counts.keys().next_back()) {
                let mut b = lo;
                while b <= hi {
                    let c = counts.get(&b).copied().unwrap_or(0);
                    if c >= *min_doc_count {
                        out.push(json!({"key_as_string": format_iso(b), "key": b, "doc_count": c}));
                    }
                    b = next_bucket(b, *calendar_interval);
                }
            }
            json!({"buckets": out})
        }
        AggKind::ValueCount { field } => {
            let n: usize = docs.iter().map(|d| keys(d, field, f.ty).len()).sum();
            json!({"value": n})
        }
        AggKind::Cardinality { field } => {
            let all: Vec<Key> = docs.iter().flat_map(|d| keys(d, field, f.ty)).collect();
            json!({"value": sorted_dedup(all).len()})
        }
        AggKind::Avg { field }
        | AggKind::Min { field }
        | AggKind::Max { field }
        | AggKind::Sum { field }
        | AggKind::Stats { field } => {
            let nums: Vec<f64> = docs
                .iter()
                .flat_map(|d| keys(d, field, f.ty))
                .filter_map(|k| match k {
                    Key::Num(v) => Some(v),
                    _ => None,
                })
                .collect();
            let count = nums.len();
            let sum: f64 = nums.iter().sum();
            let min = nums.iter().cloned().fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            });
            let max = nums.iter().cloned().fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            });
            let avg = if count > 0 {
                Some(sum / count as f64)
            } else {
                None
            };
            match a {
                AggKind::Avg { .. } => json!({"value": avg}),
                AggKind::Min { .. } => json!({"value": min}),
                AggKind::Max { .. } => json!({"value": max}),
                AggKind::Sum { .. } => json!({"value": sum}),
                _ => json!({"count": count, "min": min, "max": max, "avg": avg, "sum": sum}),
            }
        }
    }
}

fn source_of(doc: &Value, filter: &SourceFilter) -> Option<Value> {
    let o = doc.as_object()?;
    match filter {
        SourceFilter::All(false) => None,
        SourceFilter::All(true) => Some(Value::Object(
            o.iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )),
        SourceFilter::Fields(fields) => Some(Value::Object(
            o.iter()
                .filter(|(k, _)| fields.contains(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )),
    }
}

/// Evaluates a request over the records and answers in Elasticsearch's response shape.
///
/// Hits are ordered by the sort fields, records with an empty sort field last, ties and the
/// no-sort case by `_id` ascending (as strings). Aggregations are computed over the query's
/// matches; `post_filter` narrows the hits only, as in Elasticsearch.
pub fn evaluate(req: &Request, m: &Mapping, docs: &[Value]) -> Result<Value> {
    let matched: Vec<&Value> = docs.iter().filter(|d| matches(&req.query, d, m)).collect();
    let mut hits: Vec<&Value> = match &req.post_filter {
        Some(pf) => matched
            .iter()
            .copied()
            .filter(|d| matches(pf, d, m))
            .collect(),
        None => matched.clone(),
    };
    let ids: Vec<Result<String>> = hits.iter().map(|d| doc_id(d)).collect();
    if let Some(Err(e)) = ids.iter().find(|r| r.is_err()) {
        return Err(e.clone());
    }
    let sort_key = |d: &Value| -> (Vec<Option<Key>>, String) {
        let ks = req
            .sort
            .iter()
            .map(|s| keys(d, &s.field, m.fields[&s.field].ty).into_iter().next())
            .collect();
        (ks, doc_id(d).unwrap_or_default())
    };
    hits.sort_by(|a, b| {
        let (ka, ia) = sort_key(a);
        let (kb, ib) = sort_key(b);
        for (i, s) in req.sort.iter().enumerate() {
            let ord = match (&ka[i], &kb[i]) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(x), Some(y)) => {
                    let o = x.cmp(y);
                    if s.order == Order::Desc {
                        o.reverse()
                    } else {
                        o
                    }
                }
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        ia.cmp(&ib)
    });
    let total = hits.len();
    let page: Vec<Value> = hits
        .iter()
        .skip(req.from)
        .take(req.size)
        .map(|d| {
            let mut h = Map::new();
            h.insert("_id".into(), json!(doc_id(d).unwrap_or_default()));
            h.insert("_score".into(), Value::Null);
            if let Some(src) = source_of(d, &req.source) {
                h.insert("_source".into(), src);
            }
            Value::Object(h)
        })
        .collect();
    let mut out = Map::new();
    out.insert("took".into(), json!(0));
    out.insert("timed_out".into(), json!(false));
    out.insert("hits".into(), json!({"total": {"value": total, "relation": "eq"}, "max_score": Value::Null, "hits": page}));
    if !req.aggs.is_empty() {
        let aggs: Map<String, Value> = req
            .aggs
            .iter()
            .map(|a| (a.name.clone(), aggregate(&a.kind, &matched, m)))
            .collect();
        out.insert("aggregations".into(), Value::Object(aggs));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_like_unicode61() {
        assert_eq!(
            tokens("Facet-counts, v1.2 (Chat)"),
            vec!["facet", "counts", "v1", "2", "chat"]
        );
    }

    #[test]
    fn es_doubles() {
        assert_eq!(es_double(1.0), "1.0");
        assert_eq!(es_double(2.5), "2.5");
    }
}
