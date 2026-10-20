// Browser arena: 1 super-peer tab + N player tabs in headless Chromium, local
// only. Runs for a while, then checks every island's members computed the same
// lockstep state (checkpoint hashes) and reports confirmation lag.
//
//   node packages/js/test/arena.mjs [players=8] [seconds=10] [screenshot.png]

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const n = Number(process.argv[2] ?? 8);
const secs = Number(process.argv[3] ?? 10);
const shot = process.argv[4] && process.argv[4] !== '-' ? process.argv[4] : null;
const loss = Number(process.env.LOSS ?? 0);
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 9800 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });

let browser;
let failed = false;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({
    args: [
      '--disable-features=WebRtcHideLocalIpsWithMdns',
      '--disable-background-timer-throttling',
      '--disable-renderer-backgrounding',
      '--disable-backgrounding-occluded-windows',
    ],
  });
  const ctx = await browser.newContext();
  const url = (q) => `http://127.0.0.1:${port}/demo/arena.html?room=a${port}&loss=${loss}&${q}`;
  const sp = await ctx.newPage();
  sp.on('pageerror', (e) => console.log(`super error: ${e.message}`));
  await sp.goto(url('role=super'));
  const players = [];
  for (let i = 0; i < n; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`player ${i} error: ${e.message}`));
    await p.goto(url(`role=player&draw=${i === 0 ? 1 : 0}`));
    players.push(p);
  }
  await sp.waitForFunction((n) => window.arena && window.arena.players() === n, n, { timeout: 60000 });
  await Promise.all(players.map((p) => p.waitForFunction(() => window.arena && window.arena.joined(), null, { timeout: 30000 })));
  console.log(`${n} players joined the super-peer over WebRTC; running ${secs} s`);
  await new Promise((r) => setTimeout(r, secs * 1000));

  const info = await Promise.all(players.map((p) => p.evaluate(() => ({
    pid: window.arena.pid(), island: window.arena.island(), roster: window.arena.rosterSize(),
    confirmed: window.arena.confirmed(), hashes: window.arena.hashes(), lag: window.arena.lag(),
    waiting: window.arena.waiting(), rtt: window.arena.rtt(), debug: window.arena.debug(), nacks: window.arena.nacks(),
  }))));
  const byIsland = new Map();
  for (const i of info) byIsland.set(i.island, [...(byIsland.get(i.island) ?? []), i]);
  for (const [k, ms] of byIsland) {
    const ticks = Object.keys(ms[0].hashes).filter((t) => ms.every((m) => t in m.hashes));
    const bad = ticks.filter((t) => new Set(ms.map((m) => m.hashes[t])).size > 1);
    console.log(`island ${k}: ${ms.length} players; ${ticks.length} common checkpoints, ${bad.length} mismatched`);
    if (bad.length || ticks.length < secs - 3) failed = true;
  }
  const stuck = info.filter((i) => !i.lag.n);
  if (stuck.length) {
    failed = true;
    console.log(`first stuck player's view [pid, join, contiguous, inputs held]: ${JSON.stringify(stuck[0].debug)}`);
    console.log(`never confirmed: ${stuck.map((i) => `pid ${i.pid} (island ${i.island}, waiting for snapshot: ${i.waiting}, confirmed ${i.confirmed}, rtt ${i.rtt?.toFixed(1)} ms)`).join('; ')}`);
  }
  const p50s = info.filter((i) => i.lag.n).map((i) => i.lag.p50).sort((a, b) => a - b);
  const p99s = info.filter((i) => i.lag.n).map((i) => i.lag.p99).sort((a, b) => a - b);
  console.log(`confirm lag per player: p50 ${p50s[0].toFixed(1)}..${p50s.at(-1).toFixed(1)} ms, p99 ${p99s[0].toFixed(1)}..${p99s.at(-1).toFixed(1)} ms (${info.reduce((s, i) => s + i.lag.n, 0)} samples)`);
  const st = await sp.evaluate(() => window.arena.stats());
  const [repairs, upNacks] = await sp.evaluate(() => [window.arena.repairs(), window.arena.uplinkNacks()]);
  console.log(`super-peer sent ${st.txPackets} packets, received ${st.rxPackets}, retransmits ${st.retransmits}; repairs: ${info.reduce((s, i) => s + i.nacks, 0)} player requests (${repairs} served), ${upNacks} super-peer requests to players`);
  if (shot) {
    await players[0].screenshot({ path: shot });
    console.log(`screenshot ${shot}`);
  }
} finally {
  await browser?.close();
  server.kill();
}
process.exit(failed ? 1 : 0);
