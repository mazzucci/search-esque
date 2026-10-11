//! SQL for SQLite: the schema a mapping needs, and the statements a request compiles to.
//!
//! This module generates text and parameters and never touches a database, so it belongs to
//! the core and compiles to WebAssembly. The executor in `store` runs what it produces; anyone
//! else may run it too, against a store built with [`schema`] and [`row_params`].
//!
//! The schema is one `docs` table with the original record in `_source`, a column per field,
//! and shadow columns: `f__cf`, the case-folded copy of a keyword (and of each element of a
//! keyword array), so `term` and `prefix` ignore case; `f__ts`, a date as epoch milliseconds
//! (an array of them for a date array), so ranges and buckets compare numbers. Array fields
//! hold a JSON array and are queried through `json_each`. Text fields are indexed in an FTS5
//! table, one row per value (so an array's elements never form a phrase together), with the
//! `unicode61` tokenizer and diacritics kept, which is exactly how [`crate::tokenize`] splits.
//!
//! A request runs as one read transaction: the matching ids are materialised once into a temp
//! table, and the total, the hits and every aggregation are computed from that set. The filter
//! is evaluated once, and hits and aggregations can never disagree about which records they
//! describe. Every value is a bound parameter; identifiers come only from the mapping, which
//! validates field names.

use crate::ast::*;
use crate::error::{Error, Result};
use crate::mapping::{Field, FieldType, Mapping};
use crate::timeutil::{parse_bound, parse_iso, MS_PER_DAY};
use crate::tokenize::tokens;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Param {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Param>,
}

pub const DOCS: &str = "docs";
pub const FTS: &str = "fts";
pub const MATCHED: &str = "esque_matched";
pub const META: &str = "esque_meta";
pub const SOURCE: &str = "_source";
/// The FTS5 tokenizer the profile is defined by (see `tokenize`).
pub const TOKENIZER: &str = "unicode61 remove_diacritics 0";

/// Double-quoted identifier. Field names are validated by the mapping (letters, digits, `_`
/// and `-`, no `__`); the quote is doubled anyway, as defence in depth.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn col(name: &str) -> String {
    format!("d.{}", ident(name))
}

fn field<'a>(m: &'a Mapping, name: &str) -> Result<&'a Field> {
    m.fields.get(name).ok_or_else(|| {
        Error::new(
            "$",
            format!("the request names a field the mapping lacks: '{}'", name),
        )
    })
}

// ---------------------------------------------------------------- schema

/// The table's columns in order: `_id`, `_source`, then per field its column and its shadow
/// columns. Declared types are affinities; numbers and arrays get none, so a value is stored
/// exactly as given (10.0 stays a real, an integer stays an integer).
pub fn columns(m: &Mapping) -> Vec<(String, &'static str)> {
    let mut cols = vec![
        ("_id".to_string(), "TEXT PRIMARY KEY"),
        (SOURCE.to_string(), "TEXT"),
    ];
    for (name, f) in &m.fields {
        let ty = if f.array {
            "TEXT"
        } else {
            match f.ty {
                FieldType::Keyword | FieldType::Text | FieldType::Date => "TEXT",
                FieldType::Number => "",
                FieldType::Boolean => "INTEGER",
            }
        };
        cols.push((name.clone(), ty));
        if f.ty == FieldType::Keyword {
            cols.push((format!("{}__cf", name), "TEXT"));
        }
        if f.ty == FieldType::Date {
            cols.push((
                format!("{}__ts", name),
                if f.array { "TEXT" } else { "INTEGER" },
            ));
        }
    }
    cols
}

fn text_fields(m: &Mapping) -> Vec<(&String, &Field)> {
    m.fields
        .iter()
        .filter(|(_, f)| f.ty == FieldType::Text)
        .collect()
}

/// The statements that create an empty store for a mapping.
pub fn schema(m: &Mapping) -> Vec<String> {
    let cols: Vec<String> = columns(m)
        .iter()
        .map(|(n, t)| format!("{} {}", ident(n), t).trim_end().to_string())
        .collect();
    let mut out = vec![
        format!("CREATE TABLE {} ({})", DOCS, cols.join(", ")),
        format!("CREATE TABLE {} (key TEXT PRIMARY KEY, value TEXT)", META),
    ];
    let texts = text_fields(m);
    if !texts.is_empty() {
        let names: Vec<String> = texts.iter().map(|(n, _)| ident(n)).collect();
        out.push(format!(
            "CREATE VIRTUAL TABLE {} USING fts5({} UNINDEXED, {}, tokenize='{}')",
            FTS,
            ident("_id"),
            names.join(", "),
            TOKENIZER
        ));
    }
    for (name, f) in &m.fields {
        if f.aggregatable && !f.array {
            // The column the filters read: the case-folded shadow for keywords, the timestamp
            // for dates, the field itself otherwise.
            let c = match f.ty {
                FieldType::Keyword => format!("{}__cf", name),
                FieldType::Date => format!("{}__ts", name),
                _ => name.clone(),
            };
            out.push(format!(
                "CREATE INDEX {} ON {} ({})",
                ident(&format!("ix_{}", name)),
                DOCS,
                ident(&c)
            ));
        }
    }
    out
}

/// `INSERT INTO docs VALUES (?, ...)`, one placeholder per column.
pub fn insert_statement(m: &Mapping) -> String {
    let n = columns(m).len();
    format!(
        "INSERT OR REPLACE INTO {} VALUES ({})",
        DOCS,
        vec!["?"; n].join(", ")
    )
}

/// Fills the FTS table from the docs table once all rows are in: one row for every record's
/// scalar text fields, and one row per element for each text array field.
pub fn fts_fill(m: &Mapping) -> Vec<String> {
    let texts = text_fields(m);
    if texts.is_empty() {
        return Vec::new();
    }
    let mut out = vec![format!("DELETE FROM {}", FTS)];
    let scalars: Vec<String> = texts
        .iter()
        .filter(|(_, f)| !f.array)
        .map(|(n, _)| ident(n))
        .collect();
    if !scalars.is_empty() {
        out.push(format!(
            "INSERT INTO {} ({}, {}) SELECT {}, {} FROM {}",
            FTS,
            ident("_id"),
            scalars.join(", "),
            ident("_id"),
            scalars.join(", "),
            DOCS
        ));
    }
    for (n, _) in texts.iter().filter(|(_, f)| f.array) {
        out.push(format!(
            "INSERT INTO {} ({}, {}) SELECT {}, j.value FROM {}, json_each({}) j",
            FTS,
            ident("_id"),
            ident(n),
            ident("_id"),
            DOCS,
            ident(n)
        ));
    }
    out
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

fn scalar_param(v: &Value, ty: FieldType) -> Param {
    match (ty, v) {
        (FieldType::Keyword | FieldType::Text, Value::String(s)) => Param::Text(s.clone()),
        (FieldType::Date, Value::String(s)) if parse_iso(s).is_some() => Param::Text(s.clone()),
        (FieldType::Number, Value::Number(n)) => match n.as_i64() {
            Some(i) => Param::Int(i),
            None => n.as_f64().map(Param::Real).unwrap_or(Param::Null),
        },
        (FieldType::Boolean, Value::Bool(b)) => Param::Int(*b as i64),
        _ => Param::Null,
    }
}

/// The record as `_source`: everything but the `_`-prefixed keys, as given.
pub fn source_json(doc: &Value) -> Value {
    match doc.as_object() {
        Some(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        None => Value::Object(Default::default()),
    }
}

/// The parameters for one record, in column order. A value that does not fit the field's
/// type is stored as empty (the evaluator treats it as empty too); `_source` keeps the record
/// as it was.
pub fn row_params(doc: &Value, m: &Mapping, index: usize) -> Result<Vec<Param>> {
    let mut out = vec![
        Param::Text(doc_id(doc, index)?),
        Param::Text(source_json(doc).to_string()),
    ];
    for (name, f) in &m.fields {
        let v = doc.get(name).unwrap_or(&Value::Null);
        if f.array {
            let items: Vec<&Value> = match v {
                Value::Array(a) => a.iter().filter(|x| !x.is_null()).collect(),
                Value::Null => Vec::new(),
                other => vec![other],
            };
            let kept: Vec<Value> = items
                .into_iter()
                .filter(|x| scalar_param(x, f.ty) != Param::Null)
                .cloned()
                .collect();
            out.push(Param::Text(Value::Array(kept.clone()).to_string()));
            if f.ty == FieldType::Keyword {
                let folded: Vec<Value> = kept
                    .iter()
                    .map(|x| Value::String(x.as_str().unwrap_or("").to_lowercase()))
                    .collect();
                out.push(Param::Text(Value::Array(folded).to_string()));
            }
            if f.ty == FieldType::Date {
                let stamps: Vec<Value> = kept
                    .iter()
                    .filter_map(|x| x.as_str().and_then(parse_iso))
                    .map(Value::from)
                    .collect();
                out.push(Param::Text(Value::Array(stamps).to_string()));
            }
            continue;
        }
        let p = match v {
            // A list where a single value belongs is empty, as in the evaluator.
            Value::Array(_) => Param::Null,
            _ => scalar_param(v, f.ty),
        };
        if f.ty == FieldType::Keyword {
            let cf = match &p {
                Param::Text(s) => Param::Text(s.to_lowercase()),
                _ => Param::Null,
            };
            out.push(p);
            out.push(cf);
        } else if f.ty == FieldType::Date {
            let ts = match &p {
                Param::Text(s) => parse_iso(s).map(Param::Int).unwrap_or(Param::Null),
                _ => Param::Null,
            };
            out.push(p);
            out.push(ts);
        } else {
            out.push(p);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- predicates

/// The column a comparison reads: the case-folded shadow for keywords, the timestamp for
/// dates, the field itself otherwise.
fn cmp_col(name: &str, f: &Field) -> String {
    match f.ty {
        FieldType::Keyword => col(&format!("{}__cf", name)),
        FieldType::Date => col(&format!("{}__ts", name)),
        _ => col(name),
    }
}

/// The column an aggregation groups or measures: the raw keyword (case preserved), the
/// timestamp for dates, the field itself otherwise.
fn agg_col(name: &str, f: &Field) -> String {
    match f.ty {
        FieldType::Date => col(&format!("{}__ts", name)),
        _ => col(name),
    }
}

/// A condition on a field's value: directly on the column, or on each element of an array.
fn per_value(f: &Field, column: &str, cond: impl Fn(&str) -> String) -> String {
    if f.array {
        format!(
            "EXISTS (SELECT 1 FROM json_each({}) j WHERE {})",
            column,
            cond("j.value")
        )
    } else {
        cond(column)
    }
}

fn scalar_to_param(s: &Scalar, ty: FieldType) -> Param {
    match (ty, s) {
        (FieldType::Keyword, Scalar::Str(x)) => Param::Text(x.to_lowercase()),
        (FieldType::Date, Scalar::Str(x)) => parse_iso(x).map(Param::Int).unwrap_or(Param::Null),
        (_, Scalar::Str(x)) => Param::Text(x.clone()),
        (_, Scalar::Num(n)) => Param::Real(*n),
        (_, Scalar::Bool(b)) => Param::Int(*b as i64),
    }
}

fn fts_quote(token: &str) -> String {
    format!("\"{}\"", token.replace('"', "\"\""))
}

fn fts_in(field: &str, expr: String, params: &mut Vec<Param>) -> String {
    params.push(Param::Text(format!("{} : {}", ident(field), expr)));
    format!(
        "{} IN (SELECT {} FROM {} WHERE {} MATCH ?)",
        col("_id"),
        ident("_id"),
        FTS,
        FTS
    )
}

/// A leaf wrapped so that an empty field never yields NULL: `COALESCE((...), 0)`. A NULL would
/// poison `NOT` and `AND`; with the wrap, an empty field simply does not match, and its
/// negation does, which is the profile's rule.
fn leaf(expr: String) -> String {
    format!("COALESCE(({}), 0)", expr)
}

/// The WHERE predicate for a query, pushing bound parameters in order of appearance.
pub fn predicate(q: &Query, m: &Mapping, params: &mut Vec<Param>) -> Result<String> {
    Ok(match q {
        Query::MatchAll => "1".to_string(),
        Query::Bool {
            must,
            should,
            must_not,
            filter,
            minimum_should_match,
        } => {
            let mut parts: Vec<String> = Vec::new();
            for c in must.iter().chain(filter) {
                parts.push(format!("({})", predicate(c, m, params)?));
            }
            for c in must_not {
                parts.push(format!("NOT ({})", predicate(c, m, params)?));
            }
            if !should.is_empty() {
                // Lucene's rule, as the evaluator: with no required clause at least one
                // optional clause must match, whatever minimum_should_match says.
                let needed = if must.is_empty() && filter.is_empty() {
                    minimum_should_match.unwrap_or(1).max(1)
                } else {
                    minimum_should_match.unwrap_or(0)
                };
                if needed >= 1 {
                    let mut alts = Vec::new();
                    for c in should {
                        alts.push(format!("({})", predicate(c, m, params)?));
                    }
                    parts.push(format!("({})", alts.join(" OR ")));
                }
            }
            if parts.is_empty() {
                "1".to_string()
            } else {
                parts.join(" AND ")
            }
        }
        Query::Term { field: name, value } => {
            let f = field(m, name)?;
            // A date without a time part means the whole day, as in Elasticsearch.
            if let (FieldType::Date, Scalar::Str(s)) = (f.ty, value) {
                if s.len() == 10 {
                    let (Some(lo), Some(hi)) = (parse_bound(s, false), parse_bound(s, true)) else {
                        return Ok("0".to_string());
                    };
                    params.push(Param::Int(lo));
                    params.push(Param::Int(hi));
                    return Ok(leaf(per_value(f, &cmp_col(name, f), |v| {
                        format!("{} >= ? AND {} <= ?", v, v)
                    })));
                }
            }
            let p = scalar_to_param(value, f.ty);
            if p == Param::Null {
                return Ok("0".to_string());
            }
            params.push(p);
            leaf(per_value(f, &cmp_col(name, f), |v| format!("{} = ?", v)))
        }
        Query::Terms {
            field: name,
            values,
        } => {
            let f = field(m, name)?;
            let mut alts: Vec<String> = Vec::new();
            for v in values {
                match (f.ty, v) {
                    // A date without a time part means the whole day, as for `term`.
                    (FieldType::Date, Scalar::Str(s)) if s.len() == 10 => {
                        if let (Some(lo), Some(hi)) = (parse_bound(s, false), parse_bound(s, true))
                        {
                            params.push(Param::Int(lo));
                            params.push(Param::Int(hi));
                            alts.push("(\u{1} >= ? AND \u{1} <= ?)".to_string());
                        }
                    }
                    _ => {
                        let p = scalar_to_param(v, f.ty);
                        if p != Param::Null {
                            params.push(p);
                            alts.push("\u{1} = ?".to_string());
                        }
                    }
                }
            }
            if alts.is_empty() {
                return Ok("0".to_string());
            }
            let joined = alts.join(" OR ");
            leaf(per_value(f, &cmp_col(name, f), |v| {
                joined.replace('\u{1}', v)
            }))
        }
        Query::Range {
            field: name,
            gte,
            gt,
            lte,
            lt,
        } => {
            let f = field(m, name)?;
            let mut conds: Vec<(&str, Param)> = Vec::new();
            for (op, bound, round_up) in [
                (">=", gte, false),
                (">", gt, true),
                ("<=", lte, true),
                ("<", lt, false),
            ] {
                if let Some(b) = bound {
                    let p = match (f.ty, b) {
                        (FieldType::Date, Scalar::Str(s)) => parse_bound(s, round_up)
                            .map(Param::Int)
                            .unwrap_or(Param::Null),
                        _ => scalar_to_param(b, f.ty),
                    };
                    if p == Param::Null {
                        return Ok("0".to_string());
                    }
                    conds.push((op, p));
                }
            }
            let ops: Vec<&str> = conds.iter().map(|(op, _)| *op).collect();
            params.extend(conds.into_iter().map(|(_, p)| p));
            leaf(per_value(f, &cmp_col(name, f), |v| {
                ops.iter()
                    .map(|op| format!("{} {} ?", v, op))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            }))
        }
        Query::Exists { field: name } => {
            let f = field(m, name)?;
            // Dates: a value that did not parse is empty, so the timestamp column decides.
            let c = if f.ty == FieldType::Date {
                col(&format!("{}__ts", name))
            } else {
                col(name)
            };
            if f.array {
                leaf(format!("json_array_length({}) > 0", c))
            } else {
                leaf(format!("{} IS NOT NULL", c))
            }
        }
        Query::Match {
            field: name,
            query,
            operator,
        } => {
            field(m, name)?;
            let toks: Vec<String> = tokens(query).into_iter().map(|t| fts_quote(&t)).collect();
            if toks.is_empty() {
                return Ok("0".to_string());
            }
            let joined = match operator {
                Operator::Or => toks.join(" OR "),
                Operator::And => toks.join(" "),
            };
            leaf(fts_in(name, format!("({})", joined), params))
        }
        Query::MatchPhrase { field: name, query } => {
            field(m, name)?;
            let toks = tokens(query);
            if toks.is_empty() {
                return Ok("0".to_string());
            }
            leaf(fts_in(name, fts_quote(&toks.join(" ")), params))
        }
        Query::Prefix { field: name, value } => {
            let f = field(m, name)?;
            let want = value.to_lowercase();
            if f.ty == FieldType::Text {
                // A prefix with a separator inside cannot start any token.
                if tokens(value) != vec![want.clone()] {
                    return Ok("0".to_string());
                }
                return Ok(leaf(fts_in(name, format!("{}*", fts_quote(&want)), params)));
            }
            let pattern = format!(
                "{}%",
                want.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            params.push(Param::Text(pattern));
            leaf(per_value(f, &cmp_col(name, f), |v| {
                format!("{} LIKE ? ESCAPE '\\'", v)
            }))
        }
    })
}

// ---------------------------------------------------------------- the request

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompiledAgg {
    pub name: String,
    pub kind: AggKind,
    /// One statement, or one per bucket for `range`.
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Compiled {
    /// Creates the temp table of matching ids. Run first, inside the transaction.
    pub materialize: Statement,
    /// Drops it. Run last.
    pub cleanup: String,
    /// `SELECT COUNT(*)` of the hits (after `post_filter`).
    pub total: Statement,
    /// The page of hits: `_id` and `_source`; absent when `size` is 0.
    pub hits: Option<Statement>,
    pub aggs: Vec<CompiledAgg>,
}

fn from_matched() -> String {
    format!("FROM {} d JOIN {} USING ({})", DOCS, MATCHED, ident("_id"))
}

/// Floor of `x` in SQLite, which has no `floor()` unless built with math functions.
fn floor(x: &str) -> String {
    format!(
        "(CAST(({x}) AS INTEGER) - (({x}) < CAST(({x}) AS INTEGER)))",
        x = x
    )
}

/// The start of the calendar bucket holding a millisecond timestamp, as milliseconds, through
/// SQLite's date functions. Floor division keeps dates before 1970 on the right day.
fn date_bucket(ts: &str, iv: CalendarInterval) -> String {
    let modifier = match iv {
        CalendarInterval::Day => "start of day",
        CalendarInterval::Month => "start of month",
        CalendarInterval::Year => "start of year",
    };
    let seconds = format!("(({c}) - ((({c}) % 1000) + 1000) % 1000) / 1000", c = ts);
    format!(
        "CAST(strftime('%s', {}, 'unixepoch', '{}') AS INTEGER) * 1000",
        seconds, modifier
    )
}

fn compile_agg(a: &Agg, m: &Mapping) -> Result<CompiledAgg> {
    let name = a.kind.field();
    let f = field(m, name)?;
    let column = agg_col(name, f);
    let from = from_matched();
    let id = ident("_id");
    let stmt = |sql: String, params: Vec<Param>| Statement { sql, params };
    // Each value of the field once per record, for the aggregations that count values.
    let values_from = if f.array {
        format!("{}, json_each({}) j", from, column)
    } else {
        from.clone()
    };
    let value = if f.array {
        "j.value".to_string()
    } else {
        column.clone()
    };
    let statements = match &a.kind {
        AggKind::Terms { .. } => {
            if f.array {
                vec![stmt(
                    format!(
                        "SELECT j.value, COUNT(DISTINCT d.{id}) {vf} GROUP BY j.value",
                        id = id,
                        vf = values_from
                    ),
                    vec![],
                )]
            } else {
                vec![stmt(
                    format!(
                        "SELECT {col}, COUNT(*) {from} WHERE {col} IS NOT NULL GROUP BY {col}",
                        col = column,
                        from = from
                    ),
                    vec![],
                )]
            }
        }
        AggKind::Range { ranges, .. } => crate::shape::ordered_ranges(ranges)
            .into_iter()
            .map(|r| {
                let mut params = Vec::new();
                let mut ops = Vec::new();
                if let Some(lo) = r.from {
                    ops.push(">=");
                    params.push(Param::Real(lo));
                }
                if let Some(hi) = r.to {
                    ops.push("<");
                    params.push(Param::Real(hi));
                }
                let cond = per_value(f, &column, |v| {
                    ops.iter()
                        .map(|op| format!("{} {} ?", v, op))
                        .collect::<Vec<_>>()
                        .join(" AND ")
                });
                stmt(format!("SELECT COUNT(*) {} WHERE {}", from, cond), params)
            })
            .collect(),
        AggKind::Histogram { interval, .. } => {
            // First the bounds, so the executor can refuse a histogram that would overflow or
            // exceed the bucket cap before bucketing; then the buckets. The quotient is computed
            // once, under an alias, so the interval binds once and the floor expression can
            // mention it three times.
            vec![
                stmt(
                    format!("SELECT MIN({v}), MAX({v}) {vf}", v = value, vf = values_from),
                    vec![],
                ),
                stmt(
                    format!(
                        "SELECT {b} AS b, COUNT(DISTINCT id) FROM (SELECT d.{id} AS id, {v} / ? AS q {vf} WHERE {v} IS NOT NULL) GROUP BY b",
                        b = floor("q"),
                        id = id,
                        v = value,
                        vf = values_from
                    ),
                    vec![Param::Real(*interval)],
                ),
            ]
        }
        AggKind::DateHistogram {
            calendar_interval, ..
        } => vec![stmt(
            format!(
                "SELECT b, COUNT(DISTINCT id) FROM (SELECT d.{id} AS id, {b} AS b {vf} WHERE {v} IS NOT NULL) GROUP BY b",
                id = id,
                b = date_bucket(&value, *calendar_interval),
                vf = values_from,
                v = value
            ),
            vec![],
        )],
        AggKind::ValueCount { .. } => {
            if f.array && f.ty == FieldType::Keyword {
                // Elasticsearch counts a keyword array's values through sorted-set doc values,
                // so a value repeated in one record counts once.
                vec![stmt(
                    format!(
                        "SELECT COUNT(*) FROM (SELECT DISTINCT d.{id}, j.value {vf})",
                        id = id,
                        vf = values_from
                    ),
                    vec![],
                )]
            } else {
                vec![stmt(format!("SELECT COUNT({}) {}", value, values_from), vec![])]
            }
        }
        AggKind::Cardinality { .. } => vec![stmt(
            format!("SELECT COUNT(DISTINCT {}) {}", value, values_from),
            vec![],
        )],
        AggKind::Avg { .. }
        | AggKind::Min { .. }
        | AggKind::Max { .. }
        | AggKind::Sum { .. }
        | AggKind::Stats { .. } => vec![stmt(
            format!(
                // Sums run on doubles, as Elasticsearch does for every numeric type, so the engines agree.
                "SELECT COUNT({v}), CAST(MIN({v}) AS REAL), CAST(MAX({v}) AS REAL), TOTAL(CAST({v} AS REAL)) {vf}",
                v = value,
                vf = values_from
            ),
            vec![],
        )],
    };
    Ok(CompiledAgg {
        name: a.name.clone(),
        kind: a.kind.clone(),
        statements,
    })
}

/// Compiles a request. The caller runs `materialize`, then `total`, `hits` and the
/// aggregations, then `cleanup`, all inside one transaction.
pub fn compile(req: &Request, m: &Mapping) -> Result<Compiled> {
    let mut params = Vec::new();
    let where_ = predicate(&req.query, m, &mut params)?;
    let materialize = Statement {
        sql: format!(
            "CREATE TEMP TABLE {} AS SELECT d.{} FROM {} d WHERE {}",
            MATCHED,
            ident("_id"),
            DOCS,
            where_
        ),
        params,
    };
    let (pf_sql, pf_params) = match &req.post_filter {
        Some(pf) => {
            let mut p = Vec::new();
            let s = predicate(pf, m, &mut p)?;
            (format!(" WHERE {}", s), p)
        }
        None => (String::new(), Vec::new()),
    };
    let total = Statement {
        sql: format!("SELECT COUNT(*) {}{}", from_matched(), pf_sql),
        params: pf_params.clone(),
    };
    let hits = if req.size == 0 {
        None
    } else {
        let mut order: Vec<String> = Vec::new();
        for s in &req.sort {
            let f = field(m, &s.field)?;
            let c = agg_col(&s.field, f);
            let dir = match s.order {
                Order::Asc => "ASC",
                Order::Desc => "DESC",
            };
            order.push(format!("({} IS NULL) ASC", c));
            order.push(format!("{} {}", c, dir));
        }
        order.push(format!("{} ASC", col("_id")));
        let mut params = pf_params;
        params.push(Param::Int(req.size.min(MAX_SIZE) as i64));
        params.push(Param::Int(req.from.min(MAX_SIZE) as i64));
        Some(Statement {
            sql: format!(
                "SELECT d.{}, d.{} {}{} ORDER BY {} LIMIT ? OFFSET ?",
                ident("_id"),
                ident(SOURCE),
                from_matched(),
                pf_sql,
                order.join(", ")
            ),
            params,
        })
    };
    let mut aggs = Vec::new();
    for a in &req.aggs {
        aggs.push(compile_agg(a, m)?);
    }
    Ok(Compiled {
        materialize,
        cleanup: format!("DROP TABLE {}", MATCHED),
        total,
        hits,
        aggs,
    })
}

/// Milliseconds in a day, re-exported for callers that round date terms.
pub const DAY_MS: i64 = MS_PER_DAY;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parse_request;
    use serde_json::json;

    fn mapping() -> Mapping {
        Mapping::from_json(&json!({"properties": {
            "state": {"type": "keyword"}, "labels": {"type": "keyword", "array": true},
            "title": {"type": "text"}, "notes": {"type": "text", "array": true},
            "comments": {"type": "long"}, "created_at": {"type": "date"},
            "reprinted_at": {"type": "date", "array": true}
        }}))
        .unwrap()
    }

    #[test]
    fn values_are_parameters_and_leaves_are_wrapped() {
        let m = mapping();
        let req = parse_request(
            &json!({"query": {"bool": {"must_not": {"term": {"state": "OPEN"}}, "filter": [{"terms": {"labels": ["Bug"]}}, {"range": {"created_at": {"lte": "2026-09-01"}}}]}}}),
            &m,
        )
        .unwrap();
        let c = compile(&req, &m).unwrap();
        let sql = &c.materialize.sql;
        assert!(
            sql.contains("NOT (COALESCE((d.\"state__cf\" = ?), 0))"),
            "{}",
            sql
        );
        assert!(sql.contains("json_each(d.\"labels__cf\")"), "{}", sql);
        assert!(
            !sql.contains("OPEN") && !sql.contains("Bug"),
            "values must be bound: {}",
            sql
        );
        assert_eq!(
            c.materialize.params,
            vec![
                Param::Text("bug".into()),
                Param::Int(parse_bound("2026-09-01", true).unwrap()),
                Param::Text("open".into())
            ]
        );
    }

    #[test]
    fn match_uses_fts_with_a_column_filter() {
        let m = mapping();
        let req = parse_request(
            &json!({"query": {"match": {"title": {"query": "facet counts", "operator": "and"}}}}),
            &m,
        )
        .unwrap();
        let c = compile(&req, &m).unwrap();
        assert_eq!(
            c.materialize.params,
            vec![Param::Text("\"title\" : (\"facet\" \"counts\")".into())]
        );
        let req = parse_request(
            &json!({"query": {"match_phrase": {"title": "chat completions"}}}),
            &m,
        )
        .unwrap();
        assert_eq!(
            compile(&req, &m).unwrap().materialize.params,
            vec![Param::Text("\"title\" : \"chat completions\"".into())]
        );
    }

    #[test]
    fn schema_has_shadows_source_and_fts_rows_per_value() {
        let m = mapping();
        let s = schema(&m);
        assert!(s[0].contains("\"_source\" TEXT") && s[0].contains("\"state__cf\" TEXT"));
        assert!(
            s[0].contains("\"created_at__ts\" INTEGER")
                && s[0].contains("\"reprinted_at__ts\" TEXT")
        );
        assert!(s
            .iter()
            .any(|x| x.contains("fts5") && x.contains("remove_diacritics 0")));
        let fill = fts_fill(&m);
        assert!(
            fill.iter().any(|x| x.contains("json_each(\"notes\")")),
            "{:?}",
            fill
        );
    }

    #[test]
    fn date_arrays_get_a_timestamp_array() {
        let m = mapping();
        let doc = json!({"_id": "1", "reprinted_at": ["2026-01-01", "not a date", null], "notes": ["a", "b"]});
        let row = row_params(&doc, &m, 0).unwrap();
        let cols = columns(&m);
        let at = |name: &str| cols.iter().position(|(n, _)| n == name).unwrap();
        assert_eq!(
            row[at("reprinted_at")],
            Param::Text("[\"2026-01-01\"]".into())
        );
        assert_eq!(
            row[at("reprinted_at__ts")],
            Param::Text(format!("[{}]", parse_iso("2026-01-01").unwrap()))
        );
        assert_eq!(
            row[at("_source")],
            Param::Text(
                json!({"reprinted_at": ["2026-01-01", "not a date", null], "notes": ["a", "b"]})
                    .to_string()
            )
        );
        let req = parse_request(&json!({"query": {"range": {"reprinted_at": {"gte": "2025-01-01"}}}, "aggs": {"d": {"date_histogram": {"field": "reprinted_at", "calendar_interval": "year"}}}}), &m).unwrap();
        let c = compile(&req, &m).unwrap();
        assert!(
            c.materialize
                .sql
                .contains("json_each(d.\"reprinted_at__ts\")"),
            "{}",
            c.materialize.sql
        );
        assert!(
            c.aggs[0].statements[0]
                .sql
                .contains("json_each(d.\"reprinted_at__ts\")"),
            "{}",
            c.aggs[0].statements[0].sql
        );
    }

    #[test]
    fn a_date_term_without_time_means_the_day() {
        let m = mapping();
        let req = parse_request(
            &json!({"query": {"term": {"created_at": "2026-10-08"}}}),
            &m,
        )
        .unwrap();
        let c = compile(&req, &m).unwrap();
        assert!(
            c.materialize
                .sql
                .contains(">= ? AND d.\"created_at__ts\" <= ?"),
            "{}",
            c.materialize.sql
        );
        assert_eq!(c.materialize.params.len(), 2);
    }
}
