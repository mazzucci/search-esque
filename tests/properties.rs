//! Properties over randomly generated requests, within the GitHub issues mapping.
//!
//! - printing an AST to Elasticsearch JSON and parsing it back gives the same AST;
//! - the AST survives its own JSON serialisation;
//! - a query and its negation partition the corpus (the `must_not` semantics on empty fields);
//! - a terms aggregation's counts add up to the records that have the field;
//! - a page holds min(size, total - from) hits with distinct ids.

use proptest::prelude::*;
use search_esque::ast::*;
use search_esque::dsl::{parse_request, to_elasticsearch};
use search_esque::eval::evaluate;
use search_esque::Mapping;
use serde_json::{json, Value};

#[allow(dead_code)]
mod common;
use std::path::Path;
use std::sync::OnceLock;

const KEYWORDS: &[&str] = &["author", "publisher", "language", "format", "isbn"];
const KEYWORD_ARRAYS: &[&str] = &["genres", "tags"];
const TEXTS: &[&str] = &["title", "summary", "notes"];
const NUMBERS: &[&str] = &[
    "pages",
    "price",
    "rating",
    "balance",
    "copies",
    "stock_delta",
];
const BOOLS: &[&str] = &["available"];
const DATES: &[&str] = &[
    "published_at",
    "added_at",
    "last_borrowed_at",
    "reprinted_at",
];
const ALL: &[&str] = &[
    "title",
    "summary",
    "notes",
    "author",
    "publisher",
    "language",
    "format",
    "genres",
    "tags",
    "pages",
    "price",
    "rating",
    "balance",
    "copies",
    "stock_delta",
    "available",
    "published_at",
    "added_at",
    "last_borrowed_at",
    "reprinted_at",
    "isbn",
];
const AGGREGATABLE: &[&str] = &[
    "author",
    "publisher",
    "language",
    "format",
    "genres",
    "tags",
    "pages",
    "price",
    "rating",
    "balance",
    "copies",
    "stock_delta",
    "available",
    "published_at",
    "added_at",
    "last_borrowed_at",
    "reprinted_at",
    "isbn",
];
const SORTABLE: &[&str] = &[
    "author",
    "publisher",
    "language",
    "format",
    "pages",
    "price",
    "rating",
    "balance",
    "copies",
    "available",
    "published_at",
    "added_at",
    "last_borrowed_at",
    "isbn",
];
const WORDS: &[&str] = &[
    "en",
    "fr",
    "de",
    "ja",
    "hardcover",
    "paperback",
    "ebook",
    "audiobook",
    "mystery",
    "poetry",
    "fiction",
    "history",
    "staff pick",
    "signed",
    "50%_off",
    "back\\slash",
    "quote\"d",
    "Nordlys Press",
    "nordlys press",
    "Harbourline",
    "Ada Quill",
    "harbour",
    "lanterns",
    "winter",
    "garden",
    "doesn",
    "caf\u{e9}",
    "cafe\u{301}",
    "5\u{b5}s",
    "salt",
    "bread",
    "notes",
    "ending",
];
const DATE_LITERALS: &[&str] = &[
    "1850-01-01",
    "1969-12-31T23:59:59Z",
    "1970-01-01",
    "1999-06-30",
    "2024-06-15",
    "2025-01-01T00:00:00Z",
    "2026-03-15",
    "2026-06-30",
    "2026-10-01T12:30:00Z",
];

/// `PROPTEST_CASES` raises the number of cases; the default keeps the suite under half a minute.
fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

fn corpus() -> &'static (Mapping, Vec<Value>) {
    static CORPUS: OnceLock<(Mapping, Vec<Value>)> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/library");
        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("mapping.json")).unwrap())
                .unwrap();
        let d: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("docs.json")).unwrap())
                .unwrap();
        (
            Mapping::from_json(&m).unwrap(),
            d.as_array().unwrap().clone(),
        )
    })
}

fn pick(list: &'static [&'static str]) -> impl Strategy<Value = String> {
    prop::sample::select(list).prop_map(str::to_string)
}

fn word() -> impl Strategy<Value = String> {
    prop_oneof![3 => pick(WORDS), 1 => "[a-z]{1,8}"]
}

fn number() -> impl Strategy<Value = f64> {
    prop_oneof![4 => (-5i64..40).prop_map(|n| n as f64), 1 => (0i64..400).prop_map(|n| n as f64 / 10.0)]
}

fn keyword_or_array() -> impl Strategy<Value = String> {
    prop_oneof![pick(KEYWORDS), pick(KEYWORD_ARRAYS)]
}

fn term() -> impl Strategy<Value = Query> {
    prop_oneof![
        (keyword_or_array(), word()).prop_map(|(field, v)| Query::Term {
            field,
            value: Scalar::Str(v)
        }),
        (pick(NUMBERS), number()).prop_map(|(field, n)| Query::Term {
            field,
            value: Scalar::Num(n)
        }),
        (pick(BOOLS), any::<bool>()).prop_map(|(field, b)| Query::Term {
            field,
            value: Scalar::Bool(b)
        }),
        (pick(DATES), pick(DATE_LITERALS)).prop_map(|(field, d)| Query::Term {
            field,
            value: Scalar::Str(d)
        }),
    ]
}

fn terms() -> impl Strategy<Value = Query> {
    prop_oneof![
        (keyword_or_array(), prop::collection::vec(word(), 1..4)).prop_map(|(field, vs)| {
            Query::Terms {
                field,
                values: vs.into_iter().map(Scalar::Str).collect(),
            }
        }),
        (pick(NUMBERS), prop::collection::vec(number(), 1..4)).prop_map(|(field, vs)| {
            Query::Terms {
                field,
                values: vs.into_iter().map(Scalar::Num).collect(),
            }
        }),
    ]
}

fn bounds<T: Clone + std::fmt::Debug + 'static, S: Strategy<Value = T>>(
    mk: impl Fn() -> S,
) -> impl Strategy<Value = (Option<T>, Option<T>, Option<T>, Option<T>)> {
    (
        prop::option::of(mk()),
        prop::option::of(mk()),
        prop::option::of(mk()),
        prop::option::of(mk()),
    )
        .prop_filter("at least one bound", |(a, b, c, d)| {
            a.is_some() || b.is_some() || c.is_some() || d.is_some()
        })
}

fn range() -> impl Strategy<Value = Query> {
    prop_oneof![
        (pick(NUMBERS), bounds(number)).prop_map(|(field, (gte, gt, lte, lt))| Query::Range {
            field,
            gte: gte.map(Scalar::Num),
            gt: gt.map(Scalar::Num),
            lte: lte.map(Scalar::Num),
            lt: lt.map(Scalar::Num),
        }),
        (pick(DATES), bounds(|| pick(DATE_LITERALS))).prop_map(|(field, (gte, gt, lte, lt))| {
            Query::Range {
                field,
                gte: gte.map(Scalar::Str),
                gt: gt.map(Scalar::Str),
                lte: lte.map(Scalar::Str),
                lt: lt.map(Scalar::Str),
            }
        }),
    ]
}

fn phrase() -> impl Strategy<Value = String> {
    prop::collection::vec(word(), 1..3).prop_map(|ws| ws.join(" "))
}

fn leaf() -> impl Strategy<Value = Query> {
    prop_oneof![
        1 => Just(Query::MatchAll),
        4 => term(),
        3 => terms(),
        3 => range(),
        2 => pick(ALL).prop_map(|field| Query::Exists { field }),
        3 => (pick(TEXTS), phrase(), prop_oneof![Just(Operator::Or), Just(Operator::And)])
            .prop_map(|(field, query, operator)| Query::Match { field, query, operator }),
        1 => (pick(TEXTS), phrase()).prop_map(|(field, query)| Query::MatchPhrase { field, query }),
        2 => (prop_oneof![pick(KEYWORDS), pick(KEYWORD_ARRAYS), pick(TEXTS)], prop_oneof![3 => "[a-zA-Z]{1,4}", 1 => "[a-z%_\\\\\"'\u{e9} ]{1,3}"]).prop_map(|(field, value)| Query::Prefix { field, value }),
    ]
}

fn query() -> impl Strategy<Value = Query> {
    leaf().prop_recursive(3, 24, 3, |inner| {
        (
            prop::collection::vec(inner.clone(), 0..3),
            prop::collection::vec(inner.clone(), 0..3),
            prop::collection::vec(inner.clone(), 0..3),
            prop::collection::vec(inner, 0..3),
            prop::option::of(0u32..2),
        )
            .prop_map(|(must, should, must_not, filter, minimum_should_match)| {
                Query::Bool {
                    must,
                    should,
                    must_not,
                    filter,
                    minimum_should_match,
                }
            })
    })
}

fn range_bucket() -> impl Strategy<Value = RangeBucket> {
    (
        prop::option::of(number()),
        prop::option::of(number()),
        prop::option::of("[a-z]{1,5}"),
    )
        .prop_filter("a bucket needs a bound", |(f, t, _)| {
            f.is_some() || t.is_some()
        })
        .prop_map(|(from, to, key)| RangeBucket { key, from, to })
}

fn agg_kind() -> impl Strategy<Value = AggKind> {
    let order = prop_oneof![
        Just(TermsOrder::CountDesc),
        Just(TermsOrder::CountAsc),
        Just(TermsOrder::KeyAsc),
        Just(TermsOrder::KeyDesc)
    ];
    let calendar = prop_oneof![
        Just(CalendarInterval::Day),
        Just(CalendarInterval::Month),
        Just(CalendarInterval::Year)
    ];
    prop_oneof![
        4 => (pick(AGGREGATABLE), 1usize..25, order).prop_map(|(field, size, order)| AggKind::Terms { field, size, order }),
        2 => (pick(NUMBERS), prop::collection::vec(range_bucket(), 1..4)).prop_map(|(field, ranges)| AggKind::Range { field, ranges }),
        2 => (pick(NUMBERS), prop_oneof![(1i64..50).prop_map(|i| i as f64), (1i64..200).prop_map(|i| i as f64 / 8.0)], 0u64..2).prop_map(|(field, iv, m)| AggKind::Histogram { field, interval: iv, min_doc_count: m }),
        2 => (pick(DATES), calendar, 0u64..2).prop_map(|(field, ci, m)| AggKind::DateHistogram { field, calendar_interval: ci, min_doc_count: m }),
        1 => pick(NUMBERS).prop_map(|field| AggKind::Avg { field }),
        1 => pick(NUMBERS).prop_map(|field| AggKind::Min { field }),
        1 => pick(NUMBERS).prop_map(|field| AggKind::Max { field }),
        1 => pick(NUMBERS).prop_map(|field| AggKind::Sum { field }),
        1 => pick(NUMBERS).prop_map(|field| AggKind::Stats { field }),
        1 => pick(AGGREGATABLE).prop_map(|field| AggKind::ValueCount { field }),
        1 => pick(AGGREGATABLE).prop_map(|field| AggKind::Cardinality { field }),
    ]
}

fn request() -> impl Strategy<Value = Request> {
    let sort = (
        pick(SORTABLE),
        prop_oneof![Just(Order::Asc), Just(Order::Desc)],
    )
        .prop_map(|(field, order)| Sort { field, order });
    let source = prop_oneof![
        Just(SourceFilter::All(true)),
        Just(SourceFilter::All(false)),
        prop::collection::vec(pick(ALL), 1..4).prop_map(SourceFilter::Fields),
    ];
    (
        query(),
        prop::option::of(query()),
        prop::collection::vec(agg_kind(), 0..4),
        prop::collection::vec(sort, 0..3),
        0usize..200,
        0usize..400,
        source,
    )
        .prop_map(
            |(query, post_filter, kinds, sort, size, from, source)| Request {
                query,
                post_filter,
                aggs: kinds
                    .into_iter()
                    .enumerate()
                    .map(|(i, kind)| Agg {
                        name: format!("a{}", i),
                        kind,
                    })
                    .collect(),
                sort,
                size,
                from,
                source,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), .. ProptestConfig::default() })]

    #[test]
    fn elasticsearch_round_trip(req in request()) {
        let (mapping, _) = corpus();
        let printed = to_elasticsearch(&req);
        let back = parse_request(&printed, mapping).map_err(|e| TestCaseError::fail(format!("{}\n{}", e, printed)))?;
        prop_assert_eq!(back, req);
    }

    #[test]
    fn ast_json_round_trip(req in request()) {
        let v = serde_json::to_value(&req).unwrap();
        let back: Request = serde_json::from_value(v).unwrap();
        prop_assert_eq!(back, req);
    }

    #[test]
    fn a_query_and_its_negation_partition_the_corpus(q in query()) {
        let (mapping, docs) = corpus();
        let total = |query: Query| -> usize {
            let req = Request { query, ..Request::default() };
            evaluate(&req, mapping, docs).unwrap()["hits"]["total"]["value"].as_u64().unwrap() as usize
        };
        let yes = total(q.clone());
        let no = total(Query::Bool { must: vec![], should: vec![], must_not: vec![q], filter: vec![], minimum_should_match: None });
        prop_assert_eq!(yes + no, docs.len());
    }

    #[test]
    fn terms_counts_add_up_to_records_with_the_field(q in query(), field in pick(AGGREGATABLE), size in 1usize..30) {
        let (mapping, docs) = corpus();
        let is_array = mapping.fields[&field].array;
        let req = Request {
            query: q.clone(),
            aggs: vec![Agg { name: "t".into(), kind: AggKind::Terms { field: field.clone(), size, order: TermsOrder::CountDesc } }],
            size: 0,
            ..Request::default()
        };
        let res = evaluate(&req, mapping, docs).unwrap();
        let t = &res["aggregations"]["t"];
        let shown: u64 = t["buckets"].as_array().unwrap().iter().map(|b| b["doc_count"].as_u64().unwrap()).sum();
        let other = t["sum_other_doc_count"].as_u64().unwrap();
        let with_field = Request {
            query: Query::Bool { must: vec![], should: vec![], must_not: vec![], filter: vec![q, Query::Exists { field }], minimum_should_match: None },
            size: 0,
            ..Request::default()
        };
        let n = evaluate(&with_field, mapping, docs).unwrap()["hits"]["total"]["value"].as_u64().unwrap();
        if is_array {
            prop_assert!(shown + other >= n, "array field: {} + {} < {}", shown, other, n);
        } else {
            prop_assert_eq!(shown + other, n);
        }
    }

    #[test]
    fn pages_are_the_right_size_with_distinct_ids(req in request()) {
        let (mapping, docs) = corpus();
        let res = evaluate(&req, mapping, docs).unwrap();
        let total = res["hits"]["total"]["value"].as_u64().unwrap() as usize;
        let hits = res["hits"]["hits"].as_array().unwrap();
        let expected = total.saturating_sub(req.from).min(req.size);
        prop_assert_eq!(hits.len(), expected);
        let mut ids: Vec<&str> = hits.iter().map(|h| h["_id"].as_str().unwrap()).collect();
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), hits.len());
        if let SourceFilter::All(false) = req.source {
            prop_assert!(hits.iter().all(|h| h.get("_source").is_none()));
        }
    }
}

#[test]
fn the_corpus_has_what_it_was_designed_to_have() {
    let (mapping, docs) = corpus();
    let run = |v: Value| -> Value {
        let req = parse_request(&v, mapping).unwrap();
        evaluate(&req, mapping, docs).unwrap()
    };
    let total = |v: Value| run(v)["hits"]["total"]["value"].as_u64().unwrap();
    assert_eq!(total(json!({"query": {"match_all": {}}})), 400);
    // the two spellings of one publisher are two facet values, and one case-folded term
    let p = run(json!({"size": 0, "aggs": {"p": {"terms": {"field": "publisher"}}}}));
    let keys: Vec<&str> = p["aggregations"]["p"]["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["key"].as_str().unwrap())
        .collect();
    assert!(
        keys.contains(&"Nordlys Press") && keys.contains(&"nordlys press"),
        "{:?}",
        keys
    );
    let both = total(json!({"size": 0, "query": {"term": {"publisher": "NORDLYS PRESS"}}}));
    assert!(
        both == total(json!({"size": 0, "query": {"term": {"publisher": "Nordlys Press"}}}))
            && both > 0
    );
    // records before the epoch exist and are found by a range on a date
    assert!(
        total(json!({"size": 0, "query": {"range": {"published_at": {"lt": "1970-01-01"}}}})) > 100
    );
    // the last second before the epoch is in the 1969 day bucket, not 1970
    let d = run(
        json!({"size": 0, "query": {"term": {"published_at": "1969-12-31T23:59:59Z"}}, "aggs": {"d": {"date_histogram": {"field": "published_at", "calendar_interval": "day"}}}}),
    );
    assert_eq!(
        d["aggregations"]["d"]["buckets"][0]["key_as_string"],
        json!("1969-12-31T00:00:00.000Z")
    );
    // an array holding only an unparseable date is empty
    let unparseable = docs
        .iter()
        .filter(|x| x["reprinted_at"] == json!(["not a date"]))
        .count() as u64;
    let empty = docs
        .iter()
        .filter(|x| x["reprinted_at"] == json!([]))
        .count() as u64;
    assert!(unparseable > 0);
    assert_eq!(
        total(json!({"size": 0, "query": {"exists": {"field": "reprinted_at"}}}))
            + unparseable
            + empty,
        400
    );
}

#[cfg(feature = "sqlite")]
mod sqlite_differential {
    use super::*;
    use common::{close, explain};
    use search_esque::store::Store;
    use std::sync::Mutex;

    fn store() -> &'static Mutex<Store> {
        static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
        STORE.get_or_init(|| {
            let (mapping, docs) = corpus();
            let s = Store::in_memory(mapping).unwrap();
            s.load(docs).unwrap();
            Mutex::new(s)
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: cases(), .. ProptestConfig::default() })]

        /// The differential test: the SQLite executor must answer as the reference evaluator on
        /// every generated request, hits and aggregations alike (floats within 1e-9).
        #[test]
        fn sqlite_agrees_with_the_evaluator(req in request()) {
            let (mapping, docs) = corpus();
            let evaluated = evaluate(&req, mapping, docs).unwrap();
            let searched = store().lock().unwrap().search(&req).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(&evaluated["hits"]["total"], &searched["hits"]["total"], "total\n{}", to_elasticsearch(&req));
            prop_assert!(close(&evaluated["hits"]["hits"], &searched["hits"]["hits"]), "hits differ: {:?}\n{}", explain(&evaluated["hits"]["hits"], &searched["hits"]["hits"]), to_elasticsearch(&req));
            let (ea, sa) = (evaluated.get("aggregations").unwrap_or(&Value::Null), searched.get("aggregations").unwrap_or(&Value::Null));
            prop_assert!(close(ea, sa), "aggregations differ: {:?}\n{}", explain(ea, sa), to_elasticsearch(&req));
        }
    }
}
