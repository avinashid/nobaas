// A browser fetches the wasm from whatever URL it is given, root-relative included
// ('/assets/peerwire_ffi-….wasm' is what Vite's `?url` import gives). Two tabs of headless
// Chromium (loopback only) join a room with `wasm: '/pkg/peerwire_ffi.wasm'` and chat; the
// first also loads the module from a relative path, an absolute URL and a URL object, and a
// path that 404s rejects with the status. test/wasm_source.mjs checks Node's side.
//
//   node packages/js/test/wasm_url.mjs [screenshot-prefix]

import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const shot = process.argv[2];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const WASM = '/pkg/peerwire_ffi.wasm';

const page = `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>wasm from a root-relative URL</title>
<script type="importmap">{"imports":{"@peerwire/js":"/src/index.js"}}</script>
<style>
  body { font: 15px/1.5 system-ui, sans-serif; margin: 0; padding: 16px 20px; background: #0f1115; color: #e6e8ee; }
  h1 { font-size: 16px; margin: 0 0 10px; } code { color: #9ecbff; }
  ul { list-style: none; padding: 0; margin: 0 0 12px; } li { padding: 2px 0; }
  .ok::before { content: '✓ '; color: #4ade80; } .fail::before { content: '✗ '; color: #f87171; }
  #status { margin: 0 0 8px; color: #a5adbb; } #log li { color: #fbbf24; }
</style>
<h1>joinRoom({ wasm: <code>'${WASM}'</code> })</h1>
<ul id="checks"></ul>
<p id="status">joining…</p>
<ul id="log"></ul>
<script type="module">
  import { joinRoom, loadPeerwire } from '@peerwire/js';
  const $ = (id) => document.getElementById(id);
  const row = (list, cls, text) => $(list).append(Object.assign(document.createElement('li'), { className: cls, textContent: text }));
  window.results = [];
  const check = (ok, what) => { window.results.push({ ok, what }); row('checks', ok ? 'ok' : 'fail', what); };
  const load = (src) => loadPeerwire(src).then((pw) => (pw.endpoint().close(), 'loaded'), (e) => e.message);
  if (location.hash === '#checks') {
    for (const [src, what] of [
      ['pkg/peerwire_ffi.wasm', 'a relative path'],
      [location.origin + '${WASM}', 'an absolute URL'],
      [new URL('${WASM}', location.href), 'a URL object'],
    ]) { const r = await load(src); check(r === 'loaded', what + ' loads' + (r === 'loaded' ? '' : ': ' + r)); }
    const r = await load('/nope.wasm');
    check(/HTTP 404/.test(r), 'a path that 404s rejects: ' + r);
  }
  try {
    const room = await joinRoom({ appId: 'wasm-url', roomId: 'lobby', wasm: '${WASM}' });
    window.room = room;
    const sync = () => ($('status').textContent = 'joined as ' + room.self.slice(0, 6) + ', peers: ' + room.peers.length);
    room.on('peer-join', sync); room.on('peer-leave', sync); sync();
    room.on('message', (text, peer) => row('log', '', peer.slice(0, 6) + ': ' + text));
  } catch (e) {
    $('status').textContent = 'error: ' + e.message;
  }
</script>`;

const TYPES = { '.js': 'text/javascript', '.wasm': 'application/wasm' };
const server = createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  if (url.pathname === '/') return res.writeHead(200, { 'content-type': 'text/html' }).end(page);
  const file = path.join(root, path.normalize(url.pathname));
  if (!/^\/(src|pkg)\//.test(url.pathname) || !file.startsWith(root + path.sep)) return res.writeHead(404).end();
  try {
    res.writeHead(200, { 'content-type': TYPES[path.extname(file)] ?? 'application/octet-stream' }).end(await readFile(file));
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
let browser;
try {
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 520, height: 300 } });
  const tabs = [];
  for (const hash of ['#checks', '']) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`tab ${tabs.length} error: ${e.message}`));
    await p.goto(`${base}/${hash}`);
    tabs.push(p);
  }
  const [a, b] = tabs;
  for (const [i, p] of tabs.entries()) {
    await p.waitForFunction(() => /peers: 1|error/.test(document.querySelector('#status').textContent), null, { timeout: 20000 })
      .catch(() => {});
    const status = await p.$eval('#status', (e) => e.textContent);
    check(status.endsWith('peers: 1'), `tab ${i} joined with wasm '${WASM}' and sees the other (${status})`);
  }
  for (const { ok, what } of await a.evaluate(() => window.results)) check(ok, what);
  if (await a.evaluate(() => !!window.room)) {
    await a.evaluate(() => window.room.send('hello over a root-relative wasm'));
    const got = await b.getByText('hello over a root-relative wasm').waitFor({ timeout: 10000 }).then(() => true, () => false);
    check(got, 'a message crossed');
  }
  if (shot) {
    for (const [i, p] of tabs.entries()) await p.screenshot({ path: `${shot}-tab${i}.png` });
    console.log(`screenshots ${shot}-tab0.png ${shot}-tab1.png`);
  }
} finally {
  await browser?.close();
  server.close();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
