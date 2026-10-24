// A link over the signalling pipe moves to a member relay that links later, and back.
// A fake in-process RTCPeerConnection links any two members except a and b, whose
// ICE fails, so a and b link over signalling (a pipe). Checks:
// - c joins and links to both: a and b move their link to c once the core has seen c
//   relay both ways (route says { kind: 'peer', via: c } on both sides), with no
//   'peer-leave' or 'peer-join', and messages flow through c;
// - c leaves: the link goes back to the pipe at once, again with no leave or join, and
//   messages flow over signalling;
// - with Fast lane 1: d joins and the link moves to d; then a and b's ICE works again and
//   a retry (retryDirect) opens a channel: the link goes 'direct', the same session with
//   no leave or join, the room clears the pipe's slow mark, d relays nothing more for it,
//   and d leaving changes nothing;
// - FL2-2: e joins, and one of b's datagrams for a, kept back as a relaying member could,
//   comes to a on e's channel: a's link to b doesn't move onto it;
// - every datagram the transport gives an endpoint names the peer of the channel it came on.
//
//   node packages/js/test/room_relay_move.mjs
import { joinRoom } from '../src/index.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
process.on('unhandledRejection', (e) => fail(`unhandled: ${e?.stack ?? e}`));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, cond, ms = 15000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await sleep(10);
  }
  return Date.now() - t;
};

const blocked = new Set(); // 'a|b', both orders
const pcs = new Map(); // token -> FakePc
let tokens = 0;

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

// An RTCPeerConnection class for member `owner` (as in room_fallback.mjs).
const fakePc = (owner) => class FakePc {
  constructor() {
    this.owner = owner;
    this.token = String(++tokens);
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    pcs.set(this.token, this);
  }
  createDataChannel() {
    this.dc = new FakeDc();
    return this.dc;
  }
  async createOffer() {
    return { type: 'offer', sdp: `fake:${this.token}` };
  }
  async createAnswer() {
    return { type: 'answer', sdp: `fake:${this.token}` };
  }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
  }
  async setRemoteDescription(d) {
    const remote = pcs.get(d.sdp.slice(5));
    if (!remote) throw new Error('unknown remote');
    this.remote = remote;
    if (d.type === 'answer') setTimeout(() => this.connect(remote), 5);
  }
  connect(r) {
    if (blocked.has(`${this.owner}|${r.owner}`)) {
      for (const pc of [this, r]) {
        pc.connectionState = 'failed';
        pc.onconnectionstatechange?.();
      }
      return;
    }
    this.dc.other = r.dc;
    r.dc.other = this.dc;
    for (const pc of [this, r]) pc.connectionState = 'connected';
    this.dc.open();
    r.dc.open();
  }
  addEventListener() {}
  async addIceCandidate() {}
  close() {
    this.connectionState = 'closed';
    if (this.dc) this.dc.readyState = 'closed';
    pcs.delete(this.token);
  }
};

// Every datagram the transport gives an endpoint names the peer whose channel it came on.
const unnamed = [];
const room = async (name) => {
  const r = await joinRoom({
    appId: 'peerwire-test',
    roomId: `relay-move-${process.pid}`,
    presence: { name },
    fallbackMs: 300,
    RTCPeerConnection: fakePc(name),
  });
  const receive = r.endpoint.receive.bind(r.endpoint);
  r.endpoint.receive = (addr, bytes, peer) => {
    if (typeof peer !== 'string' || peer === r.self) unnamed.push(`${name}: handle ${addr}, peer ${peer}`);
    receive(addr, bytes, peer);
  };
  return r;
};
const same = (r, want) => JSON.stringify(r) === JSON.stringify(want);

blocked.add('a|b').add('b|a');
const [a, b] = await Promise.all(['a', 'b'].map(room));
const log = [];
for (const [name, r] of [['a', a], ['b', b]]) {
  r.on('peer-join', (p) => log.push(`${name} join ${p.slice(0, 8)}`));
  r.on('peer-leave', (p, why) => log.push(`${name} leave ${p.slice(0, 8)} ${why}`));
}
await until('a and b linked over signalling', () => a.peers.length === 1 && b.peers.length === 1, 30000);
if (!same(a.route(b.self), { kind: 'signaling' }) || !same(b.route(a.self), { kind: 'signaling' })) {
  fail(`a|b blocked but routes ${JSON.stringify([a.route(b.self), b.route(a.self)])}`);
}
const joins = log.length;
console.log('a|b blocked: linked over signalling');

// What the core hears from each room about the other: its last setSlow, its new sessions
// and its path changes.
const slow = new Map(); // room -> last setSlow address for the other (a pipe handle, or null)
const seen = new Map(); // room -> { connected, paths }
for (const [r, p] of [[a, b.self], [b, a.self]]) {
  const set = r.endpoint.setSlow.bind(r.endpoint);
  r.endpoint.setSlow = (peer, addr) => (peer === p && slow.set(r, addr), set(peer, addr));
  const s = { connected: 0, paths: 0 };
  seen.set(r, s);
  const ev = r.endpoint.onEvent;
  r.endpoint.onEvent = (e) => {
    if (e.peer === p && e.type === 'connected') s.connected++;
    if (e.peer === p && e.type === 'pathChanged') s.paths++;
    ev(e);
  };
}
const sessions = () => [...seen.values()].map((s) => s.connected).join();

// Messages each way between a and b; resolves once both arrived.
const both = async (what, tag) => {
  const got = [];
  const offA = a.on('message', (d, from) => from === b.self && d === `${tag} to a` && got.push('a'));
  const offB = b.on('message', (d, from) => from === a.self && d === `${tag} to b` && got.push('b'));
  a.send(`${tag} to b`, { to: b.self });
  b.send(`${tag} to a`, { to: a.self });
  await until(`messages both ways ${what}`, () => got.length === 2, 15000);
  offA();
  offB();
};
await both('over signalling', 'pipe');

// c joins and links to both directly.
const c = await room('c');
const viaC = { kind: 'peer', via: c.self };
const moved = await until('a and b moving to c', () => same(a.route(b.self), viaC) && same(b.route(a.self), viaC), 30000);
if (a.route(c.self)?.kind !== 'direct' || b.route(c.self)?.kind !== 'direct') fail('c not direct to a and b');
const relayed = c.stats().relayed;
await both('through c', 'relayed');
if (c.stats().relayed <= relayed) fail('the messages did not go through c');
const bLog = () => log.slice(joins).filter((l) => l.includes(a.self.slice(0, 8)) || l.includes(b.self.slice(0, 8)));
if (bLog().length) fail(`a and b saw each other leave or join: ${bLog()}`);
console.log(`c joins: a and b move their link to c ${moved} ms after c's room resolved, same link, messages relayed by c`);

// c leaves: back to the pipe at once.
await c.leave();
const back = await until('a and b back on signalling', () => same(a.route(b.self), { kind: 'signaling' }) && same(b.route(a.self), { kind: 'signaling' }), 30000);
if (back > 1000) fail(`back on signalling only after ${back} ms`);
if (bLog().length || !a.peers.includes(b.self) || !b.peers.includes(a.self)) fail(`a and b lost each other: ${bLog()}`);
if (![a, b].every((r) => typeof slow.get(r) === 'number')) fail(`the pipe isn't marked slow again: ${[...slow.values()]}`);
await both('back over signalling', 'pipe again');
console.log(`c leaves: a and b back on signalling ${back} ms later, same link, messages flow`);

// Fast lane 1 and 2 together: the link moves to d, then a retry takes it direct.
const d = await room('d');
const viaD = { kind: 'peer', via: d.self };
await until('a and b moving to d', () => same(a.route(b.self), viaD) && same(b.route(a.self), viaD), 30000);
blocked.clear();
if (a.retryDirect(b.self) !== 1) fail('retryDirect(b) on a link relayed by d is not 1');
const direct = await until('a and b direct after the retry', () => a.route(b.self)?.kind === 'direct' && b.route(a.self)?.kind === 'direct', 30000);
if (bLog().length || sessions() !== '0,0') fail(`a new session or a leave: ${bLog()}, connected ${sessions()}`);
if (slow.get(a) !== null || slow.get(b) !== null) fail(`the slow mark wasn't cleared: ${[...slow.values()]}`);
await sleep(200); // what d had in flight
const relayedD = d.stats().relayed;
await both('over the retry', 'direct');
await sleep(1200); // past two rounds of relay probes (500 ms)
if (d.stats().relayed !== relayedD) fail(`d relayed ${d.stats().relayed - relayedD} more datagrams after a and b went direct`);
const paths = [...seen.values()].map((s) => s.paths).join();
await d.leave();
await sleep(300);
if ([...seen.values()].map((s) => s.paths).join() !== paths || a.route(b.self)?.kind !== 'direct' || b.route(a.self)?.kind !== 'direct') {
  fail(`d leaving moved the link: ${JSON.stringify([a.route(b.self), b.route(a.self)])}`);
}
await both('after d left', 'direct again');
if (bLog().length || sessions() !== '0,0') fail(`a new session or a leave: ${bLog()}, connected ${sessions()}`);
console.log(`d joins, the link moves to d, a retry takes it direct ${direct} ms later: same link, slow mark cleared, nothing more through d, d leaving changes nothing`);

// FL2-2: one of b's datagrams for a, kept back, comes to a on e's channel. The transport
// names e as its peer, so a's core drops it unread, and the link to b stays on b's channel.
const e = await room('e');
await until('e linked to a', () => a.route(e.self)?.kind === 'direct', 30000);
const send = b.endpoint.onTransmit;
const toA = b.transport.byPeer.get(a.self);
let kept = null;
b.endpoint.onTransmit = (h, bytes) => (h === toA && !kept ? (kept = bytes) : send(h, bytes));
b.send('one kept back', { to: a.self });
await until("a datagram of b's for a, kept", () => kept !== null, 5000);
b.endpoint.onTransmit = send;
const pathsA = seen.get(a).paths;
a.transport.links.get(a.transport.byPeer.get(e.self)).dc.onmessage({ data: kept.slice().buffer });
await sleep(100);
if (seen.get(a).paths !== pathsA) fail("b's datagram on e's channel moved a's link to b");
await both("after b's datagram came on e's channel", 'not moved');
if (bLog().length || sessions() !== '0,0') fail(`a new session or a leave: ${bLog()}, connected ${sessions()}`);
console.log("e joins: one of b's datagrams for a, on e's channel, leaves the link where it was");
await e.leave();

if (unnamed.length) fail(`datagrams given to an endpoint without their channel's peer: ${unnamed.slice(0, 5)}`);
await Promise.all([a.leave(), b.leave()]);
console.log('room_relay_move: ok');
process.exit(0);
