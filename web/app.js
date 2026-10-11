// The playground: the crate compiled to WebAssembly translates the request on every keystroke;
// the official SQLite WebAssembly build runs the statements on a demo store when asked. The
// executor that interprets the rows is the crate's own, driven through the `run` callback, so
// the page runs exactly the logic the native store runs.
import init, { parse, compile, search } from './pkg/search_esque.js';

// The official SQLite build, which ships FTS5 and JSON (sql.js does not include FTS5), vendored
// by the build step (see dev/vendor_sqlite.sh) so the page depends on no CDN.
const SQLITE_WASM = './vendor/sqlite/index.mjs';

const EXAMPLES = [
  ['Available mysteries, by genre', {
    query: { bool: { filter: [{ term: { available: true } }, { terms: { genres: ['mystery'] } }] } },
    aggs: { genres: { terms: { field: 'genres', size: 5 } } },
    sort: [{ rating: 'desc' }], size: 3, _source: ['title', 'author', 'rating'],
  }],
  ['Books per format and per decade', {
    size: 0,
    aggs: {
      by_format: { terms: { field: 'format' } },
      per_year: { date_histogram: { field: 'published_at', calendar_interval: 'year', min_doc_count: 1 } },
    },
  }],
  ['Cheap or pricey: a range aggregation', {
    size: 0, query: { exists: { field: 'price' } },
    aggs: { price: { range: { field: 'price', ranges: [{ to: 10 }, { from: 10, to: 30 }, { from: 30, key: 'pricey' }] } }, stats: { stats: { field: 'price' } } },
  }],
  ['Words in the title, a phrase in a note', {
    query: { bool: { must: [{ match: { title: { query: 'harbour letters', operator: 'or' } } }], should: [{ match_phrase: { notes: 'salt bread' } }] } },
    size: 5, _source: ['title', 'notes'],
  }],
  ['Not in English, added this year, newest first', {
    query: { bool: { must_not: { term: { language: 'en' } }, filter: { range: { added_at: { gte: '2026-01-01' } } } } },
    sort: [{ added_at: 'desc' }], size: 5, _source: ['title', 'language', 'added_at'],
  }],
  ['Refused: a clause outside the profile', {
    query: { bool: { filter: [{ term: { format: 'ebook' } }, { wildcard: { title: 'har*' } }] } },
  }],
  ['Refused: a quoted number', { query: { range: { pages: { gte: '500' } } } }],
];

const $ = (id) => document.getElementById(id);
let mapping = '';
let db = null;
let debounce = null;

function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

function showError(err) {
  const box = $('error');
  if (!err) { box.hidden = true; box.innerHTML = ''; return; }
  let html = `<b>Refused at <code>${esc(err.path)}</code></b>: ${esc(err.message)}`;
  if (err.hint) html += `<br><span class="muted">Hint:</span> ${esc(err.hint)}`;
  if (err.supported && err.supported.length) html += `<br><span class="muted">Supported here:</span> <code>${err.supported.map(esc).join('</code>, <code>')}</code>`;
  box.innerHTML = html;
  box.hidden = false;
}

function statement(title, st) {
  const params = st.params.length
    ? `<p class="params">parameters: ${st.params.map((p) => `<b>${esc(JSON.stringify(p))}</b>`).join(', ')}</p>`
    : '';
  return `<div class="stmt"><h3>${esc(title)}</h3><pre class="code">${esc(st.sql)}</pre>${params}</div>`;
}

function renderSql(c) {
  const parts = [statement('1. Materialise the matching ids', c.materialize), statement('2. Total', c.total)];
  if (c.hits) parts.push(statement('3. The page of hits', c.hits));
  c.aggs.forEach((a, i) => {
    a.statements.forEach((st, j) => {
      const label = a.statements.length > 1 ? `${a.name} (${a.kind.kind}, ${j + 1} of ${a.statements.length})` : `${a.name} (${a.kind.kind})`;
      parts.push(statement(`${4 + i}. Aggregation ${label}`, st));
    });
  });
  parts.push(statement('Last. Clean up', { sql: c.cleanup, params: [] }));
  parts.push(`<p class="muted">All of it runs inside one read transaction. Every value is a bound parameter; identifiers come only from the mapping.</p>`);
  $('panel-sql').innerHTML = parts.join('');
}

function translate() {
  const req = $('request').value;
  const c = JSON.parse(compile(mapping, req));
  if (c.error) {
    showError(c.error);
    $('panel-sql').innerHTML = '';
    $('panel-ast').textContent = '';
    $('panel-canonical').textContent = '';
    $('run').disabled = true;
    return;
  }
  showError(null);
  renderSql(c);
  const p = JSON.parse(parse(mapping, req));
  $('panel-ast').textContent = JSON.stringify(p.ast, null, 2);
  $('panel-canonical').textContent = JSON.stringify(p.elasticsearch, null, 2);
  $('run').disabled = !db;
}

// The executor's driver: one statement in, its rows out, as a JSON string. Errors come back
// as {"error"} so the crate can report them with a path.
function runStatement(sql, paramsJson) {
  try {
    const params = JSON.parse(paramsJson);
    const rows = db.exec({ sql, bind: params.length ? params : undefined, returnValue: 'resultRows', rowMode: 'array' });
    // integers beyond 2^53 arrive as BigInt, which JSON cannot carry
    return JSON.stringify({ rows }, (_, v) => (typeof v === 'bigint' ? Number(v) : v));
  } catch (e) {
    return JSON.stringify({ error: String(e && e.message ? e.message : e) });
  }
}

function bucketTable(agg) {
  if (!agg.buckets) return `<pre class="code">${esc(JSON.stringify(agg, null, 2))}</pre>`;
  const max = Math.max(1, ...agg.buckets.map((b) => b.doc_count));
  const rows = agg.buckets.map((b) => {
    const key = b.key_as_string !== undefined ? b.key_as_string : b.key;
    return `<tr><td>${esc(key)}</td><td>${b.doc_count}</td><td><span class="bar" style="width:${Math.round(120 * b.doc_count / max)}px"></span></td></tr>`;
  }).join('');
  const other = agg.sum_other_doc_count ? `<tr><td class="muted">other</td><td class="muted">${agg.sum_other_doc_count}</td><td></td></tr>` : '';
  return `<table class="buckets"><tr><th>key</th><th>docs</th><th></th></tr>${rows}${other}</table>`;
}

function renderResult(res) {
  const total = res.hits.total.value;
  const n = res.hits.hits.length;
  let html = `<div class="summary"><span><b>${total}</b> matching</span><span><b>${n}</b> on this page</span><span class="ok">exact, from SQLite in this tab</span></div>`;
  for (const [name, agg] of Object.entries(res.aggregations || {})) {
    html += `<h3 class="muted" style="font-size:13px;margin:8px 0 2px">${esc(name)}</h3>${bucketTable(agg)}`;
  }
  html += `<pre class="code">${esc(JSON.stringify(res, null, 2))}</pre>`;
  $('panel-result').innerHTML = html;
}

function run() {
  const res = JSON.parse(search(mapping, $('request').value, runStatement));
  activate('result');
  if (res.error) {
    $('panel-result').innerHTML = '';
    showError(res.error);
    return;
  }
  showError(null);
  renderResult(res);
}

function activate(tab) {
  document.querySelectorAll('.tab').forEach((t) => t.classList.toggle('active', t.dataset.tab === tab));
  document.querySelectorAll('.panel').forEach((p) => p.classList.toggle('active', p.id === `panel-${tab}`));
}

function renderFields(m) {
  const props = JSON.parse(m).properties;
  $('fields').innerHTML = Object.entries(props)
    .map(([name, f]) => `<div>${esc(name)} <span class="ty">${esc(f.type)}${f.array ? '[]' : ''}</span></div>`)
    .join('');
}

async function loadDemoStore() {
  const { default: sqlite3InitModule } = await import(SQLITE_WASM);
  const sqlite3 = await sqlite3InitModule({ print: () => {}, printErr: () => {} });
  const bytes = new Uint8Array(await (await fetch('./demo/library.db')).arrayBuffer());
  const candidate = new sqlite3.oo1.DB();
  const p = sqlite3.wasm.allocFromTypedArray(bytes);
  const rc = sqlite3.capi.sqlite3_deserialize(
    candidate.pointer, 'main', p, bytes.length, bytes.length,
    sqlite3.capi.SQLITE_DESERIALIZE_FREEONCLOSE | sqlite3.capi.SQLITE_DESERIALIZE_RESIZEABLE,
  );
  if (rc) throw new Error(`could not load the demo store (SQLite error ${rc})`);
  const fts = candidate.exec({ sql: "SELECT sqlite_compileoption_used('ENABLE_FTS5')", returnValue: 'resultRows', rowMode: 'array' });
  if (!fts.length || Number(fts[0][0]) !== 1) throw new Error('this SQLite build has no FTS5, so match queries would fail');
  return candidate;
}

async function main() {
  await init();
  mapping = await (await fetch('./demo/mapping.json')).text();
  renderFields(mapping);
  const select = $('examples');
  EXAMPLES.forEach(([title], i) => {
    const o = document.createElement('option');
    o.value = String(i);
    o.textContent = title;
    select.appendChild(o);
  });
  const show = (i) => { $('request').value = JSON.stringify(EXAMPLES[i][1], null, 2); translate(); };
  select.addEventListener('change', () => show(Number(select.value)));
  $('request').addEventListener('input', () => { clearTimeout(debounce); debounce = setTimeout(translate, 150); });
  $('run').addEventListener('click', run);
  document.querySelectorAll('.tab').forEach((t) => t.addEventListener('click', () => activate(t.dataset.tab)));
  show(0);
  $('status').textContent = 'compiler ready · loading the demo store…';
  try {
    db = await loadDemoStore();
    $('status').textContent = 'compiler and demo store ready';
    $('run').disabled = $('error').hidden === false;
  } catch (e) {
    $('status').textContent = `compiler ready · demo store unavailable: ${e.message}`;
  }
}

main().catch((e) => { $('status').textContent = `failed to start: ${e.message}`; });
