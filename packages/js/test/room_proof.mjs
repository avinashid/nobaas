// NB-037: knowing a room's topic is not enough to join it. An attacker on the
// room's signalling channel (what a relay sees: hellos and sealed messages) but
// without the room name gets no link and no presence:
// - its hellos (none, forged, Rust UDP ones, another room's) make no one dial it;
// - its WebRTC offers without a room proof get no answer and no RTCPeerConnection;
// - a Nobaas link it dials itself gets only the room's link proof, never
//   presence or messages; frames before a proof are ignored, a wrong proof drops
//   the link at once and no proof drops it after Room.proofMs.
// Also checks the hello proof against an independent HMAC built the way
// traversal.rs builds Rust's, so the two stay compatible.
//
//   node packages/js/test/room_proof.mjs
import { createHash, createHmac } from 'node:crypto';
import { joinRoom, Room, loadNobaas, BroadcastSignaling, LocalTransport, toHex } from '../src/index.js';
import { RoomProof, randomRoomId } from '../src/proof.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
process.on('unhandledRejection', (e) => fail(`unhandled: ${e?.message ?? e}`));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, cond, ms = 10000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await sleep(10);
  }
};

// 1. The hello proof is Rust's: HMAC-SHA256 under sha256("nobaas/room-key/<app>/<room>")
//    over "nb-hello", the 32-byte id, the role byte and ts as a little-endian u64.
const name = 'nobaas-test/proof';
const proof = await RoomProof.create(name);
const id = 'ab'.repeat(32);
const ts = 1760000000123;
const key = createHash('sha256').update(`nobaas/room-key/${name}`).digest();
const ts8 = Buffer.alloc(8);
ts8.writeBigUInt64LE(BigInt(ts));
const want = createHmac('sha256', key).update(Buffer.concat([Buffer.from('nb-hello'), Buffer.from(id, 'hex'), Buffer.of(0), ts8])).digest('hex');
const meta = await proof.hello(id, 'peer', ts);
if (meta.p !== want || meta.ts !== ts || meta.role !== 'peer') fail(`hello proof ${JSON.stringify(meta)} != ${want}`);
if (!(await proof.checkHello(id, meta, ts + 1000))) fail('a fresh hello did not verify');
const other = await RoomProof.create('nobaas-test/proof-other');
const bad = [
  ['stale', id, meta, ts + 61000],
  ['from the future', id, meta, ts - 61000],
  ['another room', id, await other.hello(id, 'peer', ts), ts],
  ['another sender', 'cd'.repeat(32), meta, ts],
  ['another role', id, { ...meta, role: 'host' }, ts],
  ['another time', id, { ...meta, ts: ts + 1 }, ts],
  ['no proof', id, { role: 'peer', ts }, ts],
  ['no meta', id, undefined, ts],
  ['a bad id', 'zz', meta, ts],
];
for (const [what, from, m, now] of bad) if (await proof.checkHello(from, m, now)) fail(`a hello with ${what} verified`);
if (randomRoomId().length !== 22 || randomRoomId() === randomRoomId()) fail('randomRoomId');
console.log('hello proof matches the Rust construction; stale, foreign and forged hellos fail');

// 2. Two members and an attacker that knows only the topic: in Node, 'local'
//    rooms use BroadcastChannel `nobaas/<app>/<room>` for hellos and datagrams.
Room.proofMs = 500;
const opts = { appId: 'nobaas-test', roomId: `proof-${process.pid}` };
const nb = await loadNobaas(new URL('../pkg/nobaas_ffi.wasm', import.meta.url));
// The larger id never dials ('smaller id dials'), so make the members dial: if a
// forged hello were believed, they would dial the attacker.
let ep = nb.endpoint();
while (!ep.id.startsWith('ff')) {
  ep.close();
  ep = nb.endpoint();
}
const sig = new BroadcastSignaling(ep.id, `nobaas/${opts.appId}/${opts.roomId}`);
const seen = new Set(); // member ids, from their hellos
sig.onHello = (from) => seen.add(from);
const got = []; // what members sent the attacker
let connected = 0;
ep.onEvent = (e) => {
  if (e.type === 'connected') connected++;
  if (e.type === 'message') got.push(e.data[0]);
};
const lt = new LocalTransport(ep, sig);
// Hellos that don't prove the room key: none, forged, a Rust UDP peer's, another room's.
sig.hello();
sig.hello({ role: 'peer', ts: Date.now(), p: '00'.repeat(32) });
sig.hello({ nb: 1, role: 'peer', ts: Date.now(), p: '00'.repeat(32) });
sig.hello(await other.hello(ep.id));

const [a, b] = await Promise.all(['ana', 'bo'].map((n) => joinRoom({ ...opts, presence: { name: n } })));
const joins = [];
for (const r of [a, b]) r.on('peer-join', (peer) => joins.push([r.self, peer]));
await until('the two members to mesh', () => a.peers.length === 1 && b.peers.length === 1);
for (let i = 0; i < 3; i++) {
  sig.hello({ role: 'peer', ts: Date.now(), p: '11'.repeat(32) });
  await sleep(100);
}
if (connected) fail('a member dialled the attacker on an unproven hello');
if (!seen.has(a.self) || !seen.has(b.self)) fail('the attacker should see hellos (it knows the topic)');

// The attacker dials a member itself: the Noise link comes up (ids are public),
// but the room gives it only the link proof, and drops it without one.
const T = { Text: 1, Presence: 3, Proof: 5 };
lt.dial(a.self);
await until('the attacker link', () => connected === 1);
ep.send(a.self, Uint8Array.of(T.Presence, ...new TextEncoder().encode('{"name":"mallory"}')));
ep.send(a.self, Uint8Array.of(T.Text, 104, 105));
await until('a to drop the unproven link', () => !a.linked.has(ep.id), 3000);
// A wrong proof drops the link at once.
lt.drop(a.self);
const before = connected;
lt.dial(b.self);
await until('the second attacker link', () => connected === before + 1);
const t0 = Date.now();
ep.send(b.self, Uint8Array.of(T.Proof, ...new Uint8Array(32)));
await until('b to drop the bad proof', () => !b.linked.has(ep.id), 3000);
if (Date.now() - t0 > 400) fail('a wrong proof should drop the link before the deadline');
await sleep(200);
a.send('secret');
b.setPresence({ name: 'bo', typing: true });
await sleep(200);
if (got.some((t) => t !== T.Proof)) fail(`the attacker got room frames: tags ${got}`);
if (joins.some(([, p]) => p === ep.id) || a.peers.length !== 1 || b.peers.length !== 1) fail('the attacker joined');
if (a.getPresence(ep.id) || b.getPresence(ep.id)) fail('the attacker has presence');
console.log(`attacker with only the topic: no dial, ${got.length} link proofs and nothing else, links dropped; members still meshed`);

// 3. WebRTC: an offer without a valid room proof gets no RTCPeerConnection and no answer.
let pcs = 0;
class CountingPc {
  constructor() {
    pcs++;
    throw new Error('no RTCPeerConnection should be made for an unproven offer');
  }
}
const rtcOpts = { appId: 'nobaas-test', roomId: `proof-rtc-${process.pid}` };
const victim = await joinRoom({ ...rtcOpts, RTCPeerConnection: CountingPc });
const sig2 = new BroadcastSignaling(ep.id, `nobaas/${rtcOpts.appId}/${rtcOpts.roomId}`);
const answers = [];
sig2.onMessage((from, msg) => answers.push(msg.kind));
const sdp = { type: 'offer', sdp: 'v=0\r\n' };
sig2.send(victim.self, { kind: 'offer', sdp });
sig2.send(victim.self, { kind: 'offer', sdp, p: '00'.repeat(32) });
sig2.send(victim.self, { kind: 'offer', sdp, p: await other.pair('nb-rtc-offer', ep.id, victim.self) });
sig2.send(victim.self, { kind: 'ice', candidate: {} });
await sleep(300);
if (pcs || answers.length) fail(`unproven offers: ${pcs} RTCPeerConnections, answers ${answers}`);
console.log('WebRTC offers without the room proof: no RTCPeerConnection, no answer');

sig2.close();
lt.close();
ep.close();
await Promise.all([a.leave(), b.leave(), victim.leave()]);
console.log('room_proof: ok');
process.exit(0);
