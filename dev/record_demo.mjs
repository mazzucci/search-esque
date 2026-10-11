#!/usr/bin/env node
// Records the README's demo of the playground: drives the page in headless Chrome through a
// short story (run an example, edit a value and watch the SQL follow, run again, an aggregation
// with buckets, a refusal), saving a screenshot for every visible step with how long to hold
// it. dev/make_gif.py assembles the frames into media/playground.gif.
//
//     python3 -m http.server 8765 --directory web &    # the built page (see README)
//     node dev/record_demo.mjs <frames dir> [url]
//
// Needs Google Chrome and puppeteer-core (npm install --no-save puppeteer-core, here or in
// PUPPETEER_HOME). Everything in the recording is the real page: the compiler and SQLite run in
// the browser; only the hands are scripted.

import { createRequire } from 'node:module';
import { mkdirSync, writeFileSync } from 'node:fs';

const require = createRequire(process.env.PUPPETEER_HOME ? `${process.env.PUPPETEER_HOME}/package.json` : import.meta.url);
const puppeteer = require('puppeteer-core');

const dir = process.argv[2] ?? 'frames';
const url = process.argv[3] ?? 'http://127.0.0.1:8765/';
const W = 1160, H = 740;
const CHROME = process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

mkdirSync(dir, { recursive: true });
const browser = await puppeteer.launch({ executablePath: CHROME, headless: true, defaultViewport: { width: W, height: H, deviceScaleFactor: 1 } });
const page = await browser.newPage();
await page.emulateMediaFeatures([{ name: 'prefers-color-scheme', value: 'light' }]);

const frames = [];
async function shot(hold) {
  const file = `${String(frames.length).padStart(4, '0')}.png`;
  await page.screenshot({ path: `${dir}/${file}` });
  frames.push({ file, hold });
}

// A visible pointer, since a headless page paints none.
async function cursor() {
  await page.evaluate(() => {
    if (document.getElementById('rec-cursor')) return;
    const c = document.createElement('div');
    c.id = 'rec-cursor';
    c.style.cssText = 'position:fixed;left:0;top:0;width:22px;height:22px;z-index:99;pointer-events:none;transform:translate(-3px,-2px);'
      + "background:no-repeat url(\"data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 22 22'><path d='M3 2l14 9-6.5 1.2L14 19l-2.6 1.3-3.4-6.6L3 18z' fill='black' stroke='white' stroke-width='1.4' stroke-linejoin='round'/></svg>\")";
    document.body.append(c);
    addEventListener('mousemove', (e) => { c.style.left = e.clientX + 'px'; c.style.top = e.clientY + 'px'; });
  });
}
let mouse = { x: W - 120, y: H - 80 };
async function moveTo(x, y, steps = 7) {
  for (let i = 1; i <= steps; i++) {
    const t = i / steps, e = 1 - (1 - t) * (1 - t); // ease out
    await page.mouse.move(mouse.x + (x - mouse.x) * e, mouse.y + (y - mouse.y) * e);
    await shot(45);
  }
  mouse = { x, y };
}
async function centre(selector) {
  const b = await (await page.$(selector)).boundingBox();
  return [b.x + b.width / 2, b.y + b.height / 2];
}
async function click(selector, holdAfter = 1500) {
  const [x, y] = await centre(selector);
  await moveTo(x, y);
  await shot(250);
  await page.mouse.click(x, y);
  await sleep(250);
  await shot(holdAfter);
}
const idle = (ms) => sleep(ms); // let the 150 ms debounce translate

await page.goto(url);
await page.waitForFunction(() => document.getElementById('status').textContent === 'compiler and demo store ready', { timeout: 60_000 });
await cursor();
await page.mouse.move(mouse.x, mouse.y);
await sleep(200);

// 1. The first example and the SQL it compiles to.
await shot(2200);

// 2. Run it on the demo store.
await click('#run', 2600);

// 3. Back to the SQL, then edit a value and watch the parameter follow.
await click('.tab[data-tab="sql"]', 900);
const text = await page.$eval('#request', (t) => t.value);
const word = 'mystery';
const at = text.indexOf(word);
const before = text.slice(0, at);
const line = before.split('\n').length - 1, col = before.length - before.lastIndexOf('\n') - 1;
const box = await (await page.$('#request')).boundingBox();
await moveTo(box.x + 10 + (col + word.length / 2) * 7.8, box.y + 10 + (line + 0.5) * 19.5);
await page.evaluate(([a, b]) => { const t = document.getElementById('request'); t.focus(); t.setSelectionRange(a, b); t.scrollTop = 0; }, [at, at + word.length]);
await shot(700);
for (const ch of 'history') {
  await page.keyboard.type(ch);
  await page.$eval('#request', (t) => { t.scrollTop = 0; });
  await idle(190);
  await shot(140);
}
await idle(200);
await shot(1800);

// 4. Run the edited request.
await click('#run', 2600);

// 5. An aggregation-only request: buckets and bars.
const [sx, sy] = await centre('#examples');
await moveTo(sx, sy);
await shot(300);
await page.select('#examples', '1');
await idle(250);
await shot(1600);
await click('#run', 3200);

// 6. A request outside the profile is refused, with the path.
await click('.tab[data-tab="sql"]', 600);
await moveTo(sx, sy);
await shot(300);
await page.select('#examples', '5');
await idle(250);
await shot(3400);

writeFileSync(`${dir}/frames.json`, JSON.stringify(frames, null, 1));
console.log(`${frames.length} frames, ${(frames.reduce((s, f) => s + f.hold, 0) / 1000).toFixed(1)} s, in ${dir}`);
await browser.close();
