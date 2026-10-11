#!/usr/bin/env python3
"""Parity against a real Elasticsearch.

Loads fixtures/library into a single-shard index on a local Elasticsearch, runs every
answer fixture there and through `esque eval`, and compares totals, hit order, sources and
aggregations. Standard library only.

    docker run -d --name esque-es -p 127.0.0.1:9200:9200 -e discovery.type=single-node \
        -e xpack.security.enabled=false elasticsearch:9.5.5
    cargo build --release
    python3 dev/es_parity.py                      # all fixtures, both engines (evaluator and SQLite)
    python3 dev/es_parity.py --engine sql 06 23   # one engine, fixtures by number prefix
    python3 dev/es_parity.py --request '{"query": ...}'   # one ad-hoc request, both engines

What the harness changes in the request it sends to Elasticsearch, so that the comparison is
about semantics and not about things the profile defines differently on purpose:

- a final sort on the `id` keyword (a copy of `_id`), since the profile breaks ties by `_id`
  and Elasticsearch cannot sort on `_id`; with no sort at all, the profile's order is `_id`;
- `prefix` on a keyword field gets `case_insensitive: true`, and on a text field its value is
  lower-cased, since the profile's prefix ignores case;
- `cardinality` gets a high `precision_threshold`, since the profile's count is exact;
- `track_total_hits: true`.

Known, deliberate divergences are listed in DIVERGENT with the reason; a mismatch there is
reported as expected. Everything else that differs is a finding.
"""
import json
import subprocess
import sys
import urllib.request
from pathlib import Path

ES = "http://127.0.0.1:9200"
INDEX = "esque-parity"
ROOT = Path(__file__).resolve().parent.parent
FX = ROOT / "fixtures" / "library"
ESQUE = ROOT / "target" / "release" / "esque"

DIVERGENT = {
    "term_case_insensitive": "term on a keyword ignores case in the profile; Elasticsearch is case-sensitive",
    "match_apostrophe_token": "FTS5 unicode61 splits doesn't into doesn + t; the standard analyzer keeps it whole",
    "match_dotted_token": "FTS5 unicode61 splits v2.0 into v2 + 0; the standard analyzer keeps it whole",
    "match_title_micro_sign_folds_to_mu": "FTS5 case-folds the micro sign to mu; the standard analyzer lower-cases only",
}


def http(method, path, body=None, ndjson=None):
    data = None
    headers = {}
    if ndjson is not None:
        data = ndjson.encode()
        headers["Content-Type"] = "application/x-ndjson"
    elif body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(f"{ES}{path}", data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def es_mapping(mapping):
    types = {"keyword": {"type": "keyword"}, "text": {"type": "text"}, "long": {"type": "long"},
             "double": {"type": "double"}, "boolean": {"type": "boolean"}, "date": {"type": "date", "ignore_malformed": True}}
    props = {name: dict(types[spec["type"]]) for name, spec in mapping["properties"].items()}
    props["id"] = {"type": "keyword"}
    return {"settings": {"number_of_shards": 1, "number_of_replicas": 0}, "mappings": {"properties": props}}


def load_index(mapping, docs):
    http("DELETE", f"/{INDEX}")
    status, body = http("PUT", f"/{INDEX}", es_mapping(mapping))
    assert status == 200, body
    lines = []
    for d in docs:
        src = {k: v for k, v in d.items() if not k.startswith("_")}
        src["id"] = str(d["_id"])
        lines.append(json.dumps({"index": {"_index": INDEX, "_id": str(d["_id"])}}))
        lines.append(json.dumps(src, ensure_ascii=False))
    status, body = http("POST", "/_bulk?refresh=true", ndjson="\n".join(lines) + "\n")
    assert status == 200 and not body.get("errors"), str(body)[:500]
    status, body = http("GET", f"/{INDEX}/_count")
    assert body["count"] == len(docs), body


def transform_query(q, mapping):
    """Rewrites prefix clauses for Elasticsearch; everything else passes through."""
    if not isinstance(q, dict):
        return q
    out = {}
    for k, v in q.items():
        if k == "prefix":
            (field, spec), = v.items()
            value = spec["value"] if isinstance(spec, dict) else spec
            if mapping["properties"][field]["type"] == "text":
                out[k] = {field: {"value": value.lower()}}
            else:
                out[k] = {field: {"value": value, "case_insensitive": True}}
        elif k == "bool":
            out[k] = {kk: ([transform_query(c, mapping) for c in vv] if isinstance(vv, list) else transform_query(vv, mapping)) if kk in ("must", "should", "must_not", "filter") else vv for kk, vv in v.items()}
        else:
            out[k] = v
    return out


def transform(request, mapping):
    r = json.loads(json.dumps(request))
    if "query" in r:
        r["query"] = transform_query(r["query"], mapping)
    if "post_filter" in r:
        r["post_filter"] = transform_query(r["post_filter"], mapping)
    sort = r.get("sort", [])
    sort = sort if isinstance(sort, list) else [sort]
    r["sort"] = sort + [{"id": "asc"}]
    for name, agg in (r.get("aggs") or r.get("aggregations") or {}).items():
        if "cardinality" in agg:
            agg["cardinality"]["precision_threshold"] = 40000
    r["track_total_hits"] = True
    return r


def close(a, b, path="", diffs=None):
    """Structural equality with numbers compared as floats (5 == 5.0, avg within 1e-9)."""
    diffs = [] if diffs is None else diffs
    if isinstance(a, bool) or isinstance(b, bool):
        if a != b:
            diffs.append(f"{path}: {a!r} != {b!r}")
    elif isinstance(a, (int, float)) and isinstance(b, (int, float)):
        if abs(float(a) - float(b)) > 1e-9 * max(1.0, abs(float(a)), abs(float(b))):
            diffs.append(f"{path}: {a!r} != {b!r}")
    elif isinstance(a, dict) and isinstance(b, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a or k not in b:
                diffs.append(f"{path}.{k}: only in {'esque' if k in a else 'elasticsearch'} ({a.get(k, b.get(k))!r})")
            else:
                close(a[k], b[k], f"{path}.{k}", diffs)
    elif isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            diffs.append(f"{path}: {len(a)} items vs {len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            close(x, y, f"{path}[{i}]", diffs)
    elif a != b:
        diffs.append(f"{path}: {a!r} != {b!r}")
    return diffs


ENGINE = "eval"
DB = None


def esque_eval(request):
    if ENGINE == "sql":
        argv = [str(ESQUE), "search", "--db", DB, json.dumps(request)]
    else:
        argv = [str(ESQUE), "eval", "--mapping", str(FX / "mapping.json"), "--docs", str(FX / "docs.json"), json.dumps(request)]
    p = subprocess.run(argv, capture_output=True, text=True)
    return json.loads(p.stdout)


def build_db():
    """Loads the corpus into a fresh SQLite store through the CLI, for the sql engine."""
    import atexit
    import shutil
    import tempfile
    global DB
    folder = tempfile.mkdtemp()
    atexit.register(shutil.rmtree, folder, True)
    DB = str(Path(folder) / "parity.db")
    p = subprocess.run([str(ESQUE), "load", "--mapping", str(FX / "mapping.json"), "--docs", str(FX / "docs.json"), "--db", DB],
                       capture_output=True, text=True)
    assert p.returncode == 0, p.stdout + p.stderr


def compare(name, request, mapping):
    ours = esque_eval(request)
    if "error" in ours:
        return [f"esque refused: {ours['error']}"]
    status, theirs = http("POST", f"/{INDEX}/_search", transform(request, mapping))
    if status != 200:
        return [f"elasticsearch refused ({status}): {json.dumps(theirs)[:300]}"]
    diffs = []
    close(ours["hits"]["total"]["value"], theirs["hits"]["total"]["value"], "total", diffs)
    our_hits = ours["hits"]["hits"]
    their_hits = theirs["hits"]["hits"]
    close([h["_id"] for h in our_hits], [h["_id"] for h in their_hits], "hit_ids", diffs)
    for i, (h1, h2) in enumerate(zip(our_hits, their_hits)):
        s2 = h2.get("_source")
        if s2 is not None:
            s2 = {k: v for k, v in s2.items() if k != "id"}
        close(h1.get("_source"), s2, f"hits[{i}]._source", diffs)
    if "aggregations" in ours or "aggregations" in theirs:
        close(ours.get("aggregations"), theirs.get("aggregations"), "aggregations", diffs)
    return diffs


def main():
    global ENGINE
    args = sys.argv[1:]
    engines = ["eval", "sql"]
    if args[:1] == ["--engine"]:
        engines = [args[1]]
        args = args[2:]
    mapping = json.load(open(FX / "mapping.json"))
    docs = json.load(open(FX / "docs.json"))
    status, info = http("GET", "/")
    print(f"elasticsearch {info['version']['number']} at {ES}")
    load_index(mapping, docs)
    print(f"indexed {len(docs)} records into {INDEX} (1 shard)\n")

    if args[:1] == ["--request"]:
        req = json.loads(args[1])
        for ENGINE in engines:
            if ENGINE == "sql":
                build_db()
            diffs = compare("ad hoc", req, mapping)
            print(f"[{ENGINE}] " + ("MATCH" if not diffs else "\n".join(diffs)))
        return

    files = sorted((FX / "queries").glob("*.json"))
    if args:
        files = [f for f in files if any(f.name.startswith(a) for a in args)]
    acc = [0, 0, []]
    for ENGINE in engines:
        if ENGINE == "sql":
            build_db()
        print(f"--- engine: {ENGINE}")
        run_fixtures(files, mapping, acc)
    matched, expected, findings = acc
    print(f"\n{matched} match, {expected} expected divergences, {len(findings)} findings")
    sys.exit(1 if findings else 0)


def run_fixtures(files, mapping, acc):
    for f in files:
        fx = json.load(open(f))
        if "error_path" in fx["expect"]:
            continue
        name = fx["name"]
        diffs = compare(name, fx["request"], mapping)
        if not diffs:
            acc[0] += 1
            print(f"MATCH     {name}")
        elif name in DIVERGENT:
            acc[1] += 1
            print(f"EXPECTED  {name}: {DIVERGENT[name]}")
        else:
            acc[2].append((f"{ENGINE}:{name}", diffs))
            print(f"DIFFERS   {name}")
            for d in diffs[:12]:
                print(f"            {d}")
            if len(diffs) > 12:
                print(f"            ... {len(diffs) - 12} more")


if __name__ == "__main__":
    main()
