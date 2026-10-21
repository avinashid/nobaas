// Runs demo/netlab.html (the connection diagnostics page) in headless Chromium
// tabs on one machine (BroadcastChannel signalling, loopback WebRTC): the tabs
// join one private room by its #key link, link to each other, measure pings on
// the route in use, chat, and each builds a JSON report.
//
//   node packages/js/test/netlab.mjs [tabs=3] [screenshot-prefix]
//
// RTC_OFF=<i> fails tab i's WebRTC links (?rtc=off): it must still be reached,
// relayed by a member or over the signalling fallback, with pings on that route.
// Tab 0 treats every 10.x/172.x/192.168.x host as a phone sharing mobile data
// (?gwfrom=), so it adds a synthesized 127.0.0.1 hotspot candidate per peer.

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
// The first octets of this machine's LAN address, as its host candidates show it.
const { networkInterfaces } = await import('node:os');
const lan = Object.values(networkInterfaces()).flat().find((a) => a?.family === 'IPv4' && !a.internal)?.address ?? '10.0.0.1';
const HOST_FROM = lan.split('.').slice(0, 2).join('.') + '.';
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
    const q = new URLSearchParams({ signal: 'local', name: `n${i}`, ...(off === String(i) && { rtc: 'off' }), ...(i === 0 && { gw: '127.0.0.1', gwfrom: HOST_FROM }) });
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
  // Every linked peer's offer/answer exchange is traced, and its ICE checks recorded.
  const traced = r.peers.filter((p) => p.linked && p.signaling.some((e) => /^(offer|answer)-/.test(e.what)));
  if (traced.length !== tabs - 1) throw new Error(`signalling not traced for every peer: ${JSON.stringify(r.peers.map((p) => p.signaling))}`);
  const checked = r.peers.filter((p) => p.iceChecks.length);
  step(`report: signalling traced for ${traced.length} peers, ICE checks for ${checked.length}`);
  // The other side's LAN addresses are listed (a phone's hotspot address shows here).
  if (!r.peers.every((p) => Array.isArray(p.theirHosts))) throw new Error('theirHosts missing from the report');
  step(`report: their LAN addresses ${JSON.stringify(r.peers.map((p) => p.theirHosts))}`);
  step(`report: ${r.peers.length} peers, ${r.log.length} log lines, no room key`);

  // Hotspot synthesis: only for a CLAT host (192.0.0.0/29 by default), UDP, one per gateway and port.
  const synth = await pages[0].evaluate(() => {
    const h = window.netlab.hotspotCandidates;
    const sdp = 'v=0\r\na=mid:0\r\na=candidate:1 1 udp 2122260223 192.0.0.4 51796 typ host generation 0\r\n'
      + 'a=candidate:2 1 udp 2122262783 2409:40d0::1 51797 typ host generation 0\r\na=candidate:3 1 tcp 1518280447 192.0.0.4 9 typ host tcptype active\r\n'
      + 'a=candidate:4 1 udp 2122260223 192.0.0.4 51796 typ host generation 0\r\na=candidate:5 1 udp 1686052607 157.49.1.2 40000 typ srflx raddr 192.0.0.4 rport 51796\r\n';
    return { clat: h(sdp, ['192.168.43.1'], '192.0.0.'), lan: h(sdp.replaceAll('192.0.0.4', '192.168.1.5'), ['192.168.43.1'], '192.0.0.'), none: h(null) };
  });
  if (synth.clat.length !== 1 || !synth.clat[0].candidate.includes(' 192.168.43.1 51796 typ host') || synth.clat[0].sdpMid !== '0' || synth.lan.length || synth.none.length) {
    throw new Error(`hotspotCandidates wrong: ${JSON.stringify(synth)}`);
  }
  step(`hotspot candidate from a CLAT host: ${synth.clat[0].candidate}`);
  // Only peers whose LAN address came through (not a tab with WebRTC off).
  if (off !== '0') {
    const r0 = await pages[0].evaluate(() => window.netlab.report());
    const want = r0.peers.filter((p) => p.theirHosts.some((h) => h.startsWith(HOST_FROM)));
    const tried = want.filter((p) => p.hotspotTried.some((g) => g.startsWith('127.0.0.1:')));
    if (!want.length || tried.length !== want.length) throw new Error(`tab 0 did not add a hotspot candidate for every peer: ${JSON.stringify(r0.peers.map((p) => [p.theirHosts, p.hotspotTried]))}`);
    step(`tab 0 (gwfrom=${HOST_FROM}) added hotspot candidates ${JSON.stringify(tried.map((p) => p.hotspotTried))}, links still up`);
  }

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
