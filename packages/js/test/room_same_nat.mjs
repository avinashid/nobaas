// joinRoom with peers behind one public IPv4 (a laptop on a phone's hotspot), on a
// fake in-process RTCPeerConnection whose description names the member's public IPv4
// in a srflx candidate (or trickles it late, as a slow STUN answer) and whose ICE
// never connects unless the test opens the pair. Checks:
// 1. same public IPv4 in both descriptions: a 'same-nat' trace on both sides, the
//    dialler's link counts as failed after sameNatFallbackMs (3 s), not 8, and the
//    pair links over signalling;
// 2. the answerer's srflx trickles in 1 s late ('local' signalling): the same, timed
//    from the offer; 3. the dialler's own srflx comes late (sent in an ice-batch): the same;
// 4. different public IPv4s: no trace, failed after 8 s, as before;
// 5. a same-NAT pair linked over signalling (pipe batches 300 ms late, as on Nostr)
//    whose DataChannel then opens: both routes go direct and stay there while the
//    pipe's late batches arrive, and data goes over the channel, none over the pipe;
// 6. three members on one NAT, a|b relayed by c, then a|b's DataChannel opens: direct;
// 7. a same-NAT pair whose DataChannel opens at once (a LAN): direct, nothing fails.
//
//   node packages/js/test/room_same_nat.mjs
import { joinRoom } from '../src/index.js';
import { BroadcastSignaling, RTC_FALLBACK_MS, RTC_SAME_NAT_FALLBACK_MS } from '../src/webrtc.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
process.on('unhandledRejection', (e) => fail(`unhandled: ${e?.stack ?? e}`));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, cond, ms = 20000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await sleep(5);
  }
};
const near = (v, want, what) => {
  if (v == null || v < want - 50 || v > want + 600) fail(`${what}: ${v} ms, want about ${want}`);
};

class FakeDc {
  constructor() {
    this.readyState = 'connecting';
    this.bufferedAmount = 0;
    this.other = null;
  }
  send(bytes) {
    const o = this.other;
    const copy = bytes.slice().buffer;
    setTimeout(() => o.readyState === 'open' && o.onmessage?.({ data: copy }));
  }
  open() {
    this.readyState = 'open';
    this.onopen?.();
  }
}

const pcs = new Map(); // token -> pc
const byPair = new Map(); // 'x|y' -> x's pc for its link to y
let tokens = 0;
const srflx = (ip) => `candidate:1 1 udp 1686052607 ${ip} 40000 typ srflx raddr 0.0.0.0 rport 0`;

// The RTCPeerConnection class for member `owner` behind public IPv4 `ip`. With
// lateOffer/lateAnswer, an offer's or answer's srflx candidate comes that many ms after
// the description instead of in it.
const fakePc = (owner, ip, { lateOffer = 0, lateAnswer = 0 } = {}) => class FakePc {
  constructor() {
    this.owner = owner;
    this.token = String(++tokens);
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    this.remoteDescription = null;
    pcs.set(this.token, this);
  }
  createDataChannel() {
    this.dc = new FakeDc();
    return this.dc;
  }
  description(type) {
    const late = type === 'offer' ? lateOffer : lateAnswer;
    return { type, sdp: ['v=0', `a=fake:${this.token}`, ...(late ? [] : [`a=${srflx(ip)}`]), ''].join('\r\n') };
  }
  async createOffer() {
    return this.description('offer');
  }
  async createAnswer() {
    return this.description('answer');
  }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
    const late = d.type === 'offer' ? lateOffer : lateAnswer;
    if (!late) return;
    setTimeout(() => {
      const c = { candidate: srflx(ip), sdpMid: '0', sdpMLineIndex: 0 };
      this.onicecandidate?.({ candidate: { ...c, toJSON: () => c } });
      this.onicecandidate?.({ candidate: null });
    }, late);
  }
  async setRemoteDescription(d) {
    const remote = pcs.get(/a=fake:(\d+)/.exec(d.sdp)?.[1]);
    if (!remote) throw new Error('unknown remote');
    this.remoteDescription = { type: d.type, sdp: d.sdp };
    byPair.set(`${this.owner}|${remote.owner}`, this);
  }
  addEventListener() {}
  async addIceCandidate() {}
  close() {
    this.connectionState = 'closed';
    pcs.delete(this.token);
  }
};

// ICE finds a path between x and y: their DataChannels open.
const pair = (x, y) => {
  const p = byPair.get(`${x}|${y}`);
  const q = byPair.get(`${y}|${x}`);
  if (!p || !q) fail(`no link ${x}|${y} to open`);
  p.dc.other = q.dc;
  q.dc.other = p.dc;
  for (const pc of [p, q]) pc.connectionState = 'connected';
  p.dc.open();
  q.dc.open();
};
const pairWhenReady = (x, y) => until(`${x}|${y} to describe`, () => byPair.has(`${x}|${y}`) && byPair.has(`${y}|${x}`)).then(() => pair(x, y));

// BroadcastChannel signalling held until `go`, so the test hooks each transport first;
// `dgDelay` holds pipe batches that long, as a Nostr relay round trip does.
const gated = (gate, dgDelay = 0) => ({ endpoint, room }) => {
  const inner = new BroadcastSignaling(endpoint.id, `peerwire/${room}`);
  return {
    id: inner.id,
    get onHello() { return inner.onHello; },
    set onHello(fn) { inner.onHello = fn; },
    hello: (meta) => gate.then(() => inner.hello(meta)),
    send: (to, msg) => gate.then(() => (msg?.kind === 'dg' && dgDelay ? sleep(dgDelay) : null)).then(() => inner.send(to, msg)),
    onMessage: (fn) => inner.onMessage(fn),
    close: () => inner.close(),
  };
};

// What each room's transport does: when its link counts as failed, in ms after its
// clock started (as read before a 'same-nat' trace re-arms it, so a re-arm that moved
// the start would show), and its 'same-nat' traces.
function hook(r) {
  const t = r.transport;
  const seen = { failedAfter: null, sameNat: [], start: 0 };
  const link = (peer) => t.links.get(t.byPeer.get(peer));
  const onPeer = t.onPeer;
  t.onPeer = (peer, state, h) => {
    if (state === 'failed') seen.failedAfter = Date.now() - (seen.start || link(peer).watchedAt);
    onPeer(peer, state, h);
  };
  t.trace = (peer, what, d) => {
    if (what !== 'same-nat') return;
    seen.sameNat.push(d);
    seen.start = link(peer)?.watchedAt ?? 0;
  };
  return seen;
}

let rooms = 0;
// Members `names` (each [name, ip, fakePc options]) in one room; `gate` false uses
// 'local' signalling (trickled candidates), with no hold.
async function members(names, { gate = true, dgDelay = 0 } = {}) {
  const id = `same-nat-${process.pid}-${++rooms}`;
  let go;
  const held = new Promise((r) => (go = r));
  const rs = await Promise.all(names.map(([name, ip, o]) => joinRoom({
    appId: 'peerwire-test', roomId: id, presence: { name },
    signaling: gate ? gated(held, dgDelay) : 'local',
    RTCPeerConnection: fakePc(name, ip, o),
  })));
  const seen = rs.map(hook);
  const t0 = Date.now();
  go();
  return { rs, seen, t0 };
}
const dialerOf = (x, y) => (x.self < y.self ? 0 : 1);
const NAT = '203.0.113.7';

// 1-4: when the fallback starts, and that the pair links over signalling.
async function timing(label, a, b, opts, sameNat) {
  const { rs: [x, y], seen, t0 } = await members([a, b], opts);
  await until(`${label}: a link`, () => x.peers.length === 1 && y.peers.length === 1, 30000);
  const linkedMs = Date.now() - t0;
  const d = dialerOf(x, y);
  const want = sameNat ? RTC_SAME_NAT_FALLBACK_MS : RTC_FALLBACK_MS;
  near(seen[d].failedAfter, want, `${label}: the dialler's link failed after`);
  if (x.route(y.self)?.kind !== 'signaling' || y.route(x.self)?.kind !== 'signaling') fail(`${label}: routes ${JSON.stringify([x.route(y.self), y.route(x.self)])}`);
  const traces = seen.map((s) => s.sameNat);
  if (sameNat ? !traces.every((t) => t.length === 1 && t[0].ip === NAT && t[0].ms === RTC_SAME_NAT_FALLBACK_MS) : traces.some((t) => t.length)) fail(`${label}: same-nat traces ${JSON.stringify(traces)}`);
  await Promise.all([x.leave(), y.leave()]);
  return `${label}: ${sameNat ? 'same-nat traced on both sides, ' : 'no same-nat trace, '}the dialler fell back ${seen[d].failedAfter} ms after its offer; linked over signalling ${linkedMs} ms after hello`;
}

// 5: a link over signalling, then its own DataChannel opens.
async function lateOpen() {
  const { rs: [x, y] } = await members([['e1', NAT], ['e2', NAT]], { dgDelay: 300 });
  await until('5: a link over signalling', () => x.route(y.self)?.kind === 'signaling' && y.route(x.self)?.kind === 'signaling', 30000);
  // Steady traffic both ways, so pipe batches are on their way when the channel opens.
  const ticker = setInterval(() => {
    x.send('tick', { lane: 'unreliable' });
    y.send('tick', { lane: 'unreliable' });
  }, 20);
  await sleep(400);
  const t = Date.now();
  pair('e1', 'e2');
  await until('5: both routes direct', () => x.route(y.self)?.kind === 'direct' && y.route(x.self)?.kind === 'direct', 5000);
  const switchedMs = Date.now() - t;
  const sent = [x, y].map((r) => r.transport.piped.sent);
  // The pipe's late batches (300 ms) arrive meanwhile: the routes stay direct.
  for (const end = Date.now() + 1200; Date.now() < end; await sleep(5)) {
    const rs = [x.route(y.self)?.kind, y.route(x.self)?.kind];
    if (rs.some((k) => k !== 'direct')) fail(`5: a route went back: ${rs}`);
  }
  clearInterval(ticker);
  const big = new Uint8Array(50 * 1024).map((_, i) => i * 3);
  const got = [];
  y.on('message', (m) => m instanceof Uint8Array && got.push(m));
  x.send(big, { to: y.self });
  await until('5: 50 KB over the channel', () => got.length === 1, 5000);
  if (got[0].length !== big.length || got[0].some((v, i) => v !== big[i])) fail('5: 50 KB corrupted');
  const after = [x, y].map((r) => r.transport.piped.sent);
  if (after.some((n, i) => n !== sent[i])) fail(`5: the pipe still sent (${sent} → ${after} batches)`);
  const stale = x.transport.piped.stale + y.transport.piped.stale;
  await Promise.all([x.leave(), y.leave()]);
  return `5: same NAT, linked over signalling; the DataChannel opened: direct both ways in ${switchedMs} ms, stayed direct (${stale} late pipe datagrams dropped), 50 KB over the channel, no pipe batch after`;
}

// 6: relayed by a member, then the pair's own DataChannel opens.
async function lateOpenRelayed() {
  const names = [['f1', NAT], ['f2', NAT], ['f3', NAT]];
  const opened = [pairWhenReady('f1', 'f3'), pairWhenReady('f2', 'f3')];
  const { rs: [a, b, c] } = await members(names);
  await Promise.all(opened);
  await until('6: a|b relayed by c', () => a.route(b.self)?.kind === 'peer' && b.route(a.self)?.kind === 'peer', 30000);
  const t = Date.now();
  pair('f1', 'f2');
  await until('6: a|b direct', () => a.route(b.self)?.kind === 'direct' && b.route(a.self)?.kind === 'direct', 5000);
  const ms = Date.now() - t;
  await Promise.all([a.leave(), b.leave(), c.leave()]);
  return `6: three members on one NAT, a|b relayed by c; their DataChannel opened: direct both ways in ${ms} ms`;
}

// 7: the DataChannel opens at once, as two desktops on one Wi-Fi resolve each other's mDNS.
async function quick() {
  const opened = pairWhenReady('g1', 'g2');
  const { rs: [x, y], seen } = await members([['g1', NAT], ['g2', NAT]]);
  await opened;
  await until('7: a direct link', () => x.route(y.self)?.kind === 'direct' && y.route(x.self)?.kind === 'direct', 10000);
  await sleep(RTC_SAME_NAT_FALLBACK_MS + 300);
  if (seen.some((s) => s.failedAfter != null) || x.route(y.self)?.kind !== 'direct') fail(`7: ${JSON.stringify(seen)}, route ${JSON.stringify(x.route(y.self))}`);
  await Promise.all([x.leave(), y.leave()]);
  return '7: same NAT, the DataChannel opened at once: direct, and nothing failed after 3 s';
}

const results = await Promise.all([
  timing('1: same public IPv4 in both descriptions', ['a1', NAT], ['a2', NAT], {}, true),
  timing("2: the answerer's srflx trickles 1 s late", ['b1', NAT, { lateAnswer: 1000 }], ['b2', NAT, { lateAnswer: 1000 }], { gate: false }, true),
  timing("3: the dialler's own srflx comes 1 s late (ice-batch)", ['c1', NAT, { lateOffer: 1000 }], ['c2', NAT, { lateOffer: 1000 }], {}, true),
  timing('4: different public IPv4s', ['d1', NAT], ['d2', '198.51.100.9'], {}, false),
  lateOpen(),
  lateOpenRelayed(),
  quick(),
]);
for (const r of results) console.log(r);
console.log('room_same_nat: ok');
process.exit(0);
