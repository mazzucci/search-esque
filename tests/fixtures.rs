//! Every file under fixtures/github-issues/queries is a request with what it must answer.
//! The expectations were seeded by an independent Python oracle (dev/python_oracle.py) and
//! pinned to a real Elasticsearch where the two differed. Both engines are held to them here:
//! the reference evaluator and the SQLite store. And the two engines are held to each other on
//! the whole response.

use search_esque::dsl::parse_request;
use search_esque::eval::evaluate;
use search_esque::store::Store;
use search_esque::Mapping;
use serde_json::Value;
use std::fs;
use std::path::Path;

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
        if got != a {
            failures.push(format!(
                "{}: aggregations differ\n  got:      {}\n  expected: {}",
                tag, got, a
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
}

/// The two engines must give the same total, the same hits in the same order with the same
/// `_source`, and the same aggregations.
fn same_response(name: &str, a: &Value, b: &Value, failures: &mut Vec<String>) {
    if a["hits"]["total"] != b["hits"]["total"] {
        failures.push(format!(
            "{}: engines disagree on total: {} vs {}",
            name, a["hits"]["total"]["value"], b["hits"]["total"]["value"]
        ));
    }
    if a["hits"]["hits"] != b["hits"]["hits"] {
        failures.push(format!(
            "{}: engines disagree on hits\n  evaluator: {}\n  sqlite:    {}",
            name, a["hits"]["hits"], b["hits"]["hits"]
        ));
    }
    if a.get("aggregations") != b.get("aggregations") {
        failures.push(format!(
            "{}: engines disagree on aggregations\n  evaluator: {}\n  sqlite:    {}",
            name,
            a.get("aggregations").unwrap_or(&Value::Null),
            b.get("aggregations").unwrap_or(&Value::Null)
        ));
    }
}

#[test]
fn fixtures_match_expectations_on_both_engines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/github-issues");
    let mapping = Mapping::from_json(&load(&root.join("mapping.json"))).unwrap();
    let docs = load(&root.join("docs.json"));
    let docs = docs.as_array().unwrap();
    assert_eq!(
        docs.len(),
        336,
        "the corpus is the 336-record GitHub replay"
    );
    let store = Store::in_memory(&mapping).unwrap();
    assert_eq!(store.load(docs).unwrap(), 336);
    assert_eq!(store.count().unwrap(), 336);

    let mut files: Vec<_> = fs::read_dir(root.join("queries"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert!(
        files.len() >= 20,
        "expected the seeded fixtures, found {}",
        files.len()
    );

    let mut failures = Vec::new();
    for path in &files {
        let fx = load(path);
        let name = fx["name"].as_str().unwrap_or("?").to_string();
        let expect = &fx["expect"];
        let parsed = parse_request(&fx["request"], &mapping);
        if let Some(ep) = expect.get("error_path").and_then(Value::as_str) {
            match parsed {
                Err(e) if e.path == ep => {}
                Err(e) => failures.push(format!(
                    "{}: refused at {} instead of {} ({})",
                    name, e.path, ep, e.message
                )),
                Ok(_) => failures.push(format!("{}: accepted, expected a refusal at {}", name, ep)),
            }
            continue;
        }
        let req = match parsed {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: refused: {}", name, e));
                continue;
            }
        };
        let evaluated = match evaluate(&req, &mapping, docs) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: evaluation failed: {}", name, e));
                continue;
            }
        };
        let searched = match store.search(&req) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: sqlite failed: {}", name, e));
                continue;
            }
        };
        check("evaluator", &name, &evaluated, expect, &mut failures);
        check("sqlite", &name, &searched, expect, &mut failures);
        same_response(&name, &evaluated, &searched, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "{} problems over {} fixtures:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
}
