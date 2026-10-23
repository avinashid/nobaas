// room.adoptChannel: a direct DataChannel found after the room fell back (a birthday
// punch's, say) takes the link over. A fake in-process RTCPeerConnection links any two
// members except the pairs in `blocked`; a punched channel is a fake pair handed to both
// ends. Checks:
// - over signalling (two members, blocked): both ends adopt, the route says direct
//   (adopted), 100 KB goes over the channel and not the pipe; when the channel
//   closes, the link is back on signalling at once and still carries messages;
// - relayed by a member (three, a|b blocked): both adopt, the endpoint punches over
//   the channel and moves there, and the member relays nothing more; when the
//   channel closes, the link falls back to the member (the endpoint's direct timeout);
// - only one end adopts: false after CARRY_WAIT_MS, the route unchanged, the channel
//   left open to the caller; a channel that closes while waiting: false;
// - a link that is direct already: false, at once;
// - while the endpoint's path still names a relayed channel that just closed, the old
//   WebRTC link to the peer closing doesn't drop it (NB48b-5);
// - 500 CARRY get a handful of CARRIED, none past the buffer cap (NB48b-6).
//
//   node packages/js/test/room_adopt.mjs
import { joinRoom } from '../src/index.js';
import { CARRY, CARRIED, CARRY_EVERY_MS, CARRY_WAIT_MS, MAX_BUFFERED } from '../src/webrtc.js';

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
    this.sent = 0;
  }
  send(bytes) {
    if (this.readyState !== 'open') throw new Error('not open');
    this.sent++;
    const o = this.other;
    const copy = bytes.slice().buffer;
    setTimeout(() => o.readyState === 'open' && o.onmessage?.({ data: copy }));
  }
  open() {
    this.readyState = 'open';
    this.onopen?.();
  }
  // Closes both ends, as a real channel does.
  close() {
    for (const d of [this, this.other]) {
      if (!d || d.readyState === 'closed') continue;
      d.readyState = 'closed';
      setTimeout(() => d.onclose?.());
    }
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

// A punched channel: two open ends, each with a connection whose close() closes it.
function punched() {
  const ends = [0, 1].map(() => {
    const dc = new FakeDc();
    const pc = { connectionState: 'connected', onconnectionstatechange: null, closed: false };
    pc.close = () => {
      pc.closed = true;
      pc.connectionState = 'closed';
      dc.close();
    };
    return { pc, dc };
  });
  ends[0].dc.other = ends[1].dc;
  ends[1].dc.other = ends[0].dc;
  for (const e of ends) e.dc.open();
  return ends;
}

let n = 0;
const room = (name) => joinRoom({
  appId: 'peerwire-test',
  roomId: `adopt-${process.pid}-${n}`,
  presence: { name },
  fallbackMs: 300,
  RTCPeerConnection: fakePc(name),
});
const isAdopted = (r) => r?.kind === 'direct' && r.adopted === true;

// 1. Over signalling.
n = 1;
block('a1', 'b1');
{
  const [a, b] = await Promise.all(['a1', 'b1'].map((m) => room(m)));
  await until('a link over signalling', () => a.peers.length === 1 && b.peers.length === 1, 30000);
  if (a.route(b.self)?.kind !== 'signaling') fail(`a reaches b ${JSON.stringify(a.route(b.self))}`);
  const [x, y] = punched();
  const took = await Promise.all([a.adoptChannel(b.self, x.pc, x.dc), b.adoptChannel(a.self, y.pc, y.dc)]);
  if (!took[0] || !took[1]) fail(`adoptChannel over signalling: ${took}`);
  if (!isAdopted(a.route(b.self)) || !isAdopted(b.route(a.self))) fail(`routes ${JSON.stringify([a.route(b.self), b.route(a.self)])}`);
  if (a.transport.carrier(b.self)?.dc !== x.dc) fail('carrier(peer) is not the adopted channel');
  const big = new Uint8Array(100 * 1024).map((_, i) => i * 13);
  const got = [];
  b.on('message', (m) => got.push(m));
  const [piped, onDc] = [a.transport.piped.sent, x.dc.sent];
  const t0 = Date.now();
  a.send(big);
  await until('100 KB over the adopted channel', () => got.length === 1, 10000);
  if (got[0].length !== big.length || got[0].some((v, i) => v !== big[i])) fail('100 KB corrupted');
  if (a.transport.piped.sent !== piped) fail(`${a.transport.piped.sent - piped} batches still went over signalling`);
  console.log(`over signalling: both ends adopted; 100 KB in ${Date.now() - t0} ms over the channel (${x.dc.sent - onDc} datagrams), none over signalling`);
  // The channel closes: back on signalling at once, and the link carries on.
  x.pc.close();
  await until('the routes back on signalling', () => a.route(b.self)?.kind === 'signaling' && b.route(a.self)?.kind === 'signaling', 3000);
  if (!y.pc.closed) fail("the far end's connection was not closed");
  b.send('after the channel', { to: a.self });
  const back = [];
  a.on('message', (m) => back.push(m));
  await until('a message over signalling again', () => back.includes('after the channel'), 15000);
  if (a.peers.length !== 1 || b.peers.length !== 1) fail('a peer left when the channel closed');
  console.log('over signalling: the channel closed, the link went back to signalling and carried on');

  // 3. Only one end adopts: false after CARRY_WAIT_MS, nothing moved, the channel left open.
  const [p, q] = punched();
  const t1 = Date.now();
  const alone = await a.adoptChannel(b.self, p.pc, p.dc);
  if (alone !== false || Date.now() - t1 < CARRY_WAIT_MS - 100) fail(`one-sided adopt: ${alone} after ${Date.now() - t1} ms`);
  if (a.route(b.self)?.kind !== 'signaling' || a.transport.carrier(b.self)) fail('a one-sided adopt moved the link');
  if (p.pc.closed || p.dc.readyState !== 'open' || p.dc.onmessage) fail('a refused channel was not left to the caller');
  console.log(`one end only: false after ${Date.now() - t1} ms, link still on signalling, channel left open`);
  p.pc.close();
  if (q.dc.readyState !== 'closed') fail('fake close');
  // A channel that closes while waiting: false.
  const [s] = punched();
  setTimeout(() => s.pc.close(), 100);
  const t2 = Date.now();
  if ((await a.adoptChannel(b.self, s.pc, s.dc)) !== false || Date.now() - t2 > 2000) fail('a channel closed while waiting was not refused promptly');
  console.log('a channel closed while waiting: false at once');

  // 5. A CARRY flood (NB48b-6): a handful of CARRIED, at most one per CARRY_EVERY_MS,
  // sent through dcSend, so none goes past the buffer cap.
  const [f, g] = punched();
  let replies = 0;
  g.dc.onmessage = (e) => {
    const v = new Uint8Array(e.data);
    if (v.length === 1 && v[0] === CARRIED) replies++;
  };
  const adopted = a.transport.adopt(b.self, f.pc, f.dc);
  for (let i = 0; i < 500; i++) g.dc.send(Uint8Array.of(CARRY));
  if ((await adopted) < 0) fail('the flooded channel was not adopted');
  await sleep(100);
  if (replies > 3) fail(`500 CARRY got ${replies} CARRIED`);
  const flood = replies;
  await sleep(CARRY_EVERY_MS);
  const congested = a.transport.congested;
  f.dc.bufferedAmount = MAX_BUFFERED + 1;
  for (let i = 0; i < 50; i++) g.dc.send(Uint8Array.of(CARRY));
  await sleep(100);
  if (replies !== flood || a.transport.congested === congested) fail(`with the buffer full, ${replies - flood} CARRIED went out (congested ${a.transport.congested - congested})`);
  console.log(`a CARRY flood: 500 in, ${flood} CARRIED out; with the buffer full, none`);
  f.pc.close();
  await Promise.all([a.leave(), b.leave()]);
}

// 2. Relayed by a member.
n = 2;
block('a2', 'b2');
{
  const [a, b, c] = await Promise.all(['a2', 'b2', 'c2'].map((m) => room(m)));
  await until('a full mesh through the fallback', () => [a, b, c].every((r) => r.peers.length === 2), 30000);
  if (a.route(b.self)?.kind !== 'peer' || b.route(a.self)?.kind !== 'peer') fail(`a reaches b ${JSON.stringify(a.route(b.self))}, not relayed`);
  // 4. Direct already: false, at once.
  const [d] = punched();
  if ((await a.adoptChannel(c.self, d.pc, d.dc)) !== false || d.pc.closed) fail('adopting over a direct link');
  console.log('a direct link: adoptChannel is false, the channel untouched');
  d.pc.close();
  const [x, y] = punched();
  const took = await Promise.all([a.adoptChannel(b.self, x.pc, x.dc), b.adoptChannel(a.self, y.pc, y.dc)]);
  if (!took[0] || !took[1]) fail(`adoptChannel relayed: ${took}`);
  await until('both routes direct over the channel', () => isAdopted(a.route(b.self)) && isAdopted(b.route(a.self)), 5000);
  const relayed = c.stats().relayed;
  const got = [];
  b.on('message', (m) => got.push(m));
  for (let i = 0; i < 50; i++) a.send(`m${i}`, { to: b.self });
  await until('50 messages over the channel', () => got.length === 50, 10000);
  if (c.stats().relayed !== relayed) fail(`c still relayed ${c.stats().relayed - relayed} packets`);
  console.log(`relayed by c: both ends adopted, the endpoints punched over the channel and moved; 50 messages, c relayed none (punches ${a.stats().punchSucceeded}/${b.stats().punchSucceeded} succeeded)`);
  // The channel closes: the endpoint sees the direct path gone and goes back to c.
  const t0 = Date.now();
  const left = [];
  for (const r of [a, b]) r.on('peer-leave', (p, why) => left.push(why));
  y.pc.close();
  // NB48b-5: until its direct timeout the endpoint's path still names the closed channel,
  // so the route says direct; the old, failed WebRTC link to the peer closing now must
  // not drop it.
  await until('both channels gone', () => !a.transport.carrier(b.self) && !b.transport.carrier(a.self), 3000);
  for (const [r, p] of [[a, b.self], [b, a.self]]) {
    if (r.route(p)?.kind !== 'direct' || r.route(p).adopted) fail(`not in the stale window: ${JSON.stringify(r.route(p))}`);
    const old = r.transport.links.get(r.transport.byPeer.get(p));
    if (!old?.dc.onclose) fail('no old WebRTC link to close');
    old.dc.onclose();
  }
  await until('the routes back through c', () => left.length || (a.route(b.self)?.kind === 'peer' && b.route(a.self)?.kind === 'peer'), 30000);
  if (left.length) fail(`the old WebRTC link closing dropped the peer (${left})`);
  const back = [];
  a.on('message', (m) => back.push(m));
  b.send('relayed again', { to: a.self });
  await until('a message through c again', () => back.includes('relayed again'), 10000);
  if ([a, b, c].some((r) => r.peers.length !== 2) || left.length) fail(`a peer left when the channel closed (${left})`);
  console.log(`relayed by c: the channel closed, the old WebRTC links closed too, the link was back through c in ${Date.now() - t0} ms and carried on`);
  await Promise.all([a.leave(), b.leave(), c.leave()]);
}

console.log('room_adopt: ok');
process.exit(0);
