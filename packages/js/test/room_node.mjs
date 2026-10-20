// joinRoom in Node: three peers in one process meet over BroadcastChannel and,
// with no WebRTC here, send datagrams over it too (LocalTransport). Checks
// joins, presence, every message type, targeted sends, leaving, and the
// pull-style poll()/receive() path.
//
//   node packages/js/test/room_node.mjs
import { joinRoom } from '../src/index.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
const until = async (what, cond, ms = 10000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 10));
  }
};
const opts = { appId: 'nobaas-test', roomId: `node-${process.pid}` };
const t0 = performance.now();
const [a, b, c] = await Promise.all(['ana', 'bo', 'cy'].map((name) => joinRoom({ ...opts, presence: { name } })));
const got = new Map([a, b, c].map((r) => [r, []]));
const left = [];
for (const r of [a, b, c]) {
  r.on('message', (data, from, info) => got.get(r).push({ data, from, lane: info.lane }));
  r.on('peer-leave', (peer, reason) => left.push([r.self, peer, reason]));
}
await until('a full mesh', () => [a, b, c].every((r) => r.peers.length === 2));
console.log(`3 peers joined a full mesh in ${(performance.now() - t0).toFixed(0)} ms`);
if (a.getPresence(b.self)?.name !== 'bo' || c.getPresence(a.self)?.name !== 'ana') fail('presence on join');

// Other rooms of the same app stay apart.
const other = await joinRoom({ ...opts, roomId: `${opts.roomId}-other` });
await new Promise((r) => setTimeout(r, 200));
if (other.peers.length !== 0 || a.peers.length !== 2) fail('rooms leaked into each other');
await other.leave();

if (a.send('hello') !== 2) fail('broadcast count');
a.send({ x: 1, y: [2, 3] });
a.send(Uint8Array.of(1, 2, 3), { to: c.self });
a.send('tick', { lane: 'unreliable' });
await until('messages', () => got.get(b).length === 3 && got.get(c).length === 4);
const [m1, m2, m3] = got.get(b);
if (m1.data !== 'hello' || m1.from !== a.self || m1.lane !== 'reliable') fail(`text ${JSON.stringify(m1)}`);
if (m2.data.y[1] !== 3) fail('json');
if (m3.data !== 'tick' || m3.lane !== 'unreliable') fail('unreliable lane');
const bytes = got.get(c)[2].data;
if (!(bytes instanceof Uint8Array) || bytes.join() !== '1,2,3') fail('bytes, or targeted send');

// A burst bigger than the send window queues instead of failing.
const burst = 3000;
for (let i = 0; i < burst; i++) b.send(`m${i}`, { to: a.self });
await until('the burst', () => got.get(a).length === burst);
if (got.get(a).some((m, i) => m.data !== `m${i}`)) fail('burst order');
console.log(`burst of ${burst} reliable messages arrived in order`);

let seen;
c.on('presence', (peer, p) => (seen = [peer, p]));
b.setPresence({ name: 'bo', typing: true });
await until('presence update', () => seen);
if (seen[0] !== b.self || !seen[1].typing) fail('presence update');

try {
  a.send(new Uint8Array(4000), { lane: 'unreliable' });
  fail('oversized unreliable send did not throw');
} catch (e) {
  if (!(e instanceof RangeError)) throw e;
}

// Pull-style, as Godot uses it: plain events from poll()/receive(), sendTo().
const q = await joinRoom({ ...opts, presence: { name: 'gd' }, queue: true });
await until('pull peer to join', () => q.peers.length === 3 && [a, b, c].every((r) => r.peers.includes(q.self)));
const joins = q.poll().filter((e) => e.type === 'peer-join');
if (joins.length !== 3 || !joins.every((e) => typeof e.peer === 'string' && e.presence.name)) fail(`pull joins ${JSON.stringify(joins)}`);
if (q.receive() !== null) fail('empty receive');
a.sendTo(q.self, 'to godot');
a.sendTo(q.self, Uint8Array.of(9, 8), 'unreliable');
b.sendTo(null, { hp: 3 });
await until('pulled messages', () => q.queue.length >= 3);
const pulled = [q.receive(), ...q.poll()];
const [p1, p2, p3] = pulled;
if (p1.data !== 'to godot' || p1.kind !== 'text' || p1.peer !== a.self) fail(`pull text ${JSON.stringify(p1)}`);
if (!(p2.data instanceof ArrayBuffer) || new Uint8Array(p2.data).join() !== '9,8' || p2.lane !== 'unreliable') fail('pull bytes');
if (p3.kind !== 'json' || p3.data.hp !== 3 || p3.peer !== b.self) fail('pull json');
await q.leave();
await until('pull peer to leave', () => a.peers.length === 2);
console.log('pull-style poll/receive/sendTo: ok');

await c.leave();
await until('peer-leave', () => a.peers.length === 1 && b.peers.length === 1);
if (!left.some(([who, peer, reason]) => who === a.self && peer === c.self && reason === 'left')) fail(`leave reason ${JSON.stringify(left)}`);
await a.leave();
await b.leave();
console.log('room_node: presence, text/json/bytes, lanes, targeted send, backpressure, leave: ok');
