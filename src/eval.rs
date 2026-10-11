//! The reference evaluator: the profile's semantics written plainly over JSON records.
//!
//! It is the definition of what every construct means, and the oracle the SQL compiler is
//! held to. Nothing here is clever: a loop over records for the query, a map of counts for
//! each aggregation. It answers in Elasticsearch's response shape so a fixture can pin the
//! whole answer.
//!
//! Records are JSON objects with an `_id` (string or number) and one entry per mapped field.
//! A field is empty when it is missing, null, holds a value of the wrong type, or holds a list
//! where the mapping says single value. An array field holds a list; a single value there
//! counts as a list of one.

use crate::ast::*;
use crate::error::{Error, Result};
use crate::mapping::{Field, FieldType, Mapping};
use crate::shape::{self, Metrics};
use crate::sql::source_json;
use crate::timeutil::{bucket_start, parse_bound, parse_iso};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub use crate::shape::Key;
pub use crate::tokenize::tokens;

fn raw_values<'a>(doc: &'a Value, field: &str, array: bool) -> Vec<&'a Value> {
    match doc.get(field) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) if array => items.iter().filter(|v| !v.is_null()).collect(),
        Some(Value::Array(_)) => Vec::new(),
        Some(v) => vec![v],
    }
}

/// The field's values as keys: keyword and text as strings, numbers as numbers, booleans,
/// dates as epoch milliseconds. Values that do not fit the type are skipped.
fn keys(doc: &Value, field: &str, f: &Field) -> Vec<Key> {
    raw_values(doc, field, f.array)
        .into_iter()
        .filter_map(|v| match f.ty {
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

fn text_of(doc: &Value, field: &str, f: &Field) -> Vec<String> {
    raw_values(doc, field, f.array)
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
            let f = &m.fields[field];
            // A date without a time part means the whole day, as in Elasticsearch.
            if let (FieldType::Date, Scalar::Str(s)) = (f.ty, value) {
                if s.len() == 10 {
                    let (Some(lo), Some(hi)) = (parse_bound(s, false), parse_bound(s, true)) else {
                        return false;
                    };
                    return keys(doc, field, f).iter().any(
                        |k| matches!(k, Key::Num(ms) if *ms >= lo as f64 && *ms <= hi as f64),
                    );
                }
            }
            let want = match scalar_key(value, f.ty) {
                Some(k) => k,
                None => return false,
            };
            keys(doc, field, f)
                .iter()
                .any(|k| eq_ignoring_case(k, &want))
        }
        Query::Terms { field, values } => {
            let f = &m.fields[field];
            let ks = keys(doc, field, f);
            values.iter().any(|v| match (f.ty, v) {
                // A date without a time part means the whole day, as for `term`.
                (FieldType::Date, Scalar::Str(s)) if s.len() == 10 => {
                    match (parse_bound(s, false), parse_bound(s, true)) {
                        (Some(lo), Some(hi)) => ks.iter().any(
                            |k| matches!(k, Key::Num(ms) if *ms >= lo as f64 && *ms <= hi as f64),
                        ),
                        _ => false,
                    }
                }
                _ => {
                    scalar_key(v, f.ty).is_some_and(|w| ks.iter().any(|k| eq_ignoring_case(k, &w)))
                }
            })
        }
        Query::Range {
            field,
            gte,
            gt,
            lte,
            lt,
        } => {
            let f = &m.fields[field];
            let b = |s: &Option<Scalar>, round_up: bool| {
                s.as_ref().and_then(|x| match (f.ty, x) {
                    (FieldType::Date, Scalar::Str(d)) => {
                        parse_bound(d, round_up).map(|ms| Key::Num(ms as f64))
                    }
                    _ => scalar_key(x, f.ty),
                })
            };
            let (gte, gt, lte, lt) = (b(gte, false), b(gt, true), b(lte, true), b(lt, false));
            keys(doc, field, f).iter().any(|k| {
                gte.as_ref().is_none_or(|x| k >= x)
                    && gt.as_ref().is_none_or(|x| k > x)
                    && lte.as_ref().is_none_or(|x| k <= x)
                    && lt.as_ref().is_none_or(|x| k < x)
            })
        }
        Query::Exists { field } => !keys(doc, field, &m.fields[field]).is_empty(),
        Query::Match {
            field,
            query,
            operator,
        } => {
            let want = tokens(query);
            if want.is_empty() {
                return false;
            }
            let have: Vec<String> = text_of(doc, field, &m.fields[field])
                .iter()
                .flat_map(|t| tokens(t))
                .collect();
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
            // Each value of an array is its own text: a phrase never spans two elements.
            text_of(doc, field, &m.fields[field]).iter().any(|t| {
                let have = tokens(t);
                have.len() >= want.len() && have.windows(want.len()).any(|w| w == want.as_slice())
            })
        }
        Query::Prefix { field, value } => {
            let f = &m.fields[field];
            let want = value.to_lowercase();
            match f.ty {
                FieldType::Text => text_of(doc, field, f)
                    .iter()
                    .flat_map(|t| tokens(t))
                    .any(|t| t.starts_with(&want)),
                _ => raw_values(doc, field, f.array)
                    .iter()
                    .filter_map(|v| v.as_str())
                    .any(|s| s.to_lowercase().starts_with(&want)),
            }
        }
    }
}

fn doc_id(doc: &Value, index: usize) -> Result<String> {
    match doc.get("_id") {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(Error::new(
            format!("$[{}]", index),
            "every record needs an _id (a non-empty string or a number)",
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
fn aggregate(a: &AggKind, docs: &[&Value], m: &Mapping) -> Result<Value> {
    let f = &m.fields[a.field()];
    let nums = |field: &str| -> Vec<f64> {
        docs.iter()
            .flat_map(|d| keys(d, field, f))
            .filter_map(|k| match k {
                Key::Num(v) => Some(v),
                _ => None,
            })
            .collect()
    };
    Ok(match a {
        AggKind::Terms { field, size, order } => {
            let mut counts: BTreeMap<Key, u64> = BTreeMap::new();
            for d in docs {
                for k in sorted_dedup(keys(d, field, f)) {
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
                            keys(d, field, f).iter().any(|k| match k {
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
            let values = nums(field);
            if let (Some(lo), Some(hi)) = (
                values.iter().cloned().reduce(f64::min),
                values.iter().cloned().reduce(f64::max),
            ) {
                shape::histogram_bounds(lo, hi, *interval)?;
            }
            let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
            for d in docs {
                let mut seen: Vec<i64> = keys(d, field, f)
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
            shape::histogram(&counts, *interval, *min_doc_count)?
        }
        AggKind::DateHistogram {
            field,
            calendar_interval,
            min_doc_count,
        } => {
            let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
            for d in docs {
                let mut seen: Vec<i64> = keys(d, field, f)
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
            shape::date_histogram(&counts, *calendar_interval, *min_doc_count)?
        }
        AggKind::ValueCount { field } => shape::single_value(
            docs.iter()
                .map(|d| {
                    let ks = keys(d, field, f);
                    // A keyword array's repeated value counts once per record, as in Elasticsearch.
                    if f.array && f.ty == FieldType::Keyword {
                        sorted_dedup(ks).len()
                    } else {
                        ks.len()
                    }
                })
                .sum(),
        ),
        AggKind::Cardinality { field } => {
            let all: Vec<Key> = docs.iter().flat_map(|d| keys(d, field, f)).collect();
            shape::single_value(sorted_dedup(all).len())
        }
        AggKind::Avg { field }
        | AggKind::Min { field }
        | AggKind::Max { field }
        | AggKind::Sum { field }
        | AggKind::Stats { field } => shape::metric(a, Metrics::from_values(&nums(field))),
    })
}

/// Evaluates a request over the records and answers in Elasticsearch's response shape.
///
/// Hits are ordered by the sort fields, records with an empty sort field last, ties and the
/// no-sort case by `_id` ascending (as strings). Aggregations are computed over the query's
/// matches; `post_filter` narrows the hits only, as in Elasticsearch. `_source` is the record
/// as given, without its `_`-prefixed keys.
pub fn evaluate(req: &Request, m: &Mapping, docs: &[Value]) -> Result<Value> {
    for (i, d) in docs.iter().enumerate() {
        doc_id(d, i)?;
    }
    let matched: Vec<&Value> = docs.iter().filter(|d| matches(&req.query, d, m)).collect();
    let mut hits: Vec<&Value> = match &req.post_filter {
        Some(pf) => matched
            .iter()
            .copied()
            .filter(|d| matches(pf, d, m))
            .collect(),
        None => matched.clone(),
    };
    let id_of = |d: &Value| doc_id(d, 0).unwrap_or_default();
    let sort_key = |d: &Value| -> Vec<Option<Key>> {
        req.sort
            .iter()
            .map(|s| keys(d, &s.field, &m.fields[&s.field]).into_iter().next())
            .collect()
    };
    hits.sort_by(|a, b| {
        let (ka, kb) = (sort_key(a), sort_key(b));
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
        id_of(a).cmp(&id_of(b))
    });
    let total = hits.len();
    let page: Vec<Value> = hits
        .iter()
        .skip(req.from)
        .take(req.size)
        .map(|d| shape::hit(&id_of(d), shape::filter_source(source_json(d), &req.source)))
        .collect();
    let aggregations = if req.aggs.is_empty() {
        None
    } else {
        let mut out = Map::new();
        for a in &req.aggs {
            let value =
                aggregate(&a.kind, &matched, m).map_err(|e| e.at(format!("aggs.{}", a.name)))?;
            out.insert(a.name.clone(), value);
        }
        Some(out)
    };
    Ok(shape::response(total, page, aggregations))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_means_missing_null_wrong_type_or_a_list_where_one_value_belongs() {
        let m = Mapping::from_json(&json!({"properties": {"n": {"type": "long"}, "tags": {"type": "keyword", "array": true}}})).unwrap();
        let exists = |field: &str, doc: Value| {
            matches(
                &Query::Exists {
                    field: field.into(),
                },
                &doc,
                &m,
            )
        };
        assert!(!exists("n", json!({"_id": "1"})));
        assert!(!exists("n", json!({"_id": "1", "n": null})));
        assert!(!exists("n", json!({"_id": "1", "n": "5"})));
        assert!(!exists("n", json!({"_id": "1", "n": [5]})));
        assert!(exists("n", json!({"_id": "1", "n": 5})));
        assert!(!exists("tags", json!({"_id": "1", "tags": []})));
        assert!(
            exists("tags", json!({"_id": "1", "tags": "solo"})),
            "a single value is a list of one"
        );
    }
}
