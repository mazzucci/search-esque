//! The SQLite executor: a store built from a mapping and records, and `search`, which runs
//! what `sql::compile` produces and shapes the answer. The one module that does I/O; behind the
//! `sqlite` feature so the core still compiles to WebAssembly without it.

use crate::ast::*;
use crate::error::{Error, Result};
use crate::mapping::{FieldType, Mapping};
use crate::shape::{self, Key, Metrics};
use crate::sql::{self, Compiled, Param, Statement};
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{params_from_iter, Connection, ToSql};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

impl ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Param::Null => ToSqlOutput::from(rusqlite::types::Null),
            Param::Int(i) => ToSqlOutput::from(*i),
            Param::Real(f) => ToSqlOutput::from(*f),
            Param::Text(s) => ToSqlOutput::from(s.as_str()),
        })
    }
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::new("sqlite", e.to_string())
}

pub struct Store {
    conn: Connection,
    mapping: Mapping,
}

impl Store {
    /// A new store file for a mapping. Fails if the file exists.
    pub fn create(path: &str, mapping: &Mapping) -> Result<Store> {
        if std::path::Path::new(path).exists() {
            return Err(Error::new("db", format!("{} exists already", path))
                .hint("remove it, or load into a new file"));
        }
        let conn = Connection::open(path).map_err(db_err)?;
        Store::init(conn, mapping)
    }

    pub fn in_memory(mapping: &Mapping) -> Result<Store> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        Store::init(conn, mapping)
    }

    fn init(conn: Connection, mapping: &Mapping) -> Result<Store> {
        for stmt in sql::schema(mapping) {
            conn.execute_batch(&stmt).map_err(db_err)?;
        }
        conn.execute(
            &format!("INSERT INTO {} VALUES ('mapping', ?)", sql::META),
            [mapping.to_json().to_string()],
        )
        .map_err(db_err)?;
        Ok(Store {
            conn,
            mapping: mapping.clone(),
        })
    }

    /// An existing store; the mapping is read from the file.
    pub fn open(path: &str) -> Result<Store> {
        if !std::path::Path::new(path).exists() {
            return Err(Error::new("db", format!("{} does not exist", path))
                .hint("create it with `esque load`"));
        }
        let conn = Connection::open(path).map_err(db_err)?;
        let text: String = conn
            .query_row(
                &format!("SELECT value FROM {} WHERE key = 'mapping'", sql::META),
                [],
                |r| r.get(0),
            )
            .map_err(|_| Error::new("db", format!("{} is not an esque store", path)))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| Error::new("db", e.to_string()))?;
        let mapping = Mapping::from_json(&v)?;
        Ok(Store { conn, mapping })
    }

    pub fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    /// Inserts records (replacing any with the same `_id`) and rebuilds the text index.
    pub fn load(&self, docs: &[Value]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        {
            let mut ins = tx
                .prepare(&sql::insert_statement(&self.mapping))
                .map_err(db_err)?;
            for d in docs {
                let row = sql::row_params(d, &self.mapping)?;
                ins.execute(params_from_iter(row.iter())).map_err(db_err)?;
            }
        }
        if let Some(fill) = sql::fts_fill(&self.mapping) {
            tx.execute(&format!("DELETE FROM {}", sql::FTS), [])
                .map_err(db_err)?;
            tx.execute(&fill, []).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(docs.len())
    }

    pub fn count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {}", sql::DOCS), [], |r| {
                r.get(0)
            })
            .map_err(db_err)?;
        Ok(n as usize)
    }

    /// Runs a request and answers in Elasticsearch's response shape, the same shape the
    /// evaluator produces.
    pub fn search(&self, req: &Request) -> Result<Value> {
        let compiled = sql::compile(req, &self.mapping);
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        let result = self.run(&tx, req, &compiled);
        let _ = tx.execute_batch(&compiled.cleanup);
        tx.commit().map_err(db_err)?;
        result
    }

    fn run(&self, tx: &rusqlite::Transaction, req: &Request, c: &Compiled) -> Result<Value> {
        tx.execute(
            &c.materialize.sql,
            params_from_iter(c.materialize.params.iter()),
        )
        .map_err(db_err)?;
        let total: i64 = tx
            .query_row(&c.total.sql, params_from_iter(c.total.params.iter()), |r| {
                r.get(0)
            })
            .map_err(db_err)?;
        let mut hits = Vec::new();
        if let Some(h) = &c.hits {
            let cols = sql::columns(&self.mapping);
            let mut st = tx.prepare(&h.sql).map_err(db_err)?;
            let mut rows = st
                .query(params_from_iter(h.params.iter()))
                .map_err(db_err)?;
            while let Some(row) = rows.next().map_err(db_err)? {
                let id: String = row.get(0).map_err(db_err)?;
                let source = match &req.source {
                    SourceFilter::All(false) => None,
                    SourceFilter::All(true) => Some(self.source_of(row, &cols, None)?),
                    SourceFilter::Fields(fs) => Some(self.source_of(row, &cols, Some(fs))?),
                };
                hits.push(shape::hit(&id, source));
            }
        }
        let aggregations = if req.aggs.is_empty() {
            None
        } else {
            let mut out = Map::new();
            for a in &c.aggs {
                out.insert(a.name.clone(), self.aggregate(tx, a)?);
            }
            Some(out)
        };
        Ok(shape::response(total as usize, hits, aggregations))
    }

    fn source_of(
        &self,
        row: &rusqlite::Row,
        cols: &[(String, &str)],
        only: Option<&Vec<String>>,
    ) -> Result<Value> {
        let mut out = Map::new();
        for (i, (name, _)) in cols.iter().enumerate() {
            if name.starts_with('_') || name.contains("__") {
                continue;
            }
            if let Some(fs) = only {
                if !fs.contains(name) {
                    continue;
                }
            }
            let f = &self.mapping.fields[name];
            let v = row.get_ref(i).map_err(db_err)?;
            let value = if f.array {
                match v {
                    ValueRef::Text(t) => serde_json::from_slice(t).unwrap_or(Value::Array(vec![])),
                    _ => Value::Array(vec![]),
                }
            } else {
                match (f.ty, v) {
                    (_, ValueRef::Null) => Value::Null,
                    (FieldType::Boolean, ValueRef::Integer(i)) => Value::Bool(i != 0),
                    (_, ValueRef::Integer(i)) => Value::from(i),
                    (_, ValueRef::Real(x)) => Value::from(x),
                    (_, ValueRef::Text(t)) => {
                        Value::String(String::from_utf8_lossy(t).into_owned())
                    }
                    (_, ValueRef::Blob(_)) => Value::Null,
                }
            };
            out.insert(name.clone(), value);
        }
        Ok(Value::Object(out))
    }

    fn key_of(v: ValueRef, ty: FieldType) -> Option<Key> {
        Some(match (ty, v) {
            (_, ValueRef::Null) => return None,
            (FieldType::Boolean, ValueRef::Integer(i)) => Key::Bool(i != 0),
            (_, ValueRef::Integer(i)) => Key::Num(i as f64),
            (_, ValueRef::Real(x)) => Key::Num(x),
            (_, ValueRef::Text(t)) => Key::Str(String::from_utf8_lossy(t).into_owned()),
            (_, ValueRef::Blob(_)) => return None,
        })
    }

    fn aggregate(&self, tx: &rusqlite::Transaction, a: &sql::CompiledAgg) -> Result<Value> {
        let ty = self.mapping.fields[a.kind.field()].ty;
        let first = &a.statements[0];
        Ok(match &a.kind {
            AggKind::Terms { size, order, .. } => {
                let mut counts: Vec<(Key, u64)> = Vec::new();
                let mut st = tx.prepare(&first.sql).map_err(db_err)?;
                let mut rows = st.query([]).map_err(db_err)?;
                while let Some(row) = rows.next().map_err(db_err)? {
                    if let Some(k) = Store::key_of(row.get_ref(0).map_err(db_err)?, ty) {
                        let n: i64 = row.get(1).map_err(db_err)?;
                        counts.push((k, n as u64));
                    }
                }
                shape::terms(counts, *size, *order, ty)
            }
            AggKind::Range { ranges, .. } => {
                let ordered = shape::ordered_ranges(ranges);
                let mut buckets = Vec::new();
                for (r, st) in ordered.iter().zip(&a.statements) {
                    let n: i64 = Store::scalar(tx, st)?;
                    buckets.push((*r, n as u64));
                }
                shape::range(&buckets)
            }
            AggKind::Histogram {
                interval,
                min_doc_count,
                ..
            } => {
                let counts = Store::bucket_counts(tx, first)?;
                shape::histogram(&counts, *interval, *min_doc_count)
            }
            AggKind::DateHistogram {
                calendar_interval,
                min_doc_count,
                ..
            } => {
                let counts = Store::bucket_counts(tx, first)?;
                shape::date_histogram(&counts, *calendar_interval, *min_doc_count)
            }
            AggKind::ValueCount { .. } | AggKind::Cardinality { .. } => {
                let n: i64 = Store::scalar(tx, first)?;
                shape::single_value(n as usize)
            }
            kind => {
                let m = tx
                    .query_row(&first.sql, params_from_iter(first.params.iter()), |r| {
                        let count: i64 = r.get(0)?;
                        let min: Option<f64> = r.get(1)?;
                        let max: Option<f64> = r.get(2)?;
                        let sum: f64 = r.get(3)?;
                        Ok(Metrics {
                            count: count as usize,
                            min,
                            max,
                            sum,
                        })
                    })
                    .map_err(db_err)?;
                shape::metric(kind, m)
            }
        })
    }

    fn scalar(tx: &rusqlite::Transaction, st: &Statement) -> Result<i64> {
        tx.query_row(&st.sql, params_from_iter(st.params.iter()), |r| r.get(0))
            .map_err(db_err)
    }

    fn bucket_counts(tx: &rusqlite::Transaction, st: &Statement) -> Result<BTreeMap<i64, u64>> {
        let mut counts = BTreeMap::new();
        let mut prepared = tx.prepare(&st.sql).map_err(db_err)?;
        let mut rows = prepared
            .query(params_from_iter(st.params.iter()))
            .map_err(db_err)?;
        while let Some(row) = rows.next().map_err(db_err)? {
            let b: Option<i64> = row.get(0).map_err(db_err)?;
            if let Some(b) = b {
                let n: i64 = row.get(1).map_err(db_err)?;
                counts.insert(b, n as u64);
            }
        }
        Ok(counts)
    }
}
