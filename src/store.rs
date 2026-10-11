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
use std::path::Path;

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

/// The store format; bumped when the schema changes, so `open` can tell a stale file.
pub const FORMAT: &str = "search-esque-store-1";

/// The distinct tokens the bundled SQLite's FTS5 makes of a text with the store's tokenizer
/// options, sorted. `dev/tokenizer_parity.py` holds `tokenize::tokens` to this.
pub fn fts_tokens(text: &str) -> Result<Vec<String>> {
    let conn = Connection::open_in_memory().map_err(db_err)?;
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='{}'); CREATE VIRTUAL TABLE v USING fts5vocab(t, 'row');",
        sql::TOKENIZER
    ))
    .map_err(db_err)?;
    conn.execute("INSERT INTO t VALUES (?)", [text])
        .map_err(db_err)?;
    let mut st = conn
        .prepare("SELECT term FROM v ORDER BY term")
        .map_err(db_err)?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(db_err)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)
}

pub struct Store {
    conn: Connection,
    mapping: Mapping,
}

impl Store {
    /// Builds a store file from records in one go. The file is written beside its final name
    /// and moved into place only when every record loaded, so a failure leaves nothing behind.
    pub fn build(path: &str, mapping: &Mapping, docs: &[Value]) -> Result<Store> {
        if Path::new(path).exists() {
            return Err(Error::new("db", format!("{} exists already", path))
                .hint("remove it, or load into a new file"));
        }
        let tmp = format!("{}.building.{}", path, std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let result = (|| {
            let store = Store::init(Connection::open(&tmp).map_err(db_err)?, mapping)?;
            store.load(docs)?;
            drop(store);
            std::fs::rename(&tmp, path)
                .map_err(|e| Error::new("db", format!("cannot move {} into place: {}", tmp, e)))?;
            Store::open(path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// An empty store file for a mapping. Fails if the file exists.
    pub fn create(path: &str, mapping: &Mapping) -> Result<Store> {
        if Path::new(path).exists() {
            return Err(Error::new("db", format!("{} exists already", path))
                .hint("remove it, or load into a new file"));
        }
        Store::init(Connection::open(path).map_err(db_err)?, mapping)
    }

    pub fn in_memory(mapping: &Mapping) -> Result<Store> {
        Store::init(Connection::open_in_memory().map_err(db_err)?, mapping)
    }

    fn init(conn: Connection, mapping: &Mapping) -> Result<Store> {
        for stmt in sql::schema(mapping) {
            conn.execute_batch(&stmt).map_err(db_err)?;
        }
        conn.execute(
            &format!(
                "INSERT INTO {} VALUES ('format', ?), ('mapping', ?)",
                sql::META
            ),
            [FORMAT, &mapping.to_json().to_string()],
        )
        .map_err(db_err)?;
        Ok(Store {
            conn,
            mapping: mapping.clone(),
        })
    }

    /// An existing store; the mapping is read from the file.
    pub fn open(path: &str) -> Result<Store> {
        if !Path::new(path).is_file() {
            return Err(Error::new("db", format!("{} is not a file", path))
                .hint("create it with `esque load`"));
        }
        let conn = Connection::open(path).map_err(db_err)?;
        let read = |key: &str| -> Result<String> {
            conn.query_row(
                &format!("SELECT value FROM {} WHERE key = ?", sql::META),
                [key],
                |r| r.get(0),
            )
            .map_err(|_| Error::new("db", format!("{} is not an esque store", path)))
        };
        let format = read("format").map_err(|e| {
            if read("mapping").is_ok() {
                Error::new(
                    "db",
                    format!("{} was built by an older build and has no format tag", path),
                )
                .hint("rebuild it with `esque load`")
            } else {
                e
            }
        })?;
        if format != FORMAT {
            return Err(Error::new(
                "db",
                format!(
                    "{} has store format {}, this build reads {}",
                    path, format, FORMAT
                ),
            )
            .hint("rebuild it with `esque load`"));
        }
        let v: Value =
            serde_json::from_str(&read("mapping")?).map_err(|e| Error::new("db", e.to_string()))?;
        let mapping = Mapping::from_json(&v)?;
        Ok(Store { conn, mapping })
    }

    pub fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    /// Inserts records (replacing any with the same `_id`) and rebuilds the text index, all
    /// in one transaction: a bad record leaves the store as it was.
    pub fn load(&self, docs: &[Value]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        {
            let mut ins = tx
                .prepare(&sql::insert_statement(&self.mapping))
                .map_err(db_err)?;
            for (i, d) in docs.iter().enumerate() {
                let row = sql::row_params(d, &self.mapping, i)?;
                ins.execute(params_from_iter(row.iter())).map_err(db_err)?;
            }
        }
        for stmt in sql::fts_fill(&self.mapping) {
            tx.execute(&stmt, []).map_err(db_err)?;
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
        let compiled = sql::compile(req, &self.mapping)?;
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        let result = self.run(&tx, req, &compiled);
        let _ = tx.execute_batch(&compiled.cleanup);
        match (result, tx.commit()) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(db_err(e)),
            (Ok(v), Ok(())) => Ok(v),
        }
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
            let mut st = tx.prepare(&h.sql).map_err(db_err)?;
            let mut rows = st
                .query(params_from_iter(h.params.iter()))
                .map_err(db_err)?;
            while let Some(row) = rows.next().map_err(db_err)? {
                let id: String = row.get(0).map_err(db_err)?;
                let text: String = row.get(1).map_err(db_err)?;
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
                let value = self
                    .aggregate(tx, a)
                    .map_err(|e| e.at(format!("aggs.{}", a.name)))?;
                out.insert(a.name.clone(), value);
            }
            Some(out)
        };
        Ok(shape::response(total as usize, hits, aggregations))
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
        let first = a.statements.first().ok_or_else(|| {
            Error::new(
                "sqlite",
                format!("aggregation '{}' compiled to nothing", a.name),
            )
        })?;
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
                let bounds: (Option<f64>, Option<f64>) = tx
                    .query_row(&first.sql, params_from_iter(first.params.iter()), |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .map_err(db_err)?;
                if let (Some(lo), Some(hi)) = bounds {
                    shape::histogram_bounds(lo, hi, *interval)?;
                }
                let counts = Store::bucket_counts(tx, &a.statements[1])?;
                shape::histogram(&counts, *interval, *min_doc_count)?
            }
            AggKind::DateHistogram {
                calendar_interval,
                min_doc_count,
                ..
            } => {
                let counts = Store::bucket_counts(tx, first)?;
                shape::date_histogram(&counts, *calendar_interval, *min_doc_count)?
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
