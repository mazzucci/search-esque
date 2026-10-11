//! Running a compiled request on any SQLite: the [`Executor`] trait a driver implements, and
//! [`search`], the one routine both drivers share. The rusqlite store is one driver; the
//! WebAssembly build drives SQLite in the browser through a JavaScript callback. Everything here
//! is pure, so the two can only differ in what SQLite answered, and rows that do not have the
//! shape SQLite would give are refused rather than guessed at.

use crate::ast::*;
use crate::error::{Error, Result};
use crate::mapping::{FieldType, Mapping};
use crate::shape::{self, Key, Metrics};
use crate::sql::{self, Compiled, Statement};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// A value SQLite returned.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

    /// An integer: an `Int`, or a `Real` that is a whole number (a JavaScript driver cannot tell
    /// `10` from `10.0`). A real with a fraction is not an integer.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            SqlValue::Int(i) => Some(*i),
            SqlValue::Real(x) if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 => {
                Some(*x as i64)
            }
            _ => None,
        }
    }

    /// A count: a non-negative integer.
    pub fn as_count(&self) -> Option<u64> {
        self.as_i64().filter(|i| *i >= 0).map(|i| i as u64)
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
/// once, everything is read from that set, the temp table is dropped, the transaction is
/// committed. Any failure rolls the transaction back, which also drops the temp table, so the
/// connection is never left inside a transaction.
pub fn search(exec: &mut dyn Executor, req: &Request, m: &Mapping) -> Result<Value> {
    let compiled = sql::compile(req, m)?;
    exec.execute(&plain("BEGIN"))?;
    let outcome = run(exec, req, &compiled, m).and_then(|v| {
        exec.execute(&plain(&compiled.cleanup))?;
        exec.execute(&plain("COMMIT"))?;
        Ok(v)
    });
    if outcome.is_err() {
        let _ = exec.execute(&plain("ROLLBACK"));
    }
    outcome
}

fn single_count(rows: &[Vec<SqlValue>], what: &str) -> Result<u64> {
    rows.first()
        .and_then(|r| r.first())
        .and_then(SqlValue::as_count)
        .ok_or_else(|| Error::new("sqlite", format!("{} did not return a count", what)))
}

fn run(exec: &mut dyn Executor, req: &Request, c: &Compiled, m: &Mapping) -> Result<Value> {
    exec.execute(&c.materialize)?;
    let total = single_count(&exec.query(&c.total)?, "the total")?;
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
        let b = row.first().and_then(SqlValue::as_i64);
        let n = row
            .get(1)
            .and_then(SqlValue::as_count)
            .ok_or_else(|| Error::new("sqlite", "a bucket row without a count"))?;
        if let Some(b) = b {
            counts.insert(b, n);
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
                let n = row
                    .get(1)
                    .and_then(SqlValue::as_count)
                    .ok_or_else(|| Error::new("sqlite", "a terms row without a count"))?;
                if let Some(k) = row.first().and_then(|v| key_of(v, ty)) {
                    counts.push((k, n));
                }
            }
            shape::terms(counts, *size, *order, ty)
        }
        AggKind::Range { ranges, .. } => {
            let ordered = shape::ordered_ranges(ranges);
            let mut buckets = Vec::new();
            for (r, st) in ordered.iter().zip(&a.statements) {
                buckets.push((*r, single_count(&exec.query(st)?, "a range bucket")?));
            }
            shape::range(&buckets)
        }
        AggKind::Histogram {
            interval,
            min_doc_count,
            ..
        } => {
            let bounds = exec.query(first)?;
            let row = bounds
                .first()
                .ok_or_else(|| Error::new("sqlite", "the histogram bounds returned no row"))?;
            if let (Some(lo), Some(hi)) = (
                row.first().and_then(SqlValue::as_f64),
                row.get(1).and_then(SqlValue::as_f64),
            ) {
                shape::histogram_bounds(lo, hi, *interval)?;
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
            shape::single_value(single_count(&exec.query(first)?, "the count")? as usize)
        }
        kind => {
            let rows = exec.query(first)?;
            let row = rows
                .first()
                .ok_or_else(|| Error::new("sqlite", "the metric statement returned no row"))?;
            let m = Metrics {
                count: row
                    .first()
                    .and_then(SqlValue::as_count)
                    .ok_or_else(|| Error::new("sqlite", "the metric row has no count"))?
                    as usize,
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

    /// A driver that records what it was asked to run and answers queries from a script. An
    /// `execute` whose SQL is in `fail_on` fails, to test the transaction's end.
    struct Fake {
        ran: Vec<String>,
        answers: VecDeque<Result<Vec<Vec<SqlValue>>>>,
        fail_on: Vec<&'static str>,
    }

    impl Fake {
        fn with(answers: Vec<Result<Vec<Vec<SqlValue>>>>) -> Fake {
            Fake {
                ran: vec![],
                answers: VecDeque::from(answers),
                fail_on: vec![],
            }
        }
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
            if self.fail_on.contains(&st.sql.as_str()) {
                return Err(Error::new("sqlite", format!("{} failed", st.sql)));
            }
            Ok(())
        }
    }

    fn mapping() -> Mapping {
        Mapping::from_json(&json!({"properties": {"state": {"type": "keyword"}, "n": {"type": "long"}, "ok": {"type": "boolean"}}})).unwrap()
    }

    fn request(v: Value) -> Request {
        parse_request(&v, &mapping()).unwrap()
    }

    fn rows(v: Value) -> Result<Vec<Vec<SqlValue>>> {
        Ok(v.as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .map(SqlValue::from_json)
                    .collect()
            })
            .collect())
    }

    #[test]
    fn shapes_rows_from_any_driver_in_statement_order() {
        let req = request(
            json!({"query": {"term": {"state": "open"}}, "size": 2, "aggs": {"s": {"terms": {"field": "state"}}, "b": {"terms": {"field": "ok"}}, "c": {"stats": {"field": "n"}}, "v": {"value_count": {"field": "n"}}}}),
        );
        let mut fake = Fake::with(vec![
            rows(json!([[5]])),
            rows(json!([
                ["1", "{\"state\":\"open\",\"n\":1}"],
                ["2", "{\"state\":\"open\",\"n\":9}"]
            ])),
            rows(json!([["open", 5], [null, 3]])),
            rows(json!([[1, 3], [0, 2]])),
            rows(json!([[5, 1, 9, 20.0]])),
            rows(json!([[5]])),
        ]);
        let res = search(&mut fake, &req, &mapping()).unwrap();
        assert_eq!(res["hits"]["total"]["value"], json!(5));
        assert_eq!(res["hits"]["hits"][1]["_source"]["n"], json!(9));
        assert_eq!(
            res["aggregations"]["s"]["buckets"],
            json!([{"key": "open", "doc_count": 5}]),
            "a NULL key is skipped"
        );
        assert_eq!(
            res["aggregations"]["b"]["buckets"][0],
            json!({"key": 1, "key_as_string": "true", "doc_count": 3})
        );
        assert_eq!(
            res["aggregations"]["c"]["avg"],
            json!(4.0),
            "Int min and max shape like Real ones"
        );
        assert_eq!(res["aggregations"]["v"]["value"], json!(5));
        assert_eq!(fake.ran[0], "BEGIN");
        assert!(fake.ran[1].starts_with("CREATE TEMP TABLE"));
        assert!(fake.ran[2].starts_with("SELECT COUNT(*)"));
        assert!(fake.ran[3].contains("ORDER BY"));
        assert!(fake.ran[fake.ran.len() - 2].starts_with("DROP TABLE"));
        assert_eq!(fake.ran.last().map(String::as_str), Some("COMMIT"));
    }

    #[test]
    fn a_failing_statement_rolls_back_and_names_sqlite() {
        let req = request(json!({"query": {"match_all": {}}, "size": 0}));
        let mut fake = Fake::with(vec![Err(Error::new("sqlite", "disk I/O error"))]);
        let err = search(&mut fake, &req, &mapping()).unwrap_err();
        assert_eq!(
            (err.path.as_str(), err.message.as_str()),
            ("sqlite", "disk I/O error")
        );
        assert_eq!(fake.ran.last().map(String::as_str), Some("ROLLBACK"));
        assert!(!fake.ran.iter().any(|s| s == "COMMIT"));
    }

    #[test]
    fn a_failing_commit_rolls_back_and_reports() {
        let req = request(json!({"query": {"match_all": {}}, "size": 0}));
        let mut fake = Fake::with(vec![rows(json!([[3]]))]);
        fake.fail_on = vec!["COMMIT"];
        let err = search(&mut fake, &req, &mapping()).unwrap_err();
        assert_eq!(err.message, "COMMIT failed");
        assert_eq!(fake.ran.last().map(String::as_str), Some("ROLLBACK"));
    }

    #[test]
    fn a_failing_aggregation_is_named() {
        let req = request(
            json!({"query": {"match_all": {}}, "size": 0, "aggs": {"h": {"histogram": {"field": "n", "interval": 1}}}}),
        );
        let mut fake = Fake::with(vec![rows(json!([[3]])), rows(json!([]))]);
        let err = search(&mut fake, &req, &mapping()).unwrap_err();
        assert_eq!(err.path, "aggs.h");
        assert!(err.message.contains("bounds"), "{}", err.message);
    }

    #[test]
    fn bad_rows_are_refused_not_guessed() {
        let req = request(json!({"query": {"match_all": {}}, "size": 1}));
        for bad in [
            json!([]),
            json!([[null]]),
            json!([[2.9]]),
            json!([[-1]]),
            json!([["3"]]),
        ] {
            let mut fake = Fake::with(vec![rows(bad.clone())]);
            let err = search(&mut fake, &req, &mapping()).unwrap_err();
            assert!(err.message.contains("count"), "{} for {}", err.message, bad);
        }
        let mut fake = Fake::with(vec![rows(json!([[1]])), rows(json!([[1, "{}"]]))]);
        let err = search(&mut fake, &req, &mapping()).unwrap_err();
        assert!(err.message.contains("_id"), "{}", err.message);
        let mut fake = Fake::with(vec![rows(json!([[1]])), rows(json!([["1"]]))]);
        assert!(search(&mut fake, &req, &mapping()).is_err());
    }

    #[test]
    fn integers_and_whole_reals_are_the_same_row() {
        let req = request(
            json!({"query": {"match_all": {}}, "size": 0, "aggs": {"c": {"stats": {"field": "n"}}}}),
        );
        let a = search(
            &mut Fake::with(vec![rows(json!([[2]])), rows(json!([[2, 1, 9, 10]]))]),
            &req,
            &mapping(),
        )
        .unwrap();
        let b = search(
            &mut Fake::with(vec![
                rows(json!([[2.0]])),
                rows(json!([[2.0, 1.0, 9.0, 10.0]])),
            ]),
            &req,
            &mapping(),
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rows_from_javascript_keep_their_types() {
        assert_eq!(SqlValue::from_json(&json!(10)), SqlValue::Int(10));
        assert_eq!(SqlValue::from_json(&json!(10.0)), SqlValue::Real(10.0));
        assert_eq!(SqlValue::from_json(&json!(true)), SqlValue::Int(1));
        assert_eq!(SqlValue::from_json(&json!(null)), SqlValue::Null);
        assert_eq!(SqlValue::from_json(&json!("x")), SqlValue::Text("x".into()));
        assert_eq!(SqlValue::Real(2.5).as_i64(), None);
        assert_eq!(SqlValue::Real(2.0).as_i64(), Some(2));
        assert_eq!(SqlValue::Int(-1).as_count(), None);
        assert_eq!(SqlValue::Text("3".into()).as_i64(), None);
    }
}
