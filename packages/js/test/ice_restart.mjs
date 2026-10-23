// NB-046: ICE restart and port prediction in WebRtcTransport, between two real
// transports in headless Chromium (loopback page, no network; signalling is an
// in-page channel with 20 ms latency, no trickle, as over Nostr):
// - a restart by the dialling side renegotiates with new ICE credentials while the
//   DataChannel stays open, and datagrams flow after it;
// - the other side asks for one ('restart'), so offers never cross;
// - ICE 'disconnected' on a link that was open restarts it after RESTART_AFTER_MS,
//   'failed' at once; at most MAX_RESTARTS until ICE connects again;
// - networkChanged() restarts only links whose ICE isn't connected;
// - an offer carrying a port prediction makes the other side add 32 candidates
//   around it ('guesses'), and the link still opens; a malformed one adds none.
//
//   node packages/js/test/ice_restart.mjs
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
try {
  await new Promise((r) => setTimeout(r, 600));
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const page = await browser.newPage();
  page.on('pageerror', (e) => {
    console.log(`FAIL page error: ${e.message}`);
    failed++;
  });
  await page.goto(`http://127.0.0.1:${port}/`);
  const results = await page.evaluate(async () => {
    const { WebRtcTransport, RESTART_AFTER_MS, MAX_RESTARTS } = await import('/src/webrtc.js');
    const out = [];
    const ok = (cond, what) => out.push([!!cond, what]);
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
    const until = async (cond, ms = 8000) => {
      const t0 = performance.now();
      while (!cond() && performance.now() - t0 < ms) await sleep(50);
      return cond();
    };
    const id = (n) => n.toString(16).padStart(2, '0').repeat(32);
    const [A, B] = [id(0x0a), id(0x0b)]; // A < B: A dials

    // Two transports over an in-page signalling channel; `edit(from, msg)` may change
    // or drop (return null) a message on the way.
    const pair = async (edit = (f, m) => m) => {
      const chans = {};
      const traces = [];
      const mk = (self) => {
        const s = {
          send: (to, msg) => {
            const m = edit(self, structuredClone(msg));
            if (m) setTimeout(() => chans[to].fn(self, m), 20);
          },
          onMessage(fn) { this.fn = fn; },
          close() {},
        };
        chans[self] = s;
        const states = [];
        const got = [];
        const ep = { set onTransmit(f) { this.tx = f; }, connect() {}, receive: (h, b) => got.push(b[0]), setRelayOnly() {} };
        const tr = new WebRtcTransport(ep, s, { trickle: false, onPeer: (p, st) => states.push(st) });
        tr.trace = (p, what, d) => traces.push({ by: self === A ? 'A' : 'B', what, d });
        return { tr, ep, states, got };
      };
      const a = mk(A), b = mk(B);
      a.tr.dial(B);
      const opened = await until(() => a.states.includes('open') && b.states.includes('open'));
      const la = a.tr.links.get(a.tr.byPeer.get(B));
      const lb = b.tr.links.get(b.tr.byPeer.get(A));
      // A datagram each way, tagged `n`, arrives.
      const flows = async (n) => {
        a.ep.tx(la.handle, Uint8Array.of(n));
        b.ep.tx(lb.handle, Uint8Array.of(n));
        return until(() => a.got.includes(n) && b.got.includes(n), 3000);
      };
      const ufrag = () => /a=ice-ufrag:(\S+)/.exec(la.pc.localDescription.sdp)[1];
      const connected = () => ['connected', 'completed'].includes(la.pc.iceConnectionState) && ['connected', 'completed'].includes(lb.pc.iceConnectionState);
      const restarts = (by, reason) => traces.filter((t) => t.what === 'ice-restart' && t.by === by && (!reason || t.d.reason === reason)).length;
      // Makes ICE look `state` to the transport (the real state is still read by the browser).
      const fake = (l, state) => {
        Object.defineProperty(l.pc, 'iceConnectionState', { get: () => state, configurable: true });
        l.pc.dispatchEvent(new Event('iceconnectionstatechange'));
      };
      const real = (l) => delete l.pc.iceConnectionState;
      const close = () => {
        a.tr.close();
        b.tr.close();
      };
      return { a, b, la, lb, opened, flows, ufrag, connected, restarts, fake, real, traces, close };
    };

    // 1. A restart by the dialling side: new ICE credentials, the DataChannel stays open.
    {
      const p = await pair();
      ok(p.opened && (await p.flows(1)), 'linked, datagrams flow');
      const before = p.ufrag();
      p.a.tr.restartIce(p.la, 'test');
      const renewed = await until(() => p.ufrag() !== before && p.lb.pc.signalingState === 'stable' && p.la.pc.signalingState === 'stable' && p.connected());
      ok(renewed, `dialler's restart: new ufrag (${before} -> ${p.ufrag()}), both stable and connected`);
      ok(p.la.dc.readyState === 'open' && !p.a.states.includes('closed') && !p.b.states.includes('closed'), 'the DataChannel never closed');
      ok(await p.flows(2), 'datagrams flow after the restart');
      ok(p.a.tr.restarts === 1 && p.traces.some((t) => t.what === 'offer-received' && t.by === 'B' && t.d.restart), 'B saw a restart offer');
      // 2. The other side asks: A restarts, B never offers.
      const before2 = p.ufrag();
      p.b.tr.restartIce(p.lb, 'test');
      ok(await until(() => p.ufrag() !== before2 && p.connected()), 'B asked, A restarted');
      ok(p.b.tr.restarts === 0 && p.a.tr.restarts === 2 && p.restarts('A', 'asked') === 1, 'only the dialler offers');
      ok(await p.flows(3), 'datagrams flow after it');
      p.close();
    }

    // 3. ICE 'disconnected' on a link that was open: a restart after the grace.
    {
      const p = await pair();
      await p.flows(1);
      const before = p.ufrag();
      const t0 = performance.now();
      p.fake(p.la, 'disconnected');
      p.real(p.la);
      await sleep(RESTART_AFTER_MS / 2);
      ok(p.restarts('A') === 0, 'not before the grace');
      ok(await until(() => p.ufrag() !== before && p.connected()), `restarted after ${Math.round(performance.now() - t0)} ms of 'disconnected'`);
      ok(p.restarts('A', 'ice') === 1 && (await p.flows(4)), 'reason ice; datagrams flow');
      // 'failed' restarts at once.
      const before2 = p.ufrag();
      const t1 = performance.now();
      p.fake(p.la, 'failed');
      p.real(p.la);
      ok(await until(() => p.ufrag() !== before2, 1000), `'failed' restarts at once (${Math.round(performance.now() - t1)} ms)`);
      p.close();
    }

    // 4. At most MAX_RESTARTS while ICE stays down; connected again starts the count over.
    {
      const p = await pair();
      await p.flows(1);
      Object.defineProperty(p.la.pc, 'iceConnectionState', { get: () => 'disconnected', configurable: true });
      for (let i = 0; i < MAX_RESTARTS + 3; i++) {
        p.a.tr.restartIce(p.la, 'test');
        await sleep(150);
      }
      ok(p.a.tr.restarts === MAX_RESTARTS, `${p.a.tr.restarts} restarts of ${MAX_RESTARTS + 3} asked while ICE stays down`);
      p.real(p.la);
      p.la.pc.dispatchEvent(new Event('iceconnectionstatechange'));
      await until(() => p.connected());
      p.la.pc.dispatchEvent(new Event('iceconnectionstatechange'));
      p.a.tr.restartIce(p.la, 'test');
      ok(p.a.tr.restarts === MAX_RESTARTS + 1, 'connected again: a restart is allowed');
      await until(() => p.connected() && p.la.pc.signalingState === 'stable');
      // 5. networkChanged() restarts only links whose ICE is down.
      const n = p.a.tr.restarts;
      p.a.tr.networkChanged();
      await sleep(300);
      ok(p.a.tr.restarts === n, 'networkChanged: a connected link is left alone');
      Object.defineProperty(p.la.pc, 'iceConnectionState', { get: () => 'disconnected', configurable: true });
      p.a.tr.networkChanged();
      await sleep(300);
      p.real(p.la);
      ok(p.a.tr.restarts === n + 1, 'networkChanged: a link with ICE down restarts at once');
      p.close();
    }

    // 6. Port prediction: an offer that predicts a port makes B add 32 candidates around
    // it; the link still opens. A malformed prediction adds none.
    for (const [nat, want] of [[{ ip: '203.0.113.7', next: 40000, step: 1 }, 32], [{ ip: '203.0.113.7 1 typ host', next: 40000, step: 1 }, 0]]) {
      const p = await pair((from, m) => (from === A && m.kind === 'offer' ? { ...m, nat } : m));
      const g = p.traces.find((t) => t.what === 'guesses' && t.by === 'B');
      ok(p.opened && (g?.d.n ?? 0) === want, `prediction ${JSON.stringify(nat)}: ${g?.d.n ?? 0} candidates added, linked`);
      ok(p.b.tr.badSignals === 0, 'no signal refused');
      p.close();
    }
    return out;
  });
  for (const [pass, what] of results) {
    console.log(`${pass ? 'ok  ' : 'FAIL'} ${what}`);
    if (!pass) failed++;
  }
} catch (e) {
  console.log(`FAIL ${e.stack ?? e}`);
  failed++;
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
