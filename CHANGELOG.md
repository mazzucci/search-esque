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
- Fixtures: a generated 400-record library catalogue designed around the edge cases (case
  variants, awkward keyword characters, empty and duplicate arrays, a date array, a text array,
  whole-number floats, negatives, nulls, dates before 1970, decomposed accents), and 80 requests
  with their answers.
- Parity with a real Elasticsearch 9.5.5 for both engines: every fixture matches; three
  semantics adopted from it (date-only bounds round up for `gt` and `lte`, range buckets
  ordered by `from`, `minimum_should_match: 0` still needs one `should`).
- The SQLite store is held to the evaluator on 200 random requests per test run.
- From the first independent review, fixed before merge: date arrays could not be loaded or
  queried; `_source` was rebuilt from columns and lost data (it is now stored as given);
  the evaluator's tokenizer did not match FTS5 (it is now measured from SQLite itself,
  case folding included, and `dev/tokenizer_parity.py` checks it); text arrays were indexed
  as one JSON string (now one FTS row per value, so a phrase never spans two elements); the
  mapping accepted names the schema could not hold (`__`, case-insensitive duplicates, FTS5's
  reserved names); a failed `load` left a half-built file behind (the store is now built beside
  its name and moved into place); `from + size` beyond 10,000 and histograms beyond 65,536
  buckets are refused as in Elasticsearch; `sum` and `avg` use compensated summation on both
  engines; a value of the wrong type is empty on both engines; a `term` on a date without a
  time part means the whole day, as in Elasticsearch.
- `esque tokens` and `esque fts-tokens`, to see how a text splits on each side.
- The playground (`web/`, published with GitHub Pages): the request on the left, the compiled
  statements on the right, and a Run button that executes them on a demo store with the
  official SQLite WebAssembly build, through the crate's own executor. The executor moved out
  of the store into `exec`, behind a driver trait, so rusqlite and the browser run the same
  code; the `wasm` feature exposes `parse`, `compile`, `mapping`, `schema`, `tokens` and
  `search` to JavaScript.
- From the playground's review: parameters cross the JavaScript boundary typed, and the page
  binds reals with `sqlite3_bind_double`, since a JSON `10.0` becomes `10` and SQLite would
  bind it as an integer (integer division in histograms, wrap-around for huge bounds); the
  executor rolls back on any failure instead of committing, so a connection is never left in
  a transaction; rows that do not have SQLite's shape are refused (a fractional or negative
  count, a hit without `_id` and `_source`, a missing histogram bounds row); a refusal retires
  the previous result in the page; the vendored SQLite build is checksum-pinned and ships its
  licence; `dev/wasm_parity.mjs` runs every fixture through the WebAssembly path in CI; the
  corpus gains `stock_delta`, a negative whole-number field, with a histogram fixture over it.
- From the second review: the case fold is measured from the bundled SQLite for every code
  point (nothing comes from the toolchain's Unicode tables any more); `terms` on a date without
  a time part means the whole day, like `term`; a histogram whose bucket indexes would overflow
  is refused before bucketing; sums run on doubles on both engines, as in Elasticsearch; a
  refused aggregation's error names it (`aggs.h`); a store file built before the format tag
  gets a rebuild hint; the evaluator's fixture checks run without the `sqlite` feature too.
