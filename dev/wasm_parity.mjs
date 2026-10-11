#!/usr/bin/env node
// Every fixture through the WebAssembly path, exactly as the page runs it (the same binding
// callback as web/app.js), compared with the native `esque search` on the same store file.
// Needs: web/pkg (wasm-pack), web/vendor/sqlite (dev/vendor_sqlite.sh), web/demo (esque load),
// target/release/esque. Node 20 or later.
//
//     node dev/wasm_parity.mjs

import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const WEB = path.join(ROOT, 'web');
const ESQUE = path.join(ROOT, 'target', 'release', 'esque');
const DB = path.join(WEB, 'demo', 'library.db');

const { default: init, search } = await import(pathToFileURL(path.join(WEB, 'pkg', 'search_esque.js')));
await init(fs.readFileSync(path.join(WEB, 'pkg', 'search_esque_bg.wasm')));
const { default: sqlite3InitModule } = await import(pathToFileURL(path.join(WEB, 'vendor', 'sqlite', 'index.mjs')));
// the browser build fetches its wasm by URL, which Node cannot; hand it the bytes instead
const sqlite3 = await sqlite3InitModule({ print: () => {}, printErr: () => {}, wasmBinary: fs.readFileSync(path.join(WEB, 'vendor', 'sqlite', 'sqlite3.wasm')) });

const bytes = new Uint8Array(fs.readFileSync(DB));
const db = new sqlite3.oo1.DB();
const p = sqlite3.wasm.allocFromTypedArray(bytes);
const rc = sqlite3.capi.sqlite3_deserialize(db.pointer, 'main', p, bytes.length, bytes.length,
  sqlite3.capi.SQLITE_DESERIALIZE_FREEONCLOSE | sqlite3.capi.SQLITE_DESERIALIZE_RESIZEABLE);
if (rc) throw new Error(`deserialize failed: ${rc}`);

const { makeRunner } = await import(pathToFileURL(path.join(WEB, 'driver.js')));
const runStatement = makeRunner(sqlite3, db);

const mapping = fs.readFileSync(path.join(WEB, 'demo', 'mapping.json'), 'utf8');

function close(a, b, where, diffs) {
  if (typeof a === 'number' && typeof b === 'number') {
    if (Math.abs(a - b) > 1e-9 * Math.max(1, Math.abs(a), Math.abs(b))) diffs.push(`${where}: ${a} != ${b}`);
  } else if (Array.isArray(a) && Array.isArray(b)) {
    if (a.length !== b.length) diffs.push(`${where}: ${a.length} items vs ${b.length}`);
    a.forEach((x, i) => close(x, b[i], `${where}[${i}]`, diffs));
  } else if (a && b && typeof a === 'object' && typeof b === 'object') {
    for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) {
      if (!(k in a) || !(k in b)) diffs.push(`${where}.${k}: only on one side`);
      else close(a[k], b[k], `${where}.${k}`, diffs);
    }
  } else if (a !== b) diffs.push(`${where}: ${JSON.stringify(a)} != ${JSON.stringify(b)}`);
  return diffs;
}

const dir = path.join(ROOT, 'fixtures', 'library', 'queries');
const files = fs.readdirSync(dir).filter((f) => f.endsWith('.json')).sort();
let ok = 0;
const bad = [];
for (const f of files) {
  const fx = JSON.parse(fs.readFileSync(path.join(dir, f), 'utf8'));
  const request = JSON.stringify(fx.request);
  const native = JSON.parse(spawnSync(ESQUE, ['search', '--db', DB, request], { encoding: 'utf8' }).stdout);
  const wasm = JSON.parse(search(mapping, request, runStatement));
  const diffs = [];
  if (native.error || wasm.error) {
    if (!(native.error && wasm.error && native.error.path === wasm.error.path)) diffs.push(`refusal: native ${JSON.stringify(native.error)} vs wasm ${JSON.stringify(wasm.error)}`);
  } else {
    close(native.hits, wasm.hits, 'hits', diffs);
    close(native.aggregations ?? null, wasm.aggregations ?? null, 'aggregations', diffs);
  }
  if (diffs.length) bad.push([fx.name, diffs]);
  else ok += 1;
}
for (const [name, diffs] of bad) console.log(`DIFFERS ${name}\n  ${diffs.slice(0, 6).join('\n  ')}`);
console.log(`\n${ok} of ${files.length} fixtures answer identically through WebAssembly and natively`);
process.exit(bad.length ? 1 : 0);
