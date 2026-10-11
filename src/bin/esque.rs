//! `esque`: the command line for search-esque.
//!
//!   esque parse   --mapping M.json (REQUEST_JSON | --request FILE)
//!       the AST and the canonical Elasticsearch form of a request, or the refusal
//!   esque compile --mapping M.json (REQUEST_JSON | --request FILE)
//!       the SQL statements and parameters the request compiles to
//!   esque eval    --mapping M.json --docs DOCS.json (REQUEST_JSON | --request FILE)
//!       the reference evaluator over the records
//!   esque load    --mapping M.json --docs DOCS.json --db STORE.db
//!       builds a SQLite store from the records (the file must not exist)
//!   esque search  --db STORE.db (REQUEST_JSON | --request FILE)
//!       runs the request on the store
//!   esque mapping M.json
//!       validates a mapping and prints its canonical form
//!
//! Every answer is JSON on stdout. A refusal prints `{"error": {...}}` and exits with 2.

use search_esque::dsl::{parse_request, to_elasticsearch};
use search_esque::eval::evaluate;
use search_esque::sql::compile;
use search_esque::{Error, Mapping};
use serde_json::{json, Value};
use std::process::exit;

fn usage() -> ! {
    eprintln!(
        "usage:\n  esque parse   --mapping M.json (REQUEST_JSON | --request FILE)\n  esque compile --mapping M.json (REQUEST_JSON | --request FILE)\n  esque eval    --mapping M.json --docs DOCS.json (REQUEST_JSON | --request FILE)\n  esque load    --mapping M.json --docs DOCS.json --db STORE.db\n  esque search  --db STORE.db (REQUEST_JSON | --request FILE)\n  esque mapping M.json"
    );
    exit(1)
}

fn read_json(path: &str, what: &str) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::new(what, format!("cannot read {}: {}", path, e)))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::new(what, format!("{} is not valid JSON: {}", path, e)))
}

fn read_docs(path: &str) -> Result<Vec<Value>, Error> {
    match read_json(path, "docs")? {
        Value::Array(items) => Ok(items),
        _ => Err(Error::new(
            "docs",
            "the docs file is a JSON list of records",
        )),
    }
}

fn fail(e: Error) -> ! {
    println!("{}", serde_json::to_string_pretty(&e.to_json()).unwrap());
    exit(2)
}

struct Args {
    mapping: Option<String>,
    docs: Option<String>,
    request: Option<String>,
    db: Option<String>,
    positional: Vec<String>,
}

fn parse_args(args: &[String]) -> Args {
    let mut out = Args {
        mapping: None,
        docs: None,
        request: None,
        db: None,
        positional: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).cloned();
        match args[i].as_str() {
            "--mapping" => {
                out.mapping = value;
                i += 2;
            }
            "--docs" => {
                out.docs = value;
                i += 2;
            }
            "--request" => {
                out.request = value;
                i += 2;
            }
            "--db" => {
                out.db = value;
                i += 2;
            }
            other => {
                out.positional.push(other.to_string());
                i += 1;
            }
        }
    }
    out
}

fn request_of(a: &Args) -> Result<Value, Error> {
    match (&a.request, a.positional.first()) {
        (Some(p), _) => read_json(p, "request"),
        (None, Some(text)) => serde_json::from_str(text)
            .map_err(|e| Error::new("$", format!("the request is not valid JSON: {}", e))),
        (None, None) => usage(),
    }
}

fn mapping_of(a: &Args) -> Result<Mapping, Error> {
    let path = a.mapping.clone().unwrap_or_else(|| usage());
    read_json(&path, "mapping").and_then(|v| Mapping::from_json(&v))
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = argv.first().map(String::as_str).unwrap_or_else(|| usage());
    let a = parse_args(&argv[1..]);
    let result: Result<Value, Error> = match cmd {
        "mapping" => {
            let path = a
                .positional
                .first()
                .cloned()
                .or(a.mapping.clone())
                .unwrap_or_else(|| usage());
            read_json(&path, "mapping")
                .and_then(|v| Mapping::from_json(&v))
                .map(|m| m.to_json())
        }
        "parse" => mapping_of(&a).and_then(|m| {
            let req = parse_request(&request_of(&a)?, &m)?;
            Ok(json!({"ast": req, "elasticsearch": to_elasticsearch(&req)}))
        }),
        "compile" => mapping_of(&a).and_then(|m| {
            let req = parse_request(&request_of(&a)?, &m)?;
            serde_json::to_value(compile(&req, &m)).map_err(|e| Error::new("$", e.to_string()))
        }),
        "eval" => mapping_of(&a).and_then(|m| {
            let req = parse_request(&request_of(&a)?, &m)?;
            let docs = read_docs(&a.docs.clone().unwrap_or_else(|| usage()))?;
            evaluate(&req, &m, &docs)
        }),
        #[cfg(feature = "sqlite")]
        "load" => mapping_of(&a).and_then(|m| {
            let docs = read_docs(&a.docs.clone().unwrap_or_else(|| usage()))?;
            let db = a.db.clone().unwrap_or_else(|| usage());
            let store = search_esque::store::Store::create(&db, &m)?;
            let n = store.load(&docs)?;
            Ok(json!({"db": db, "records": n, "fields": m.fields.len()}))
        }),
        #[cfg(feature = "sqlite")]
        "search" => {
            let db = a.db.clone().unwrap_or_else(|| usage());
            search_esque::store::Store::open(&db).and_then(|store| {
                let req = parse_request(&request_of(&a)?, store.mapping())?;
                store.search(&req)
            })
        }
        _ => usage(),
    };
    match result {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => fail(e),
    }
}
