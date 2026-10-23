// NB-034, in Node over an in-memory wire: SDP host-candidate filtering, and a
// relay-only endpoint linking only through a relay and refusing direct dials.
import { loadPeerwire, Lane } from '../src/peerwire.js';
import { WebRtcTransport, withoutHostCandidates } from '../src/webrtc.js';

const fail = (m) => {
  console.error(`FAIL: ${m}`);
  process.exit(1);
};

const sdp = ['v=0', 'a=candidate:1 1 udp 2122260223 192.168.1.5 50000 typ host generation 0',
  'a=candidate:2 1 udp 2122260223 4b1e7a2c.local 50001 typ host', 'a=candidate:3 1 udp 1686052607 203.0.113.9 61000 typ srflx raddr 0.0.0.0 rport 0',
  'a=mid:0'].join('\r\n');
const kept = withoutHostCandidates(sdp);
if (/typ host/.test(kept) || !/typ srflx/.test(kept) || !kept.includes('a=mid:0')) fail(`filtered SDP: ${kept}`);

// MIC-1: once a page has mic or camera permission, a srflx candidate's related address is
// its local socket, and gathering writes the default candidate (a host one when no STUN
// server answered) into the connection line and the m-line port. None of that is sent.
for (const c of ['c=IN IP4 10.0.0.170', 'c=IN IP6 2001:db8::5']) {
  const gathered = ['v=0', 'm=application 56167 UDP/DTLS/SCTP webrtc-datachannel', c,
    'a=candidate:1 1 udp 2122260223 10.0.0.170 56167 typ host generation 0',
    'a=candidate:3 1 udp 1686052607 203.0.113.9 61000 typ srflx raddr 10.0.0.170 rport 56167 generation 0', 'a=mid:0'].join('\r\n');
  const pub = withoutHostCandidates(gathered);
  const want = ['v=0', 'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', 'c=IN IP4 0.0.0.0',
    'a=candidate:3 1 udp 1686052607 203.0.113.9 61000 typ srflx raddr 0.0.0.0 rport 0 generation 0', 'a=mid:0'].join('\r\n');
  if (pub !== want) fail(`public-only description of a gathered SDP (${c}): ${JSON.stringify(pub)}`);
}
// The candidates a public-only transport sends on their own (trickled, or late after its
// description) have their related address cleared too; under 'direct' they go as they are.
{
  const srflx = 'candidate:3 1 udp 1686052607 203.0.113.9 61000 typ srflx raddr 10.0.0.170 rport 56167 generation 0';
  class Pc {
    constructor() { this.iceGatheringState = 'complete'; this.localDescription = null; }
    createDataChannel() { return { readyState: 'connecting' }; }
    async createOffer() { return { type: 'offer', sdp: 'v=0\r\na=mid:0\r\n' }; }
    async setLocalDescription(d) { this.localDescription = { ...d, toJSON: () => d }; }
    addEventListener() {}
    close() {}
  }
  const sent = async (privacy, trickle) => {
    const out = [];
    const ep = { set onTransmit(f) {}, setRelayOnly() {} };
    const sig = { send: (to, m) => out.push(m), onMessage() {}, close() {} };
    const t = new WebRtcTransport(ep, sig, { privacy, trickle, RTCPeerConnection: Pc });
    const peer = 'ab'.repeat(32);
    await t.dial(peer);
    const pc = t.links.get(t.byPeer.get(peer)).pc;
    const c = { candidate: srflx, sdpMid: '0', sdpMLineIndex: 0 };
    pc.onicecandidate({ candidate: { ...c, toJSON: () => c } });
    pc.onicecandidate({ candidate: null });
    t.close();
    return out.flatMap((m) => (m.kind === 'ice' ? [m.candidate] : m.kind === 'ice-batch' ? m.c : [])).map((x) => x.candidate);
  };
  const [pubTrickled, pubLate, direct] = [await sent('public-only', true), await sent('public-only', false), await sent('direct', true)];
  const cleared = (list) => list.length === 1 && list[0].includes('raddr 0.0.0.0 rport 0') && !list[0].includes('10.0.0.170');
  if (!cleared(pubTrickled) || !cleared(pubLate) || direct[0] !== srflx) fail(`candidates sent: trickled ${pubTrickled}, late ${pubLate}, direct ${direct}`);
}

const wasm = process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname;
const pw = await loadPeerwire(wasm);
// Three endpoints, every pair wired: handle n reaches endpoint n.
const eps = [pw.endpoint({ relayOnly: true }), pw.endpoint(), pw.endpoint()];
const [a, r, b] = eps;
eps.forEach((ep, i) => {
  ep.onTransmit = (to, bytes) => setImmediate(() => eps[to].receive(i, bytes));
});
const events = eps.map(() => []);
eps.forEach((ep, i) => {
  ep.onEvent = (e) => events[i].push(e);
});
const until = async (cond, ms = 3000) => {
  const end = performance.now() + ms;
  while (!cond() && performance.now() < end) await new Promise((res) => setTimeout(res, 1));
  return cond();
};
// A direct dial from B to relay-only A goes unanswered.
b.connect(0, a.peerId);
if (await until(() => b.isConnected(a.id), 700)) fail('a relay-only endpoint answered a direct dial');
a.connect(1, r.peerId);
b.connect(1, r.peerId);
if (!(await until(() => a.isConnected(r.id) && b.isConnected(r.id)))) fail('no links to the relay');
a.connectVia(r.peerId, b.peerId);
if (!(await until(() => a.isConnected(b.id) && b.isConnected(a.id)))) fail('no relayed link');
a.send(b.id, new TextEncoder().encode('psst'), Lane.Reliable);
if (!(await until(() => events[2].some((e) => e.type === 'message')))) fail('no message through the relay');
await until(() => false, 300);
const direct = events[0].concat(events[2]).filter((e) => e.type === 'pathChanged');
if (direct.length) fail(`the relayed link moved: ${JSON.stringify(direct)}`);
console.log(`privacy: SDP filtered (${kept.split('\r\n').length} lines kept; a gathered one names no local address, nor do its candidates); relay-only link up through the relay, direct dial refused, relayed ${r.stats().relayed} packets`);
eps.forEach((ep) => ep.close());
process.exit(0);
