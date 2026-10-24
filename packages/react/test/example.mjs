// Builds packages/react/example with Vite (a production build, as an app using the packages would) and plays
// it in tabs of headless Chromium on loopback: members see each other by name with no ghost from StrictMode,
// chat crosses, the shared counter follows either tab, a cursor shows on the other tab, a rename shows, a late
// tab catches up on the counter and cursor, a member outside React that sends wrongly typed presence, messages,
// counter and cursor neither breaks the pages nor shows, and a closed tab leaves. No page may throw.
//
//   node packages/react/test/example.mjs [screenshot-prefix]
import { createServer } from 'node:http';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const { build } = await import('vite');
const shot = process.argv[2];
const here = path.dirname(fileURLToPath(import.meta.url));

const fail = (m) => {
  throw new Error(m);
};

const out = await mkdtemp(path.join(tmpdir(), 'peerwire-react-example-'));
const t0 = Date.now();
await build({ configFile: path.join(here, '../example/vite.config.ts'), logLevel: 'warn', build: { outDir: out, emptyOutDir: true } });
console.log(`vite build in ${Date.now() - t0} ms`);

// The build at /, and packages/js at /pwjs/ for the member outside React (eve.html).
const js = path.join(here, '../../js');
const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm', '.svg': 'image/svg+xml' };
const server = createServer(async (req, res) => {
  const url = path.normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
  let file = url.startsWith('/pwjs/') ? path.join(js, url.slice(5)) : path.join(out, url);
  if (file.endsWith(path.sep)) file = path.join(file, 'index.html');
  try {
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': types[path.extname(file)] ?? 'application/octet-stream' }).end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}/`;

let browser;
const errors = [];
try {
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 900, height: 640 } });
  const room = `example-${process.pid}`;
  const open = async (name) => {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
    await p.goto(`${base}?room=${room}&name=${name}`);
    return p;
  };
  const member = (p, name) => p.locator(`#members li[data-name="${name}"]`);
  const [a, b] = [await open('ana'), await open('bo')];
  await member(a, 'bo').waitFor({ timeout: 20000 });
  await member(b, 'ana').waitFor({ timeout: 20000 });
  console.log(`two tabs list each other in ${Date.now() - t0} ms`);
  await a.waitForTimeout(500);
  for (const p of [a, b]) if ((await p.locator('#members li').count()) !== 1) fail('a tab lists a ghost member');

  await a.fill('#chat input', 'hello from ana');
  await a.press('#chat input', 'Enter');
  await b.locator('#log li', { hasText: 'ana: hello from ana' }).waitFor({ timeout: 10000 });
  await a.locator('#log li', { hasText: 'me: hello from ana' }).waitFor({ timeout: 1000 });
  console.log('chat crossed, named on the other tab');

  const plus = (p) => p.getByRole('button', { name: '+1' }).click();
  const count = (p, n) => p.locator('#count', { hasText: new RegExp(`^${n}$`) }).waitFor({ timeout: 10000 });
  await plus(a);
  await plus(a);
  await count(b, 2);
  await plus(b);
  await count(a, 3);
  console.log('shared counter followed both tabs');

  const box = await a.locator('#board').boundingBox();
  await a.mouse.move(box.x + box.width * 0.25, box.y + box.height * 0.75);
  await b.locator('.cursor[data-name="ana"]').waitFor({ timeout: 10000 });
  const style = await b.locator('.cursor[data-name="ana"]').getAttribute('style');
  if (!/left: 25%/.test(style) || !/top: 75%/.test(style)) fail(`cursor position ${style}`);
  console.log('cursor shown on the other tab at the same place');

  await b.fill('#name', 'bo2');
  await member(a, 'bo2').waitFor({ timeout: 10000 });
  console.log('rename shown');

  const c = await open('cy');
  await member(c, 'ana').waitFor({ timeout: 20000 });
  await count(c, 3);
  await c.locator('.cursor[data-name="ana"]').waitFor({ timeout: 10000 });
  console.log('late tab caught up on the counter and the cursor');

  // A member outside React, with an object for a name, sends an object message, an object for the counter and a
  // cursor that isn't one. Rendered as is, any of them would throw in React and unmount the page.
  const eve = await ctx.newPage();
  eve.on('pageerror', (e) => errors.push(`eve: ${e.message}`));
  await ctx.route('**/eve.html', (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: `<!doctype html><script type="importmap">{"imports":{"@peerwire/js":"/pwjs/src/index.js"}}</script>
<script type="module">
import { joinRoom } from '@peerwire/js';
window.room = await joinRoom({ appId: 'peerwire-react-example', roomId: ${JSON.stringify(room)}, presence: { name: { evil: 1 } } });
</script>`,
    }),
  );
  await eve.goto(`${base}eve.html`);
  await eve.waitForFunction(() => window.room?.peers.length === 3, null, { timeout: 20000 });
  await eve.evaluate(() => {
    room.send({ x: 1 });
    room.send({ $peerwire: 'set', k: 'count', v: { evil: 1 }, c: Date.now() + 1000 });
    room.send({ $peerwire: 'live', k: 'cursor', v: { x: 'a', y: {} }, n: 1 });
    room.send('plain text from eve');
  });
  for (const p of [a, b, c]) {
    await p.locator('#log li', { hasText: 'plain text from eve' }).waitFor({ timeout: 10000 });
    await count(p, 0);
    if ((await p.locator('#members li').count()) !== 2) fail('a member with an object for a name is listed');
    if ((await p.locator('.cursor').count()) > 2) fail('a cursor that is not one is shown');
  }
  await plus(c);
  await count(a, 1);
  await count(b, 1);
  await eve.close();
  console.log('a member outside React sent the wrong types: not shown, pages up, the counter works again');

  if (shot) for (const [i, p] of [a, b, c].entries()) await p.screenshot({ path: `${shot}-tab${i}.png` });
  await a.close();
  await b.locator('#members li[data-name="ana"]').waitFor({ state: 'detached', timeout: 5000 });
  await b.locator('.cursor[data-name="ana"]').waitFor({ state: 'detached', timeout: 1000 });
  console.log('closed tab left, its cursor went');
  if (errors.length) fail(`page errors:\n${errors.join('\n')}`);
  console.log('ok');
} finally {
  await browser?.close();
  server.close();
  await rm(out, { recursive: true, force: true });
}
