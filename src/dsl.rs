//! The walk over Elasticsearch JSON into the AST, and the printer back.
//!
//! The walk is a whitelist. Every object is checked key by key; a key that is not in the
//! profile is refused with the path it sits at and the list of what is accepted there. The
//! messages distinguish three cases: a clause Elasticsearch has that the profile does not
//! support (`wildcard`), one that is planned but not here yet (`query_string`), and a name
//! that is not Elasticsearch at all.
//!
//! [`to_elasticsearch`] prints an AST back to a canonical Elasticsearch request. Parsing
//! that output gives the same AST: the round-trip property test holds the two together.

use crate::ast::*;
use crate::error::{Error, Result};
use crate::mapping::{FieldType, Mapping};
use crate::timeutil::{date_hint, parse_iso};
use serde_json::{json, Map, Value};

pub const QUERY_CLAUSES: &[&str] = &[
    "bool",
    "term",
    "terms",
    "range",
    "exists",
    "match",
    "match_phrase",
    "prefix",
    "match_all",
];

const QUERY_PLANNED: &[&str] = &["query_string"];

const QUERY_UNSUPPORTED: &[&str] = &[
    "nested",
    "script",
    "script_score",
    "wildcard",
    "regexp",
    "fuzzy",
    "function_score",
    "multi_match",
    "match_phrase_prefix",
    "ids",
    "geo_distance",
    "geo_bounding_box",
    "more_like_this",
    "constant_score",
    "dis_max",
    "boosting",
    "span_term",
    "span_near",
    "terms_set",
    "knn",
    "intervals",
    "percolate",
    "rank_feature",
    "simple_query_string",
    "combined_fields",
    "match_none",
    "has_child",
    "has_parent",
    "parent_id",
    "wrapper",
    "pinned",
    "distance_feature",
    "shape",
    "geo_shape",
];

pub const AGG_TYPES: &[&str] = &[
    "terms",
    "range",
    "histogram",
    "date_histogram",
    "avg",
    "min",
    "max",
    "sum",
    "value_count",
    "stats",
    "cardinality",
];

const AGG_PLANNED: &[&str] = &[
    "percentiles",
    "extended_stats",
    "top_hits",
    "composite",
    "date_range",
];

const AGG_UNSUPPORTED: &[&str] = &[
    "filters",
    "filter",
    "nested",
    "reverse_nested",
    "global",
    "sampler",
    "diversified_sampler",
    "significant_terms",
    "significant_text",
    "rare_terms",
    "multi_terms",
    "missing",
    "geo_distance",
    "geohash_grid",
    "geotile_grid",
    "geo_bounds",
    "geo_centroid",
    "ip_range",
    "adjacency_matrix",
    "auto_date_histogram",
    "variable_width_histogram",
    "children",
    "parent",
    "scripted_metric",
    "weighted_avg",
    "median_absolute_deviation",
    "string_stats",
    "boxplot",
    "t_test",
    "rate",
    "matrix_stats",
    "bucket_sort",
    "bucket_script",
    "bucket_selector",
    "derivative",
    "cumulative_sum",
    "moving_fn",
    "moving_avg",
    "moving_percentiles",
    "serial_diff",
    "avg_bucket",
    "sum_bucket",
    "min_bucket",
    "max_bucket",
    "stats_bucket",
    "extended_stats_bucket",
    "percentiles_bucket",
    "normalize",
    "inference",
    "categorize_text",
    "frequent_item_sets",
    "time_series",
];

const TOP_LEVEL: &[&str] = &[
    "query",
    "aggs",
    "aggregations",
    "sort",
    "size",
    "from",
    "post_filter",
    "_source",
    "track_total_hits",
];

const TOP_PLANNED: &[&str] = &["highlight"];

const TOP_UNSUPPORTED: &[&str] = &[
    "explain",
    "profile",
    "timeout",
    "min_score",
    "search_after",
    "collapse",
    "suggest",
    "script_fields",
    "docvalue_fields",
    "stored_fields",
    "rescore",
    "version",
    "seq_no_primary_term",
    "indices_boost",
    "terminate_after",
    "pit",
    "runtime_mappings",
    "knn",
    "fields",
    "ext",
    "stats",
    "slice",
    "rank",
    "retriever",
];

// ---------------------------------------------------------------- small helpers

fn obj<'a>(v: &'a Value, path: &str, what: &str) -> Result<&'a Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| Error::new(path, format!("{} must be an object", what)))
}

fn one_key<'a>(
    m: &'a Map<String, Value>,
    path: &str,
    what: &str,
) -> Result<(&'a String, &'a Value)> {
    if m.len() != 1 {
        return Err(Error::new(
            path,
            format!("{} has exactly one key, found {}", what, m.len()),
        ));
    }
    Ok(m.iter().next().unwrap())
}

fn check_keys(m: &Map<String, Value>, path: &str, allowed: &[&str]) -> Result<()> {
    for k in m.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(Error::new(
                format!("{}.{}", path, k),
                format!("'{}' is not supported here", k),
            )
            .supported(allowed));
        }
    }
    Ok(())
}

fn as_usize(v: &Value, path: &str, what: &str) -> Result<usize> {
    match v.as_u64() {
        Some(n) if v.is_u64() || v.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false) => {
            Ok(n as usize)
        }
        _ => Err(Error::new(
            path,
            format!("{} must be a non-negative integer", what),
        )),
    }
}

fn as_str<'a>(v: &'a Value, path: &str, what: &str) -> Result<&'a str> {
    v.as_str()
        .ok_or_else(|| Error::new(path, format!("{} must be a string", what)))
}

fn clause_list<'a>(v: &'a Value, path: &str) -> Result<Vec<&'a Value>> {
    match v {
        Value::Array(items) => Ok(items.iter().collect()),
        Value::Object(_) => Ok(vec![v]),
        _ => Err(Error::new(
            path,
            "expected a query object or a list of query objects",
        )),
    }
}

// ---------------------------------------------------------------- values

/// A literal checked against the field's type. Wrong types are refused, never coerced.
fn scalar_for(v: &Value, ty: FieldType, field: &str, path: &str) -> Result<Scalar> {
    match ty {
        FieldType::Keyword | FieldType::Text => match v {
            Value::String(s) => Ok(Scalar::Str(s.clone())),
            _ => Err(
                Error::new(path, format!("bad value for '{}'", field)).hint(format!(
                    "{} is a {} field; give a string",
                    field,
                    ty.name()
                )),
            ),
        },
        FieldType::Number => match v {
            Value::Number(n) => Ok(Scalar::Num(n.as_f64().unwrap_or(f64::NAN))),
            _ => Err(
                Error::new(path, format!("bad value for '{}'", field)).hint(format!(
                    "{} is a number field; give a number (not a quoted string)",
                    field
                )),
            ),
        },
        FieldType::Boolean => match v {
            Value::Bool(b) => Ok(Scalar::Bool(*b)),
            _ => Err(
                Error::new(path, format!("bad value for '{}'", field)).hint(format!(
                    "{} is a boolean field; give true or false, unquoted",
                    field
                )),
            ),
        },
        FieldType::Date => match v {
            Value::String(s) if parse_iso(s).is_some() => Ok(Scalar::Str(s.clone())),
            Value::String(s) => Err(
                Error::new(path, format!("bad date for '{}': {:?}", field, s)).hint(date_hint(s)),
            ),
            _ => Err(
                Error::new(path, format!("bad value for '{}'", field)).hint(format!(
                    "{} is a date field; give an ISO date string",
                    field
                )),
            ),
        },
    }
}

// ---------------------------------------------------------------- queries

pub fn parse_query(v: &Value, m: &Mapping, path: &str) -> Result<Query> {
    let o = obj(v, path, "a query")?;
    let (kind, body) = one_key(o, path, "a query clause")?;
    let p = format!("{}.{}", path, kind);
    match kind.as_str() {
        "match_all" => {
            check_keys(obj(body, &p, "match_all")?, &p, &[])?;
            Ok(Query::MatchAll)
        }
        "bool" => parse_bool(body, m, &p),
        "term" => {
            let (field, spec) = one_key(obj(body, &p, "term")?, &p, "term")?;
            let fp = format!("{}.{}", p, field);
            let f = m.field(&fp, field)?;
            if f.ty == FieldType::Text {
                return Err(
                    Error::new(&fp, format!("'{}' is a text field", field)).hint(
                        "use match for text fields; term is for keyword, number, boolean and date",
                    ),
                );
            }
            let raw = match spec {
                Value::Object(inner) => {
                    check_keys(inner, &fp, &["value"])?;
                    inner
                        .get("value")
                        .ok_or_else(|| Error::new(&fp, "term needs a value"))?
                }
                other => other,
            };
            Ok(Query::Term {
                field: field.clone(),
                value: scalar_for(raw, f.ty, field, &fp)?,
            })
        }
        "terms" => {
            let (field, spec) = one_key(obj(body, &p, "terms")?, &p, "terms")?;
            let fp = format!("{}.{}", p, field);
            let f = m.field(&fp, field)?;
            if f.ty == FieldType::Text {
                return Err(Error::new(&fp, format!("'{}' is a text field", field))
                    .hint("use match for text fields"));
            }
            let items = spec
                .as_array()
                .ok_or_else(|| Error::new(&fp, "terms takes a list of values"))?;
            if items.is_empty() {
                return Err(Error::new(&fp, "terms needs at least one value"));
            }
            let values = items
                .iter()
                .enumerate()
                .map(|(i, x)| scalar_for(x, f.ty, field, &format!("{}[{}]", fp, i)))
                .collect::<Result<Vec<_>>>()?;
            Ok(Query::Terms {
                field: field.clone(),
                values,
            })
        }
        "range" => {
            let (field, spec) = one_key(obj(body, &p, "range")?, &p, "range")?;
            let fp = format!("{}.{}", p, field);
            let f = m.field(&fp, field)?;
            if !matches!(f.ty, FieldType::Number | FieldType::Date) {
                return Err(
                    Error::new(&fp, format!("'{}' is a {} field", field, f.ty.name()))
                        .hint("range needs a number or date field"),
                );
            }
            let inner = obj(spec, &fp, "range bounds")?;
            check_keys(inner, &fp, &["gte", "gt", "lte", "lt"])?;
            if inner.is_empty() {
                return Err(Error::new(&fp, "range needs at least one bound")
                    .supported(&["gte", "gt", "lte", "lt"]));
            }
            let bound = |k: &str| -> Result<Option<Scalar>> {
                inner
                    .get(k)
                    .map(|x| scalar_for(x, f.ty, field, &format!("{}.{}", fp, k)))
                    .transpose()
            };
            Ok(Query::Range {
                field: field.clone(),
                gte: bound("gte")?,
                gt: bound("gt")?,
                lte: bound("lte")?,
                lt: bound("lt")?,
            })
        }
        "exists" => {
            let inner = obj(body, &p, "exists")?;
            check_keys(inner, &p, &["field"])?;
            let fp = format!("{}.field", p);
            let field = as_str(
                inner
                    .get("field")
                    .ok_or_else(|| Error::new(&fp, "exists needs a field"))?,
                &fp,
                "field",
            )?;
            m.field(&fp, field)?;
            Ok(Query::Exists {
                field: field.to_string(),
            })
        }
        "match" => {
            let (field, spec) = one_key(obj(body, &p, "match")?, &p, "match")?;
            let fp = format!("{}.{}", p, field);
            let f = m.field(&fp, field)?;
            if f.ty != FieldType::Text {
                return Err(
                    Error::new(&fp, format!("'{}' is a {} field", field, f.ty.name()))
                        .hint("match needs a text field; use term for exact values"),
                );
            }
            let (query, operator) = match spec {
                Value::String(s) => (s.clone(), Operator::Or),
                Value::Object(inner) => {
                    check_keys(inner, &fp, &["query", "operator"])?;
                    let q = as_str(
                        inner
                            .get("query")
                            .ok_or_else(|| Error::new(&fp, "match needs a query"))?,
                        &format!("{}.query", fp),
                        "query",
                    )?;
                    let op = match inner.get("operator") {
                        None => Operator::Or,
                        Some(Value::String(s)) if s.eq_ignore_ascii_case("or") => Operator::Or,
                        Some(Value::String(s)) if s.eq_ignore_ascii_case("and") => Operator::And,
                        Some(_) => {
                            return Err(Error::new(
                                format!("{}.operator", fp),
                                "operator is \"or\" or \"and\"",
                            ))
                        }
                    };
                    (q.to_string(), op)
                }
                _ => {
                    return Err(Error::new(
                        &fp,
                        "match takes a string or an object with query and operator",
                    ))
                }
            };
            if query.trim().is_empty() {
                return Err(Error::new(&fp, "match needs some text")
                    .hint("use match_all to match everything"));
            }
            Ok(Query::Match {
                field: field.clone(),
                query,
                operator,
            })
        }
        "match_phrase" => {
            let (field, spec) = one_key(obj(body, &p, "match_phrase")?, &p, "match_phrase")?;
            let fp = format!("{}.{}", p, field);
            let f = m.field(&fp, field)?;
            if f.ty != FieldType::Text {
                return Err(
                    Error::new(&fp, format!("'{}' is a {} field", field, f.ty.name()))
                        .hint("match_phrase needs a text field"),
                );
            }
            let query = match spec {
                Value::String(s) => s.clone(),
                Value::Object(inner) => {
                    check_keys(inner, &fp, &["query"])?;
                    as_str(
                        inner
                            .get("query")
                            .ok_or_else(|| Error::new(&fp, "match_phrase needs a query"))?,
                        &format!("{}.query", fp),
                        "query",
                    )?
                    .to_string()
                }
                _ => {
                    return Err(Error::new(
                        &fp,
                        "match_phrase takes a string or an object with query",
                    ))
                }
            };
            if query.trim().is_empty() {
                return Err(Error::new(&fp, "match_phrase needs some text"));
            }
            Ok(Query::MatchPhrase {
                field: field.clone(),
                query,
            })
        }
        "prefix" => {
            let (field, spec) = one_key(obj(body, &p, "prefix")?, &p, "prefix")?;
            let fp = format!("{}.{}", p, field);
            let f = m.field(&fp, field)?;
            if !matches!(f.ty, FieldType::Keyword | FieldType::Text) {
                return Err(
                    Error::new(&fp, format!("'{}' is a {} field", field, f.ty.name()))
                        .hint("prefix needs a keyword or text field"),
                );
            }
            let value = match spec {
                Value::String(s) => s.clone(),
                Value::Object(inner) => {
                    check_keys(inner, &fp, &["value"])?;
                    as_str(
                        inner
                            .get("value")
                            .ok_or_else(|| Error::new(&fp, "prefix needs a value"))?,
                        &format!("{}.value", fp),
                        "value",
                    )?
                    .to_string()
                }
                _ => {
                    return Err(Error::new(
                        &fp,
                        "prefix takes a string or an object with value",
                    ))
                }
            };
            if value.is_empty() {
                return Err(Error::new(&fp, "prefix needs a non-empty value"));
            }
            Ok(Query::Prefix {
                field: field.clone(),
                value,
            })
        }
        k if QUERY_PLANNED.contains(&k) => Err(Error::new(
            &p,
            format!("'{}' is not supported yet (planned)", k),
        )
        .supported(QUERY_CLAUSES)),
        k if QUERY_UNSUPPORTED.contains(&k) => {
            Err(Error::new(&p, format!("'{}' is not supported", k)).supported(QUERY_CLAUSES))
        }
        k => Err(Error::new(&p, format!("unknown query clause '{}'", k)).supported(QUERY_CLAUSES)),
    }
}

fn parse_bool(body: &Value, m: &Mapping, p: &str) -> Result<Query> {
    let inner = obj(body, p, "bool")?;
    check_keys(
        inner,
        p,
        &[
            "must",
            "should",
            "must_not",
            "filter",
            "minimum_should_match",
        ],
    )?;
    let part = |k: &str| -> Result<Vec<Query>> {
        match inner.get(k) {
            None => Ok(Vec::new()),
            Some(v) => {
                let kp = format!("{}.{}", p, k);
                clause_list(v, &kp)?
                    .iter()
                    .enumerate()
                    .map(|(i, c)| parse_query(c, m, &format!("{}[{}]", kp, i)))
                    .collect()
            }
        }
    };
    let minimum_should_match = match inner.get("minimum_should_match") {
        None => None,
        Some(v) => {
            let mp = format!("{}.minimum_should_match", p);
            match v.as_u64() {
                Some(n @ (0 | 1)) => Some(n as u32),
                _ => {
                    return Err(Error::new(&mp, "only 0 and 1 are supported")
                        .hint("the default is 1 when there is no must or filter, else 0"))
                }
            }
        }
    };
    Ok(Query::Bool {
        must: part("must")?,
        should: part("should")?,
        must_not: part("must_not")?,
        filter: part("filter")?,
        minimum_should_match,
    })
}

// ---------------------------------------------------------------- aggregations

fn agg_field<'a>(
    inner: &Map<String, Value>,
    m: &'a Mapping,
    p: &str,
) -> Result<(&'a str, &'a crate::mapping::Field)> {
    let fp = format!("{}.field", p);
    let name = as_str(
        inner
            .get("field")
            .ok_or_else(|| Error::new(&fp, "the aggregation needs a field"))?,
        &fp,
        "field",
    )?;
    let (key, f) = m
        .fields
        .get_key_value(name)
        .ok_or_else(|| m.field(&fp, name).err().unwrap())?;
    if !f.aggregatable {
        return Err(
            Error::new(&fp, format!("'{}' is not aggregatable", name)).hint(
                if f.ty == FieldType::Text {
                    "text fields cannot be aggregated; use a keyword field"
                } else {
                    "mark it aggregatable in the mapping"
                },
            ),
        );
    }
    Ok((key.as_str(), f))
}

fn parse_agg(name: &str, v: &Value, m: &Mapping, path: &str) -> Result<Agg> {
    if name.is_empty() || name.contains(['[', ']', '>']) {
        return Err(Error::new(
            path,
            "aggregation names must not be empty or contain [ ] >",
        ));
    }
    let o = obj(v, path, "an aggregation")?;
    if o.contains_key("aggs") || o.contains_key("aggregations") {
        return Err(Error::new(
            format!("{}.aggs", path),
            "sub-aggregations are not supported yet (planned)",
        ));
    }
    let (kind, body) = one_key(o, path, "an aggregation")?;
    let p = format!("{}.{}", path, kind);
    let inner = obj(body, &p, kind)?;
    let kind = match kind.as_str() {
        "terms" => {
            check_keys(inner, &p, &["field", "size", "order"])?;
            let (field, _) = agg_field(inner, m, &p)?;
            let size = match inner.get("size") {
                None => DEFAULT_TERMS_SIZE,
                Some(s) => {
                    let n = as_usize(s, &format!("{}.size", p), "size")?;
                    if n == 0 {
                        return Err(Error::new(format!("{}.size", p), "size must be at least 1"));
                    }
                    n
                }
            };
            let order = match inner.get("order") {
                None => TermsOrder::CountDesc,
                Some(ov) => parse_terms_order(ov, &format!("{}.order", p))?,
            };
            AggKind::Terms {
                field: field.to_string(),
                size,
                order,
            }
        }
        "range" => {
            check_keys(inner, &p, &["field", "ranges"])?;
            let (field, f) = agg_field(inner, m, &p)?;
            if f.ty != FieldType::Number {
                return Err(Error::new(format!("{}.field", p), format!("'{}' is a {} field", field, f.ty.name()))
                    .hint(if f.ty == FieldType::Date { "date_range is not supported yet; use date_histogram, or a range query per bucket" } else { "range needs a number field" }));
            }
            let rp = format!("{}.ranges", p);
            let items = inner
                .get("ranges")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::new(&rp, "ranges is a list of {from, to} objects"))?;
            if items.is_empty() {
                return Err(Error::new(&rp, "ranges needs at least one bucket"));
            }
            let mut ranges = Vec::new();
            for (i, it) in items.iter().enumerate() {
                let ip = format!("{}[{}]", rp, i);
                let r = obj(it, &ip, "a range bucket")?;
                check_keys(r, &ip, &["key", "from", "to"])?;
                let num = |k: &str| -> Result<Option<f64>> {
                    match r.get(k) {
                        None | Some(Value::Null) => Ok(None),
                        Some(Value::Number(n)) => Ok(n.as_f64()),
                        Some(_) => Err(Error::new(format!("{}.{}", ip, k), "bounds are numbers")),
                    }
                };
                let (from, to) = (num("from")?, num("to")?);
                if from.is_none() && to.is_none() {
                    return Err(Error::new(&ip, "a bucket needs from, to or both"));
                }
                let key = match r.get("key") {
                    None => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => return Err(Error::new(format!("{}.key", ip), "key is a string")),
                };
                ranges.push(RangeBucket { key, from, to });
            }
            AggKind::Range {
                field: field.to_string(),
                ranges,
            }
        }
        "histogram" => {
            check_keys(inner, &p, &["field", "interval", "min_doc_count"])?;
            let (field, f) = agg_field(inner, m, &p)?;
            if f.ty != FieldType::Number {
                return Err(Error::new(
                    format!("{}.field", p),
                    format!("'{}' is a {} field", field, f.ty.name()),
                )
                .hint(if f.ty == FieldType::Date {
                    "use date_histogram for date fields"
                } else {
                    "histogram needs a number field"
                }));
            }
            let ivp = format!("{}.interval", p);
            let interval = inner
                .get("interval")
                .and_then(Value::as_f64)
                .ok_or_else(|| Error::new(&ivp, "histogram needs a numeric interval"))?;
            if interval <= 0.0 || !interval.is_finite() {
                return Err(Error::new(&ivp, "interval must be greater than 0"));
            }
            let min_doc_count = match inner.get("min_doc_count") {
                None => 0,
                Some(x) => as_usize(x, &format!("{}.min_doc_count", p), "min_doc_count")? as u64,
            };
            AggKind::Histogram {
                field: field.to_string(),
                interval,
                min_doc_count,
            }
        }
        "date_histogram" => {
            if inner.contains_key("fixed_interval") || inner.contains_key("interval") {
                return Err(Error::new(
                    format!("{}.fixed_interval", p),
                    "only calendar_interval is supported",
                )
                .supported(&["calendar_interval", "field", "min_doc_count"]));
            }
            check_keys(inner, &p, &["field", "calendar_interval", "min_doc_count"])?;
            let (field, f) = agg_field(inner, m, &p)?;
            if f.ty != FieldType::Date {
                return Err(Error::new(
                    format!("{}.field", p),
                    format!("'{}' is a {} field", field, f.ty.name()),
                )
                .hint("date_histogram needs a date field"));
            }
            let cp = format!("{}.calendar_interval", p);
            let ci = as_str(
                inner
                    .get("calendar_interval")
                    .ok_or_else(|| Error::new(&cp, "date_histogram needs a calendar_interval"))?,
                &cp,
                "calendar_interval",
            )?;
            let calendar_interval = match ci {
                "day" | "1d" => CalendarInterval::Day,
                "month" | "1M" => CalendarInterval::Month,
                "year" | "1y" => CalendarInterval::Year,
                "week" | "1w" | "quarter" | "1q" | "hour" | "1h" | "minute" | "1m" | "second"
                | "1s" => {
                    return Err(
                        Error::new(&cp, format!("'{}' is not supported yet (planned)", ci))
                            .supported(&["day", "month", "year"]),
                    )
                }
                _ => {
                    return Err(
                        Error::new(&cp, format!("unknown calendar interval '{}'", ci))
                            .supported(&["day", "month", "year"]),
                    )
                }
            };
            let min_doc_count = match inner.get("min_doc_count") {
                None => 0,
                Some(x) => as_usize(x, &format!("{}.min_doc_count", p), "min_doc_count")? as u64,
            };
            AggKind::DateHistogram {
                field: field.to_string(),
                calendar_interval,
                min_doc_count,
            }
        }
        k @ ("avg" | "min" | "max" | "sum" | "stats") => {
            check_keys(inner, &p, &["field"])?;
            let (field, f) = agg_field(inner, m, &p)?;
            if f.ty != FieldType::Number {
                return Err(Error::new(
                    format!("{}.field", p),
                    format!("'{}' is a {} field", field, f.ty.name()),
                )
                .hint(if f.ty == FieldType::Date {
                    "metrics on date fields are not supported yet (planned)"
                } else {
                    "metric aggregations need a number field"
                }));
            }
            let field = field.to_string();
            match k {
                "avg" => AggKind::Avg { field },
                "min" => AggKind::Min { field },
                "max" => AggKind::Max { field },
                "sum" => AggKind::Sum { field },
                _ => AggKind::Stats { field },
            }
        }
        "value_count" => {
            check_keys(inner, &p, &["field"])?;
            let (field, _) = agg_field(inner, m, &p)?;
            AggKind::ValueCount {
                field: field.to_string(),
            }
        }
        "cardinality" => {
            if inner.contains_key("precision_threshold") {
                return Err(Error::new(
                    format!("{}.precision_threshold", p),
                    "not needed: counts are exact",
                )
                .supported(&["field"]));
            }
            check_keys(inner, &p, &["field"])?;
            let (field, _) = agg_field(inner, m, &p)?;
            AggKind::Cardinality {
                field: field.to_string(),
            }
        }
        k if AGG_PLANNED.contains(&k) => {
            return Err(
                Error::new(&p, format!("'{}' is not supported yet (planned)", k))
                    .supported(AGG_TYPES),
            )
        }
        k if AGG_UNSUPPORTED.contains(&k) => {
            return Err(Error::new(&p, format!("'{}' is not supported", k)).supported(AGG_TYPES))
        }
        k => {
            return Err(
                Error::new(&p, format!("unknown aggregation type '{}'", k)).supported(AGG_TYPES)
            )
        }
    };
    Ok(Agg {
        name: name.to_string(),
        kind,
    })
}

fn parse_terms_order(v: &Value, p: &str) -> Result<TermsOrder> {
    let o = match v {
        Value::Array(items) if items.len() == 1 => obj(&items[0], p, "order")?,
        Value::Array(_) => return Err(Error::new(p, "only one order criterion is supported")),
        other => obj(other, p, "order")?,
    };
    let (key, dir) = one_key(o, p, "order")?;
    let desc = match dir.as_str() {
        Some(s) if s.eq_ignore_ascii_case("desc") => true,
        Some(s) if s.eq_ignore_ascii_case("asc") => false,
        _ => return Err(Error::new(p, "order direction is \"asc\" or \"desc\"")),
    };
    match (key.as_str(), desc) {
        ("_count", true) => Ok(TermsOrder::CountDesc),
        ("_count", false) => Ok(TermsOrder::CountAsc),
        ("_key", true) => Ok(TermsOrder::KeyDesc),
        ("_key", false) => Ok(TermsOrder::KeyAsc),
        (k, _) => {
            Err(Error::new(p, format!("cannot order by '{}'", k)).supported(&["_count", "_key"]))
        }
    }
}

// ---------------------------------------------------------------- sort, source

fn parse_sort(v: &Value, m: &Mapping, path: &str) -> Result<Vec<Sort>> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut out = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let p = if v.is_array() {
            format!("{}[{}]", path, i)
        } else {
            path.to_string()
        };
        let (field, order) = match it {
            Value::String(s) => (s.as_str(), Order::Asc),
            Value::Object(o) => {
                let (field, spec) = one_key(o, &p, "a sort entry")?;
                let order = match spec {
                    Value::String(s) => parse_order(s, &format!("{}.{}", p, field))?,
                    Value::Object(inner) => {
                        let fp = format!("{}.{}", p, field);
                        check_keys(inner, &fp, &["order"])?;
                        match inner.get("order") {
                            None => Order::Asc,
                            Some(Value::String(s)) => parse_order(s, &format!("{}.order", fp))?,
                            Some(_) => {
                                return Err(Error::new(
                                    format!("{}.order", fp),
                                    "order is \"asc\" or \"desc\"",
                                ))
                            }
                        }
                    }
                    _ => {
                        return Err(Error::new(
                            format!("{}.{}", p, field),
                            "a sort entry is \"asc\", \"desc\" or {\"order\": ...}",
                        ))
                    }
                };
                (field.as_str(), order)
            }
            _ => {
                return Err(Error::new(
                    &p,
                    "a sort entry is a field name or {field: order}",
                ))
            }
        };
        let fp = format!("{}.{}", p, field);
        match field {
            "_score" => {
                return Err(
                    Error::new(&fp, "sorting by _score is not supported yet (planned)")
                        .hint("sort by a field, or leave sort out for _id order"),
                )
            }
            "_doc" | "_id" if field == "_doc" => {
                return Err(
                    Error::new(&fp, "_doc is not supported").hint("leave sort out for _id order")
                )
            }
            _ => {}
        }
        let f = m.field(&fp, field)?;
        if f.array {
            return Err(Error::new(
                &fp,
                format!("'{}' is an array field and cannot be sorted on", field),
            ));
        }
        if f.ty == FieldType::Text {
            return Err(Error::new(
                &fp,
                format!("'{}' is a text field and cannot be sorted on", field),
            )
            .hint("sort on a keyword field"));
        }
        out.push(Sort {
            field: field.to_string(),
            order,
        });
    }
    Ok(out)
}

fn parse_order(s: &str, p: &str) -> Result<Order> {
    if s.eq_ignore_ascii_case("asc") {
        Ok(Order::Asc)
    } else if s.eq_ignore_ascii_case("desc") {
        Ok(Order::Desc)
    } else {
        Err(Error::new(p, "order is \"asc\" or \"desc\""))
    }
}

fn parse_source(v: &Value, m: &Mapping, p: &str) -> Result<SourceFilter> {
    match v {
        Value::Bool(b) => Ok(SourceFilter::All(*b)),
        Value::String(s) => {
            if s.contains('*') {
                return Err(Error::new(p, "wildcards in _source are not supported")
                    .hint("list the field names"));
            }
            m.field(p, s)?;
            Ok(SourceFilter::Fields(vec![s.clone()]))
        }
        Value::Array(items) => {
            let mut fields = Vec::new();
            for (i, it) in items.iter().enumerate() {
                let ip = format!("{}[{}]", p, i);
                let s = as_str(it, &ip, "a _source entry")?;
                if s.contains('*') {
                    return Err(Error::new(&ip, "wildcards in _source are not supported")
                        .hint("list the field names"));
                }
                m.field(&ip, s)?;
                fields.push(s.to_string());
            }
            Ok(SourceFilter::Fields(fields))
        }
        _ => Err(Error::new(
            p,
            "_source is true, false, a field name or a list of field names",
        )),
    }
}

// ---------------------------------------------------------------- the request

/// Walks an Elasticsearch search request into a [`Request`], refusing anything outside the profile.
pub fn parse_request(v: &Value, m: &Mapping) -> Result<Request> {
    let o = obj(v, "$", "a search request")?;
    for k in o.keys() {
        let p = format!("$.{}", k);
        if TOP_LEVEL.contains(&k.as_str()) {
            continue;
        }
        if TOP_PLANNED.contains(&k.as_str()) {
            return Err(
                Error::new(&p, format!("'{}' is not supported yet (planned)", k))
                    .supported(TOP_LEVEL),
            );
        }
        if TOP_UNSUPPORTED.contains(&k.as_str()) {
            return Err(Error::new(&p, format!("'{}' is not supported", k)).supported(TOP_LEVEL));
        }
        if ["filters", "facets", "q", "limit", "offset"].contains(&k.as_str()) {
            return Err(Error::new(&p, format!("'{}' is not Elasticsearch", k))
                .hint("this is the Elasticsearch query DSL: query, aggs, sort, size, from")
                .supported(TOP_LEVEL));
        }
        return Err(Error::new(&p, format!("unknown request key '{}'", k)).supported(TOP_LEVEL));
    }
    if o.contains_key("aggs") && o.contains_key("aggregations") {
        return Err(Error::new(
            "$.aggregations",
            "give aggs or aggregations, not both",
        ));
    }
    let mut req = Request::default();
    if let Some(q) = o.get("query") {
        req.query = parse_query(q, m, "query")?;
    }
    if let Some(q) = o.get("post_filter") {
        req.post_filter = Some(parse_query(q, m, "post_filter")?);
    }
    if let Some((key, aggs)) = o
        .get_key_value("aggs")
        .or_else(|| o.get_key_value("aggregations"))
    {
        let ao = obj(aggs, key, "aggs")?;
        for (name, spec) in ao {
            req.aggs
                .push(parse_agg(name, spec, m, &format!("{}.{}", key, name))?);
        }
    }
    if let Some(s) = o.get("sort") {
        req.sort = parse_sort(s, m, "sort")?;
    }
    if let Some(s) = o.get("size") {
        req.size = as_usize(s, "size", "size")?;
        if req.size > MAX_SIZE {
            return Err(Error::new("size", format!("size is at most {}", MAX_SIZE))
                .hint("page with from, or narrow the query"));
        }
    }
    if let Some(f) = o.get("from") {
        req.from = as_usize(f, "from", "from")?;
    }
    if req.from.saturating_add(req.size) > MAX_SIZE {
        return Err(Error::new(
            "from",
            format!(
                "from + size is at most {} (Elasticsearch's result window)",
                MAX_SIZE
            ),
        )
        .hint("narrow the query, or page in smaller steps"));
    }
    if let Some(s) = o.get("_source") {
        req.source = parse_source(s, m, "_source")?;
    }
    if let Some(t) = o.get("track_total_hits") {
        if !(t.is_boolean() || t.is_u64()) {
            return Err(Error::new(
                "track_total_hits",
                "true, false or a number (totals are always exact here)",
            ));
        }
    }
    Ok(req)
}

// ---------------------------------------------------------------- printing

fn scalar_json(s: &Scalar) -> Value {
    match s {
        Scalar::Bool(b) => Value::Bool(*b),
        Scalar::Num(n) => num_json(*n),
        Scalar::Str(s) => Value::String(s.clone()),
    }
}

/// Whole numbers print without a fraction, so `5` stays `5` and not `5.0`.
pub fn num_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

pub fn query_json(q: &Query) -> Value {
    match q {
        Query::MatchAll => json!({"match_all": {}}),
        Query::Bool {
            must,
            should,
            must_not,
            filter,
            minimum_should_match,
        } => {
            let mut b = Map::new();
            for (k, v) in [
                ("must", must),
                ("should", should),
                ("must_not", must_not),
                ("filter", filter),
            ] {
                if !v.is_empty() {
                    b.insert(
                        k.to_string(),
                        Value::Array(v.iter().map(query_json).collect()),
                    );
                }
            }
            if let Some(n) = minimum_should_match {
                b.insert("minimum_should_match".to_string(), json!(n));
            }
            json!({"bool": b})
        }
        Query::Term { field, value } => json!({"term": {field: {"value": scalar_json(value)}}}),
        Query::Terms { field, values } => {
            json!({"terms": {field: values.iter().map(scalar_json).collect::<Vec<_>>()}})
        }
        Query::Range {
            field,
            gte,
            gt,
            lte,
            lt,
        } => {
            let mut b = Map::new();
            for (k, v) in [("gte", gte), ("gt", gt), ("lte", lte), ("lt", lt)] {
                if let Some(s) = v {
                    b.insert(k.to_string(), scalar_json(s));
                }
            }
            json!({"range": {field: b}})
        }
        Query::Exists { field } => json!({"exists": {"field": field}}),
        Query::Match {
            field,
            query,
            operator,
        } => {
            json!({"match": {field: {"query": query, "operator": match operator { Operator::Or => "or", Operator::And => "and" }}}})
        }
        Query::MatchPhrase { field, query } => json!({"match_phrase": {field: {"query": query}}}),
        Query::Prefix { field, value } => json!({"prefix": {field: {"value": value}}}),
    }
}

fn agg_json(a: &AggKind) -> Value {
    let body = match a {
        AggKind::Terms { field, size, order } => {
            let (k, d) = match order {
                TermsOrder::CountDesc => ("_count", "desc"),
                TermsOrder::CountAsc => ("_count", "asc"),
                TermsOrder::KeyAsc => ("_key", "asc"),
                TermsOrder::KeyDesc => ("_key", "desc"),
            };
            json!({"field": field, "size": size, "order": {k: d}})
        }
        AggKind::Range { field, ranges } => {
            let rs: Vec<Value> = ranges
                .iter()
                .map(|r| {
                    let mut o = Map::new();
                    if let Some(k) = &r.key {
                        o.insert("key".into(), json!(k));
                    }
                    if let Some(f) = r.from {
                        o.insert("from".into(), json!(f));
                    }
                    if let Some(t) = r.to {
                        o.insert("to".into(), json!(t));
                    }
                    Value::Object(o)
                })
                .collect();
            json!({"field": field, "ranges": rs})
        }
        AggKind::Histogram {
            field,
            interval,
            min_doc_count,
        } => json!({"field": field, "interval": interval, "min_doc_count": min_doc_count}),
        AggKind::DateHistogram {
            field,
            calendar_interval,
            min_doc_count,
        } => {
            let ci = match calendar_interval {
                CalendarInterval::Day => "day",
                CalendarInterval::Month => "month",
                CalendarInterval::Year => "year",
            };
            json!({"field": field, "calendar_interval": ci, "min_doc_count": min_doc_count})
        }
        other => json!({"field": other.field()}),
    };
    json!({a.type_name(): body})
}

/// The canonical Elasticsearch request for an AST. `parse_request` of the result is the same AST.
pub fn to_elasticsearch(r: &Request) -> Value {
    let mut o = Map::new();
    o.insert("query".into(), query_json(&r.query));
    if let Some(pf) = &r.post_filter {
        o.insert("post_filter".into(), query_json(pf));
    }
    if !r.aggs.is_empty() {
        let aggs: Map<String, Value> = r
            .aggs
            .iter()
            .map(|a| (a.name.clone(), agg_json(&a.kind)))
            .collect();
        o.insert("aggs".into(), Value::Object(aggs));
    }
    if !r.sort.is_empty() {
        let sort: Vec<Value> = r
            .sort
            .iter()
            .map(|s| json!({&s.field: {"order": match s.order { Order::Asc => "asc", Order::Desc => "desc" }}}))
            .collect();
        o.insert("sort".into(), Value::Array(sort));
    }
    o.insert("size".into(), json!(r.size));
    o.insert("from".into(), json!(r.from));
    match &r.source {
        SourceFilter::All(true) => {}
        SourceFilter::All(false) => {
            o.insert("_source".into(), json!(false));
        }
        SourceFilter::Fields(f) => {
            o.insert("_source".into(), json!(f));
        }
    }
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping() -> Mapping {
        Mapping::from_json(&json!({"properties": {
            "state": {"type": "keyword"}, "labels": {"type": "keyword", "array": true},
            "title": {"type": "text"}, "comments": {"type": "long"}, "created_at": {"type": "date"},
            "draft": {"type": "boolean"}
        }}))
        .unwrap()
    }

    #[test]
    fn refusals_name_the_path() {
        let m = mapping();
        let e = parse_request(&json!({"query": {"bool": {"filter": [{"term": {"state": "open"}}, {"wildcard": {"title": "fa*"}}]}}}), &m).unwrap_err();
        assert_eq!(e.path, "query.bool.filter[1].wildcard");
        assert!(e.supported.contains(&"term".to_string()));

        let e = parse_request(&json!({"query": {"term": {"label": "bug"}}}), &m).unwrap_err();
        assert_eq!(e.path, "query.term.label");
        assert_eq!(e.hint.as_deref(), Some("did you mean 'labels'?"));

        let e = parse_request(&json!({"query": {"range": {"comments": {"gte": "5"}}}}), &m)
            .unwrap_err();
        assert_eq!(e.path, "query.range.comments.gte");

        let e = parse_request(
            &json!({"query": {"range": {"created_at": {"gte": "2026-07"}}}}),
            &m,
        )
        .unwrap_err();
        assert!(e.hint.unwrap().contains("2026-01-01"));

        let e =
            parse_request(&json!({"aggs": {"by": {"terms": {"field": "title"}}}}), &m).unwrap_err();
        assert_eq!(e.path, "aggs.by.terms.field");

        let e = parse_request(&json!({"filters": []}), &m).unwrap_err();
        assert_eq!(e.path, "$.filters");
    }

    #[test]
    fn shorthand_and_longhand_agree() {
        let m = mapping();
        let a = parse_request(&json!({"query": {"term": {"state": "open"}}}), &m).unwrap();
        let b = parse_request(
            &json!({"query": {"term": {"state": {"value": "open"}}}}),
            &m,
        )
        .unwrap();
        assert_eq!(a, b);
        let a = parse_request(&json!({"query": {"match": {"title": "facet counts"}}}), &m).unwrap();
        let b = parse_request(
            &json!({"query": {"match": {"title": {"query": "facet counts", "operator": "OR"}}}}),
            &m,
        )
        .unwrap();
        assert_eq!(a, b);
        let a = parse_request(&json!({"sort": ["comments", {"created_at": "desc"}]}), &m).unwrap();
        let b = parse_request(
            &json!({"sort": [{"comments": {"order": "asc"}}, {"created_at": {"order": "desc"}}]}),
            &m,
        )
        .unwrap();
        assert_eq!(a.sort, b.sort);
    }

    #[test]
    fn prints_what_it_parsed() {
        let m = mapping();
        let v = json!({
            "query": {"bool": {"must": [{"match": {"title": "facet"}}], "filter": [{"terms": {"labels": ["bug", "regression"]}}, {"range": {"comments": {"gte": 5}}}], "must_not": {"term": {"draft": true}}}},
            "aggs": {"by_label": {"terms": {"field": "labels", "size": 5}}, "per_month": {"date_histogram": {"field": "created_at", "calendar_interval": "month"}}, "c": {"stats": {"field": "comments"}}},
            "sort": [{"comments": "desc"}], "size": 3, "from": 1, "_source": ["title", "state"]
        });
        let req = parse_request(&v, &m).unwrap();
        let printed = to_elasticsearch(&req);
        assert_eq!(parse_request(&printed, &m).unwrap(), req);
        assert_eq!(
            printed["query"]["bool"]["filter"][1]["range"]["comments"]["gte"],
            json!(5)
        );
    }
}
