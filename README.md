# search-esque

**Elasticsearch-compatible query DSL and aggregations over SQLite.**

Write the query you already know, `bool`, `term`, `range`, `match`, `aggs` with `terms` and
`date_histogram`, and run it over a plain SQLite file. No search engine, no extension, no
server. The same crate compiles to a native binary, to WebAssembly for the browser, and to a
Python extension.

Why SQLite only: this is for light, local, throwaway data. The index is one file, facets are
exact counts, and nothing you do here can hammer a shared database server with aggregation
queries. A `.db` on a laptop, or in a browser tab, is the whole deployment.

> Status: early. The profile, the Elasticsearch JSON walker, the reference evaluator, the SQL
> compiler and the SQLite executor are in place and tested against each other and against a
> real Elasticsearch. Next: the `query_string` grammar, then WebAssembly. Not on crates.io yet.

## What is accepted

The profile is a bounded subset of the Elasticsearch search API. Everything outside it is
refused with an error that names the path in the request and lists what is accepted there,
never approximated silently.

| | Accepted |
|---|---|
| Queries | `bool` (`must`, `should`, `must_not`, `filter`, `minimum_should_match` 0 or 1), `term`, `terms`, `range`, `exists`, `match` (`operator`), `match_phrase`, `prefix`, `match_all` |
| Aggregations | `terms`, `range`, `histogram`, `date_histogram` (day, month, year); `avg`, `min`, `max`, `sum`, `value_count`, `stats`, `cardinality` |
| Request | `size`, `from`, `sort`, `post_filter`, `_source`, `track_total_hits` |
| Response | `hits.total` (always exact, `relation: eq`), `hits.hits[]` with `_id` and `_source`, `aggregations` in Elasticsearch shape |

Planned: `query_string`, `highlight`, sorting by `_score`, `percentiles`, `extended_stats`,
`top_hits`, `composite`, `date_range`, sub-aggregations. Not planned: `nested`, scripts,
`wildcard`, `regexp`, `fuzzy`, `function_score`, pipeline aggregations.

Deliberate differences from Elasticsearch, each held by a test:

- `term` on a keyword field ignores letter case.
- A quoted number for a number field is refused, not coerced.
- Counts are exact, never approximate. `terms` has no `shard_size`.
- `should` is supported only directly in a `bool`; `minimum_should_match` beyond 0 and 1 is refused.
- With no `sort`, hits come back in `_id` order.
- Text is tokenized the way FTS5's `unicode61` does: lower-cased alphanumeric runs. Elasticsearch's
  standard analyzer keeps `doesn't` and `v1.41.0` whole; here they split.

Checked against a real Elasticsearch 9.5.5 (`dev/es_parity.py`, single shard, same 336 records):
every fixture matches, including date-only range bounds rounding up for `gt` and `lte`, range
buckets ordered by `from`, and `minimum_should_match: 0` still needing one `should`.

## The mapping

```json
{"properties": {
  "state":      {"type": "keyword"},
  "labels":     {"type": "keyword", "array": true},
  "title":      {"type": "text"},
  "comments":   {"type": "long"},
  "draft":      {"type": "boolean"},
  "created_at": {"type": "date"}
}}
```

Elasticsearch's type names are accepted; every numeric type becomes `number`. A field is
multi-valued only when it says `array: true`. `aggregatable` defaults to true for every type
but `text`.

## Try it

```bash
cargo build --release
F=fixtures/github-issues

# build a store from JSON records, then query it
target/release/esque load --mapping $F/mapping.json --docs $F/docs.json --db issues.db
target/release/esque search --db issues.db '{
  "query": {"bool": {"filter": [{"term": {"state": "open"}}, {"terms": {"labels": ["bug"]}}]}},
  "aggs": {"labels": {"terms": {"field": "labels", "size": 5}}},
  "sort": [{"comments": "desc"}], "size": 3
}'

# or see the SQL a request compiles to
target/release/esque compile --mapping $F/mapping.json '{"query": {"term": {"state": "open"}}, "size": 0}'
```

`esque compile` prints the statements and their bound parameters. `esque parse` prints the
AST and the canonical Elasticsearch form of a request, or the refusal. `esque eval` runs the
reference evaluator over a JSON list of records: the definition of the semantics, not the fast
path. `esque mapping` validates a mapping.

## How it works

A request runs as one read transaction on one `docs` table. The matching ids are materialised
once into a temp table; the total, the page of hits and every aggregation are computed from
that set, so the filter runs once and hits and aggregations can never disagree. Keywords have
a case-folded shadow column, dates an epoch-millisecond one, arrays are JSON queried through
`json_each`, text goes through an FTS5 table with the `unicode61` tokenizer. Every value is a
bound parameter; identifiers come only from the validated mapping. The compiler itself does no
I/O: with `--no-default-features` the crate has no SQLite dependency and is ready for
WebAssembly.

```
{"error": {"path": "query.bool.filter[1].wildcard", "message": "'wildcard' is not supported",
           "supported": ["bool", "term", "terms", "range", "exists", "match", "match_phrase", "prefix", "match_all"]}}
```

## How it is tested

- `fixtures/github-issues`: 336 real GitHub issues and pull requests from one public open-source
  project's repository (fetched 2026-10-10) with an Elasticsearch-style mapping, and 46 request fixtures whose
  expectations were computed by an independent Python oracle (`dev/python_oracle.py`).
- Both engines, the evaluator and the SQLite store, answer every fixture, and are compared to
  each other on the whole response: total, hits in order with `_source`, aggregations.
- Property tests (`tests/properties.rs`): printing an AST to Elasticsearch JSON and parsing it
  back is the identity; a query and its `must_not` partition the corpus; a `terms`
  aggregation's counts add up to the records that have the field; pages have the right size
  and distinct ids; and the SQLite store answers exactly as the evaluator on 200 random
  requests per run.
- Parity with a real Elasticsearch 9.5.5 (`dev/es_parity.py`, single shard, same records), for
  both engines, in CI as a required check.

## Layout

```
src/mapping.rs    fields, types, what each allows
src/ast.rs        the typed request
src/dsl.rs        Elasticsearch JSON -> AST (whitelist, path errors) and back
src/eval.rs       the reference evaluator
src/sql.rs        the schema and the statements a request compiles to (no I/O)
src/store.rs      the SQLite executor (feature `sqlite`, on by default)
src/shape.rs      Elasticsearch's response shape, shared by both engines
src/timeutil.rs   ISO dates and calendar buckets without a dependency
src/bin/esque.rs  the command line
```

## License

MIT.
