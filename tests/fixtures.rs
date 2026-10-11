//! Every file under fixtures/github-issues/queries is a request with what it must answer.
//! The expectations were seeded by an independent Python oracle (dev/python_oracle.py) and are
//! held here against the Rust evaluator. Later, the SQL compiler is held to the same files.

use search_esque::dsl::parse_request;
use search_esque::eval::evaluate;
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

#[test]
fn fixtures_match_expectations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/github-issues");
    let mapping = Mapping::from_json(&load(&root.join("mapping.json"))).unwrap();
    let docs = load(&root.join("docs.json"));
    let docs = docs.as_array().unwrap();
    assert_eq!(
        docs.len(),
        336,
        "the corpus is the 336-record GitHub replay"
    );

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
        let res = match evaluate(&req, &mapping, docs) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: evaluation failed: {}", name, e));
                continue;
            }
        };
        if let Some(t) = expect.get("total") {
            let got = &res["hits"]["total"]["value"];
            if got != t {
                failures.push(format!("{}: total {} expected {}", name, got, t));
            }
        }
        if let Some(a) = expect.get("aggregations") {
            let got = &res["aggregations"];
            if got != a {
                failures.push(format!(
                    "{}: aggregations differ\n  got:      {}\n  expected: {}",
                    name, got, a
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
                failures.push(format!("{}: hit ids {:?} expected {}", name, got, ids));
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
                    name, got, want
                ));
            }
        }
        if let Some(n) = expect.get("hit_count") {
            let got = res["hits"]["hits"].as_array().unwrap().len();
            if n.as_u64() != Some(got as u64) {
                failures.push(format!(
                    "{}: {} hits on the page, expected {}",
                    name, got, n
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} fixtures failed:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
}
