//! The SQLite store: a file (or memory) built from a mapping and records, searched through
//! `exec::search` with rusqlite as the driver. The one module that does I/O; behind the `sqlite`
//! feature so the core still compiles to WebAssembly without it.

use crate::error::{Error, Result};
use crate::exec::{self, Executor, SqlValue};
use crate::mapping::Mapping;
use crate::sql::{self, Param, Statement};
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{params_from_iter, Connection, ToSql};
use serde_json::Value;
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

/// rusqlite as an `Executor`.
struct Conn<'a>(&'a Connection);

impl Executor for Conn<'_> {
    fn query(&mut self, st: &Statement) -> Result<Vec<Vec<SqlValue>>> {
        let mut prepared = self.0.prepare(&st.sql).map_err(db_err)?;
        let n = prepared.column_count();
        let rows = prepared
            .query_map(params_from_iter(st.params.iter()), |r| {
                (0..n)
                    .map(|i| {
                        Ok(match r.get_ref(i)? {
                            ValueRef::Null => SqlValue::Null,
                            ValueRef::Integer(i) => SqlValue::Int(i),
                            ValueRef::Real(x) => SqlValue::Real(x),
                            ValueRef::Text(t) => {
                                SqlValue::Text(String::from_utf8_lossy(t).into_owned())
                            }
                            ValueRef::Blob(_) => SqlValue::Null,
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    fn execute(&mut self, st: &Statement) -> Result<()> {
        if st.params.is_empty() {
            self.0.execute_batch(&st.sql).map_err(db_err)
        } else {
            self.0
                .execute(&st.sql, params_from_iter(st.params.iter()))
                .map(|_| ())
                .map_err(db_err)
        }
    }
}

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
    pub fn search(&self, req: &crate::ast::Request) -> Result<Value> {
        exec::search(&mut Conn(&self.conn), req, &self.mapping)
    }
}
