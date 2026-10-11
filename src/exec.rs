//! Running a compiled request on any SQLite: the [`Executor`] trait a driver implements, and
//! [`search`], the one routine both drivers share. The rusqlite store is one driver; the
//! WebAssembly build drives SQLite in the browser through a JavaScript callback. Everything here
//! is pure, so the two can only differ in what SQLite answered.

use crate::ast::*;
use crate::error::{Error, Result};
use crate::mapping::{FieldType, Mapping};
use crate::shape::{self, Key, Metrics};
use crate::sql::{self, Compiled, Statement};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// A value SQLite returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl SqlValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SqlValue::Int(i) => Some(*i as f64),
            SqlValue::Real(x) => Some(*x),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SqlValue::Int(i) => Some(*i),
            SqlValue::Real(x) if x.is_finite() => Some(*x as i64),
            _ => None,
        }
    }

    /// From a JSON value, as a JavaScript driver returns rows: integers stay integers.
    pub fn from_json(v: &Value) -> SqlValue {
        match v {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Int(*b as i64),
            Value::Number(n) => match n.as_i64() {
                Some(i) => SqlValue::Int(i),
                None => n.as_f64().map(SqlValue::Real).unwrap_or(SqlValue::Null),
            },
            Value::String(s) => SqlValue::Text(s.clone()),
            other => SqlValue::Text(other.to_string()),
        }
    }
}

/// What a driver provides: run a statement with its bound parameters and give back the rows.
pub trait Executor {
    fn query(&mut self, stmt: &Statement) -> Result<Vec<Vec<SqlValue>>>;

    /// A statement run for its effect; the default discards the rows.
    fn execute(&mut self, stmt: &Statement) -> Result<()> {
        self.query(stmt).map(|_| ())
    }
}

fn plain(sql: &str) -> Statement {
    Statement {
        sql: sql.to_string(),
        params: Vec::new(),
    }
}

/// Runs a request on a store through the driver and answers in Elasticsearch's response shape,
/// the same shape the evaluator produces. One transaction: the matching ids are materialised
/// once, everything is read from that set, the temp table is dropped.
pub fn search(exec: &mut dyn Executor, req: &Request, m: &Mapping) -> Result<Value> {
    let compiled = sql::compile(req, m)?;
    exec.execute(&plain("BEGIN"))?;
    let result = run(exec, req, &compiled, m);
    let _ = exec.execute(&plain(&compiled.cleanup));
    let end = exec.execute(&plain("COMMIT"));
    match (result, end) {
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}

fn first_i64(rows: &[Vec<SqlValue>], what: &str) -> Result<i64> {
    rows.first()
        .and_then(|r| r.first())
        .and_then(SqlValue::as_i64)
        .ok_or_else(|| Error::new("sqlite", format!("{} returned no number", what)))
}

fn run(exec: &mut dyn Executor, req: &Request, c: &Compiled, m: &Mapping) -> Result<Value> {
    exec.execute(&c.materialize)?;
    let total = first_i64(&exec.query(&c.total)?, "the total")?;
    let mut hits = Vec::new();
    if let Some(h) = &c.hits {
        for row in exec.query(h)? {
            let (id, text) = match (row.first(), row.get(1)) {
                (Some(SqlValue::Text(id)), Some(SqlValue::Text(src))) => (id.clone(), src.clone()),
                _ => return Err(Error::new("sqlite", "a hit row without _id and _source")),
            };
            let source: Value = serde_json::from_str(&text).map_err(|e| {
                Error::new(
                    "sqlite",
                    format!("record {} has an unreadable _source: {}", id, e),
                )
            })?;
            hits.push(shape::hit(&id, shape::filter_source(source, &req.source)));
        }
    }
    let aggregations = if req.aggs.is_empty() {
        None
    } else {
        let mut out = Map::new();
        for a in &c.aggs {
            let value = aggregate(exec, a, m).map_err(|e| e.at(format!("aggs.{}", a.name)))?;
            out.insert(a.name.clone(), value);
        }
        Some(out)
    };
    Ok(shape::response(total as usize, hits, aggregations))
}

fn key_of(v: &SqlValue, ty: FieldType) -> Option<Key> {
    Some(match (ty, v) {
        (_, SqlValue::Null) => return None,
        (FieldType::Boolean, SqlValue::Int(i)) => Key::Bool(*i != 0),
        (_, SqlValue::Int(i)) => Key::Num(*i as f64),
        (_, SqlValue::Real(x)) => Key::Num(*x),
        (_, SqlValue::Text(t)) => Key::Str(t.clone()),
    })
}

fn bucket_counts(exec: &mut dyn Executor, st: &Statement) -> Result<BTreeMap<i64, u64>> {
    let mut counts = BTreeMap::new();
    for row in exec.query(st)? {
        if let (Some(Some(b)), Some(Some(n))) = (
            row.first().map(SqlValue::as_i64),
            row.get(1).map(SqlValue::as_i64),
        ) {
            counts.insert(b, n as u64);
        }
    }
    Ok(counts)
}

fn aggregate(exec: &mut dyn Executor, a: &sql::CompiledAgg, m: &Mapping) -> Result<Value> {
    let ty = m
        .fields
        .get(a.kind.field())
        .ok_or_else(|| Error::new("aggs", format!("unknown field '{}'", a.kind.field())))?
        .ty;
    let first = a.statements.first().ok_or_else(|| {
        Error::new(
            "aggs",
            format!("aggregation '{}' compiled to nothing", a.name),
        )
    })?;
    Ok(match &a.kind {
        AggKind::Terms { size, order, .. } => {
            let mut counts: Vec<(Key, u64)> = Vec::new();
            for row in exec.query(first)? {
                if let (Some(k), Some(Some(n))) = (
                    row.first().and_then(|v| key_of(v, ty)),
                    row.get(1).map(SqlValue::as_i64),
                ) {
                    counts.push((k, n as u64));
                }
            }
            shape::terms(counts, *size, *order, ty)
        }
        AggKind::Range { ranges, .. } => {
            let ordered = shape::ordered_ranges(ranges);
            let mut buckets = Vec::new();
            for (r, st) in ordered.iter().zip(&a.statements) {
                let n = first_i64(&exec.query(st)?, "a range bucket")?;
                buckets.push((*r, n as u64));
            }
            shape::range(&buckets)
        }
        AggKind::Histogram {
            interval,
            min_doc_count,
            ..
        } => {
            let bounds = exec.query(first)?;
            if let Some(row) = bounds.first() {
                if let (Some(lo), Some(hi)) = (
                    row.first().and_then(SqlValue::as_f64),
                    row.get(1).and_then(SqlValue::as_f64),
                ) {
                    shape::histogram_bounds(lo, hi, *interval)?;
                }
            }
            let buckets = a
                .statements
                .get(1)
                .ok_or_else(|| Error::new("aggs", "histogram without its bucket statement"))?;
            let counts = bucket_counts(exec, buckets)?;
            shape::histogram(&counts, *interval, *min_doc_count)?
        }
        AggKind::DateHistogram {
            calendar_interval,
            min_doc_count,
            ..
        } => {
            let counts = bucket_counts(exec, first)?;
            shape::date_histogram(&counts, *calendar_interval, *min_doc_count)?
        }
        AggKind::ValueCount { .. } | AggKind::Cardinality { .. } => {
            let n = first_i64(&exec.query(first)?, "the count")?;
            shape::single_value(n as usize)
        }
        kind => {
            let rows = exec.query(first)?;
            let row = rows
                .first()
                .ok_or_else(|| Error::new("sqlite", "the metric statement returned no row"))?;
            let m = Metrics {
                count: row.first().and_then(SqlValue::as_i64).unwrap_or(0) as usize,
                min: row.get(1).and_then(SqlValue::as_f64),
                max: row.get(2).and_then(SqlValue::as_f64),
                sum: row.get(3).and_then(SqlValue::as_f64).unwrap_or(0.0),
            };
            shape::metric(kind, m)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parse_request;
    use serde_json::json;
    use std::collections::VecDeque;

    /// A driver that records what it was asked to run and answers queries from a script.
    struct Fake {
        ran: Vec<String>,
        answers: VecDeque<Result<Vec<Vec<SqlValue>>>>,
    }

    impl Executor for Fake {
        fn query(&mut self, st: &Statement) -> Result<Vec<Vec<SqlValue>>> {
            self.ran.push(st.sql.clone());
            self.answers
                .pop_front()
                .unwrap_or_else(|| Err(Error::new("sqlite", "unexpected query")))
        }

        fn execute(&mut self, st: &Statement) -> Result<()> {
            self.ran.push(st.sql.clone());
            Ok(())
        }
    }

    fn mapping() -> Mapping {
        Mapping::from_json(&json!({"properties": {"state": {"type": "keyword"}, "n": {"type": "long"}, "ok": {"type": "boolean"}}})).unwrap()
    }

    #[test]
    fn shapes_rows_from_any_driver() {
        let m = mapping();
        let req = parse_request(
            &json!({"query": {"term": {"state": "open"}}, "size": 2, "aggs": {"s": {"terms": {"field": "state"}}, "b": {"terms": {"field": "ok"}}, "c": {"stats": {"field": "n"}}, "v": {"value_count": {"field": "n"}}}}),
            &m,
        )
        .unwrap();
        let mut fake = Fake {
            ran: vec![],
            answers: VecDeque::from(vec![
                Ok(vec![vec![SqlValue::Int(5)]]),
                Ok(vec![
                    vec![
                        SqlValue::Text("1".into()),
                        SqlValue::Text(r#"{"state":"open","n":1}"#.into()),
                    ],
                    vec![
                        SqlValue::Text("2".into()),
                        SqlValue::Text(r#"{"state":"open","n":9}"#.into()),
                    ],
                ]),
                Ok(vec![vec![SqlValue::Text("open".into()), SqlValue::Int(5)]]),
                Ok(vec![
                    vec![SqlValue::Int(1), SqlValue::Int(3)],
                    vec![SqlValue::Int(0), SqlValue::Int(2)],
                ]),
                Ok(vec![vec![
                    SqlValue::Int(5),
                    SqlValue::Real(1.0),
                    SqlValue::Real(9.0),
                    SqlValue::Real(20.0),
                ]]),
                Ok(vec![vec![SqlValue::Int(5)]]),
            ]),
        };
        let res = search(&mut fake, &req, &m).unwrap();
        assert_eq!(res["hits"]["total"]["value"], json!(5));
        assert_eq!(res["hits"]["hits"][1]["_source"]["n"], json!(9));
        assert_eq!(
            res["aggregations"]["s"]["buckets"],
            json!([{"key": "open", "doc_count": 5}])
        );
        assert_eq!(
            res["aggregations"]["b"]["buckets"][0],
            json!({"key": 1, "key_as_string": "true", "doc_count": 3})
        );
        assert_eq!(res["aggregations"]["c"]["avg"], json!(4.0));
        assert_eq!(res["aggregations"]["v"]["value"], json!(5));
        assert_eq!(fake.ran.first().map(String::as_str), Some("BEGIN"));
        assert!(fake.ran.iter().any(|s| s.starts_with("CREATE TEMP TABLE")));
        assert_eq!(fake.ran.last().map(String::as_str), Some("COMMIT"));
        assert!(fake.ran[fake.ran.len() - 2].starts_with("DROP TABLE"));
    }

    #[test]
    fn a_failing_statement_still_cleans_up_and_ends_the_transaction() {
        let m = mapping();
        let req = parse_request(&json!({"query": {"match_all": {}}, "size": 0}), &m).unwrap();
        let mut fake = Fake {
            ran: vec![],
            answers: VecDeque::from(vec![Err(Error::new("sqlite", "disk I/O error"))]),
        };
        let err = search(&mut fake, &req, &m).unwrap_err();
        assert_eq!(err.message, "disk I/O error");
        assert!(fake.ran.iter().any(|s| s.starts_with("DROP TABLE")));
        assert_eq!(fake.ran.last().map(String::as_str), Some("COMMIT"));
    }

    #[test]
    fn rows_from_javascript_keep_their_types() {
        assert_eq!(SqlValue::from_json(&json!(10)), SqlValue::Int(10));
        assert_eq!(SqlValue::from_json(&json!(10.0)), SqlValue::Real(10.0));
        assert_eq!(SqlValue::from_json(&json!(true)), SqlValue::Int(1));
        assert_eq!(SqlValue::from_json(&json!(null)), SqlValue::Null);
        assert_eq!(SqlValue::from_json(&json!("x")), SqlValue::Text("x".into()));
        assert_eq!(SqlValue::Real(2.5).as_i64(), Some(2));
        assert_eq!(SqlValue::Text("3".into()).as_i64(), None);
    }
}
