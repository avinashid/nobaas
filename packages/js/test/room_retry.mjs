// joinRoom retrying WebRTC while a link runs on a fallback (Room.retryDirect,
// src/retry.js), on a fake in-process RTCPeerConnection whose first N attempts between
// two members fail. Checks:
// 1. a pair linked over signalling: the backoff's retry opens a new connection and the
//    link moves onto it: both routes 'direct', the same session (one 'connected', no
//    peer-leave or second peer-join), data over the channel and none over the pipe;
//    the retry's offer said `retry`;
// 2. a retry that fails closes quietly: no 'failed', the link stays on the pipe, no
//    peer-leave; the next retry, twice as late, goes direct;
// 3. the other side (the larger id) calls retryDirect(): it asks, the dialling side
//    retries, and the pair goes direct; retryDirect() on a direct room is 0;
// 4. a pair relayed by a third member goes direct the same way;
// 5. privacy 'public-only': the retry's offer and answer carry no host candidate and
//    name no local address (with 'direct' they do);
// 6. triggers: a microphone grant retries in a 'direct' room, not with retryOnGrant
//    false nor in 'public-only'; 'online' and a new connection type retry, a 'change'
//    with the same type doesn't;
// 7. leave() stops the retries;
// 8. a retry offer that doesn't prove the room key replaces nothing;
// 9. nor does a proven one while the answerer's link is open: the pair stays direct;
// 10. 20 proven retry offers with new descriptions, 50 ms apart, make one connection and
//     one answer (WebRtcTransport.retryAnswerMs); a copy of the one taken is answered again;
// 11. a 'retry' ask sent to the larger id is ignored: it doesn't ask back, nobody retries;
// 12. a retry whose createOffer throws ends as a failed one: its link goes, and the next
//     retry goes direct.
//
//   node packages/js/test/room_retry.mjs
import { joinRoom } from '../src/index.js';
import { BroadcastSignaling, WebRtcTransport } from '../src/webrtc.js';
import { DirectRetries } from '../src/retry.js';

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
    await sleep(10);
  }
};

DirectRetries.firstMs = 1500;
DirectRetries.gapMs = 200;
DirectRetries.askMs = 1000;

// Pairs whose next attempts fail: 'a|b' (both orders) -> attempts left to fail.
const failing = new Map();
const failNext = (x, y, n) => (failing.set(`${x}|${y}`, n), failing.set(`${y}|${x}`, n));
const pcs = new Map(); // token -> FakePc
let tokens = 0;
const made = new Map(); // owner -> connections made
const throwOffer = new Set(); // owners whose next createOffer throws

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

// A description naming a LAN host candidate and a public one whose related address is
// the LAN socket, as Chrome's do once the page has microphone permission.
const sdpOf = (token) => [`fake:${token}`, 'c=IN IP4 192.168.7.20', 'a=candidate:1 1 udp 2122260223 192.168.7.20 50000 typ host',
  'a=candidate:2 1 udp 1686052607 203.0.113.20 50001 typ srflx raddr 192.168.7.20 rport 50000', ''].join('\r\n');

const fakePc = (owner) => class FakePc {
  constructor() {
    this.owner = owner;
    this.token = String(++tokens);
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    this.remoteDescription = null;
    pcs.set(this.token, this);
    made.set(owner, (made.get(owner) ?? 0) + 1);
  }
  createDataChannel() {
    this.dc = new FakeDc();
    return this.dc;
  }
  async createOffer() {
    if (throwOffer.delete(this.owner)) throw new Error('createOffer failed');
    return { type: 'offer', sdp: sdpOf(this.token) };
  }
  async createAnswer() {
    return { type: 'answer', sdp: sdpOf(this.token) };
  }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
  }
  async setRemoteDescription(d) {
    const remote = pcs.get(d.sdp.split('\r\n')[0].slice(5));
    if (!remote) throw new Error('unknown remote');
    this.remote = remote;
    this.remoteDescription = d;
    // The offerer applying the answer completes the pair.
    if (d.type === 'answer') setTimeout(() => this.connect(remote), 5);
  }
  connect(r) {
    const key = `${this.owner}|${r.owner}`;
    if (failing.get(key) > 0 || this.connectionState === 'closed' || r.connectionState === 'closed') {
      if (failing.get(key) > 0) failNext(this.owner, r.owner, failing.get(key) - 1);
      for (const pc of [this, r]) {
        if (pc.connectionState === 'closed') continue;
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

// Signalling over BroadcastChannel that records what each member sends.
const sent = []; // { from, to, body }
const recorded = ({ endpoint, room }) => {
  const s = new BroadcastSignaling(endpoint.id, `peerwire/${room}`);
  const send = s.send.bind(s);
  s.send = (to, body) => {
    sent.push({ from: endpoint.id, to, body });
    return send(to, body);
  };
  return s;
};

let n = 0;
const room = async (name, extra) => {
  const r = await joinRoom({
    appId: 'peerwire-test',
    roomId: `retry-${process.pid}-${n}`,
    presence: { name },
    signaling: recorded,
    fallbackMs: 300,
    RTCPeerConnection: fakePc(name),
    ...extra,
  });
  r.owner = name;
  // Every endpoint event and transport state, to check the session stays the same one.
  r.seen = { connected: 0, pathChanged: 0, failed: 0, retryFailed: 0, joins: 0, leaves: 0 };
  const ev = r.endpoint.onEvent;
  r.endpoint.onEvent = (e) => {
    if (e.type === 'connected') r.seen.connected++;
    if (e.type === 'pathChanged') r.seen.pathChanged++;
    ev(e);
  };
  const onPeer = r.transport.onPeer;
  r.transport.onPeer = (peer, state, h) => {
    if (state === 'failed') r.seen.failed++;
    if (state === 'retry-failed') r.seen.retryFailed++;
    onPeer(peer, state, h);
  };
  r.on('peer-join', () => r.seen.joins++);
  r.on('peer-leave', () => r.seen.leaves++);
  return r;
};
// Two members, a < b (a dials); their first `fails` attempts fail.
const pair = async (fails, extra = {}) => {
  n++;
  const [x, y] = [`a${n}`, `b${n}`];
  failNext(x, y, fails);
  const rs = await Promise.all([room(x, extra), room(y, extra)]);
  rs.sort((p, q) => (p.self < q.self ? -1 : 1));
  return rs;
};
const routes = (a, b) => [a.route(b.self)?.kind, b.route(a.self)?.kind];
const offersFrom = (r) => sent.filter((s) => s.from === r.self && s.body?.kind === 'offer');

// 1. Over signalling, then the backoff's retry goes direct: the same session.
{
  const [a, b] = await pair(1);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  const t0 = Date.now();
  await until('both routes direct after the retry', () => routes(a, b).join() === 'direct,direct');
  const took = Date.now() - t0;
  if (took < 1500 * 0.8 - 100) fail(`went direct ${took} ms after the fallback, before the first retry was due`);
  // Not at the next keepalive (10 s): the room's nudge moves an idle link at once.
  if (took > 6000) fail(`went direct ${took} ms after the fallback: the link waited for a keepalive`);
  if (a.transport.retried !== 1 || b.transport.retried !== 0) fail(`retries begun: a ${a.transport.retried}, b ${b.transport.retried}`);
  const retryOffers = offersFrom(a).filter((s) => s.body.retry === true);
  if (retryOffers.length !== 1 || offersFrom(b).length) fail(`retry offers ${retryOffers.length}, offers from b ${offersFrom(b).length}`);
  // Only the dialling side keeps the backoff: b never asked.
  if (sent.some((s) => s.from === b.self && s.body?.kind === 'retry')) fail('the larger id asked for a retry on its own backoff');
  for (const r of [a, b]) {
    if (r.seen.connected !== 1 || r.seen.joins !== 1 || r.seen.leaves || r.seen.failed > 1 || !r.seen.pathChanged) fail(`a new session or a leave: ${JSON.stringify(r.seen)}`);
  }
  // Data now goes over the new channel: once the pipe has sent what was queued for it,
  // nothing more goes over signalling.
  const pipe = a.transport.pipes.get(a.transport.pipeByPeer.get(b.self));
  await until('the pipe idle', () => !pipe || (!pipe.q.length && !pipe.timer));
  const before = a.transport.piped.sent;
  const got = [];
  b.on('message', (m) => got.push(m));
  const big = new Uint8Array(64 * 1024).map((_, i) => i * 13);
  a.send(big);
  a.send('after the retry');
  await until('data over the new channel', () => got.length === 2);
  if (got[0].length !== big.length || got[0].some((x, i) => x !== big[i]) || got[1] !== 'after the retry') fail('data corrupted');
  if (a.transport.piped.sent !== before || pipe?.q.length) fail(`${a.transport.piped.sent - before} batches went over signalling after the retry`);
  console.log(`1. over signalling, then the retry went direct ${took} ms after the fallback: one session, no leave, 64 KB over the channel and none over the pipe`);
  await Promise.all([a.leave(), b.leave()]);
}

// 2. The first retry fails quietly; the second, twice as late, goes direct.
{
  const [a, b] = await pair(2);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  const t0 = Date.now();
  await until('the first retry failed', () => a.seen.retryFailed === 1, 10000);
  const t1 = Date.now() - t0;
  if (routes(a, b).join() !== 'signaling,signaling' || a.seen.failed !== 1 || b.seen.failed > 1 || a.seen.leaves || b.seen.leaves) fail(`after a failed retry: ${routes(a, b)}, ${JSON.stringify([a.seen, b.seen])}`);
  if (a.transport.has(b.self)) fail('the failed retry left its connection behind');
  await until('direct on the second retry', () => routes(a, b).join() === 'direct,direct', 15000);
  const t2 = Date.now() - t0;
  if (a.transport.retried !== 2 || t2 - t1 < 3000 * 0.8 - 300) fail(`second retry: ${a.transport.retried} begun, ${t2 - t1} ms after the first failed`);
  if (a.seen.connected !== 1 || b.seen.connected !== 1 || a.seen.leaves || b.seen.leaves) fail(`a new session or a leave: ${JSON.stringify([a.seen, b.seen])}`);
  console.log(`2. a failed retry closed quietly (still over signalling, no 'failed', no leave); the next, ${t2 - t1} ms later, went direct`);
  await Promise.all([a.leave(), b.leave()]);
}

// 3. The larger id asks: retryDirect() on b sends 'retry', a dials afresh.
{
  DirectRetries.firstMs = 60000; // no backoff retry in this test
  const [a, b] = await pair(1);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  if (b.retryDirect(a.self) !== 1) fail('retryDirect(peer) on a peer over signalling is not 1');
  await until('direct after b asked', () => routes(a, b).join() === 'direct,direct', 10000);
  const asks = sent.filter((s) => s.from === b.self && s.body?.kind === 'retry');
  if (asks.length !== 1 || a.transport.retried !== 1 || b.transport.retried !== 0) fail(`asks ${asks.length}, retries a ${a.transport.retried}, b ${b.transport.retried}`);
  if (a.retryDirect() !== 0 || b.retryDirect() !== 0) fail('retryDirect() on a direct room is not 0');
  console.log("3. the larger id's retryDirect() asked once, the dialling side retried, and the pair went direct");
  await Promise.all([a.leave(), b.leave()]);
  DirectRetries.firstMs = 1500;
}

// 4. Relayed by a third member, then direct.
{
  n++;
  const names = [`a${n}`, `b${n}`, `c${n}`];
  failNext(names[0], names[1], 1);
  const [x, y, c] = await Promise.all(names.map((m) => room(m)));
  const [a, b] = [x, y].sort((p, q) => (p.self < q.self ? -1 : 1));
  await until('a full mesh', () => [a, b, c].every((r) => r.peers.length === 2), 20000);
  if (a.route(b.self)?.kind !== 'peer' || a.route(b.self).via !== c.self) fail(`a reaches b ${JSON.stringify(a.route(b.self))}, not through c`);
  await until('a and b direct after the retry', () => routes(a, b).join() === 'direct,direct');
  if ([a, b].some((r) => r.seen.connected !== 2 || r.seen.leaves || r.seen.joins !== 2)) fail(`a new session or a leave: ${JSON.stringify([a.seen, b.seen])}`);
  console.log('4. relayed by a member, then the retry went direct: same session, no leave');
  await Promise.all([a.leave(), b.leave(), c.leave()]);
}

// 5. 'public-only': the retry's descriptions carry no host candidate, name no LAN address.
for (const privacy of ['public-only', 'direct']) {
  const [a, b] = await pair(1, { privacy });
  await until('direct after the retry', () => routes(a, b).join() === 'direct,direct');
  const retryOffer = offersFrom(a).find((s) => s.body.retry === true);
  const answer = sent.filter((s) => s.from === b.self && s.body?.kind === 'answer').at(-1);
  const sdps = [retryOffer?.body.sdp.sdp, answer?.body.sdp.sdp];
  if (sdps.some((x) => typeof x !== 'string')) fail(`${privacy}: no retry offer or answer recorded`);
  const leaks = sdps.filter((x) => / typ host/.test(x) || x.includes('192.168.7.20'));
  if (privacy === 'public-only' && leaks.length) fail(`public-only retry leaked: ${leaks.join(' | ')}`);
  if (privacy === 'direct' && leaks.length !== 2) fail('direct retry should carry the host candidate (the check above would miss a leak)');
  console.log(`5. ${privacy}: the retry's offer and answer ${privacy === 'direct' ? 'carry' : 'carry no'} host candidate or LAN address`);
  await Promise.all([a.leave(), b.leave()]);
}

// 6. Triggers. A fake navigator.permissions whose microphone status we change.
{
  DirectRetries.firstMs = 60000;
  const statuses = [];
  const nav = globalThis.navigator;
  Object.defineProperty(nav, 'permissions', {
    configurable: true,
    value: {
      query: async ({ name }) => {
        const st = new EventTarget();
        st.name = name;
        st.state = 'prompt';
        statuses.push(st);
        return st;
      },
    },
  });
  const grant = () => {
    for (const st of statuses.filter((s) => s.name === 'microphone')) {
      st.state = 'granted';
      st.dispatchEvent(new Event('change'));
    }
  };
  for (const [what, extra, want] of [['direct', {}, 1], ['retryOnGrant false', { retryOnGrant: false }, 0], ['public-only', { privacy: 'public-only' }, 0]]) {
    statuses.length = 0;
    const [a, b] = await pair(1, extra);
    await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
    grant();
    await sleep(800);
    if (a.transport.retried !== want) fail(`a microphone grant in a ${what} room: ${a.transport.retried} retries, want ${want}`);
    await Promise.all([a.leave(), b.leave()]);
  }
  delete nav.permissions;
  console.log("6. a microphone grant retries in a 'direct' room, not with retryOnGrant false nor in 'public-only'");
  const [a, b] = await pair(3);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  a.onNetwork({ type: 'change' }); // a new speed estimate: same connection type
  await sleep(500);
  if (a.transport.retried) fail("a 'change' with the same connection type retried");
  a.onNetwork({ type: 'online' });
  await until("a retry on 'online'", () => a.transport.retried === 1, 3000);
  await until('the retry failed', () => a.seen.retryFailed === 1, 5000);
  b.netType = 'wifi'; // as if b's last reading was Wi-Fi: now there's no type
  b.onNetwork({ type: 'change' }); // b, the larger id, asks a
  await until('a retry asked for on a new connection type', () => a.transport.retried === 2, 3000);
  console.log("   'online' and a new connection type retry; a 'change' with the same type doesn't");
  // 7. leave() stops them.
  await Promise.all([a.leave(), b.leave()]);
  if (a.retries.peers.size || a.retries.due.size || a.retries.busy) fail('retries left after leave');
  const was = a.transport.retried;
  if (a.retryDirect() !== 0) fail('retryDirect() after leave is not 0');
  await sleep(500);
  if (a.transport.retried !== was) fail('a retry after leave');
  console.log('7. leave() stops the retries');
}

// 8. A forged retry offer (no valid proof of the room key) leaves b's link to a alone.
{
  DirectRetries.firstMs = 60000;
  const [a, b] = await pair(1);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  const old = b.transport.byPeer.get(a.self);
  const bad = b.transport.badSignals;
  const forger = new BroadcastSignaling(a.self, `peerwire/peerwire-test/retry-${process.pid}-${n}`);
  forger.send(b.self, { kind: 'offer', sdp: { type: 'offer', sdp: sdpOf('999') }, p: '00'.repeat(32), retry: true });
  await until('the forged offer refused', () => b.transport.badSignals > bad, 3000);
  forger.close();
  if (b.transport.byPeer.get(a.self) !== old) fail("a retry offer without the room's proof replaced the link");
  console.log("8. a retry offer that doesn't prove the room key is refused, and the link stays");
  await Promise.all([a.leave(), b.leave()]);
}

// 9. A proven retry offer to a pair that is direct: b's open link stays, data flows.
{
  const [a, b] = await pair(0);
  await until('a direct pair', () => routes(a, b).join() === 'direct,direct');
  const old = b.transport.byPeer.get(a.self);
  const p = await a.transport.offerProof.sign(b.self);
  const pc = new (fakePc(`x${n}`))();
  a.transport.signal(b.self, { kind: 'offer', sdp: { type: 'offer', sdp: sdpOf(pc.token) }, p, retry: true });
  await sleep(500);
  const got = [];
  b.on('message', (m) => got.push(m));
  a.send('still here');
  await until('data after the offer', () => got.length === 1, 3000);
  if (b.transport.byPeer.get(a.self) !== old || routes(a, b).join() !== 'direct,direct' || a.seen.leaves || b.seen.leaves) fail(`a retry offer replaced an open link: ${routes(a, b)}`);
  console.log('9. a proven retry offer to a direct pair is ignored: the open link stays');
  await Promise.all([a.leave(), b.leave()]);
}

// 10. A flood of proven retry offers, each with a new description, to the answering side:
// one makes a connection and an answer, the rest are refused until retryAnswerMs is over.
{
  DirectRetries.firstMs = 60000;
  const floor = WebRtcTransport.retryAnswerMs;
  WebRtcTransport.retryAnswerMs = 10000; // well over the flood, however slow the host
  const [a, b] = await pair(1);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  b.transport.fallbackMs = 10000; // the connection b makes lives through the flood (8 s by default)
  const answers = () => sent.filter((s) => s.from === b.self && s.to === a.self && s.body?.kind === 'answer');
  const [made0, answers0, bad0, resent0] = [made.get(b.owner), answers().length, b.transport.badSignals, b.transport.resent];
  const p = await a.transport.offerProof.sign(b.self);
  const offers = [];
  for (let i = 0; i < 20; i++) {
    const pc = new (fakePc(`x${n}`))();
    offers.push({ kind: 'offer', sdp: { type: 'offer', sdp: sdpOf(pc.token) }, p, retry: true });
    a.transport.signal(b.self, offers[i]);
    await sleep(50);
  }
  await until('the flood handled', () => b.transport.badSignals - bad0 + answers().length - answers0 >= 20, 5000);
  await sleep(300);
  const conns = made.get(b.owner) - made0;
  if (conns !== 1 || answers().length - answers0 !== 1) fail(`20 retry offers made ${conns} connections and ${answers().length - answers0} answers, want 1 and 1`);
  if (b.transport.badSignals - bad0 !== 19) fail(`${b.transport.badSignals - bad0} retry offers refused, want 19`);
  // A copy of the offer taken (its answer lost, say) is answered again, with no new connection.
  const before = made.get(b.owner);
  a.transport.signal(b.self, offers[0]);
  await until('the answer sent again', () => b.transport.resent > resent0, 3000);
  if (made.get(b.owner) !== before || answers().length - answers0 !== 2) fail(`a copy of the taken offer: ${made.get(b.owner) - before} connections, ${answers().length - answers0} answers`);
  if (routes(a, b).join() !== 'signaling,signaling' || a.seen.leaves || b.seen.leaves) fail(`after the flood: ${routes(a, b)}, ${JSON.stringify([a.seen, b.seen])}`);
  console.log(`10. 20 proven retry offers 50 ms apart made 1 connection and 1 answer (19 refused); a copy of the one taken was answered again`);
  await Promise.all([a.leave(), b.leave()]);
  WebRtcTransport.retryAnswerMs = floor;
}

// 11. A 'retry' ask sent to the larger id (only the dialling side acts on one): no ask back.
{
  DirectRetries.firstMs = 60000;
  const [a, b] = await pair(1);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  const from = sent.length;
  a.transport.signal(b.self, { kind: 'retry' });
  await sleep(800);
  const back = sent.slice(from).filter((s) => s.from === b.self && s.body?.kind === 'retry');
  if (back.length || a.transport.retried || b.transport.retried) fail(`an ask to the larger id: ${back.length} asks back, retries a ${a.transport.retried}, b ${b.transport.retried}`);
  console.log("11. a 'retry' ask sent to the larger id is ignored: no ask back, no retry");
  await Promise.all([a.leave(), b.leave()]);
}

// 12. A retry whose createOffer throws: its link goes ('retry-failed'), the next goes direct.
{
  DirectRetries.firstMs = 60000;
  const [a, b] = await pair(1);
  await until('a link over signalling', () => routes(a, b).join() === 'signaling,signaling');
  throwOffer.add(a.owner);
  if (a.retryDirect(b.self) !== 1) fail('retryDirect(peer) on a peer over signalling is not 1');
  await until("the retry that couldn't start failed", () => a.seen.retryFailed === 1, 3000);
  if (a.transport.has(b.self)) fail("the retry that couldn't start left its link behind");
  if (a.retryDirect(b.self) !== 1) fail('the second retryDirect(peer) is not 1');
  await until('direct on the next retry', () => routes(a, b).join() === 'direct,direct', 5000);
  if (a.transport.retried !== 2 || a.seen.leaves || b.seen.leaves) fail(`retries ${a.transport.retried}, ${JSON.stringify([a.seen, b.seen])}`);
  console.log("12. a retry whose createOffer threw failed quietly and left no link; the next went direct");
  await Promise.all([a.leave(), b.leave()]);
}

console.log('room retry: all passed');
process.exit(0);
