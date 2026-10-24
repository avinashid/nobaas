// A link over the signalling pipe moves to a member relay that links later, and back.
// A fake in-process RTCPeerConnection links any two members except a and b, whose
// ICE fails, so a and b link over signalling (a pipe). Checks:
// - c joins and links to both: a and b move their link to c once the core has seen c
//   relay both ways (route says { kind: 'peer', via: c } on both sides), with no
//   'peer-leave' or 'peer-join', and messages flow through c;
// - c leaves: the link goes back to the pipe at once, again with no leave or join, and
//   messages flow over signalling.
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

const room = (name) => joinRoom({
  appId: 'peerwire-test',
  roomId: `relay-move-${process.pid}`,
  presence: { name },
  fallbackMs: 300,
  RTCPeerConnection: fakePc(name),
});
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
await both('back over signalling', 'pipe again');
console.log(`c leaves: a and b back on signalling ${back} ms later, same link, messages flow`);

await Promise.all([a.leave(), b.leave()]);
console.log('room_relay_move: ok');
process.exit(0);
