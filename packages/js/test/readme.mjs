// Runs the ten-line chat from packages/js/README.md, verbatim, in two tabs of
// headless Chromium (loopback only), and checks a message gets across. The
// snippet is read out of the README, so the README can't drift from the code.
//
//   node packages/js/test/readme.mjs [screenshot-prefix]

import { spawn } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const shot = process.argv[2];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const readme = await readFile(path.join(root, 'README.md'), 'utf8');
const snippet = readme.match(/<!-- readme-chat[^>]*-->\s*```html\n([\s\S]*?)```/)?.[1];
if (!snippet) throw new Error('README chat snippet not found');
const js = snippet.match(/<script type="module">\n([\s\S]*?)<\/script>/)[1].trim().split('\n');
console.log(`README chat: ${js.length} lines of JS`);
if (js.length > 10) throw new Error('the README chat must stay within ten lines');
const importMap = '<script type="importmap">{"imports":{"@nobaas/js":"/src/index.js"}}</script>';
const page = `<!doctype html><meta charset="utf-8"><title>readme chat</title>${importMap}\n${snippet}`;

const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
let browser;
let ok = false;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 480, height: 260 } });
  await ctx.route('**/readme-chat.html', (route) => route.fulfill({ contentType: 'text/html', body: page }));
  const tabs = [];
  for (let i = 0; i < 2; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`tab ${i} error: ${e.message}`));
    await p.goto(`http://127.0.0.1:${port}/readme-chat.html`);
    tabs.push(p);
  }
  const t0 = Date.now();
  await Promise.all(tabs.map((p) => p.getByText('joined').waitFor({ timeout: 20000 })));
  console.log(`both tabs saw the other join in ${Date.now() - t0} ms`);
  const [a, b] = tabs;
  await a.fill('#text', 'hello from tab 0');
  await a.press('#text', 'Enter');
  await b.getByText('hello from tab 0').waitFor({ timeout: 10000 });
  await b.fill('#text', 'hi back');
  await b.press('#text', 'Enter');
  await a.getByText('hi back').waitFor({ timeout: 10000 });
  console.log('messages crossed both ways');
  if (shot) {
    for (const [i, p] of tabs.entries()) await p.screenshot({ path: `${shot}-tab${i}.png` });
    console.log(`screenshots ${shot}-tab0.png ${shot}-tab1.png`);
  }
  ok = true;
} finally {
  await browser?.close();
  server.kill();
}
process.exit(ok ? 0 : 1);
