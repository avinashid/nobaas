// MIC-2: a Net Lab birthday punch closes everything it opened, whenever it ends (the
// peer's abort here; the 70 s timeout and a rejoin end it the same way), and takes one
// offer and one answer only. Headless Chromium tabs on loopback (BroadcastChannel
// signalling). Tab A plays the peer through its live Room. Tab V is the punch's other
// side; it holds its certificate and every RTCPeerConnection description 1 s while
// __hold is set, keeps every connection it makes, and is reloaded before each case (a
// new member, so the one-punch-per-peer cooldown starts afresh). Each case checks the
// connections V made after accepting and the reason its card gives:
// 1. hard side, aborted while its certificate is made: no fleet opens;
// 2. hard side, aborted while its fleet opens: every fleet connection closes;
// 3. hard side, a second offer straight after the first: one fleet only, closed at the end;
// 4. easy side, aborted while its spray connection opens: it closes;
// 5. easy side, aborted while it applies the answer: no stale error on the card;
// 6. easy side, a second answer straight after the first: ignored, the punch runs on.
// Sprays aim only at TEST-NET-3 (203.0.113.9), which goes nowhere.
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
const HOLD = 1000;
const TESTNET = '203.0.113.9';

// In V: every RTCPeerConnection is kept in __pcs, and while __hold is set the
// certificate and each description wait that long, as on a slow device.
const holdPcs = () => {
  const Pc = window.RTCPeerConnection;
  window.__pcs = [];
  window.__hold = 0;
  const wait = () => new Promise((r) => setTimeout(r, window.__hold));
  window.RTCPeerConnection = class extends Pc {
    static async generateCertificate(...a) {
      if (window.__hold) await wait();
      return Pc.generateCertificate(...a);
    }
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
  const url = `http://127.0.0.1:${port}/demo/netlab.html?signal=local&fleet=${FLEET}#key=close${port}abcdefgh`;
  const a = await ctx.newPage();
  a.on('pageerror', (e) => errors.push(`A: ${e.message}`));
  await a.goto(url.replace('?', '?name=alpha&'));
  const v = await ctx.newPage();
  v.on('pageerror', (e) => errors.push(`V: ${e.message}`));
  await v.addInitScript(holdPcs);
  await v.goto(url.replace('?', '?name=victor&'));
  const aId = await a.waitForFunction(() => window.netlab?.self).then((h) => h.jsonValue());

  // A's live Room, to send what a peer's Net Lab would; it notes the offers V sends.
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
    window.__offers = new Map();
    window.__room.on('message', (d, from) => d?.t === 'bday' && d.step === 'offer' && window.__offers.set(from, d.sdp));
  });
  const offer = await a.evaluate(async () => {
    const pc = new RTCPeerConnection();
    pc.createDataChannel('x');
    await pc.setLocalDescription(await pc.createOffer());
    const sdp = pc.localDescription.sdp;
    pc.close();
    return sdp;
  });

  let vId = null;
  const tell = (msg) => a.evaluate(([t, m]) => window.__room.send(m, { to: t }), [vId, msg]);
  const pause = (ms) => v.waitForTimeout(ms);
  // A new V, linked to A, invited to a punch as `role` (with A's offer for a hard side),
  // accepted there by a click; `hold` holds its setup from the click on.
  async function accept(role, hold) {
    if (vId) await v.reload();
    vId = await v.waitForFunction(() => window.netlab?.self).then((h) => h.jsonValue());
    await v.waitForFunction((x) => window.netlab.peers().some((p) => p.id === x) && window.netlab.report().me.network, aId, { timeout: 40000 });
    await a.waitForFunction((x) => window.netlab.peers().some((p) => p.id === x), vId, { timeout: 40000 });
    await tell({ t: 'bday', step: 'start', yourRole: role });
    if (role === 'hard') await tell({ t: 'bday', step: 'offer', sdp: offer, addr: null });
    await v.waitForFunction((x) => window.netlab.birthday().find((b) => b.id === x)?.status === 'invited', aId, { timeout: 20000 });
    await pause(300); // the offer follows the invite on the same lane
    const before = await v.evaluate((h) => { window.__hold = h; return window.__pcs.length; }, hold ? HOLD : 0);
    await v.click(`#peers [data-bday="${aId}"]`);
    return before;
  }
  // What the punch left: V's connections made since `before`, and its card.
  const after = (before) => v.evaluate(([x, n]) => ({
    states: window.__pcs.slice(n).map((pc) => pc.connectionState),
    bday: window.netlab.birthday().find((b) => b.id === x),
    errors: window.netlab.report().log.filter((l) => l.level === 'error').map((l) => l.msg),
  }), [aId, before]);
  const stopped = (r) => r.bday?.status === 'failed' && r.bday.why === 'the peer stopped: test stop' && !r.errors.length;
  const abort = () => tell({ t: 'bday', step: 'abort', why: 'test stop' });
  const states = (r) => `${r.states.length} connection(s) ${JSON.stringify([...new Set(r.states)])}`;
  const settle = (role) => pause(role === 'hard' ? HOLD * (FLEET + 1) + 2000 : HOLD + 2000); // the held setup finishes

  // 1. Aborted while the hard side makes its certificate: no fleet opens.
  let n = await accept('hard', true);
  await pause(300);
  await abort();
  await settle('hard');
  let r = await after(n);
  ok(r.states.length === 0 && stopped(r), `1 hard side, aborted while its certificate is made: ${states(r)}, card "${r.bday?.why}"`);

  // 2. Aborted while its fleet opens: every fleet connection closes.
  n = await accept('hard', true);
  await pause(HOLD + 500);
  await abort();
  await settle('hard');
  r = await after(n);
  ok(r.states.length === FLEET && r.states.every((s) => s === 'closed') && stopped(r), `2 hard side, aborted while its fleet opens: ${states(r)}, card "${r.bday?.why}"`);

  // 3. A second offer straight after the first: one fleet, and the abort closes it all.
  n = await accept('hard', true);
  await tell({ t: 'bday', step: 'offer', sdp: offer, addr: null });
  await v.waitForFunction(() => [...document.querySelectorAll('#log div')].some((d) => /opened \d+ sockets, answering/.test(d.textContent)), null, { timeout: 30000 });
  await settle('hard'); // a second fleet, if one were opening, would be open by now
  await abort();
  await pause(1500);
  r = await after(n);
  ok(r.states.length === FLEET && r.states.every((s) => s === 'closed') && stopped(r), `3 hard side, a second offer: ${states(r)}, card "${r.bday?.why}"`);

  // 4. Aborted while the easy side's spray connection opens: it closes.
  n = await accept('easy', true);
  await pause(300);
  await abort();
  await settle('easy');
  r = await after(n);
  ok(r.states.length === 1 && r.states[0] === 'closed' && stopped(r), `4 easy side, aborted while its spray connection opens: ${states(r)}, card "${r.bday?.why}"`);

  // 5. Aborted while it applies the answer: the closed connection's error stays off the card.
  n = await accept('easy', false);
  await a.waitForFunction((x) => window.__offers.has(x), vId, { timeout: 20000 });
  await v.evaluate((h) => { window.__hold = h; }, HOLD);
  await tell({ t: 'bday', step: 'answer', sdp: offer, ip: TESTNET });
  await pause(300);
  await abort();
  await settle('easy');
  r = await after(n);
  ok(r.states.length === 1 && stopped(r), `5 easy side, aborted while applying the answer: card "${r.bday?.why}"`);

  // 6. Two answers back to back: the second is ignored, and the punch runs on. (Its spray
  // of 1024 candidates holds up what V hears next for seconds, so wait for the abort.)
  n = await accept('easy', false);
  const theirs = await a.waitForFunction((x) => window.__offers.get(x), vId, { timeout: 20000 }).then((h) => h.jsonValue());
  const answer = await a.evaluate(async (sdp) => {
    window.__answerPc = new RTCPeerConnection();
    await window.__answerPc.setRemoteDescription({ type: 'offer', sdp });
    await window.__answerPc.setLocalDescription(await window.__answerPc.createAnswer());
    return window.__answerPc.localDescription.sdp;
  }, theirs);
  await tell({ t: 'bday', step: 'answer', sdp: answer, ip: TESTNET });
  await tell({ t: 'bday', step: 'answer', sdp: answer, ip: TESTNET });
  await v.waitForFunction(() => [...document.querySelectorAll('#log div')].some((d) => /spraying \d+ ports at 203\.0\.113\.9/.test(d.textContent)), null, { timeout: 15000 });
  await pause(1500);
  const running = await after(n);
  await abort();
  await v.waitForFunction((x) => window.netlab.birthday().find((b) => b.id === x)?.status !== 'running', aId, { timeout: 30000 }).catch(() => {});
  r = await after(n);
  await a.evaluate(() => window.__answerPc.close());
  ok(running.bday?.status === 'running' && !running.errors.length && running.states.every((s) => s !== 'closed') && stopped(r),
    `6 easy side, a second answer: still ${running.bday?.status}${running.bday?.why ? ` (${running.bday.why})` : ''}, then card "${r.bday?.why}"`);
  ok(!errors.length, `no page errors${errors.length ? `: ${errors.join('; ')}` : ''}`);
} catch (e) {
  ok(false, e.stack ?? String(e));
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
