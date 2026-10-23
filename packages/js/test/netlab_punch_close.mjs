// MIC-2: a Net Lab birthday punch that ends while it is still opening its connections
// (the peer's abort here; the 70 s timeout and a rejoin end it the same way) closes the
// connections that open afterwards, and writes no stale error on the card. Headless
// Chromium tabs on loopback (BroadcastChannel signalling). Tab A plays the peer through
// its live Room: it invites tab H to be the hard side (a fleet) and tab E the easy side
// (one spray connection), and has tab F's easy side apply an answer. Each of H, E and F
// holds every RTCPeerConnection's description 1 s and keeps the connections it makes; A
// sends `abort` while a setup or the answer is held.
//
//   node packages/js/test/netlab_punch_close.mjs
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
let browser;
let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
const errors = [];
const FLEET = 4;

// In H and E: every RTCPeerConnection is kept in __pcs, and while __hold is set its
// descriptions wait that long, as a slow device's would.
const holdPcs = () => {
  const Pc = window.RTCPeerConnection;
  window.__pcs = [];
  window.__hold = 0;
  const wait = () => new Promise((r) => setTimeout(r, window.__hold));
  window.RTCPeerConnection = class extends Pc {
    constructor(...a) {
      super(...a);
      window.__pcs.push(this);
    }
    async setRemoteDescription(d) {
      if (window.__hold) await wait();
      return super.setRemoteDescription(d);
    }
    async setLocalDescription(d) {
      if (window.__hold) await wait();
      return super.setLocalDescription(d);
    }
  };
};

try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({
    args: ['--disable-features=WebRtcHideLocalIpsWithMdns', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows'],
  });
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 } });
  const key = `close${port}abcdefgh`;
  const open = async (name, hold) => {
    const p = await ctx.newPage();
    p.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
    if (hold) await p.addInitScript(holdPcs);
    await p.goto(`http://127.0.0.1:${port}/demo/netlab.html?signal=local&name=${name}&fleet=${FLEET}#key=${key}`);
    return p;
  };
  const a = await open('alpha');
  const h = await open('hotel', true);
  const e = await open('echo', true);
  const f = await open('foxtrot', true);
  for (const p of [a, h, e, f]) await p.waitForFunction(() => window.netlab?.peers().length === 3 && window.netlab.report().me.network, null, { timeout: 40000 });
  const [ha, ea, fa, aId] = await Promise.all([h, e, f, a].map((p) => p.evaluate(() => window.netlab.self)));

  // A's live Room, to send what a peer's Net Lab would.
  await a.evaluate(async () => {
    const { Room } = await import('/src/room.js');
    const send = Room.prototype.send;
    window.__room = await new Promise((resolve) => {
      Room.prototype.send = function (...x) {
        Room.prototype.send = send;
        resolve(this);
        return send.apply(this, x);
      };
    });
  });
  const tell = (to, msg) => a.evaluate(([t, m]) => window.__room.send(m, { to: t }), [to, msg]);
  const offer = await a.evaluate(async () => {
    const pc = new RTCPeerConnection();
    pc.createDataChannel('x');
    await pc.setLocalDescription(await pc.createOffer());
    const sdp = pc.localDescription.sdp;
    pc.close();
    return sdp;
  });

  // Invites `victim` as the `role` side (with A's offer for a hard side), holds its setup,
  // accepts there by a click, aborts mid-setup; returns what the punch left behind.
  async function abortMidSetup(victim, id, role) {
    await tell(id, { t: 'bday', step: 'start', yourRole: role });
    if (role === 'hard') await tell(id, { t: 'bday', step: 'offer', sdp: offer, addr: null });
    await victim.waitForFunction((x) => window.netlab.birthday().find((b) => b.id === x)?.status === 'invited', aId, { timeout: 20000 });
    await victim.waitForTimeout(300); // the offer follows the invite on the same lane
    const before = await victim.evaluate(() => { window.__hold = 1000; return window.__pcs.length; });
    await victim.click(`#peers [data-bday="${aId}"]`);
    await victim.waitForTimeout(300);
    await tell(id, { t: 'bday', step: 'abort', why: 'test stop' });
    await victim.waitForTimeout((role === 'hard' ? FLEET : 1) * 1000 + 2500); // the held setup finishes
    return victim.evaluate(([x, n]) => ({
      states: window.__pcs.slice(n).map((pc) => pc.connectionState),
      bday: window.netlab.birthday().find((b) => b.id === x),
      errors: window.netlab.report().log.filter((l) => l.level === 'error').map((l) => l.msg),
    }), [aId, before]);
  }

  const hard = await abortMidSetup(h, ha, 'hard');
  ok(hard.states.length >= FLEET && hard.states.every((s) => s === 'closed'), `hard side, aborted while its fleet opened: ${hard.states.length} connections, ${JSON.stringify([...new Set(hard.states)])}`);
  ok(hard.bday?.status === 'failed' && hard.bday.why === 'the peer stopped: test stop' && !hard.errors.length, `and its card keeps "${hard.bday?.why}"${hard.errors.length ? `, errors ${hard.errors}` : ''}`);
  const easy = await abortMidSetup(e, ea, 'easy');
  ok(easy.states.length >= 1 && easy.states.every((s) => s === 'closed'), `easy side, aborted while its spray connection opened: ${easy.states.length} connection(s), ${JSON.stringify([...new Set(easy.states)])}`);
  ok(easy.bday?.status === 'failed' && easy.bday.why === 'the peer stopped: test stop' && !easy.errors.length, `and its card keeps "${easy.bday?.why}"${easy.errors.length ? `, errors ${easy.errors}` : ''}`);
  // F: the easy side's punch runs, then ends while it applies the hard side's answer (a
  // TEST-NET address to spray, which goes nowhere): the closed connection's error stays
  // out of the card.
  await tell(fa, { t: 'bday', step: 'start', yourRole: 'easy' });
  await f.waitForFunction((x) => window.netlab.birthday().find((b) => b.id === x)?.status === 'invited', aId, { timeout: 20000 });
  await f.click(`#peers [data-bday="${aId}"]`);
  await f.waitForTimeout(4000); // its spray connection gathers (3 s at most) and offers
  const n = await f.evaluate(() => { window.__hold = 1000; return window.__pcs.length; });
  await tell(fa, { t: 'bday', step: 'answer', sdp: offer, ip: '203.0.113.9' });
  await f.waitForTimeout(300);
  await tell(fa, { t: 'bday', step: 'abort', why: 'test stop' });
  await f.waitForTimeout(2500);
  const late = await f.evaluate(([x, k]) => ({ bday: window.netlab.birthday().find((b) => b.id === x), made: window.__pcs.length - k }), [aId, n]);
  ok(late.bday?.status === 'failed' && late.bday.why === 'the peer stopped: test stop' && late.made === 0, `easy side, aborted while applying the answer: its card keeps "${late.bday?.why}"`);
  ok(!errors.length, `no page errors${errors.length ? `: ${errors.join('; ')}` : ''}`);
} catch (e) {
  ok(false, e.stack ?? String(e));
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
