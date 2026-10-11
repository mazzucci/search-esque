//! The WebAssembly surface (feature `wasm`): the same functions the command line offers, for a
//! page. Everything crosses the boundary as JSON strings; a refusal comes back as
//! `{"error": {...}}`. Running a request needs SQLite in the browser: the page passes a
//! callback that executes one statement and returns its rows, and the core's executor does the
//! rest, so the page runs exactly the logic the native store runs.

use crate::dsl::{parse_request, to_elasticsearch};
use crate::error::{Error, Result};
use crate::exec::{self, Executor, SqlValue};
use crate::mapping::Mapping;
use crate::sql::Statement;
use serde::Serialize;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

fn out<T: Serialize>(r: Result<T>) -> String {
    match r {
        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|e| {
            json!({"error": {"path": "$", "message": e.to_string()}}).to_string()
        }),
        Err(e) => e.to_json().to_string(),
    }
}

fn mapping_of(text: &str) -> Result<Mapping> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| Error::new("mapping", format!("the mapping is not valid JSON: {}", e)))?;
    Mapping::from_json(&v)
}

fn request_of(text: &str) -> Result<Value> {
    serde_json::from_str(text)
        .map_err(|e| Error::new("$", format!("the request is not valid JSON: {}", e)))
}

/// The AST and the canonical Elasticsearch form of a request, or the refusal.
#[wasm_bindgen]
pub fn parse(mapping: &str, request: &str) -> String {
    out((|| {
        let m = mapping_of(mapping)?;
        let req = parse_request(&request_of(request)?, &m)?;
        Ok(json!({"ast": req, "elasticsearch": to_elasticsearch(&req)}))
    })())
}

/// The statements and bound parameters a request compiles to, or the refusal.
#[wasm_bindgen]
pub fn compile(mapping: &str, request: &str) -> String {
    out((|| {
        let m = mapping_of(mapping)?;
        let req = parse_request(&request_of(request)?, &m)?;
        serde_json::to_value(crate::sql::compile(&req, &m)?)
            .map_err(|e| Error::new("$", e.to_string()))
    })())
}

/// The canonical mapping, or the refusal.
#[wasm_bindgen]
pub fn mapping(mapping: &str) -> String {
    out(mapping_of(mapping).map(|m| m.to_json()))
}

/// The statements that create an empty store for a mapping, and the insert statement.
#[wasm_bindgen]
pub fn schema(mapping: &str) -> String {
    out(mapping_of(mapping).map(
        |m| json!({"create": crate::sql::schema(&m), "insert": crate::sql::insert_statement(&m)}),
    ))
}

/// The tokens the profile splits a text into.
#[wasm_bindgen]
pub fn tokens(text: &str) -> String {
    serde_json::to_string(&crate::tokenize::tokens(text)).unwrap_or_default()
}

/// A JavaScript function `(sql, paramsJson) => json` as the executor. It returns a JSON string:
/// `{"rows": [[...], ...]}` or `{"error": "..."}`. Parameters arrive as a JSON array in
/// statement order; null, numbers and strings, as SQLite's own types.
struct JsExecutor<'a>(&'a js_sys::Function);

impl Executor for JsExecutor<'_> {
    fn query(&mut self, st: &Statement) -> Result<Vec<Vec<SqlValue>>> {
        let params =
            serde_json::to_string(&st.params).map_err(|e| Error::new("sqlite", e.to_string()))?;
        let answer = self
            .0
            .call2(
                &JsValue::NULL,
                &JsValue::from_str(&st.sql),
                &JsValue::from_str(&params),
            )
            .map_err(|e| Error::new("sqlite", format!("the run callback threw: {:?}", e)))?;
        let text = answer
            .as_string()
            .ok_or_else(|| Error::new("sqlite", "the run callback must return a JSON string"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            Error::new(
                "sqlite",
                format!("the run callback returned invalid JSON: {}", e),
            )
        })?;
        if let Some(err) = v.get("error") {
            return Err(Error::new(
                "sqlite",
                err.as_str().unwrap_or("the statement failed").to_string(),
            ));
        }
        let rows = v.get("rows").and_then(Value::as_array).ok_or_else(|| {
            Error::new("sqlite", "the run callback must return {\"rows\": [...]}")
        })?;
        Ok(rows
            .iter()
            .map(|r| {
                r.as_array()
                    .map(|cells| cells.iter().map(SqlValue::from_json).collect())
                    .unwrap_or_default()
            })
            .collect())
    }
}

/// Runs a request against a store the page holds, through its `run(sql, paramsJson)` callback,
/// and answers in Elasticsearch's response shape, or the refusal.
#[wasm_bindgen]
pub fn search(mapping: &str, request: &str, run: &js_sys::Function) -> String {
    out((|| {
        let m = mapping_of(mapping)?;
        let req = parse_request(&request_of(request)?, &m)?;
        exec::search(&mut JsExecutor(run), &req, &m)
    })())
}
