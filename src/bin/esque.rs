//! `esque`: the command line for search-esque.
//!
//!   esque parse   --mapping M.json  (REQUEST_JSON | --request FILE)
//!       prints the AST and the canonical Elasticsearch form of a request, or the refusal
//!   esque eval    --mapping M.json --docs DOCS.json (REQUEST_JSON | --request FILE)
//!       runs the reference evaluator over the records and prints the response
//!   esque mapping M.json
//!       validates a mapping and prints its canonical form
//!
//! Every answer is JSON on stdout. A refusal prints `{"error": {...}}` and exits with 2.

use search_esque::dsl::{parse_request, to_elasticsearch};
use search_esque::eval::evaluate;
use search_esque::{Error, Mapping};
use serde_json::{json, Value};
use std::process::exit;

fn usage() -> ! {
    eprintln!(
        "usage:\n  esque parse   --mapping M.json (REQUEST_JSON | --request FILE)\n  esque eval    --mapping M.json --docs DOCS.json (REQUEST_JSON | --request FILE)\n  esque mapping M.json"
    );
    exit(1)
}

fn read_json(path: &str, what: &str) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::new(what, format!("cannot read {}: {}", path, e)))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::new(what, format!("{} is not valid JSON: {}", path, e)))
}

fn fail(e: Error) -> ! {
    println!("{}", serde_json::to_string_pretty(&e.to_json()).unwrap());
    exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or_else(|| usage());
    let mut mapping_path = None;
    let mut docs_path = None;
    let mut request_path = None;
    let mut positional = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--mapping" => {
                mapping_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--docs" => {
                docs_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--request" => {
                request_path = args.get(i + 1).cloned();
                i += 2;
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }
    let result = match cmd {
        "mapping" => {
            let path = positional
                .first()
                .cloned()
                .or(mapping_path)
                .unwrap_or_else(|| usage());
            read_json(&path, "mapping")
                .and_then(|v| Mapping::from_json(&v))
                .map(|m| m.to_json())
        }
        "parse" | "eval" => {
            let mapping = read_json(&mapping_path.unwrap_or_else(|| usage()), "mapping")
                .and_then(|v| Mapping::from_json(&v));
            let request = match (&request_path, positional.first()) {
                (Some(p), _) => read_json(p, "request"),
                (None, Some(text)) => serde_json::from_str(text)
                    .map_err(|e| Error::new("$", format!("the request is not valid JSON: {}", e))),
                (None, None) => usage(),
            };
            match (mapping, request) {
                (Err(e), _) | (_, Err(e)) => Err(e),
                (Ok(m), Ok(v)) => parse_request(&v, &m).and_then(|req| {
                    if cmd == "parse" {
                        Ok(json!({"ast": req, "elasticsearch": to_elasticsearch(&req)}))
                    } else {
                        let docs = read_json(&docs_path.unwrap_or_else(|| usage()), "docs")?;
                        let docs = docs.as_array().ok_or_else(|| {
                            Error::new("docs", "the docs file is a JSON list of records")
                        })?;
                        evaluate(&req, &m, docs)
                    }
                }),
            }
        }
        _ => usage(),
    };
    match result {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => fail(e),
    }
}
