// @peerwire/react's RoomStore on fake rooms in one process: what members send each other goes through a queue
// that counts it and can lose a message, as room.js can lose the first one to a newcomer. Checks the caps
// (values, shared state in all, each member's per-peer quota, keys a hook reads pushing out keys none reads),
// the hybrid clock (a clock far ahead is ignored, a key can't be stamped out of reach), and catching up (a
// join costs each value once, a lost first message, a member that asks over and over, writes that cross).
// Needs the build: `npm run build` (packages/react).
//
//   node packages/react/test/store.mjs
import { existsSync } from 'node:fs';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
if (!existsSync(new URL('../dist/store.js', import.meta.url))) fail('no dist/: run `npm run build` in packages/react');
const { RoomStore, MAX_KEYS, MAX_VALUE_LENGTH, MAX_STORE_LENGTH, MAX_PEER_KEYS, MAX_PEER_LENGTH, MAX_CLOCK_DRIFT } = await import('../dist/store.js');

const MAX_MESSAGE = 1 << 20; // room.send's limit for a reliable message
const id = (c) => c.repeat(64);
const full = 'x'.repeat(MAX_VALUE_LENGTH - 2); // MAX_VALUE_LENGTH as JSON, with its quotes

// Members of one room. `lose(from, to, m)` true drops a message; `sent` counts what each sends each.
function net() {
  const nodes = new Map();
  let queue = [];
  const n = { lose: null, sent: new Map(), log: [] };
  n.node = (c, { start = true } = {}) => {
    const self = id(c);
    const on = {};
    const room = {
      self,
      peers: [],
      on: (e, fn) => ((on[e] ??= []).push(fn), () => {}),
      getPresence() {},
      send(m, o) {
        const text = JSON.stringify(m);
        if (text.length > (o?.lane === 'unreliable' ? 1200 : MAX_MESSAGE)) throw new RangeError('too large');
        const to = o?.to ? [o.to] : room.peers;
        for (const t of to) queue.push([self, t, text]);
        return to.length;
      },
    };
    const node = { self, room, emit: (e, ...a) => (on[e] ?? []).forEach((f) => f(...a)), st: new RoomStore(room) };
    if (start) node.st.start();
    nodes.set(self, node);
    return node;
  };
  // Raw: a member with no store, whose messages the test sends with `n.from(raw, to, m)`.
  n.raw = (c) => {
    const self = id(c);
    const raw = { self, got: [], room: { peers: [] }, emit: (e, m) => e === 'message' && raw.got.push(m) };
    nodes.set(self, raw);
    return raw;
  };
  n.from = (a, b, m) => queue.push([a.self, b.self, JSON.stringify(m)]);
  n.join = (a, b) => {
    a.room.peers.push(b.self);
    b.room.peers.push(a.self);
    a.emit('peer-join', b.self);
    b.emit('peer-join', a.self);
  };
  n.leave = (a, b) => {
    a.room.peers.splice(a.room.peers.indexOf(b.self), 1);
    b.room.peers.splice(b.room.peers.indexOf(a.self), 1);
    a.emit('peer-leave', b.self);
    b.emit('peer-leave', a.self);
  };
  n.flush = () => {
    while (queue.length) {
      const q = queue;
      queue = [];
      for (const [f, t, text] of q) {
        const m = JSON.parse(text);
        if (n.lose?.(f, t, m)) continue;
        n.sent.set(`${f}>${t}`, (n.sent.get(`${f}>${t}`) ?? 0) + text.length);
        n.log.push([f, t, m]);
        nodes.get(t).emit('message', m, f);
      }
    }
  };
  n.bytes = (a, b) => n.sent.get(`${a.self}>${b.self}`) ?? 0;
  // Shared values a sent b in catch-up messages.
  n.values = (a, b) => n.log.filter(([f, t, m]) => f === a.self && t === b.self && m.$peerwire === 'sync').reduce((k, [, , m]) => k + (m.s?.length ?? 0), 0);
  return n;
}
const read = (node, k) => node.st.getShared(k)?.v;
const throws = (fn) => {
  try {
    fn();
    return null;
  } catch (e) {
    return e;
  }
};

// The caps on shared values: a value over MAX_VALUE_LENGTH is ignored, MAX_STORE_LENGTH in all, replacing a
// value frees room, and a local write past them throws.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  n.from(x, a, { $peerwire: 'set', k: 'over', v: `${full}x`, c: 1 });
  const fit = MAX_STORE_LENGTH / MAX_VALUE_LENGTH;
  for (let i = 0; i <= fit; i++) n.from(x, a, { $peerwire: 'set', k: `b${i}`, v: full, c: 1 });
  n.flush();
  if (read(a, 'over') !== undefined) fail('a value over MAX_VALUE_LENGTH was kept');
  const kept = () => Array.from({ length: fit + 2 }, (_, i) => i).filter((i) => read(a, `b${i}`) !== undefined).length;
  if (kept() !== fit || read(a, `b${fit}`) !== undefined) fail(`kept ${kept()} values of ${MAX_VALUE_LENGTH}, not the first ${fit}`);
  for (const k of ['b0', 'b1']) n.from(x, a, { $peerwire: 'set', k, v: 'small', c: 2 });
  n.from(x, a, { $peerwire: 'set', k: `b${fit}`, v: full, c: 1 });
  n.flush();
  if (read(a, `b${fit}`) === undefined || kept() !== fit + 1) fail('replacing a value did not free room');
  const e = throws(() => a.st.setShared('mine', `${full}x`));
  if (!(e instanceof RangeError)) fail(`a local value over MAX_VALUE_LENGTH: ${e}`);
  console.log(`caps: values over ${MAX_VALUE_LENGTH} ignored, ${fit} of that size fill ${MAX_STORE_LENGTH}, replacing frees room`);
}

// RH-3: a member that fills the store can't keep out the keys the app reads (a hook is subscribed) or writes:
// those push out keys no hook reads, oldest first. Keys nobody reads still wait for room.
{
  const n = net();
  const [a, x, y] = [n.node('a'), n.raw('b'), n.raw('c')];
  const unsub = a.st.subscribe('s:count', () => {});
  const now = Date.now();
  for (let i = 0; i < MAX_KEYS; i += 64) {
    n.from(x, a, { $peerwire: 'sync', s: Array.from({ length: 64 }, (_, j) => [`j${i + j}`, 0, now, x.self]), p: [] });
  }
  n.flush();
  if (read(a, `j${MAX_KEYS - 1}`) !== 0) fail('the junk keys did not fill the store');
  n.from(y, a, { $peerwire: 'set', k: 'count', v: 5, c: now + 1 });
  n.from(y, a, { $peerwire: 'set', k: 'unread', v: 5, c: now + 1 });
  n.flush();
  if (read(a, 'count') !== 5) fail('a key a hook reads was kept out by a full store');
  if (read(a, 'unread') !== undefined) fail('a key no hook reads pushed out another');
  if (read(a, 'j0') !== undefined || read(a, 'j1') !== 0) fail('the oldest unread key was not the one pushed out');
  a.st.setShared('mine', 1);
  if (read(a, 'mine') !== 1 || read(a, 'j1') !== undefined) fail('a local write to a full store');
  // A store full of keys hooks read: a local write past the caps throws, and nothing is sent.
  const b = net().node('d');
  for (let i = 0; i < MAX_KEYS; i++) {
    b.st.subscribe(`s:k${i}`, () => {});
    b.st.setShared(`k${i}`, i);
  }
  const err = throws(() => b.st.setShared('one-more', 1));
  if (!(err instanceof RangeError) || read(b, 'one-more') !== undefined) fail(`a local write past MAX_KEYS: ${err}`);
  unsub();
  console.log(`RH-3 shared: ${MAX_KEYS} junk keys can't keep out a key a hook reads or a local write; past the caps a local write throws`);
}

// RH-3: per-peer values have a quota per member, MAX_PEER_KEYS keys and MAX_PEER_LENGTH in all, so one member
// can't crowd out another; ours has the same quota.
{
  const n = net();
  const [a, x, y] = [n.node('a'), n.raw('b'), n.raw('c')];
  for (let i = 0; i <= MAX_PEER_KEYS; i++) n.from(x, a, { $peerwire: 'live', k: `j${i}`, v: 0, n: 1 });
  n.from(y, a, { $peerwire: 'live', k: 'cursor', v: { x: 1 }, n: 1 });
  n.flush();
  if (a.st.getLive(`j${MAX_PEER_KEYS - 1}`)[x.self] !== 0 || a.st.getLive(`j${MAX_PEER_KEYS}`)[x.self] !== undefined) fail('MAX_PEER_KEYS');
  if (a.st.getLive('cursor')[y.self]?.x !== 1) fail("one member's keys kept out another's");
  const z = n.raw('e');
  const fit = MAX_PEER_LENGTH / MAX_VALUE_LENGTH;
  for (let i = 0; i <= fit; i++) n.from(z, a, { $peerwire: 'live', k: `big${i}`, v: full, n: 1 + i });
  n.from(y, a, { $peerwire: 'live', k: 'big0', v: 'small', n: 2 });
  n.flush();
  if (a.st.getLive(`big${fit - 1}`)[z.self] !== full || a.st.getLive(`big${fit}`)[z.self] !== undefined) fail('MAX_PEER_LENGTH');
  if (a.st.getLive('big0')[y.self] !== 'small') fail("one member's values kept out another's");
  // Replacing a value frees its room.
  for (const k of ['big0', 'big1']) n.from(z, a, { $peerwire: 'live', k, v: 'small', n: 99 });
  n.from(z, a, { $peerwire: 'live', k: `big${fit}`, v: full, n: 100 });
  n.flush();
  if (a.st.getLive(`big${fit}`)[z.self] !== full) fail('replacing a per-peer value did not free room');
  const b = net().node('d');
  for (let i = 0; i < MAX_PEER_KEYS; i++) b.st.setLive(`k${i}`, i);
  if (!(throws(() => b.st.setLive('one-more', 1)) instanceof RangeError)) fail('a local per-peer key past MAX_PEER_KEYS');
  const c = net().node('f');
  for (let i = 0; i < fit; i++) c.st.setLive(`k${i}`, full);
  if (!(throws(() => c.st.setLive('one-more', 1)) instanceof RangeError)) fail('a local per-peer value past MAX_PEER_LENGTH');
  c.st.setLive('k0', 1);
  c.st.setLive('one-more', 1);
  console.log(`RH-3 per-peer: ${MAX_PEER_KEYS} keys and ${MAX_PEER_LENGTH} per member, ours too; one member can't crowd out another`);
}

// RH-1: the clock is per key, wall time or one past the value it replaces. A clock further ahead than
// MAX_CLOCK_DRIFT is ignored, so nothing can stamp a key out of reach, and no write throws for its clock.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  n.join(a, x);
  n.from(x, a, { $peerwire: 'set', k: 'zz', v: 'max', c: Number.MAX_SAFE_INTEGER });
  n.from(x, a, { $peerwire: 'set', k: 'zz', v: 'far', c: Date.now() + MAX_CLOCK_DRIFT + 60_000 });
  n.flush();
  if (read(a, 'zz') !== undefined) fail(`a clock past MAX_CLOCK_DRIFT took: ${read(a, 'zz')}`);
  const pin = Date.now() + MAX_CLOCK_DRIFT - 1000;
  n.from(x, a, { $peerwire: 'set', k: 'zz', v: 'ahead', c: pin });
  n.flush();
  if (read(a, 'zz') !== 'ahead') fail('a clock within MAX_CLOCK_DRIFT was refused');
  const before = Date.now();
  a.st.setShared('count', 1);
  a.st.setShared('count', 2);
  const sets = n.log.length;
  n.flush();
  const stamps = n.log.slice(sets).filter(([f, , m]) => f === a.self && m.$peerwire === 'set').map(([, , m]) => m.c);
  if (stamps.length !== 2 || stamps[0] < before || stamps[1] <= stamps[0] || stamps[1] > Date.now() + 1) fail(`wall-time stamps: ${stamps}`);
  a.st.setShared('zz', 'ours');
  n.flush();
  const over = n.log.findLast(([f, , m]) => f === a.self && m.k === 'zz');
  if (read(a, 'zz') !== 'ours' || over?.[2].c !== pin + 1) fail(`a write over a clock ahead: ${JSON.stringify(over?.[2])}`);
  // A joiner catches up on the key stamped ahead, from a, and writes over it.
  const y = n.node('c');
  n.join(a, y);
  n.flush();
  if (read(y, 'zz') !== 'ours' || read(y, 'count') !== 2) fail('catch-up of a key stamped ahead');
  y.st.setShared('zz', 'y');
  y.st.setShared('new', 1);
  n.flush();
  if (read(a, 'zz') !== 'y' || read(a, 'new') !== 1) fail("a joiner's writes after the catch-up");
  // A member whose clock is behind still writes over the current value: one past it.
  n.from(x, a, { $peerwire: 'set', k: 'new', v: 'behind', c: 5 });
  n.from(x, a, { $peerwire: 'set', k: 'other', v: 'behind', c: 5 });
  n.flush();
  if (read(a, 'new') !== 1 || read(a, 'other') !== 'behind') fail('a write stamped behind');
  console.log(`RH-1: clocks past now + ${MAX_CLOCK_DRIFT} ms ignored (MAX_SAFE_INTEGER too), a key stamped ahead is written over, no throw`);
}

// RH-4: a join costs each value once, both ways at once, with a digest of versions first.
{
  const n = net();
  const [a, b] = [n.node('a'), n.node('b')];
  for (let i = 0; i < 100; i++) a.st.setShared(`k${i}`, 'v'.repeat(1000));
  b.st.setShared('mine', 'b');
  b.st.setLive('cursor', { x: 1 });
  a.st.setLive('cursor', { x: 2 });
  n.flush();
  n.join(a, b);
  n.flush();
  const state = 100 * 1002;
  if (read(b, 'k99') !== 'v'.repeat(1000) || read(a, 'mine') !== 'b') fail('the join did not catch up both ways');
  if (b.st.getLive('cursor')[a.self]?.x !== 2 || a.st.getLive('cursor')[b.self]?.x !== 1) fail('per-peer values on the join');
  if (n.values(a, b) !== 100 || n.values(b, a) !== 1) fail(`values sent: ${n.values(a, b)} and ${n.values(b, a)}, not 100 and 1`);
  const ratio = n.bytes(a, b) / state;
  // Each value once, with its version (a clock and a 64-hex writer id) in the digest and again beside it.
  if (ratio > 1.25) fail(`a sent ${n.bytes(a, b)} B for ${state} B of state (${ratio.toFixed(2)}x)`);
  console.log(`RH-4: a join sent ${n.bytes(a, b)} B for ${state} B of shared state (${ratio.toFixed(2)}x), each value once`);

  // The first message a sends the newcomer is lost (room.js lists it before our presence goes): both still
  // catch up, from the other's ask and the ask back. With per-peer values alone, nothing in a digest says
  // what is missing: the answer asks back because the other hasn't answered.
  for (const [losing, shared] of [['a', true], ['c', true], ['a', false], ['c', false]]) {
    const m = net();
    const [p, q] = [m.node('a'), m.node('c')];
    if (shared) p.st.setShared('from-a', 1);
    if (shared) q.st.setShared('from-c', 2);
    p.st.setLive('cursor', 'a');
    q.st.setLive('cursor', 'c');
    m.flush();
    let first = true;
    m.lose = (f) => f === id(losing) && first && !(first = false);
    m.join(p, q);
    m.flush();
    if (first) fail('nothing was lost');
    if (shared && (read(p, 'from-c') !== 2 || read(q, 'from-a') !== 1)) fail(`lost first message from ${losing}: shared state`);
    if (p.st.getLive('cursor')[q.self] !== 'c' || q.st.getLive('cursor')[p.self] !== 'a') fail(`lost first message from ${losing}: per-peer`);
  }
  console.log("RH-4: a newcomer's lost first message, either way, with and without shared state: both catch up");
}

// RH-4: a member that asks over and over gets each value once, and at most 4 answers.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  for (let i = 0; i < 100; i++) a.st.setShared(`k${i}`, 'v'.repeat(1000));
  n.join(a, x);
  n.flush();
  x.got.length = 0;
  for (let i = 0; i < 20; i++) n.from(x, a, { $peerwire: 'sync', ask: true, h: [['wanted', Date.now(), x.self]] });
  n.flush();
  const answers = x.got.filter((m) => m.$peerwire === 'sync' && m.ask === true).length;
  if (n.values(a, x) !== 100 || answers > 4) fail(`20 asks: ${n.values(a, x)} values in ${answers} answers`);
  console.log(`RH-4: 20 asks: 100 values sent once, ${answers} answers`);
}

// Writes that cross: both members hold different versions of the same keys, and settle on the same ones.
// A member whose store starts late catches up when it starts.
{
  const n = net();
  const [a, b] = [n.node('a'), n.node('b', { start: false })];
  a.st.setShared('k', 'a');
  b.st.setShared('k', 'b');
  b.st.setShared('j', 'b');
  for (const t = Date.now() + 2; Date.now() < t; );
  a.st.setShared('j', 'a');
  n.flush();
  n.join(a, b);
  n.flush();
  if (read(a, 'k') !== 'a' || read(b, 'k') !== 'b') fail('a store that has not started answered');
  b.st.start();
  n.flush();
  for (const k of ['k', 'j']) if (read(a, k) !== read(b, k)) fail(`${k}: ${read(a, k)} and ${read(b, k)}`);
  if (read(a, 'j') !== 'a') fail('the later write did not win');
  console.log('crossing writes settle alike; a store that starts late catches up');
}

// Malformed catch-ups are ignored whole or entry by entry.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  n.join(a, x);
  const now = Date.now();
  for (const m of [
    { $peerwire: 'sync', s: 'nope' },
    { $peerwire: 'sync', s: null },
    { $peerwire: 'sync', h: new Array(MAX_KEYS + 1).fill(['k', now, x.self]) },
    { $peerwire: 'sync', s: [[1, 2, 3], null, ['k', 1, now, 'not a peer id'], ['k', 1, 'x', x.self], ['k', 1, now + MAX_CLOCK_DRIFT * 2, x.self]] },
    { $peerwire: 'sync', h: [null, ['k'], ['k', 'x', x.self], [{}, now, x.self]], p: [null, [1], ['cursor', { x: 9 }, 0]] },
  ]) {
    n.from(x, a, m);
  }
  n.flush();
  if (read(a, 'k') !== undefined || a.st.getLive('cursor')[x.self] !== undefined) fail('a malformed entry took');
  n.from(x, a, { $peerwire: 'sync', s: [['k', 1, now, x.self]], p: [['cursor', { x: 9 }, 1]] });
  n.flush();
  if (read(a, 'k') !== 1 || a.st.getLive('cursor')[x.self]?.x !== 9) fail('a valid catch-up after malformed ones');
  console.log('malformed catch-ups ignored');
}

console.log('ok');
