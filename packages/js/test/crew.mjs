// Plays demo/crew.html (the Among Us-like demo) in headless Chromium tabs on one
// machine (BroadcastChannel signalling, loopback WebRTC): one host and three
// players meet, the host starts, the impostor kills, a crew member reports the
// body, everyone votes the impostor out and the crew wins. Then a second round
// in which the crew wins by finishing every task.
//
//   node packages/js/test/crew.mjs [tabs=4] [screenshot-prefix]

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const tabs = Number(process.argv[2] ?? 4);
const shot = process.argv[3];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const step = (s) => console.log(`ok: ${s}`);
let browser;
let ok = false;
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 1300, height: 700 } });
  const pages = [];
  for (let i = 0; i < tabs; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => console.log(`tab ${i} error: ${e.message}`));
    const q = new URLSearchParams({ signal: 'local', room: `t${port}`, name: `p${i}`, auto: i ? 'join' : 'host' });
    await p.goto(`http://127.0.0.1:${port}/demo/crew.html?${q}`);
    pages.push(p);
  }
  const all = (fn, arg, timeout = 30000) => Promise.all(pages.map((p) => p.waitForFunction(fn, arg, { timeout })));
  const ev = (i, fn, arg) => pages[i].evaluate(fn, arg);

  await all((n) => window.crew.phase === 'lobby' && Object.keys(window.crew.state.players).length === n, tabs);
  step(`${tabs} tabs in the waiting room`);
  if (shot) await pages[1].screenshot({ path: `${shot}-lobby.png` });

  await ev(0, () => window.crew.start());
  await all(() => window.crew.phase === 'play' && window.crew.role);
  const roles = await Promise.all(pages.map((p) => p.evaluate(() => ({ id: window.crew.self, role: window.crew.role.role }))));
  const imp = roles.findIndex((r) => r.role === 'impostor');
  if (roles.filter((r) => r.role === 'impostor').length !== 1) throw new Error(`expected one impostor: ${JSON.stringify(roles)}`);
  step(`game started, impostor is tab ${imp}`);

  // The impostor walks onto a crew member and kills once the first cooldown is over.
  const victim = roles.findIndex((r, i) => i !== imp);
  await pages[victim].evaluate(() => window.crew.goto(800, 280));
  await pages[imp].evaluate(() => window.crew.goto(830, 280));
  await pages[imp].waitForFunction(() => window.crew.killTarget(), null, { timeout: 20000 });
  await ev(imp, () => window.crew.kill());
  await all((id) => window.crew.state.players[id].alive === false && window.crew.state.bodies.length === 1, roles[victim].id);
  step(`tab ${imp} killed tab ${victim}`);
  if (shot) await pages[imp].screenshot({ path: `${shot}-kill.png` });

  // Someone else finds the body and reports it.
  const finder = roles.findIndex((r, i) => i !== imp && i !== victim);
  await ev(finder, () => window.crew.goto(800, 300));
  await pages[finder].waitForTimeout(300);
  await ev(finder, () => window.crew.report());
  await all(() => window.crew.phase === 'meeting');
  step(`tab ${finder} reported: meeting`);
  await ev(finder, () => window.crew.say('it was them'));
  await all(() => document.getElementById('chatlog').textContent.includes('it was them'));
  if (shot) await pages[finder].screenshot({ path: `${shot}-meeting.png` });

  // Every living player votes the impostor out.
  for (let i = 0; i < tabs; i++) if (i !== victim) await ev(i, (t) => window.crew.vote(t), roles[imp].id);
  await all(() => window.crew.phase === 'over' && window.crew.state.result.winner === 'crew');
  step('impostor voted out: crew wins on every tab');
  if (shot) await pages[victim].screenshot({ path: `${shot}-over.png` });

  // Round two: back to the waiting room, start, and win on tasks.
  await ev(0, () => document.getElementById('again').click());
  await all(() => window.crew.phase === 'lobby');
  await ev(0, () => window.crew.start());
  await all(() => window.crew.phase === 'play' && window.crew.role);
  for (const p of pages) await p.evaluate(() => { if (window.crew.role.role === 'crew') for (const i of window.crew.role.tasks) window.crew.finishTask(i); });
  await all(() => window.crew.phase === 'over' && window.crew.state.result.why === 'Every task is done.');
  step('round two: crew wins on tasks');

  ok = true;
} catch (e) {
  console.log(`FAIL: ${e.message}`);
} finally {
  await browser?.close();
  server.kill();
}
console.log(ok ? 'PASS crew' : 'FAIL crew');
process.exit(ok ? 0 : 1);
