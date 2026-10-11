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
use crate::error::{Error, Result};
use crate::mapping::{FieldType, Mapping};
use crate::shape::{self, Metrics};
use crate::timeutil::{bucket_start, parse_bound, parse_iso};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub use crate::shape::Key;

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

fn sorted_dedup(mut v: Vec<Key>) -> Vec<Key> {
    v.sort();
    v.dedup();
    v
}

/// Counts for one aggregation over the matched records, shaped by `shape`, so the evaluator
/// and the SQLite executor can only differ in the numbers.
fn aggregate(a: &AggKind, docs: &[&Value], m: &Mapping) -> Value {
    let f = &m.fields[a.field()];
    let nums = |field: &str| -> Vec<f64> {
        docs.iter()
            .flat_map(|d| keys(d, field, f.ty))
            .filter_map(|k| match k {
                Key::Num(v) => Some(v),
                _ => None,
            })
            .collect()
    };
    match a {
        AggKind::Terms { field, size, order } => {
            let mut counts: BTreeMap<Key, u64> = BTreeMap::new();
            for d in docs {
                for k in sorted_dedup(keys(d, field, f.ty)) {
                    *counts.entry(k).or_insert(0) += 1;
                }
            }
            shape::terms(counts.into_iter().collect(), *size, *order, f.ty)
        }
        AggKind::Range { field, ranges } => {
            let buckets: Vec<(&RangeBucket, u64)> = shape::ordered_ranges(ranges)
                .into_iter()
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
                    (r, n as u64)
                })
                .collect();
            shape::range(&buckets)
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
            shape::histogram(&counts, *interval, *min_doc_count)
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
            shape::date_histogram(&counts, *calendar_interval, *min_doc_count)
        }
        AggKind::ValueCount { field } => {
            shape::single_value(docs.iter().map(|d| keys(d, field, f.ty).len()).sum())
        }
        AggKind::Cardinality { field } => {
            let all: Vec<Key> = docs.iter().flat_map(|d| keys(d, field, f.ty)).collect();
            shape::single_value(sorted_dedup(all).len())
        }
        AggKind::Avg { field }
        | AggKind::Min { field }
        | AggKind::Max { field }
        | AggKind::Sum { field }
        | AggKind::Stats { field } => shape::metric(a, Metrics::from_values(&nums(field))),
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
        .map(|d| shape::hit(&doc_id(d).unwrap_or_default(), source_of(d, &req.source)))
        .collect();
    let aggregations = if req.aggs.is_empty() {
        None
    } else {
        Some(
            req.aggs
                .iter()
                .map(|a| (a.name.clone(), aggregate(&a.kind, &matched, m)))
                .collect::<Map<String, Value>>(),
        )
    };
    Ok(shape::response(total, page, aggregations))
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
}
