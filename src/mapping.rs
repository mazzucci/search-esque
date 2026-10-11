//! The mapping: which fields exist, their types, and what each allows.
//!
//! The JSON form mirrors the shape of an Elasticsearch mapping so that it reads familiar:
//!
//! ```json
//! {"properties": {
//!   "state":  {"type": "keyword"},
//!   "labels": {"type": "keyword", "array": true},
//!   "title":  {"type": "text"},
//!   "comments": {"type": "long"},
//!   "created_at": {"type": "date"}
//! }}
//! ```
//!
//! Differences from Elasticsearch, on purpose: a field is multi-valued only when it says
//! `array: true` (Elasticsearch lets any field hold a list); the numeric types collapse into
//! one `number` type; and `aggregatable` is explicit (default: everything but `text`), which
//! plays the role of Elasticsearch's doc values and Tantivy's fast fields.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Keyword,
    Text,
    Number,
    Boolean,
    Date,
}

pub const TYPE_NAMES: &[&str] = &[
    "keyword", "text", "long", "integer", "double", "float", "number", "boolean", "date",
];

const FIELD_OPTIONS: &[&str] = &["type", "array", "aggregatable"];

impl FieldType {
    /// Accepts Elasticsearch's type names; every numeric type becomes `Number`.
    pub fn parse(name: &str) -> Option<FieldType> {
        Some(match name {
            "keyword" => FieldType::Keyword,
            "text" => FieldType::Text,
            "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float"
            | "scaled_float" | "unsigned_long" | "number" => FieldType::Number,
            "boolean" | "bool" => FieldType::Boolean,
            "date" => FieldType::Date,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            FieldType::Keyword => "keyword",
            FieldType::Text => "text",
            FieldType::Number => "number",
            FieldType::Boolean => "boolean",
            FieldType::Date => "date",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    #[serde(rename = "type")]
    pub ty: FieldType,
    pub array: bool,
    pub aggregatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Mapping {
    pub fields: BTreeMap<String, Field>,
}

impl Mapping {
    pub fn from_json(v: &Value) -> Result<Mapping> {
        let props = v
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                Error::new("$", "a mapping is an object with \"properties\"")
                    .hint(r#"{"properties": {"state": {"type": "keyword"}}}"#)
            })?;
        let mut fields = BTreeMap::new();
        for (name, spec) in props {
            let path = format!("$.properties.{}", name);
            if name.is_empty() || name.starts_with('_') || name.contains('.') {
                return Err(Error::new(
                    &path,
                    "field names must not be empty, start with _ or contain a dot",
                ));
            }
            let obj = spec
                .as_object()
                .ok_or_else(|| Error::new(&path, "a field definition is an object"))?;
            for k in obj.keys() {
                if !FIELD_OPTIONS.contains(&k.as_str()) {
                    return Err(Error::new(
                        format!("{}.{}", path, k),
                        "not a supported field option",
                    )
                    .supported(FIELD_OPTIONS));
                }
            }
            let tname = obj.get("type").and_then(Value::as_str).ok_or_else(|| {
                Error::new(format!("{}.type", path), "every field needs a type")
                    .supported(TYPE_NAMES)
            })?;
            let ty = FieldType::parse(tname).ok_or_else(|| {
                Error::new(
                    format!("{}.type", path),
                    format!("unknown type '{}'", tname),
                )
                .supported(TYPE_NAMES)
            })?;
            let array = match obj.get("array") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(Error::new(
                        format!("{}.array", path),
                        "array is true or false",
                    ))
                }
            };
            let aggregatable = match obj.get("aggregatable") {
                None => ty != FieldType::Text,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(Error::new(
                        format!("{}.aggregatable", path),
                        "aggregatable is true or false",
                    ))
                }
            };
            if ty == FieldType::Text && aggregatable {
                return Err(Error::new(
                    format!("{}.aggregatable", path),
                    "text fields cannot be aggregated",
                )
                .hint("add a keyword field for the aggregation"));
            }
            fields.insert(
                name.clone(),
                Field {
                    ty,
                    array,
                    aggregatable,
                },
            );
        }
        Ok(Mapping { fields })
    }

    /// The canonical JSON form, with every option spelled out.
    pub fn to_json(&self) -> Value {
        let props: serde_json::Map<String, Value> = self
            .fields
            .iter()
            .map(|(n, f)| {
                (
                    n.clone(),
                    json!({"type": f.ty.name(), "array": f.array, "aggregatable": f.aggregatable}),
                )
            })
            .collect();
        json!({ "properties": props })
    }

    /// The field, or an error at `path` with a "did you mean" hint when a name is close.
    pub fn field(&self, path: &str, name: &str) -> Result<&Field> {
        self.fields.get(name).ok_or_else(|| {
            let mut e = Error::new(path, format!("unknown field '{}'", name));
            if let Some(s) = suggest(name, self.fields.keys()) {
                e = e.hint(format!("did you mean '{}'?", s));
            }
            e
        })
    }

    pub fn names(&self) -> Vec<&str> {
        self.fields.keys().map(String::as_str).collect()
    }
}

/// The closest name within two edits, if any.
pub fn suggest<'a>(word: &str, options: impl Iterator<Item = &'a String>) -> Option<&'a String> {
    options
        .map(|o| (edit_distance(word, o), o))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, o)| o)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_close_names() {
        let m = Mapping::from_json(
            &json!({"properties": {"labels": {"type": "keyword", "array": true}}}),
        )
        .unwrap();
        let e = m.field("query.term", "label").unwrap_err();
        assert_eq!(e.hint.as_deref(), Some("did you mean 'labels'?"));
        assert_eq!(e.path, "query.term");
    }

    #[test]
    fn text_is_not_aggregatable_by_default() {
        let m = Mapping::from_json(
            &json!({"properties": {"title": {"type": "text"}, "n": {"type": "long"}}}),
        )
        .unwrap();
        assert!(!m.fields["title"].aggregatable);
        assert!(m.fields["n"].aggregatable);
        assert_eq!(m.fields["n"].ty, FieldType::Number);
    }
}
