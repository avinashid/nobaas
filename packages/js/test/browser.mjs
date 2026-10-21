// Opens N tabs of the demo in headless Chromium (local only: loopback server,
// no STUN/TURN), waits for a full mesh of Peerwire links over WebRTC DataChannels,
// then measures app-level round trips and throughput between tab 0 and tab 1.
//
//   node packages/js/test/browser.mjs [tabs=2] [screenshot.png]

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const tabs = Number(process.argv[2] ?? 2);
const shot = process.argv[3];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const pct = (v, p) => v[Math.min(v.length - 1, Math.round((v.length - 1) * p))];

let browser;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext();
  const pages = [];
  const t0 = Date.now();
  for (let i = 0; i < tabs; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`tab ${i} error: ${e.message}`));
    await p.goto(`http://127.0.0.1:${port}/demo/?room=t${port}`);
    pages.push(p);
  }
  await Promise.all(pages.map((p) => p.waitForFunction((n) => window.demo && window.demo.peers().length === n, tabs - 1, { timeout: 30000 })));
  const tMesh = Date.now() - t0;
  console.log(`${tabs} tabs: full mesh of ${(tabs * (tabs - 1)) / 2} Peerwire links over DataChannels in ${tMesh} ms (including page loads)`);

  const [a, b] = pages;
  const idB = await b.evaluate(() => window.demo.id);
  const idA = await a.evaluate(() => window.demo.id);
  for (const [name, lane] of [['reliable', 0], ['unreliable', 1]]) {
    const rtts = await a.evaluate(([id, lane]) => window.demo.ping(id, 100, lane), [idB, lane]);
    console.log(`app round trip, ${name}: ${rtts.length}/100, p50 ${pct(rtts, 0.5).toFixed(2)} ms, p99 ${pct(rtts, 0.99).toFixed(2)} ms`);
  }
  for (const [n, size] of [[20000, 64], [2000, 1024]]) {
    const before = await b.evaluate((id) => window.demo.received(id), idA);
    const t1 = Date.now();
    await a.evaluate(([id, n, size]) => window.demo.bench(id, n, size), [idB, n, size]);
    await b.waitForFunction(([id, want]) => window.demo.received(id) >= want, [idA, before + n], { timeout: 60000, polling: 5 });
    const s = (Date.now() - t1) / 1000;
    const st = await a.evaluate(() => window.demo.stats());
    const sb = await b.evaluate(() => window.demo.stats());
    console.log(`reliable ${n} x ${size} B: ${(n / s).toFixed(0)} msg/s, ${((n * size * 8) / s / 1e6).toFixed(1)} Mbit/s; sender tx ${st.txPackets} retransmits ${st.retransmits}; receiver rx ${sb.rxPackets}`);
  }
  console.log(`Peerwire smoothed RTT tab0->tab1: ${(await a.evaluate((id) => window.demo.rtt(id), idB)).toFixed(2)} ms`);
  if (shot) {
    await a.evaluate(() => {
      document.getElementById('text').value = 'hello from tab 0';
      document.getElementById('chat').requestSubmit();
    });
    await new Promise((r) => setTimeout(r, 1200));
    await b.screenshot({ path: shot, fullPage: true });
    console.log(`screenshot ${shot}`);
  }
} finally {
  await browser?.close();
  server.kill();
}
