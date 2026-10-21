// Runs demo/netlab.html (the connection diagnostics page) in headless Chromium
// tabs on one machine (BroadcastChannel signalling, loopback WebRTC): the tabs
// join one private room by its #key link, link to each other, measure pings on
// the route in use, chat, and each builds a JSON report.
//
//   node packages/js/test/netlab.mjs [tabs=3] [screenshot-prefix]
//
// RTC_OFF=<i> fails tab i's WebRTC links (?rtc=off): it must still be reached,
// relayed by a member or over the signalling fallback, with pings on that route.

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const tabs = Number(process.argv[2] ?? 3);
const shot = process.argv[3];
const off = process.env.RTC_OFF;
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const step = (s) => console.log(`ok: ${s}`);
let browser;
let ok = false;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const key = `test${port}abcdefgh`;
  const pages = [];
  const errors = [];
  for (let i = 0; i < tabs; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => errors.push(`tab ${i}: ${e.message}`));
    const q = new URLSearchParams({ signal: 'local', name: `n${i}`, ...(off === String(i) && { rtc: 'off' }) });
    await p.goto(`http://127.0.0.1:${port}/demo/netlab.html?${q}#key=${key}`);
    pages.push(p);
  }
  const all = (fn, arg, timeout = 40000) => Promise.all(pages.map((p) => p.waitForFunction(fn, arg, { timeout })));

  await all(() => window.netlab?.key && window.netlab.self);
  step(`${tabs} tabs joined the private room from the #key link`);
  await all((n) => window.netlab.peers().length === n - 1, tabs);
  const routes = await Promise.all(pages.map((p) => p.evaluate(() => window.netlab.peers().map((x) => `${x.name}:${x.route}`).join(' '))));
  routes.forEach((r, i) => console.log(`  n${i} reaches ${r}`));
  step('every tab linked to every other');

  // Pings come back on every route; the fallback pings every 3 s, so allow time.
  await all(() => window.netlab.peers().every((x) => x.ping.n >= 2), null, 60000);
  const pings = await Promise.all(pages.map((p) => p.evaluate(() => window.netlab.peers().map((x) => `${x.name}:${Math.round(x.ping.avg)}ms/${x.route}`).join(' '))));
  pings.forEach((r, i) => console.log(`  n${i} pings ${r}`));
  step('pings measured to every peer');
  if (off != null) {
    const r = await pages[Number(off)].evaluate(() => window.netlab.peers().map((x) => x.route));
    if (r.some((k) => k === 'direct')) throw new Error(`tab ${off} has WebRTC off but a direct route: ${r}`);
    step(`tab ${off} (WebRTC off) reached by fallback: ${r.join(', ')}`);
  }

  await pages[0].evaluate(() => window.netlab.say('hello from n0'));
  await all(() => window.netlab.chat().some((l) => l.includes('hello from n0')));
  step('chat reached every tab');

  await pages[1].waitForFunction(() => window.netlab.report().me.network, null, { timeout: 15000 }); // the STUN probe takes up to 8 s
  const rep = await pages[1].evaluate(() => JSON.stringify(window.netlab.report()));
  const r = JSON.parse(rep);
  if (r.peers.filter((p) => p.linked).length !== tabs - 1 || !r.log.length || r.room.kind !== 'private' || !r.me.network) throw new Error(`bad report: ${rep.slice(0, 400)}`);
  if (rep.includes(key)) throw new Error('the report must not contain the private room key');
  step(`report: ${r.peers.length} peers, ${r.log.length} log lines, no room key`);

  const shown = await pages[0].evaluate(() => ({ cards: document.querySelectorAll('.peer').length, wide: document.documentElement.scrollWidth }));
  if (shown.cards !== tabs - 1) throw new Error(`expected ${tabs - 1} peer cards, got ${shown.cards}`);
  if (shown.wide > 400) throw new Error(`page scrolls sideways on a phone: ${shown.wide}px wide`);
  step(`mobile layout: ${shown.cards} peer cards, no sideways scroll`);
  if (shot) await pages[0].screenshot({ path: `${shot}-phone.png`, fullPage: true });
  if (errors.length) throw new Error(`page errors: ${errors.join('; ')}`);
  ok = true;
  console.log('NETLAB PASS');
} catch (e) {
  console.log(`NETLAB FAIL: ${e.message}`);
} finally {
  await browser?.close();
  server.kill();
  process.exit(ok ? 0 : 1);
}
