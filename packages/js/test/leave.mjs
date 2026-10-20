// Closes a room tab while it streams, in headless Chromium on loopback, and checks
// the other tabs see its peer-leave at once: the Bye and the close go out from the
// pagehide handler, before the page is torn down. Then crashes a tab (no pagehide),
// whose leave may take up to the core's 30 s idle_timeout.
//
//   node packages/js/test/leave.mjs [--runs=10] [--tabs=3] [--stress=0] [--crash=1]
//
// --stress=N runs N busy-loop processes meanwhile, to close tabs on a loaded host.

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const arg = (k, d) => Number(process.argv.find((a) => a.startsWith(`--${k}=`))?.split('=')[1] ?? d);
const runs = arg('runs', 10);
const tabs = arg('tabs', 3);
const stress = arg('stress', 0);
const crashes = arg('crash', 1);
const FAST_MS = 5000; // a closed tab's leave must arrive within this
const IDLE_MS = 45000; // the core's 30 s idle_timeout plus margin, for the crash path
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const burners = Array.from({ length: stress }, () => spawn(process.execPath, ['-e', 'for(;;);'], { stdio: 'ignore' }));
let browser;
let failed = 0;

// Opens `tabs` members of a fresh room, links them all, and starts tab 0's stream.
async function room(ctx, name) {
  const pages = [];
  for (let i = 0; i < tabs; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`tab ${i} error: ${e.message}`));
    await p.goto(`http://127.0.0.1:${port}/test/leave.html?room=${name}`);
    pages.push(p);
  }
  await Promise.all(pages.map((p) => p.waitForFunction((n) => window.t?.peers() === n, tabs - 1, { timeout: 30000 })));
  await pages[0].evaluate(() => window.t.load());
  // Let the stream fill the send window before the tab goes.
  await pages[0].waitForFunction(() => window.t.sent() >= 256, null, { timeout: 30000 });
  await new Promise((r) => setTimeout(r, 300));
  return pages;
}

// Waits for every other tab to see tab 0 leave; returns [ms, reason] per tab.
async function leaves(pages, id, t0, timeout) {
  return Promise.all(pages.slice(1).map(async (p) => {
    const h = await p.waitForFunction((id) => window.t.leaves().find((l) => l.peer === id), id, { timeout, polling: 50 });
    const l = await h.jsonValue();
    return [l.at - t0, l.reason];
  }));
}

try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const all = [];
  for (let run = 1; run <= runs; run++) {
    const ctx = await browser.newContext();
    const pages = await room(ctx, `c${port}-${run}`);
    const id = await pages[0].evaluate(() => window.t.id);
    const t0 = Date.now();
    await pages[0].close();
    let got;
    try {
      got = await leaves(pages, id, t0, IDLE_MS);
    } catch {
      got = [[Infinity, 'none']];
    }
    const worst = Math.max(...got.map((g) => g[0]));
    all.push(worst);
    const bad = worst > FAST_MS;
    if (bad) failed++;
    console.log(`close ${run}/${runs}: leave seen in ${got.map((g) => `${g[0]} ms (${g[1]})`).join(', ')}${bad ? '  FAIL: slower than 5 s' : ''}`);
    await ctx.close();
  }
  if (runs) console.log(`closes: ${runs - failed}/${runs} within ${FAST_MS} ms, worst ${Math.max(...all)} ms`);
  for (let run = 1; run <= crashes; run++) {
    const ctx = await browser.newContext();
    const pages = await room(ctx, `k${port}-${run}`);
    const id = await pages[0].evaluate(() => window.t.id);
    const t0 = Date.now();
    // A crashed renderer runs no pagehide: only the idle_timeout tells the others.
    await pages[0].goto('chrome://crash').catch(() => {});
    try {
      const got = await leaves(pages, id, t0, IDLE_MS);
      console.log(`crash ${run}/${crashes}: leave seen in ${got.map((g) => `${g[0]} ms (${g[1]})`).join(', ')}`);
    } catch {
      failed++;
      console.log(`crash ${run}/${crashes}: FAIL, no leave within ${IDLE_MS} ms`);
    }
    await ctx.close();
  }
} finally {
  await browser?.close();
  server.kill();
  for (const b of burners) b.kill();
}
console.log(failed ? `FAIL: ${failed} slow or missing leave(s)` : 'PASS');
process.exit(failed ? 1 : 0);
