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
// - without trickle (Nostr), a lost first offer or first answer still links two
//   transports (offer resent, repeated offer answered again with the same answer);
//   candidates that overtake their description are held, then added; gathering
//   stops GATHER_SETTLE_MS after the first public candidate; candidateSummary.
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

    // Lost signals (Nostr relays drop events): without trickle, a dropped first offer
    // or first answer is recovered by the offer resend (OFFER_RESEND_MS) and the
    // answer resend for a repeated offer.
    const { candidateSummary, OFFER_RESEND_MS } = await import('/src/webrtc.js');
    const pair = async (dropFirst) => {
      const [A, B] = [id(0xa1), id(0xb2)];
      const chans = {};
      const dropped = {};
      const traces = [];
      const mk = (self) => {
        const s = {
          send: (to, msg) => {
            if (msg.kind === dropFirst && !dropped[msg.kind]) return void (dropped[msg.kind] = true); // lost on the way
            setTimeout(() => chans[to].fn(self, structuredClone(msg)), 20);
          },
          onMessage(fn) { this.fn = fn; },
          close() {},
        };
        chans[self] = s;
        let opened = false;
        const ep = { set onTransmit(f) {}, connect() {}, receive() {}, setRelayOnly() {} };
        const tr = new WebRtcTransport(ep, s, { trickle: false, onPeer: (p, st) => { if (st === 'open') opened = true; } });
        tr.trace = (p, what, d) => traces.push(`${self === A ? 'A' : 'B'} ${what}${d.resend ? ' (resend)' : ''}`);
        return { tr, open: () => opened };
      };
      const a = mk(A), b = mk(B);
      const t0 = performance.now();
      a.tr.dial(B);
      while (!(a.open() && b.open()) && performance.now() - t0 < 9000) await new Promise((r) => setTimeout(r, 100));
      const res = { open: a.open() && b.open(), ms: Math.round(performance.now() - t0), resentA: a.tr.resent, resentB: b.tr.resent, traces };
      a.tr.close();
      b.tr.close();
      return res;
    };
    const lostOffer = await pair('offer');
    ok(lostOffer.open && lostOffer.resentA === 1 && lostOffer.ms >= OFFER_RESEND_MS[0], `first offer lost: the resent offer links them (${lostOffer.ms} ms, A resent ${lostOffer.resentA})`);
    const lostAnswer = await pair('answer');
    ok(lostAnswer.open && lostAnswer.resentA === 1 && lostAnswer.resentB === 1, `first answer lost: the repeated offer gets the answer again (${lostAnswer.ms} ms, A resent ${lostAnswer.resentA}, B resent ${lostAnswer.resentB})`);
    ok(lostAnswer.traces.includes('B answer-sent (resend)') && lostAnswer.traces.includes('A answer-received'), `traces show it (${lostAnswer.traces.join(', ')})`);
    const clean = await pair('none');
    ok(clean.open && clean.resentA === 0 && clean.resentB === 0, `nothing lost: linked with no resend (${clean.ms} ms)`);

    // A repeated offer is answered with the same answer, and makes no new link.
    const S = id(0x5e);
    const offer = await realOffer();
    sig.fn(S, { kind: 'offer', sdp: offer });
    await settle();
    const answers = () => sent.filter(([to, m]) => to === S && m.kind === 'answer').map(([, m]) => m.sdp.sdp);
    const pcBefore = t.links.get(t.byPeer.get(S))?.pc;
    sig.fn(S, { kind: 'offer', sdp: offer });
    await settle();
    ok(answers().length === 2 && answers()[0] === answers()[1] && t.links.get(t.byPeer.get(S))?.pc === pcBefore, 'a repeated offer gets the same answer again, on the same link');

    // Candidates that overtake their description are held, then added; a bad batch is counted.
    const D = id(0xd4);
    const dialer = new WebRtcTransport(ep, { send() {}, onMessage(fn) {}, close() {} }, { trickle: false });
    await dialer.dial(D);
    const dl = dialer.links.get(dialer.byPeer.get(D));
    const remote = new RTCPeerConnection();
    remote.createDataChannel('peerwire', { negotiated: true, id: 0 });
    await remote.setRemoteDescription(dl.pc.localDescription);
    await remote.setLocalDescription(await remote.createAnswer());
    const cand = { candidate: 'candidate:9 1 udp 2122260223 192.0.2.7 50007 typ host', sdpMid: '0', sdpMLineIndex: 0 };
    await dialer.onSignal(D, { kind: 'ice-batch', c: [cand] });
    ok(dl.held.length === 1, 'an ice-batch before the answer is held');
    const badBefore2 = dialer.badSignals;
    await dialer.onSignal(D, { kind: 'ice-batch', c: 'x' });
    await dialer.onSignal(D, { kind: 'ice-batch', c: new Array(33).fill(cand) });
    ok(dialer.badSignals === badBefore2 + 2, 'an ice-batch that is not a list of at most 32 is counted');
    await dialer.onSignal(D, { kind: 'answer', sdp: remote.localDescription.toJSON() });
    await settle();
    ok(dl.held.length === 0 && dl.pc.remoteDescription.sdp.includes('192.0.2.7'), 'the held candidate is added once the answer comes');
    dialer.close();
    remote.close();

    // Gathering without trickle stops GATHER_SETTLE_MS after the first public candidate,
    // or at GATHER_MAX_MS when none comes.
    const { GATHER_SETTLE_MS, GATHER_MAX_MS } = await import('/src/webrtc.js');
    const fakePc = () => Object.assign(new EventTarget(), { iceGatheringState: 'gathering' });
    const g = new WebRtcTransport(ep, { send() {}, onMessage() {}, close() {} }, { trickle: false });
    const timed = async (pc, after) => { const s = performance.now(); const p = g.gathered(pc); after?.(); await p; return Math.round(performance.now() - s); };
    const srflx = (pc) => setTimeout(() => {
      const e = new Event('icecandidate');
      e.candidate = { candidate: 'candidate:1 1 udp 1 157.49.1.1 4 typ srflx raddr 0.0.0.0 rport 0' };
      pc.dispatchEvent(e);
    }, 100);
    const pcA = fakePc();
    const settled = await timed(pcA, () => srflx(pcA));
    ok(settled >= 100 + GATHER_SETTLE_MS - 20 && settled < 100 + GATHER_SETTLE_MS + 200, `gathering settles ${GATHER_SETTLE_MS} ms after the first public candidate (${settled} ms)`);
    const capped = await timed(fakePc());
    ok(capped >= GATHER_MAX_MS - 20 && capped < GATHER_MAX_MS + 200, `with no public candidate it stops at ${GATHER_MAX_MS} ms (${capped} ms)`);
    g.close();

    const sum = candidateSummary(['a=candidate:1 1 udp 1 192.168.1.2 1 typ host', 'a=candidate:2 1 udp 1 2409:40d0::1 2 typ host',
      'a=candidate:3 1 udp 1 abcd.local 3 typ host', 'a=candidate:4 1 udp 1 157.49.1.1 4 typ srflx raddr 0.0.0.0 rport 0',
      'a=candidate:5 1 udp 1 2409:40d0::1 5 typ srflx raddr :: rport 0', 'a=candidate:6 1 udp 1 1.2.3.4 6 typ relay'].join('\r\n'));
    ok(JSON.stringify(sum) === JSON.stringify({ host4: 1, host6: 1, mdns: 1, srflx4: 1, srflx6: 1, prflx: 0, relay: 1 }), `candidateSummary counts by type and family (${JSON.stringify(sum)})`);
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
