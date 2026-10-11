# Changelog

## 0.1.0 (unreleased)

- The profile: `bool`, `term`, `terms`, `range`, `exists`, `match`, `match_phrase`, `prefix`,
  `match_all`; aggregations `terms`, `range`, `histogram`, `date_histogram` (day, month, year),
  `avg`, `min`, `max`, `sum`, `value_count`, `stats`, `cardinality`; `size`, `from`, `sort`,
  `post_filter`, `_source`. Everything else is refused with the path in the request and the
  list of what is accepted there.
- An Elasticsearch-shaped mapping (`properties`, Elasticsearch type names, `array`, `aggregatable`).
- The reference evaluator, answering in Elasticsearch's response shape.
- `esque parse`, `esque eval` and `esque mapping`.
- Fixtures: 336 real GitHub issues and pull requests, 52 requests with their answers.
- Parity with a real Elasticsearch 9.5.5: every fixture matches; three semantics adopted from
  it (date-only bounds round up for `gt` and `lte`, range buckets ordered by `from`,
  `minimum_should_match: 0` still needs one `should`).
