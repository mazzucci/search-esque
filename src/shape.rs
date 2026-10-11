//! Elasticsearch's response shape, built from counts and values.
//!
//! Both engines compute counts their own way: the evaluator by looping over records, the SQLite
//! executor by running the compiled statements. Both hand what they counted to these functions,
//! so the two can only differ in the numbers, never in the shape. Everything here is pure.

use crate::ast::{AggKind, CalendarInterval, RangeBucket, TermsOrder};
use crate::dsl::num_json;
use crate::mapping::FieldType;
use crate::timeutil::{format_iso, next_bucket};
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// A comparable view of a field value. Ordered within a kind; kinds never mix for one field.
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

pub fn key_json(k: &Key) -> Value {
    match k {
        Key::Bool(b) => Value::Bool(*b),
        Key::Num(n) => num_json(*n),
        Key::Str(s) => Value::String(s.clone()),
    }
}

/// Elasticsearch prints range bucket bounds as doubles: `1.0`, `5.0`.
pub fn es_double(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1.0e16 {
        format!("{}.0", n as i64)
    } else {
        format!("{}", n)
    }
}

/// A `terms` aggregation from (value, count) pairs: ordered, cut to `size`, the rest summed
/// into `sum_other_doc_count`. Booleans and dates carry `key_as_string` as Elasticsearch does.
pub fn terms(counts: Vec<(Key, u64)>, size: usize, order: TermsOrder, ty: FieldType) -> Value {
    let mut buckets = counts;
    buckets.sort_by(|(ka, ca), (kb, cb)| match order {
        TermsOrder::CountDesc => cb.cmp(ca).then_with(|| ka.cmp(kb)),
        TermsOrder::CountAsc => ca.cmp(cb).then_with(|| ka.cmp(kb)),
        TermsOrder::KeyAsc => ka.cmp(kb),
        TermsOrder::KeyDesc => kb.cmp(ka),
    });
    let shown = buckets.len().min(size);
    let other: u64 = buckets[shown..].iter().map(|(_, c)| c).sum();
    let out: Vec<Value> = buckets[..shown]
        .iter()
        .map(|(k, c)| {
            let mut b = Map::new();
            match (ty, k) {
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

/// Range buckets in the order Elasticsearch returns them: by `from`, then `to`, open ends
/// first and last respectively, whatever order the request gave them in.
pub fn ordered_ranges(ranges: &[RangeBucket]) -> Vec<&RangeBucket> {
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
    ordered
}

/// A `range` aggregation from buckets, already ordered, with their counts.
pub fn range(buckets: &[(&RangeBucket, u64)]) -> Value {
    let out: Vec<Value> = buckets
        .iter()
        .map(|(r, n)| {
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

/// A `histogram` from bucket index (value divided by interval, floored) to count. Empty
/// buckets between the first and last are filled when `min_doc_count` allows, as in
/// Elasticsearch.
pub fn histogram(counts: &BTreeMap<i64, u64>, interval: f64, min_doc_count: u64) -> Value {
    let mut out = Vec::new();
    if let (Some(&lo), Some(&hi)) = (counts.keys().next(), counts.keys().next_back()) {
        for b in lo..=hi {
            let c = counts.get(&b).copied().unwrap_or(0);
            if c >= min_doc_count {
                out.push(json!({"key": num_json(b as f64 * interval), "doc_count": c}));
            }
        }
    }
    json!({"buckets": out})
}

/// A `date_histogram` from bucket start (epoch milliseconds) to count, gaps filled.
pub fn date_histogram(
    counts: &BTreeMap<i64, u64>,
    iv: CalendarInterval,
    min_doc_count: u64,
) -> Value {
    let mut out = Vec::new();
    if let (Some(&lo), Some(&hi)) = (counts.keys().next(), counts.keys().next_back()) {
        let mut b = lo;
        while b <= hi {
            let c = counts.get(&b).copied().unwrap_or(0);
            if c >= min_doc_count {
                out.push(json!({"key_as_string": format_iso(b), "key": b, "doc_count": c}));
            }
            b = next_bucket(b, iv);
        }
    }
    json!({"buckets": out})
}

/// What the metric aggregations need: how many values, the smallest, the largest, the sum.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Metrics {
    pub count: usize,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub sum: f64,
}

impl Metrics {
    pub fn from_values(nums: &[f64]) -> Metrics {
        let min = nums.iter().cloned().fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.min(v)))
        });
        let max = nums.iter().cloned().fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.max(v)))
        });
        Metrics {
            count: nums.len(),
            min,
            max,
            sum: nums.iter().sum(),
        }
    }

    pub fn avg(&self) -> Option<f64> {
        if self.count > 0 {
            Some(self.sum / self.count as f64)
        } else {
            None
        }
    }
}

/// `avg`, `min`, `max`, `sum` or `stats`, in Elasticsearch's shape.
pub fn metric(kind: &AggKind, m: Metrics) -> Value {
    match kind {
        AggKind::Avg { .. } => json!({"value": m.avg()}),
        AggKind::Min { .. } => json!({"value": m.min}),
        AggKind::Max { .. } => json!({"value": m.max}),
        AggKind::Sum { .. } => json!({"value": m.sum}),
        _ => json!({"count": m.count, "min": m.min, "max": m.max, "avg": m.avg(), "sum": m.sum}),
    }
}

pub fn single_value(n: usize) -> Value {
    json!({"value": n})
}

/// One hit: `_id`, a null `_score` (ranking is not part of the profile yet), and `_source`
/// unless the request turned it off.
pub fn hit(id: &str, source: Option<Value>) -> Value {
    let mut h = Map::new();
    h.insert("_id".into(), json!(id));
    h.insert("_score".into(), Value::Null);
    if let Some(src) = source {
        h.insert("_source".into(), src);
    }
    Value::Object(h)
}

/// The response envelope. `total` is always exact, so `relation` is always `eq`.
pub fn response(total: usize, hits: Vec<Value>, aggregations: Option<Map<String, Value>>) -> Value {
    let mut out = Map::new();
    out.insert("took".into(), json!(0));
    out.insert("timed_out".into(), json!(false));
    out.insert(
        "hits".into(),
        json!({"total": {"value": total, "relation": "eq"}, "max_score": Value::Null, "hits": hits}),
    );
    if let Some(aggs) = aggregations {
        out.insert("aggregations".into(), Value::Object(aggs));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn es_doubles() {
        assert_eq!(es_double(1.0), "1.0");
        assert_eq!(es_double(2.5), "2.5");
    }

    #[test]
    fn terms_cut_and_sum_the_rest() {
        let v = terms(
            vec![
                (Key::Str("a".into()), 3),
                (Key::Str("b".into()), 5),
                (Key::Str("c".into()), 3),
            ],
            2,
            TermsOrder::CountDesc,
            FieldType::Keyword,
        );
        assert_eq!(
            v["buckets"],
            json!([{"key": "b", "doc_count": 5}, {"key": "a", "doc_count": 3}])
        );
        assert_eq!(v["sum_other_doc_count"], json!(3));
    }
}
