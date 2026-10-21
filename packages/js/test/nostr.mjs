// Two isolated browser contexts (no shared BroadcastChannel) find each other
// through public Nostr relays, exchange sealed WebRTC offers, connect over a
// DataChannel with public STUN, and run the Peerwire handshake. Scope: public Nostr
// relays and public STUN only; the peers are both on this machine.
//
//   node packages/js/test/nostr.mjs

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8500 + Math.floor(Math.random() * 400);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const room = `t-${Math.random().toString(36).slice(2)}`;
let browser;
let code = 1;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const pages = [];
  const t0 = Date.now();
  for (let i = 0; i < 2; i++) {
    const ctx = await browser.newContext(); // separate profile: no BroadcastChannel between them
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`tab ${i} error: ${e.message}`));
    await p.goto(`http://127.0.0.1:${port}/demo/?signal=nostr&room=${room}`);
    pages.push(p);
  }
  const ok = await Promise.all(pages.map((p) => p.waitForFunction(() => window.demo && window.demo.peers().length === 1, null, { timeout: 45000 }).then(() => true, () => false)));
  const t = Date.now() - t0;
  for (const [i, p] of pages.entries()) {
    console.log(`tab ${i}: peers ${JSON.stringify(await p.evaluate(() => window.demo.peers().map((x) => x.slice(0, 12))))}, signaling ${JSON.stringify(await p.evaluate(() => window.demo.signaling()))}`);
  }
  if (ok.every(Boolean)) {
    const idB = await pages[1].evaluate(() => window.demo.id);
    const rtts = await pages[0].evaluate((id) => window.demo.ping(id, 20), idB);
    console.log(`connected through Nostr signalling in ${t} ms (2 page loads, relay round trips, ICE); app RTT p50 ${rtts[Math.floor(rtts.length / 2)].toFixed(2)} ms`);
    code = 0;
  } else {
    console.log(`did not connect within 45 s`);
  }
} finally {
  await browser?.close();
  server.kill();
}
process.exit(code);
