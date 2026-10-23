// NB-048: the Net Lab birthday-punch diagnostic (demo/netlab.html + src/birthday.js),
// two tabs in headless Chromium on loopback (BroadcastChannel signalling). The two
// tabs join a room and link; one triggers a birthday punch from its peer card; this
// checks the whole Net Lab wiring — role choice (one hard, one easy), the offer/answer
// + spray coordination over the room's own signalling, the fleet opening, and the UI
// and log reporting. It does NOT assert a connection: there is only one NAT on this
// machine (and the public IP does not hairpin), so the two sockets can't actually meet.
// Cross-NAT success is proven by the fleet test (birthday_fleet.mjs, the mechanism) and
// the native simulator (NB-044, the maths); real two-device success is what the Net Lab
// button is for.
//
//   node packages/js/test/netlab_birthday.mjs [screenshot-prefix]
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const shot = process.argv[2];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
let browser;
let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
try {
  await new Promise((r) => setTimeout(r, 600));
  // The route line is logged from a 1 s render timer in each tab: keep background tabs'
  // timers running (as arena.mjs), or a loaded host can starve them past the waits.
  browser = await chromium.launch({
    args: [
      '--disable-features=WebRtcHideLocalIpsWithMdns',
      '--disable-background-timer-throttling',
      '--disable-renderer-backgrounding',
      '--disable-backgrounding-occluded-windows',
    ],
  });
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 } });
  const key = `bday${port}abcdefgh`;
  const pages = [];
  const errors = [];
  for (let i = 0; i < 2; i++) {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => errors.push(`tab ${i}: ${e.message}`));
    // A small fleet for a fast loopback run; the coordination is the same at any size.
    await p.goto(`http://127.0.0.1:${port}/demo/netlab.html?signal=local&name=n${i}&fleet=8#key=${key}`);
    pages.push(p);
  }
  const all = (fn, arg, timeout = 40000) => Promise.all(pages.map((p) => p.waitForFunction(fn, arg, { timeout })));

  await all(() => window.netlab?.self);
  await all(() => window.netlab.peers().length === 1 && window.netlab.peers()[0].route);
  ok(true, 'two tabs linked');
  // The log says where each direct link goes: address, family, protocol, candidate kinds.
  await all(() => [...document.querySelectorAll('#log div')].some((d) => /route to \S+: direct to \S+:\d+ \(IPv[46] UDP, your .+ ↔ their .+\)/.test(d.textContent)));
  ok(true, 'each tab logs the direct route with its address and candidate pair');
  // The network probe must finish (roles and the hard side's IP come from it).
  await all(() => window.netlab.report().me.network);

  // Tab 0 initiates via its button (the real control path).
  await pages[0].click('#peers [data-bday]');
  ok(true, 'birthday punch triggered from the peer card');
  // NB48-1: tab 1 does NOT auto-run — a remote request only invites; it needs a local click.
  await pages[1].waitForFunction(() => window.netlab.birthday()[0]?.status === 'invited', null, { timeout: 20000 });
  ok(await pages[1].evaluate(() => !window.netlab.birthday()[0].ctx && window.netlab.birthday()[0].status === 'invited'), 'a remote request only invites — no fleet without a local click');
  await pages[1].click('#peers [data-bday]'); // the user accepts
  ok(true, 'tab 1 accepted with a local click');
  // Both tabs take a role; one hard, one easy.
  const roles = await Promise.all(pages.map((p) => p.waitForFunction(() => window.netlab.birthday()[0]?.status === 'running' && window.netlab.birthday()[0]?.role, null, { timeout: 20000 }).then(() => p.evaluate(() => window.netlab.birthday()[0].role))));
  console.log(`  roles: tab0 ${roles[0]}, tab1 ${roles[1]}`);
  ok(roles.includes('hard') && roles.includes('easy'), 'one side is hard, the other easy');

  // The coordination round-trips over the room's signalling: the hard side opens its
  // fleet of sockets and answers; the easy side receives it and sprays the hard IP.
  const hard = pages[roles.indexOf('hard')], easy = pages[roles.indexOf('easy')];
  await hard.waitForFunction(() => [...document.querySelectorAll('#log div')].some((d) => /opened \d+ sockets, answering/.test(d.textContent)), null, { timeout: 20000 });
  ok(true, 'the hard side opened its fleet and answered');
  await easy.waitForFunction(() => [...document.querySelectorAll('#log div')].some((d) => /spraying \d+ ports at \d+\.\d+\.\d+\.\d+/.test(d.textContent)), null, { timeout: 20000 });
  ok(true, 'the easy side received the answer and sprayed the hard IP');

  // The card shows the running state and offers a control; the run can be stopped.
  ok(await pages[0].evaluate(() => /Punching|running/.test(document.querySelector('#peers .bdayrow')?.textContent ?? '')), 'the peer card shows the punch running');
  if (shot) await pages[0].screenshot({ path: `${shot}-birthday.png`, fullPage: true });

  // NB48b-2: one punch at a time per tab. With tab 0's punch still running, a third
  // member joins and tab 0 tries to punch it twice: both are refused, and the refusal
  // must not free the slot for the second try.
  const third = await ctx.newPage();
  third.on('pageerror', (e) => errors.push(`tab 2: ${e.message}`));
  await third.goto(`http://127.0.0.1:${port}/demo/netlab.html?signal=local&name=n2&fleet=8#key=${key}`);
  await pages[0].waitForFunction(() => window.netlab.peers().length === 2, null, { timeout: 40000 });
  const running = await pages[0].evaluate(async () => {
    const other = window.netlab.birthday().findIndex((b) => b.status !== 'running');
    await window.netlab.punch(other);
    await window.netlab.punch(other);
    return window.netlab.birthday().filter((b) => b.status === 'running').length;
  });
  ok(running === 1, `a refused punch keeps the one slot: ${running} running (NB48b-2)`);

  // An accepted invite whose buffered offer openFleet refuses fails at once and frees the
  // slot (not after the 70 s timer, with an unhandled rejection). Tab 0 plays a hostile
  // easy side to the idle third tab: an invite, then an offer that isn't SDP.
  const [t0, t1] = await Promise.all(pages.map((p) => p.evaluate(() => window.netlab.self)));
  await third.waitForFunction((ids) => ids.every((id) => window.netlab.peers().some((p) => p.id === id)), [t0, t1], { timeout: 40000 });
  await pages[0].evaluate(async (to) => {
    const { Room } = await import('/src/room.js');
    const send = Room.prototype.send;
    const room = await new Promise((resolve) => {
      Room.prototype.send = function (...a) {
        Room.prototype.send = send;
        resolve(this);
        return send.apply(this, a);
      };
    });
    room.send({ t: 'bday', step: 'start', yourRole: 'hard' }, { to });
    room.send({ t: 'bday', step: 'offer', sdp: 'v=0 not an offer', addr: '203.0.113.9:40000' }, { to });
  }, await third.evaluate(() => window.netlab.self));
  await third.waitForFunction((id) => window.netlab.birthday().find((b) => b.id === id)?.status === 'invited', t0, { timeout: 20000 });
  await third.waitForTimeout(500); // the offer follows the invite on the same reliable lane
  await third.click(`#peers [data-bday="${t0}"]`); // the user accepts
  await third.waitForFunction((id) => window.netlab.birthday().find((b) => b.id === id)?.status !== 'running', t0, { timeout: 10000 }).catch(() => {});
  const refused = await third.evaluate((id) => window.netlab.birthday().find((b) => b.id === id), t0);
  const unhandled = await third.evaluate(() => [...document.querySelectorAll('#log div')].filter((d) => /unhandled/.test(d.textContent)).length);
  ok(refused.status === 'failed' && /^error: /.test(refused.why) && !unhandled, `a bad buffered offer fails the accepted punch at once: ${refused.status} (${refused.why ?? ''}), ${unhandled} unhandled`);
  const next = await third.evaluate(async (id) => {
    await window.netlab.punch(window.netlab.peers().findIndex((p) => p.id === id));
    return window.netlab.birthday().find((b) => b.id === id);
  }, t1);
  ok(next.status === 'running', `and its slot is free: a punch with another peer runs (${next.status}${next.why ? `: ${next.why}` : ''})`);

  if (errors.length) throw new Error(`page errors: ${errors.join('; ')}`);
} catch (e) {
  ok(false, e.stack ?? String(e));
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
