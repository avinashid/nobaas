// NB-048: a candidate gathered after the first late 'ice-batch' is sent too, not
// lost (the bug a field report showed: a public address gathered after an mDNS-only
// answer never reached the peer). A fake RTCPeerConnection drives the non-trickle
// late path directly, so timing is deterministic; no browser.
//
//   node packages/js/test/late_ice.mjs
import { WebRtcTransport, LATE_FLUSH_MS } from '../src/webrtc.js';

let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const id = (n) => n.toString(16).padStart(2, '0').repeat(32);

// Just enough of RTCPeerConnection for link() to build and for us to fire candidates.
class FakePc {
  constructor() {
    this.iceGatheringState = 'gathering';
    this.iceConnectionState = 'new';
    this.connectionState = 'new';
    this.signalingState = 'stable';
    this.localDescription = { sdp: 'v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n', toJSON() { return { type: 'offer', sdp: this.sdp }; } };
  }
  createDataChannel() { return { binaryType: '', readyState: 'connecting' }; }
  addEventListener() {}
  removeEventListener() {}
  close() {}
}
const candidate = (addr, port, type = 'srflx') => ({
  candidate: `candidate:1 1 udp 1686052607 ${addr} ${port} typ ${type} raddr 0.0.0.0 rport 0`,
  toJSON() { return { candidate: this.candidate, sdpMid: '0', sdpMLineIndex: 0 }; },
});

const sent = [];
const sig = { send: (to, msg) => sent.push([to, msg]), onMessage() {}, close() {} };
const ep = { set onTransmit(f) {}, connect() {}, receive() {}, setRelayOnly() {} };
const t = new WebRtcTransport(ep, sig, { trickle: false, RTCPeerConnection: FakePc });

const peer = id(0xab);
const l = t.link(peer);
l.described = true; // as describe() sets once the answer/offer is out
const batches = () => sent.filter(([to, m]) => to === peer && m.kind === 'ice-batch');

// A candidate before the first batch is buffered, not sent.
l.pc.onicecandidate({ candidate: candidate('203.0.113.9', 40000) });
ok(batches().length === 0 && l.late.length === 1, 'a candidate before the first batch is buffered');

// The first batch (gathering settles) sends it and clears the buffer.
t.sendLate(l);
ok(batches().length === 1 && batches()[0][1].c.length === 1 && l.late.length === 0 && l.lateSent === 1, 'the first batch sends the buffered candidate');

// A candidate found AFTER the first batch (the bug): it is buffered and a flush is armed.
l.pc.onicecandidate({ candidate: candidate('203.0.113.9', 40002) });
ok(l.late.length === 1 && l.lateTimer, 'a candidate after the first batch is buffered and a flush is armed');
await sleep(LATE_FLUSH_MS + 50);
ok(batches().length === 2 && batches()[1][1].c[0].candidate.includes('40002'), 'it is sent in a second batch (the bug is fixed)');

// Gathering complete flushes whatever is left at once.
l.pc.onicecandidate({ candidate: candidate('203.0.113.9', 40004) });
l.pc.onicecandidate({ candidate: null });
ok(batches().length === 3 && batches()[2][1].c[0].candidate.includes('40004'), 'gathering complete flushes the rest at once');

// Bounded: past MAX_LATE candidates in all, no more are buffered.
const before = l.lateSent + l.late.length;
for (let i = 0; i < 200; i++) l.pc.onicecandidate({ candidate: candidate('203.0.113.9', 50000 + i) });
ok(l.lateSent + l.late.length <= 64 && l.lateSent + l.late.length >= before, `bounded at MAX_LATE: ${l.lateSent + l.late.length} candidates in all`);

// Over MAX_ICE_BATCH (32) queued at once go out in successive batches, each within the cap.
const t2 = new WebRtcTransport(ep, sig, { trickle: false, RTCPeerConnection: FakePc });
const l2 = t2.link(id(0xcd));
l2.described = true;
sent.length = 0;
for (let i = 0; i < 40; i++) l2.late.push(candidate('198.51.100.1', 40000 + i).toJSON());
t2.sendLate(l2);
await sleep(20);
const b2 = sent.filter(([, m]) => m.kind === 'ice-batch');
ok(b2.length === 2 && b2.every(([, m]) => m.c.length <= 32) && b2[0][1].c.length === 32 && b2[1][1].c.length === 8, `40 queued go out as ${b2.map(([, m]) => m.c.length).join(' + ')}, each ≤ 32`);

t.close();
t2.close();
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
