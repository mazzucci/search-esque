# Changelog

## 0.1.0 (unreleased)

- The profile: `bool`, `term`, `terms`, `range`, `exists`, `match`, `match_phrase`, `prefix`,
  `match_all`; aggregations `terms`, `range`, `histogram`, `date_histogram` (day, month, year),
  `avg`, `min`, `max`, `sum`, `value_count`, `stats`, `cardinality`; `size`, `from`, `sort`,
  `post_filter`, `_source`. Everything else is refused with the path in the request and the
  list of what is accepted there.
- An Elasticsearch-shaped mapping (`properties`, Elasticsearch type names, `array`, `aggregatable`).
- The reference evaluator, answering in Elasticsearch's response shape.
- The SQL compiler and the SQLite executor: one `docs` table with case-folded and timestamp
  shadow columns, JSON arrays through `json_each`, text through FTS5 `unicode61`; a request
  runs as one read transaction over a materialised set of matching ids, every value bound.
  `--no-default-features` leaves the compiler alone, with no SQLite dependency.
- `esque parse`, `esque compile`, `esque eval`, `esque load`, `esque search` and `esque mapping`.
- Fixtures: 336 real GitHub issues and pull requests, 52 requests with their answers.
- Parity with a real Elasticsearch 9.5.5 for both engines: every fixture matches; three
  semantics adopted from it (date-only bounds round up for `gt` and `lte`, range buckets
  ordered by `from`, `minimum_should_match: 0` still needs one `should`).
- The SQLite store is held to the evaluator on 200 random requests per test run.
