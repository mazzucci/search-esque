//! Properties over randomly generated requests, within the GitHub issues mapping.
//!
//! - printing an AST to Elasticsearch JSON and parsing it back gives the same AST;
//! - the AST survives its own JSON serialisation;
//! - a query and its negation partition the corpus (the `must_not` semantics on empty fields);
//! - a terms aggregation's counts add up to the records that have the field;
//! - a page holds min(size, total - from) hits with distinct ids.

#![cfg(feature = "sqlite")]

use proptest::prelude::*;
use search_esque::ast::*;
use search_esque::dsl::{parse_request, to_elasticsearch};
use search_esque::eval::evaluate;
use search_esque::store::Store;
use search_esque::Mapping;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;

const KEYWORDS: &[&str] = &[
    "state",
    "state_reason",
    "author",
    "author_type",
    "author_association",
    "closed_by",
];
const KEYWORD_ARRAYS: &[&str] = &["labels"];
const TEXTS: &[&str] = &["title", "body"];
const NUMBERS: &[&str] = &["comments", "reactions"];
const BOOLS: &[&str] = &["draft"];
const DATES: &[&str] = &["created_at", "updated_at", "closed_at", "merged_at"];
const ALL: &[&str] = &[
    "state",
    "state_reason",
    "author",
    "author_type",
    "author_association",
    "closed_by",
    "labels",
    "title",
    "body",
    "comments",
    "reactions",
    "draft",
    "created_at",
    "updated_at",
    "closed_at",
    "merged_at",
];
const AGGREGATABLE: &[&str] = &[
    "state",
    "state_reason",
    "author",
    "author_type",
    "author_association",
    "closed_by",
    "labels",
    "comments",
    "reactions",
    "draft",
    "created_at",
    "updated_at",
    "closed_at",
    "merged_at",
];
const SORTABLE: &[&str] = &[
    "state",
    "state_reason",
    "author",
    "author_type",
    "author_association",
    "closed_by",
    "comments",
    "reactions",
    "draft",
    "created_at",
    "updated_at",
    "closed_at",
    "merged_at",
];
const WORDS: &[&str] = &[
    "open",
    "closed",
    "completed",
    "not_planned",
    "reopened",
    "bug",
    "enhancement",
    "User",
    "Bot",
    "MEMBER",
    "NONE",
    "facet",
    "search",
    "index",
    "error",
    "chat",
    "completions",
    "filter",
    "sort",
];
const DATE_LITERALS: &[&str] = &[
    "2023-05-25T12:58:21Z",
    "2025-01-01",
    "2026-01-15",
    "2026-06-01T00:00:00Z",
    "2026-09-01",
    "2026-10-01T12:30:00Z",
];

fn corpus() -> &'static (Mapping, Vec<Value>) {
    static CORPUS: OnceLock<(Mapping, Vec<Value>)> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/github-issues");
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
        2 => (prop_oneof![pick(KEYWORDS), pick(TEXTS)], "[a-z]{1,4}").prop_map(|(field, value)| Query::Prefix { field, value }),
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
        2 => (pick(NUMBERS), 1i64..50, 0u64..2).prop_map(|(field, iv, m)| AggKind::Histogram { field, interval: iv as f64, min_doc_count: m }),
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
    #![proptest_config(ProptestConfig { cases: 200, .. ProptestConfig::default() })]

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
fn the_known_good_numbers() {
    let (mapping, docs) = corpus();
    let total = |v: Value| -> u64 {
        let req = parse_request(&v, mapping).unwrap();
        evaluate(&req, mapping, docs).unwrap()["hits"]["total"]["value"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(total(json!({"query": {"match_all": {}}})), 336);
    assert_eq!(
        total(json!({"query": {"bool": {"must_not": {"term": {"state_reason": "completed"}}}}})),
        299
    );
    assert_eq!(
        total(json!({"query": {"bool": {"must_not": {"term": {"draft": true}}}}})),
        316
    );
    assert_eq!(
        total(
            json!({"query": {"bool": {"must_not": {"range": {"closed_at": {"gte": "2026-09-01"}}}}}})
        ),
        265
    );
    let req = parse_request(
        &json!({"size": 0, "aggs": {"s": {"terms": {"field": "state"}}}}),
        mapping,
    )
    .unwrap();
    let res = evaluate(&req, mapping, docs).unwrap();
    assert_eq!(
        res["aggregations"]["s"]["buckets"],
        json!([{"key": "closed", "doc_count": 241}, {"key": "open", "doc_count": 95}])
    );
}

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
    #![proptest_config(ProptestConfig { cases: 200, .. ProptestConfig::default() })]

    /// The differential test: the SQLite executor must answer exactly as the reference evaluator
    /// on every generated request, hits and aggregations alike.
    #[test]
    fn sqlite_agrees_with_the_evaluator(req in request()) {
        let (mapping, docs) = corpus();
        let evaluated = evaluate(&req, mapping, docs).unwrap();
        let searched = store().lock().unwrap().search(&req).map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(&evaluated["hits"]["total"], &searched["hits"]["total"], "total\n{}", to_elasticsearch(&req));
        prop_assert_eq!(&evaluated["hits"]["hits"], &searched["hits"]["hits"], "hits\n{}", to_elasticsearch(&req));
        prop_assert_eq!(evaluated.get("aggregations"), searched.get("aggregations"), "aggregations\n{}", to_elasticsearch(&req));
    }
}
