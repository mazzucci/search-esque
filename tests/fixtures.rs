//! Every file under fixtures/library/queries is a request with what it must answer. The
//! expectations were seeded by an independent Python oracle (dev/python_oracle.py) and pinned
//! to a real Elasticsearch where the two differed. The reference evaluator is held to them
//! always; with the `sqlite` feature the store is held to them too, and the two engines are
//! held to each other on the whole response.

use search_esque::ast::Request;
use search_esque::dsl::parse_request;
use search_esque::eval::evaluate;
use search_esque::{Error, Mapping};
use serde_json::Value;
use std::fs;
use std::path::Path;

#[allow(dead_code)]
mod common;
use common::{close, explain};

fn load(path: &Path) -> Value {
    serde_json::from_str(
        &fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {}", path.display(), e)),
    )
    .unwrap()
}

/// What a response must satisfy, from a fixture's `expect`.
fn check(engine: &str, name: &str, res: &Value, expect: &Value, failures: &mut Vec<String>) {
    let tag = format!("{} [{}]", name, engine);
    if let Some(t) = expect.get("total") {
        let got = &res["hits"]["total"]["value"];
        if got != t {
            failures.push(format!("{}: total {} expected {}", tag, got, t));
        }
    }
    if let Some(a) = expect.get("aggregations") {
        let got = &res["aggregations"];
        if !close(got, a) {
            failures.push(format!(
                "{}: aggregations differ: {}\n  got:      {}\n  expected: {}",
                tag,
                explain(got, a).join("; "),
                got,
                a
            ));
        }
    }
    if let Some(ids) = expect.get("hit_ids") {
        let got: Vec<Value> = res["hits"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["_id"].clone())
            .collect();
        if Value::Array(got.clone()) != *ids {
            failures.push(format!("{}: hit ids {:?} expected {}", tag, got, ids));
        }
    }
    if let Some(keys) = expect.get("source_keys") {
        let first = &res["hits"]["hits"][0]["_source"];
        let mut got: Vec<String> = first
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        got.sort();
        let mut want: Vec<String> = keys
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect();
        want.sort();
        if got != want {
            failures.push(format!(
                "{}: _source keys {:?} expected {:?}",
                tag, got, want
            ));
        }
    }
    if let Some(n) = expect.get("hit_count") {
        let got = res["hits"]["hits"].as_array().unwrap().len();
        if n.as_u64() != Some(got as u64) {
            failures.push(format!("{}: {} hits on the page, expected {}", tag, got, n));
        }
    }
}

/// The two engines must give the same total, the same hits in the same order with the same
/// `_source`, and the same aggregations (floats within 1e-9).
fn same_response(name: &str, a: &Value, b: &Value, failures: &mut Vec<String>) {
    if a["hits"]["total"] != b["hits"]["total"] {
        failures.push(format!(
            "{}: engines disagree on total: {} vs {}",
            name, a["hits"]["total"]["value"], b["hits"]["total"]["value"]
        ));
    }
    if !close(&a["hits"]["hits"], &b["hits"]["hits"]) {
        failures.push(format!(
            "{}: engines disagree on hits: {:?}",
            name,
            explain(&a["hits"]["hits"], &b["hits"]["hits"])
        ));
    }
    let (aa, ba) = (
        a.get("aggregations").unwrap_or(&Value::Null),
        b.get("aggregations").unwrap_or(&Value::Null),
    );
    if !close(aa, ba) {
        failures.push(format!(
            "{}: engines disagree on aggregations: {:?}",
            name,
            explain(aa, ba)
        ));
    }
}

/// A refusal's path, whether it came from the walker or from running the request.
fn refusal_matches(
    name: &str,
    path: &str,
    result: Result<Value, Error>,
    engine: &str,
    failures: &mut Vec<String>,
) {
    match result {
        Err(e) if e.path == path => {}
        Err(e) => failures.push(format!(
            "{} [{}]: refused at {} instead of {} ({})",
            name, engine, e.path, path, e.message
        )),
        Ok(_) => failures.push(format!(
            "{} [{}]: accepted, expected a refusal at {}",
            name, engine, path
        )),
    }
}

struct Corpus {
    mapping: Mapping,
    docs: Vec<Value>,
    fixtures: Vec<(String, Value)>,
}

fn corpus() -> Corpus {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/library");
    let mapping = Mapping::from_json(&load(&root.join("mapping.json"))).unwrap();
    let docs = load(&root.join("docs.json")).as_array().unwrap().clone();
    assert_eq!(
        docs.len(),
        400,
        "the corpus is the 400-record generated library"
    );
    let mut files: Vec<_> = fs::read_dir(root.join("queries"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert!(
        files.len() >= 60,
        "expected the seeded fixtures, found {}",
        files.len()
    );
    let fixtures = files
        .iter()
        .map(|p| {
            let fx = load(p);
            (fx["name"].as_str().unwrap_or("?").to_string(), fx)
        })
        .collect();
    Corpus {
        mapping,
        docs,
        fixtures,
    }
}

/// A second engine to hold to the fixtures: a request in, a response or a refusal out.
type Engine<'a> = &'a dyn Fn(&Request) -> Result<Value, Error>;

/// Runs every fixture through the evaluator and, when given, a second engine.
fn run_all(c: &Corpus, second: Option<Engine>) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, fx) in &c.fixtures {
        let expect = &fx["expect"];
        let parsed = parse_request(&fx["request"], &c.mapping);
        let error_path = expect.get("error_path").and_then(Value::as_str);
        let req = match (parsed, error_path) {
            (Err(e), Some(path)) => {
                if e.path != path {
                    failures.push(format!(
                        "{}: refused at {} instead of {} ({})",
                        name, e.path, path, e.message
                    ));
                }
                continue;
            }
            (Err(e), None) => {
                failures.push(format!("{}: refused: {}", name, e));
                continue;
            }
            (Ok(r), _) => r,
        };
        let evaluated = evaluate(&req, &c.mapping, &c.docs);
        let searched = second.map(|f| f(&req));
        if let Some(path) = error_path {
            refusal_matches(name, path, evaluated, "evaluator", &mut failures);
            if let Some(s) = searched {
                refusal_matches(name, path, s, "sqlite", &mut failures);
            }
            continue;
        }
        let evaluated = match evaluated {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: evaluation failed: {}", name, e));
                continue;
            }
        };
        check("evaluator", name, &evaluated, expect, &mut failures);
        if let Some(s) = searched {
            match s {
                Ok(s) => {
                    check("sqlite", name, &s, expect, &mut failures);
                    same_response(name, &evaluated, &s, &mut failures);
                }
                Err(e) => failures.push(format!("{}: sqlite failed: {}", name, e)),
            }
        }
    }
    failures
}

#[test]
fn fixtures_match_expectations_on_the_evaluator() {
    let c = corpus();
    let failures = run_all(&c, None);
    assert!(
        failures.is_empty(),
        "{} problems over {} fixtures:\n{}",
        failures.len(),
        c.fixtures.len(),
        failures.join("\n")
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn fixtures_match_expectations_on_both_engines() {
    use search_esque::store::Store;
    let c = corpus();
    let store = Store::in_memory(&c.mapping).unwrap();
    assert_eq!(store.load(&c.docs).unwrap(), 400);
    assert_eq!(store.count().unwrap(), 400);
    let search = |req: &Request| store.search(req);
    let failures = run_all(&c, Some(&search));
    assert!(
        failures.is_empty(),
        "{} problems over {} fixtures:\n{}",
        failures.len(),
        c.fixtures.len(),
        failures.join("\n")
    );
}
