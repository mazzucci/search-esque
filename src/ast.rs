//! The typed request: what survives the walk over Elasticsearch JSON.
//!
//! Small on purpose. Every node here has an exact meaning in the evaluator and an exact
//! translation in the SQL compiler, and nothing that cannot be rendered to a person as a
//! sentence gets in. The whole tree serialises to JSON, so a fixture can pin an expected
//! AST and a future port in another language is tested against the same files.

use serde::{Deserialize, Serialize};

/// A literal in a request. Dates travel as their ISO string and are parsed where compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Num(f64),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operator {
    /// Any query token matches (Elasticsearch's default for `match`).
    Or,
    /// Every query token must match.
    And,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Query {
    MatchAll,
    /// Elasticsearch's `bool`. With `should` present and no `must` or `filter`, at least one
    /// `should` clause must match, even when `minimum_should_match` is 0 (Lucene's rule, checked
    /// against Elasticsearch 9.5); with a `must` or `filter`, `should` is optional unless
    /// `minimum_should_match` is 1. Only 0 and 1 are accepted. A `must_not` clause matches
    /// records where the field is empty, since nothing there matched the inner clause.
    Bool {
        must: Vec<Query>,
        should: Vec<Query>,
        must_not: Vec<Query>,
        filter: Vec<Query>,
        minimum_should_match: Option<u32>,
    },
    /// Exact match on one value. On a keyword field letter case is ignored (a divergence from
    /// Elasticsearch, where `term` is case-sensitive). On an array field any element may match.
    /// On a date field a value without a time part means the whole day, as in Elasticsearch
    /// (checked against 9.5).
    Term {
        field: String,
        value: Scalar,
    },
    /// Any of the values.
    Terms {
        field: String,
        values: Vec<Scalar>,
    },
    /// Bounds on a number or date field. At least one bound is present. A date bound without a
    /// time part is rounded as Elasticsearch does: `gte` and `lt` to midnight, `gt` and `lte` to
    /// the last millisecond of that day, so `lte: "2026-09-01"` includes the whole day.
    Range {
        field: String,
        gte: Option<Scalar>,
        gt: Option<Scalar>,
        lte: Option<Scalar>,
        lt: Option<Scalar>,
    },
    /// The field has a value that fits its type (for an array field: at least one element).
    /// A value of the wrong type, or a list where a single value belongs, is empty.
    Exists {
        field: String,
    },
    /// Word match on a text field. Text is split the way FTS5's `unicode61` tokenizer does, on
    /// Unicode separators and punctuation, and lower-cased; letters, digits, combining marks and
    /// symbols are token characters (see `tokenize`). This differs from Elasticsearch's standard
    /// analyzer, which keeps `doesn't` as one token and splits `a+b`.
    Match {
        field: String,
        query: String,
        operator: Operator,
    },
    /// The query's tokens appear consecutively, in order.
    MatchPhrase {
        field: String,
        query: String,
    },
    /// A keyword starts with the value (case ignored), or any token of a text field does.
    Prefix {
        field: String,
        value: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sort {
    pub field: String,
    pub order: Order,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermsOrder {
    /// Elasticsearch's default: by count, largest first, then by key ascending.
    CountDesc,
    CountAsc,
    KeyAsc,
    KeyDesc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeBucket {
    pub key: Option<String>,
    pub from: Option<f64>,
    pub to: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarInterval {
    Day,
    Month,
    Year,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AggKind {
    /// Counts per distinct value. A record with several values (array field) counts once per
    /// distinct value. Counts are exact; `doc_count_error_upper_bound` is always 0.
    Terms {
        field: String,
        size: usize,
        order: TermsOrder,
    },
    /// Counts per `[from, to)` bucket on a number field. Buckets come back ordered by `from`
    /// then `to`, as Elasticsearch returns them, not in the order given.
    Range {
        field: String,
        ranges: Vec<RangeBucket>,
    },
    /// Fixed-width buckets on a number field. Empty buckets between the first and last are
    /// returned when `min_doc_count` is 0, as in Elasticsearch, and a histogram that would span
    /// more than 65,536 buckets is refused, as it is there.
    Histogram {
        field: String,
        interval: f64,
        min_doc_count: u64,
    },
    /// Calendar buckets on a date field, in UTC. Keys are epoch milliseconds of the bucket
    /// start, with `key_as_string` in ISO form.
    DateHistogram {
        field: String,
        calendar_interval: CalendarInterval,
        min_doc_count: u64,
    },
    Avg {
        field: String,
    },
    Min {
        field: String,
    },
    Max {
        field: String,
    },
    Sum {
        field: String,
    },
    /// Number of values, not of records: an array field contributes each element, except that
    /// a keyword array counts a value repeated in one record once, as Elasticsearch's sorted-set
    /// doc values do.
    ValueCount {
        field: String,
    },
    Stats {
        field: String,
    },
    /// Exact distinct count (Elasticsearch's is approximate).
    Cardinality {
        field: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agg {
    pub name: String,
    pub kind: AggKind,
}

impl AggKind {
    pub fn field(&self) -> &str {
        match self {
            AggKind::Terms { field, .. }
            | AggKind::Range { field, .. }
            | AggKind::Histogram { field, .. }
            | AggKind::DateHistogram { field, .. }
            | AggKind::Avg { field }
            | AggKind::Min { field }
            | AggKind::Max { field }
            | AggKind::Sum { field }
            | AggKind::ValueCount { field }
            | AggKind::Stats { field }
            | AggKind::Cardinality { field } => field,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            AggKind::Terms { .. } => "terms",
            AggKind::Range { .. } => "range",
            AggKind::Histogram { .. } => "histogram",
            AggKind::DateHistogram { .. } => "date_histogram",
            AggKind::Avg { .. } => "avg",
            AggKind::Min { .. } => "min",
            AggKind::Max { .. } => "max",
            AggKind::Sum { .. } => "sum",
            AggKind::ValueCount { .. } => "value_count",
            AggKind::Stats { .. } => "stats",
            AggKind::Cardinality { .. } => "cardinality",
        }
    }
}

/// Which fields of a record come back in `_source`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceFilter {
    All(bool),
    Fields(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub query: Query,
    pub post_filter: Option<Query>,
    pub aggs: Vec<Agg>,
    pub sort: Vec<Sort>,
    pub size: usize,
    pub from: usize,
    pub source: SourceFilter,
}

impl Default for Request {
    fn default() -> Self {
        Request {
            query: Query::MatchAll,
            post_filter: None,
            aggs: Vec::new(),
            sort: Vec::new(),
            size: 10,
            from: 0,
            source: SourceFilter::All(true),
        }
    }
}

pub const DEFAULT_SIZE: usize = 10;
pub const MAX_SIZE: usize = 10_000;
pub const DEFAULT_TERMS_SIZE: usize = 10;
