// joinRoom's fallback when WebRTC can't link two members (both behind symmetric
// NATs, no TURN). A fake in-process RTCPeerConnection links any two members
// except the pairs in `blocked`, whose ICE fails. Checks:
// - three members, one pair blocked: the pair links through the third member
//   (signalling waits for Room.peerFirstMs), and messages and presence flow;
// - two members, blocked: they link over the signalling channel (a pipe), and a
//   100 KB reliable message arrives whole despite the pipe's rate cap;
// - fallback 'peers' with no one to relay, and 'off': no link, as before.
//
//   node packages/js/test/room_fallback.mjs
import { joinRoom } from '../src/index.js';
import { MAX_PIPE_PEERS, WebRtcTransport } from '../src/webrtc.js';

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

// An RTCPeerConnection class for member `owner`.
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
    // The offerer applying the answer completes the pair.
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
    pcs.delete(this.token);
  }
};
const block = (x, y) => (blocked.add(`${x}|${y}`), blocked.add(`${y}|${x}`));

let n = 0;
const room = (name, extra) => joinRoom({
  appId: 'peerwire-test',
  roomId: `fallback-${process.pid}-${n}`,
  presence: { name },
  fallbackMs: 300,
  RTCPeerConnection: fakePc(name),
  ...extra,
});

// 1. A and B can't link; C links to both.
n = 1;
block('a1', 'b1');
const [a, b, c] = await Promise.all(['a1', 'b1', 'c1'].map((m) => room(m)));
await until('a full mesh through the fallback', () => [a, b, c].every((r) => r.peers.length === 2), 30000);
const ab = a.route(b.self);
// A member relaying is preferred: signalling joins the dial only if it doesn't answer.
if (ab?.kind !== 'peer' || ab.via !== c.self) fail(`a reaches b ${JSON.stringify(ab)}, not through c`);
if (a.route(c.self)?.kind !== 'direct' || b.route(c.self)?.kind !== 'direct') fail('a or b not direct to c');
if (b.getPresence(a.self)?.name !== 'a1') fail('presence over the fallback');
const atB = [];
const atA = [];
b.on('message', (d, from) => from === a.self && atB.push(d));
a.on('message', (d, from) => from === b.self && atA.push(d));
a.send('over the fallback', { to: b.self });
b.send({ back: true }, { to: a.self });
await until('messages both ways', () => atB.length === 1 && atA.length === 1);
if (atB[0] !== 'over the fallback' || atA[0]?.back !== true) fail('wrong messages');
console.log('3 members, a|b blocked: a reaches b relayed by c, messages and presence both ways');
await Promise.all([a.leave(), b.leave(), c.leave()]);

// 2. Two members, blocked: only signalling is left.
n = 2;
block('a2', 'b2');
const [d, e] = await Promise.all(['a2', 'b2'].map((m) => room(m)));
await until('a link over signalling', () => d.peers.length === 1 && e.peers.length === 1, 30000);
if (d.route(e.self)?.kind !== 'signaling' || e.route(d.self)?.kind !== 'signaling') fail(`routes ${JSON.stringify([d.route(e.self), e.route(d.self)])}`);
const big = new Uint8Array(100 * 1024).map((_, i) => i * 7);
const gotBig = [];
e.on('message', (m) => gotBig.push(m));
const t0 = Date.now();
d.send(big);
await until('100 KB over signalling', () => gotBig.length === 1, 30000);
if (gotBig[0].length !== big.length || gotBig[0].some((x, i) => x !== big[i])) fail('100 KB corrupted');
const piped = d.transport.piped;
console.log(`2 members, blocked: linked over signalling; 100 KB arrived whole in ${Date.now() - t0} ms (${piped.sent} batches sent)`);
await Promise.all([d.leave(), e.leave()]);

// 3. 'peers' with no third member, and 'off': WebRTC only, so no link.
for (const fallback of ['peers', 'off']) {
  n++;
  block(`a-${fallback}`, `b-${fallback}`);
  const rs = await Promise.all([`a-${fallback}`, `b-${fallback}`].map((m) => room(m, { fallback })));
  await sleep(1500);
  if (rs.some((r) => r.peers.length)) fail(`fallback '${fallback}' linked a blocked pair`);
  await Promise.all(rs.map((r) => r.leave()));
}
console.log("fallback 'peers' alone and 'off': a blocked pair stays apart");

// 4. NB43-1: proven peers that send a 'dg' and never link don't pile up pipes.
{
  const linked = '0'.repeat(64);
  const ep = { id: 'f'.repeat(64), receive() {}, isConnected: (p) => p === linked, set onTransmit(_) {} };
  const sig = { onMessage() {}, send() {}, close() {} };
  const t = new WebRtcTransport(ep, sig, { pipes: true, RTCPeerConnection: fakePc('x') });
  t.allowPipe(linked);
  t.pipeReceive(linked, ['AAAA']);
  for (let i = 1; i <= 1000; i++) {
    const id = i.toString(16).padStart(64, '1');
    t.allowPipe(id);
    t.pipeReceive(id, ['AAAA']);
  }
  if (t.pipeAllowed.size > MAX_PIPE_PEERS || t.pipes.size > MAX_PIPE_PEERS + 1 || t.pipeByPeer.size !== t.pipes.size) fail(`pipes not bounded: allowed ${t.pipeAllowed.size}, pipes ${t.pipes.size}`);
  if (!t.pipeByPeer.has(linked)) fail("an evicted peer's pipe was dropped while its session used it");
  t.close();
  console.log(`1,000 proven peers with a 'dg' each: ${t.pipeAllowed.size} allowed, pipes bounded; a linked peer's pipe kept`);
}

console.log('room_fallback: ok');
process.exit(0);
