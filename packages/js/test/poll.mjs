// Opens demo/poll.html (the pull-style room: poll() and sendTo() every frame,
// as Godot's JavaScriptBridge would drive it) in N tabs of headless Chromium,
// loopback only, and checks every tab sees every other's dot.
//
//   node packages/js/test/poll.mjs [tabs=3] [screenshot.png]

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const tabs = Number(process.argv[2] ?? 3);
const shot = process.argv[3];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const LEAVE_MS = 45000; // the core's 30 s idle_timeout plus margin for a loaded host
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
let browser;
let ok = false;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 800, height: 620 } });
  const pages = [];
  for (let i = 0; i < tabs; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`tab ${i} error: ${e.message}`));
    await p.goto(`http://127.0.0.1:${port}/demo/poll.html?room=t${port}`);
    pages.push(p);
  }
  const t0 = Date.now();
  await Promise.all(pages.map((p) => p.waitForFunction((n) => window.demo && window.demo.seen().length === n, tabs - 1, { timeout: 30000 })));
  console.log(`${tabs} tabs: every tab polled every other tab's position in ${Date.now() - t0} ms`);
  // A closing tab says Bye on pagehide and its data channels close, so the
  // leave is normally at once. If the renderer goes before either is sent
  // (seen under load), the others only learn of it from the core's idle_timeout
  // (30 s, EndpointConfig in peerwire-core), so wait past that, not up to it.
  const t1 = Date.now();
  await pages[0].close();
  const waits = await Promise.all(pages.slice(1).map(async (p) => {
    await p.waitForFunction((n) => window.demo.seen().length === n, tabs - 2, { timeout: LEAVE_MS });
    return Date.now() - t1;
  }));
  const slow = Math.max(...waits) > 5000 ? ' (Bye and channel close were lost: the leave came by idle_timeout)' : '';
  console.log(`closing a tab reached the others as a polled peer-leave in ${waits.join(', ')} ms${slow}`);
  if (shot) {
    const p = await ctx.newPage();
    await p.goto(`http://127.0.0.1:${port}/demo/poll.html?room=t${port}`);
    await p.waitForFunction((n) => window.demo && window.demo.seen().length === n, tabs - 1, { timeout: 30000 });
    await new Promise((r) => setTimeout(r, 500));
    await p.screenshot({ path: shot });
    console.log(`screenshot ${shot}`);
  }
  ok = true;
} finally {
  await browser?.close();
  server.kill();
}
process.exit(ok ? 0 : 1);
