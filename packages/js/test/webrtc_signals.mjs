// NB-014: WebRtcTransport takes signals from untrusted carriers. In headless
// Chromium (loopback page, no network), with a fake signalling channel:
// - malformed offers (not SDP, not an object) leave no link behind and no
//   unhandled rejection (which would end a Node host), and a real offer from the
//   same peer is answered afterwards;
// - a fuzz of random signals (mutated real SDP, junk candidates, junk messages)
//   from several peers throws nothing, rejects nothing unhandled, leaves only
//   links that took a remote description, and a new peer's real offer is still
//   answered;
// - refused signals (bad `from`, empty message, malformed offer) are counted in
//   `badSignals`; a real offer is not.
//
//   node packages/js/test/webrtc_signals.mjs [cases=300]
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const cases = Number(process.argv[2] ?? 300);
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
  const results = await page.evaluate(async ({ cases, seed }) => {
    const { WebRtcTransport } = await import('/src/webrtc.js');
    const unhandled = [];
    window.addEventListener('unhandledrejection', (e) => {
      unhandled.push(String(e.reason?.message ?? e.reason));
      e.preventDefault();
    });
    const sent = [];
    const sig = { send: (to, msg) => sent.push([to, msg]), onMessage(fn) { this.fn = fn; }, close() {} };
    const ep = { set onTransmit(f) {}, connect() {}, setRelayOnly() {} };
    const t = new WebRtcTransport(ep, sig, {});
    const settle = () => new Promise((r) => setTimeout(r, 300));
    const answered = (peer) => sent.some(([to, m]) => to === peer && m.kind === 'answer');
    const realOffer = async () => {
      const pc = new RTCPeerConnection();
      pc.createDataChannel('x');
      await pc.setLocalDescription(await pc.createOffer());
      return pc.localDescription.toJSON();
    };
    const out = [];
    const ok = (cond, what) => out.push([!!cond, what]);
    const id = (n) => n.toString(16).padStart(2, '0').repeat(32);

    // Regression: malformed offers.
    const [P, Q] = [id(1), id(2)];
    sig.fn(P, { kind: 'offer', sdp: { type: 'offer', sdp: 'garbage' } });
    sig.fn(Q, { kind: 'offer', sdp: 5 });
    await settle();
    ok(unhandled.length === 0, `malformed offers reject nothing unhandled (${unhandled.join(' | ')})`);
    ok(!t.has(P) && !t.has(Q), 'malformed offers leave no link behind');
    sig.fn('not a peer', { kind: 'offer', sdp: await realOffer() });
    sig.fn(P, null);
    await settle();
    ok(t.badSignals === 4, `2 malformed offers, a bad from and an empty message are counted (badSignals ${t.badSignals})`);
    sig.fn(P, { kind: 'offer', sdp: await realOffer() });
    await settle();
    ok(answered(P) && t.has(P), 'a real offer from the same peer is answered afterwards');
    ok(t.badSignals === 4, `a real offer is not counted (badSignals ${t.badSignals})`);

    // Fuzz.
    let s = seed >>> 0;
    const next = () => ((s = (Math.imul(s ^ (s >>> 15), 0x2c1b3c6d) + 0x9e3779b9) >>> 0), s);
    const pick = (xs) => xs[next() % xs.length];
    const real = await realOffer();
    const mutate = (text) => {
      let v = text;
      for (let k = 1 + (next() % 4); k > 0; k--) {
        const at = next() % (v.length + 1);
        v = pick([
          () => v.slice(0, at) + String.fromCharCode(next() % 128) + v.slice(at + 1),
          () => v.slice(0, at) + v.slice(at + 1 + (next() % 40)),
          () => v.slice(0, at) + pick(['\r\n', 'a=', 'm=application 0', 'c=IN IP4 0.0.0.0', '\0', '9'.repeat(30)]) + v.slice(at),
          () => v.slice(0, at),
        ])();
      }
      return v;
    };
    const sdp = () => pick([
      () => real,
      () => ({ type: 'offer', sdp: mutate(real.sdp) }),
      () => ({ type: pick(['answer', 'pranswer', 'rollback', 'x']), sdp: mutate(real.sdp) }),
      () => pick([null, undefined, 5, 'v=0', [], {}, { type: 'offer' }]),
    ])();
    const peers = [3, 4, 5, 6, 7, 8].map(id);
    let threw = 0;
    const badBefore = t.badSignals;
    for (let i = 0; i < cases; i++) {
      const msg = pick([
        () => ({ kind: 'offer', sdp: sdp() }),
        () => ({ kind: 'answer', sdp: sdp() }),
        () => ({ kind: 'ice', candidate: pick([null, 5, 'x', {}, { candidate: mutate('candidate:1 1 udp 2122260223 127.0.0.1 5000 typ host'), sdpMid: '0' }]) }),
        () => pick([null, 0, 'offer', [], { kind: 'dg' }, { kind: 7 }]),
      ])();
      try {
        sig.fn(pick([...peers, 'not a peer', null]), msg);
      } catch {
        threw++;
      }
      if (i % 50 === 49) await settle();
    }
    await new Promise((r) => setTimeout(r, 1000));
    ok(threw === 0, `${cases} random signals: none throws`);
    ok(unhandled.length === 0, `${cases} random signals: no unhandled rejection (${unhandled.slice(0, 3).join(' | ')})`);
    const links = [...t.links.values()];
    ok(links.every((l) => l.pc.remoteDescription), `every link left (${links.length}) took a remote description`);
    ok(t.badSignals > badBefore, `random signals are counted (badSignals +${t.badSignals - badBefore} of ${cases})`);
    const R = id(9);
    sig.fn(R, { kind: 'offer', sdp: await realOffer() });
    await settle();
    ok(answered(R), "a new peer's real offer is still answered");
    return out;
  }, { cases, seed: Number(process.env.PW_FUZZ_SEED ?? 0x6e62) });
  for (const [ok, what] of results) {
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
    if (!ok) failed++;
  }
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'webrtc signals: all passed');
process.exit(failed ? 1 : 0);
