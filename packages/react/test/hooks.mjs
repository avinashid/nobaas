// @peerwire/react in Node: React components rendered with react-dom into jsdom, members of one room over
// BroadcastChannel (signaling 'local'). Checks server rendering (initial values, no room), joining under StrictMode with no ghost member, presence both
// ways, messages and their limit, shared state (concurrent writes converge, a late joiner catches up), per-peer
// values, leaving, switching rooms, a failed join, a member that sends malformed or flooding state messages, the
// value and memory caps and the limit on catch-up answers, and `accept` keeping a member's wrongly typed
// presence, messages, shared and per-peer values out.
// Needs the build: `npm run build` (packages/react) and packages/js/build.sh.
//
//   node packages/react/test/hooks.mjs
import { existsSync } from 'node:fs';
import { JSDOM } from 'jsdom';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
process.on('uncaughtException', (e) => fail(`uncaught: ${e?.stack ?? e}`));
process.on('unhandledRejection', (e) => fail(`unhandled rejection: ${e?.stack ?? e}`));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, cond, ms = 10000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await sleep(10);
  }
};

if (!existsSync(new URL('../dist/index.js', import.meta.url))) fail('no dist/: run `npm run build` in packages/react');
const { createElement: h, StrictMode, useState } = await import('react');
const { renderToString } = await import('react-dom/server');
const { joinRoom } = await import('@peerwire/js');
const lib = await import('../dist/index.js');
const { RoomProvider, useRoom, usePeers, usePresence, useMessages, useRoomEvent, useSharedState, usePeerState, usePeerwireRoom } = lib;

const appId = 'peerwire-react-test';
const roomId = `hooks-${process.pid}`;

// The store's memory caps, on a fake room: a value over MAX_VALUE_LENGTH is ignored, values past
// MAX_STORE_LENGTH in all are ignored, replacing or dropping values frees room, and a local write over the cap throws.
{
  const { RoomStore, MAX_VALUE_LENGTH, MAX_STORE_LENGTH } = await import('../dist/store.js');
  const on = {};
  const fake = { self: 'a'.repeat(64), peers: [], on: (e, fn) => ((on[e] = fn), () => {}), getPresence() {}, send() {} };
  const st = new RoomStore(fake);
  const x = 'b'.repeat(64);
  const full = 'x'.repeat(MAX_VALUE_LENGTH - 2); // MAX_VALUE_LENGTH as JSON, with its quotes
  on.message({ $peerwire: 'set', k: 'over', v: `${full}x`, c: 1 }, x);
  if (st.getShared('over')) fail('a value over MAX_VALUE_LENGTH was kept');
  for (let i = 0; i < 300; i++) on.message({ $peerwire: 'set', k: `b${i}`, v: full, c: 1 }, x);
  const kept = () => Array.from({ length: 301 }, (_, i) => i).filter((i) => st.getShared(`b${i}`)).length;
  const fit = MAX_STORE_LENGTH / MAX_VALUE_LENGTH;
  if (kept() !== fit) fail(`kept ${kept()} values of ${MAX_VALUE_LENGTH}, not ${fit}`);
  for (const k of ['b0', 'b1']) on.message({ $peerwire: 'set', k, v: 'small', c: 2 }, x);
  on.message({ $peerwire: 'set', k: 'b300', v: full, c: 1 }, x);
  if (!st.getShared('b300') || kept() !== fit + 1) fail('replacing values did not free room');
  on.message({ $peerwire: 'set', k: 'b301', v: full, c: 1 }, x);
  if (st.getShared('b301')) fail('a value past MAX_STORE_LENGTH was kept');
  // Per-peer values count too, and a member that leaves frees its own.
  const y = 'c'.repeat(64);
  on.message({ $peerwire: 'live', k: 'p0', v: full, n: 1 }, y);
  if (st.getLive('p0')[y]) fail('a per-peer value past MAX_STORE_LENGTH was kept');
  for (const k of ['b2', 'b3']) on.message({ $peerwire: 'set', k, v: 'small', c: 2 }, x);
  for (const k of ['p1', 'p2']) on.message({ $peerwire: 'live', k, v: full, n: 1 }, y);
  if (!st.getLive('p1')[y] || !st.getLive('p2')[y]) fail('per-peer values in freed room were not kept');
  on.message({ $peerwire: 'set', k: 'b301', v: full, c: 1 }, x);
  if (st.getShared('b301')) fail('a value past MAX_STORE_LENGTH was kept, with per-peer values');
  on['peer-leave'](y);
  on.message({ $peerwire: 'set', k: 'b301', v: full, c: 1 }, x);
  if (!st.getShared('b301') || st.getLive('p1')[y]) fail('a member that left still holds room');
  try {
    st.setShared('mine', `${full}x`);
    fail('a local value over MAX_VALUE_LENGTH was sent');
  } catch (e) {
    if (!(e instanceof RangeError)) throw e;
  }
  console.log(`store caps: values over ${MAX_VALUE_LENGTH} ignored, ${fit} of that size fill ${MAX_STORE_LENGTH}, replacing frees room`);
}

// Server rendering, before there is any DOM: every hook renders its initial value and nothing joins.
function OnServer() {
  const { status } = useRoom();
  const peers = usePeers();
  const [me] = usePresence();
  const { messages } = useMessages();
  const [count] = useSharedState('count', 7);
  const [cursors] = usePeerState('cursor');
  useRoomEvent('message', () => {});
  return h('p', null, `${status} ${peers.length} ${me} ${messages.length} ${count} ${Object.keys(cursors).length}`);
}
const html = renderToString(h(RoomProvider, { appId, roomId }, h(OnServer)));
if (html !== '<p>joining 0 undefined 0 7 0</p>') fail(`server render: ${html}`);
console.log('server render: initial values, no room');

const dom = new JSDOM('<!doctype html><html><body></body></html>');
globalThis.window = dom.window;
globalThis.document = dom.window.document;
// Only what Node lacks: jsdom's Event would break Node's own BroadcastChannel.
for (const k of ['HTMLElement', 'HTMLIFrameElement', 'Node', 'Element']) globalThis[k] ??= dom.window[k];
const { createRoot } = await import('react-dom/client');

// One member: a component that puts what its hooks return in `out`, and their setters.
function Probe({ out }) {
  const { room, status } = useRoom();
  const peers = usePeers();
  const named = usePeers({ accept: (p) => typeof p?.name === 'string' });
  const [me, setMe] = usePresence();
  const { messages, send } = useMessages({ limit: 5 });
  const { messages: texts } = useMessages({ accept: (d) => typeof d === 'string' });
  const [count, setCount] = useSharedState('count', 0);
  const [late] = useSharedState('k1099', 'none');
  const [big] = useSharedState('big', '');
  const [cursors, setCursor] = usePeerState('cursor');
  const [points] = usePeerState('cursor', { accept: (v) => typeof v?.x === 'number' });
  const [tick, setTick] = useState(0);
  // A new handler every render: the latest one must be called.
  useRoomEvent('message', (data) => out.raw.push([tick, data]));
  out.maxPeers = Math.max(out.maxPeers ?? 0, peers.length);
  Object.assign(out, { room, status, peers, named, me, messages, texts, count, late, big, cursors, points });
  Object.assign(out, { setMe, send, setCount, setCursor, setTick, tick });
  return null;
}

const members = {};
function mount(name, { strict = false, presence = { name } } = {}) {
  const out = { raw: [] };
  const root = createRoot(document.body.appendChild(document.createElement('div')));
  const render = (p) => {
    const tree = h(RoomProvider, { appId, roomId, presence: p }, h(Probe, { out }));
    root.render(strict ? h(StrictMode, null, tree) : tree);
  };
  render(presence);
  return (members[name] = { name, out, root, render });
}
const id = (m) => m.out.room?.self;
const names = (m) => m.out.peers.map((p) => p.presence?.name).sort().join(',');

// Join: A under StrictMode (mounted twice in development), B and C plain.
const t0 = performance.now();
const [A, B, C] = [mount('ana', { strict: true }), mount('bo'), mount('cy')];
await until('a first render', () => A.out.status);
if (A.out.status !== 'joining' || A.out.room !== null) fail(`first render: ${A.out.status}`);
await until('three members in a full mesh', () => [A, B, C].every((m) => m.out.status === 'joined' && m.out.peers.length === 2));
console.log(`3 React members joined in ${(performance.now() - t0).toFixed(0)} ms`);
if (names(A) !== 'bo,cy' || names(B) !== 'ana,cy' || names(C) !== 'ana,bo') fail(`presence on join: ${names(A)} / ${names(B)} / ${names(C)}`);
if (A.out.me?.name !== 'ana') fail('our presence');
await sleep(500);
for (const m of [A, B, C]) {
  const others = [A, B, C].filter((o) => o !== m).map(id).sort().join();
  if (m.out.peers.map((p) => p.id).sort().join() !== others) fail(`${m.name} lists a member that isn't one`);
  if (m.out.maxPeers !== 2) fail(`${m.name} saw ${m.out.maxPeers} members at once: a ghost from StrictMode's first mount`);
}
console.log('StrictMode: no ghost member');

// Presence: the setter, a functional update, and RoomProvider's prop.
A.out.setMe((p) => ({ ...p, typing: true }));
await until('presence from the setter', () => B.out.peers.find((p) => p.id === id(A))?.presence?.typing === true && A.out.me?.typing === true);
B.render({ name: 'bo2' });
await until('presence from the prop', () => names(A) === 'bo2,cy' && names(C) === 'ana,bo2');
console.log('presence: setter, functional update and prop');

// Messages, our own echoed with mine: true, and the limit of 5.
B.out.send('hi');
B.out.send({ x: 1 });
await until('messages', () => [A, C].every((m) => m.out.messages.length === 2));
const [m1, m2] = A.out.messages;
if (m1.data !== 'hi' || m1.peer !== id(B) || m1.kind !== 'text' || m1.mine || m2.data.x !== 1 || m2.kind !== 'json') fail(`message ${JSON.stringify([m1, m2])}`);
const mine = B.out.messages;
if (mine.length !== 2 || !mine.every((m) => m.mine && m.peer === id(B))) fail('our own messages');
for (let i = 0; i < 7; i++) C.out.send(`n${i}`);
await until('the limit', () => A.out.messages.at(-1)?.data === 'n6');
if (A.out.messages.length !== 5 || A.out.messages[0].data !== 'n2') fail(`limit: ${A.out.messages.map((m) => m.data)}`);
if (new Set(A.out.messages.map((m) => m.id)).size !== 5) fail('message ids repeat');
A.out.setTick(1);
await until('a rerender', () => A.out.tick === 1);
B.out.send('after');
await until('the latest handler', () => A.out.raw.some(([t, d]) => t === 1 && d === 'after'));
console.log('messages: echo, limit, ids, latest handler');

// Shared state: a write reaches everyone; two at once converge; a functional update builds on the winner.
A.out.setCount(1);
await until('a shared write', () => [A, B, C].every((m) => m.out.count === 1));
B.out.setCount(10);
C.out.setCount(20);
await until('concurrent writes to converge', () => new Set([A, B, C].map((m) => m.out.count)).size === 1 && A.out.count !== 1);
const won = A.out.count;
A.out.setCount((c) => c + 1);
await until('a functional update', () => [A, B, C].every((m) => m.out.count === won + 1));
console.log(`shared state: concurrent 10 and 20 settled on ${won} everywhere, then ${won + 1}`);

// Per-peer values: a burst of cursor moves; the last one is what everyone keeps.
for (let x = 1; x <= 20; x++) A.out.setCursor({ x });
await until('the last cursor value', () => [B, C].every((m) => m.out.cursors[id(A)]?.x === 20));
if (id(A) in A.out.cursors) fail('our own value is in the record');
B.out.setCursor({ x: -5 });
await until('a second member', () => A.out.cursors[id(B)]?.x === -5 && C.out.cursors[id(B)]?.x === -5);
console.log('per-peer values: the last of a burst arrives, ours stays out');

// None of the state messages reached the apps' handlers or message lists.
for (const m of [A, B, C]) {
  if (m.out.raw.some(([, d]) => typeof d === 'object' && d?.$peerwire)) fail(`${m.name}: a state message reached useRoomEvent`);
  if (m.out.messages.some((x) => typeof x.data === 'object' && x.data?.$peerwire)) fail(`${m.name}: a state message reached useMessages`);
}

// A late joiner catches up on shared state and per-peer values.
const D = mount('di');
await until('the late joiner', () => D.out.peers?.length === 3 && [A, B, C].every((m) => m.out.peers.length === 3));
await until('the late joiner to catch up', () => D.out.count === won + 1 && D.out.cursors[id(A)]?.x === 20 && D.out.cursors[id(B)]?.x === -5);
console.log('late joiner: shared state and per-peer values');

// A member outside React that sends malformed state messages, then a flood of keys, then a valid write.
const E = await joinRoom({ appId, roomId, presence: { name: 'eve' } });
await until('the raw member', () => E.peers.length === 4 && D.out.peers.length === 4);
const bad = [
  { $peerwire: 'set', k: 'count', v: 999, c: 'x' },
  { $peerwire: 'set', k: 5, v: 999, c: 1e9 },
  { $peerwire: 'set', k: 'x'.repeat(300), v: 1, c: 1 },
  { $peerwire: 'set', k: 'count', v: 999, c: Number.MAX_SAFE_INTEGER + 2 },
  { $peerwire: 'set', k: 'count', v: 999, c: -1 },
  { $peerwire: 'set', k: 'count', v: 999, c: 1.5 },
  { $peerwire: 'sync', s: 'nope', p: [] },
  { $peerwire: 'sync', s: [[1, 2, 3], null, ['count', 999, 1e9, 'not a peer id']], p: [null, [1], ['cursor', { x: 9 }, 0]] },
  { $peerwire: 'sync', s: new Array(1025).fill(['k', 1, 1, E.self]), p: [] },
  { $peerwire: 'live', k: 'cursor', v: { x: 999 }, n: 0 },
  { $peerwire: 'live', k: 'cursor', v: { x: 999 }, n: 'x' },
  { $peerwire: 'nonsense' },
  { $peerwire: 'sync', ask: true },
];
for (const m of bad) E.send(m);
E.send({ $peerwire: 'live', k: 'cursor', v: { x: 7 }, n: 5 });
await until('the valid value after the malformed ones', () => A.out.cursors[E.self]?.x === 7);
if ([A, B, C, D].some((m) => m.out.count !== won + 1)) fail(`a malformed write took: ${[A, B, C, D].map((m) => m.out.count)}`);
// A value over MAX_VALUE_LENGTH is ignored; a smaller one for the key takes.
E.send({ $peerwire: 'set', k: 'big', v: 'x'.repeat(lib.MAX_VALUE_LENGTH), c: 1 });
E.send({ $peerwire: 'set', k: 'big', v: 'small', c: 2 });
await until('the small value', () => [A, B, C, D].every((m) => m.out.big === 'small'));
// Asking for a catch-up again and again: each member answers at most 4 times while E stays.
const answers = new Map();
E.on('message', (d, from) => {
  if (d?.$peerwire === 'sync' && d.ask === false && d.s.some((e) => e[0] === 'count')) answers.set(from, (answers.get(from) ?? 0) + 1);
});
for (let i = 0; i < 20; i++) E.send({ $peerwire: 'sync', ask: true, s: [], p: [] });
await until('answers from every member', () => [A, B, C, D].every((m) => answers.get(id(m)) >= 1));
await sleep(500);
if ([A, B, C, D].some((m) => answers.get(id(m)) > 4)) fail(`catch-up answers: ${[...answers.values()]}`);
console.log(`20 catch-up asks answered ${[A, B, C, D].map((m) => answers.get(id(m))).join(', ')} times`);
// Keys past MAX_KEYS (1024) are ignored: 'count', 'big' and 1100 more offered; 'k1099' must not take.
for (let i = 0; i < 1100; i++) E.send({ $peerwire: 'set', k: `k${i}`, v: `v${i}`, c: 1 });
E.send({ $peerwire: 'live', k: 'cursor', v: { x: 8 }, n: 6 });
await until('the flood', () => A.out.cursors[E.self]?.x === 8);
if (A.out.late !== 'none') fail(`key ${lib.MAX_KEYS + 1} was kept: ${A.out.late}`);
E.send({ $peerwire: 'set', k: 'count', v: 42, c: 1e6 });
await until('a valid write from a raw member', () => [A, B, C, D].every((m) => m.out.count === 42));
console.log(`malformed state messages ignored, keys capped at ${lib.MAX_KEYS}, a valid write from outside React taken`);

// Well-formed messages of the wrong type: what `accept` (or useSharedState's default, the kind of `initial`)
// keeps out, while hooks without a check still see them.
E.setPresence({ name: { evil: 1 } });
E.send({ x: 1 });
E.send({ $peerwire: 'set', k: 'count', v: { evil: 1 }, c: 2e6 });
E.send({ $peerwire: 'live', k: 'cursor', v: { x: 'evil' }, n: 7 });
E.send('after the wrong types');
await until('the wrong types to arrive', () => [A, B, C, D].every((m) => m.out.messages.at(-1)?.data === 'after the wrong types'));
for (const m of [A, B, C, D]) {
  const e = m.out.peers.find((p) => p.id === E.self);
  if (typeof e?.presence?.name !== 'object' || m.out.named.some((p) => p.id === E.self)) fail(`${m.name}: presence check`);
  if (m.out.named.length !== m.out.peers.length - 1) fail(`${m.name}: the check dropped a well-named member`);
  if (!m.out.messages.some((x) => x.data?.x === 1) || m.out.texts.some((x) => typeof x.data !== 'string')) fail(`${m.name}: message check`);
  if (m.out.texts.at(-1)?.data !== 'after the wrong types') fail(`${m.name}: the message check dropped text`);
  if (m.out.count !== 0) fail(`${m.name}: an object in a number's shared state read as ${JSON.stringify(m.out.count)}, not initial`);
  const [other, x] = m === B ? [A, 20] : [B, -5];
  if (m.out.cursors[E.self]?.x !== 'evil' || E.self in m.out.points || m.out.points[id(other)]?.x !== x) fail(`${m.name}: per-peer check`);
}
E.send({ $peerwire: 'set', k: 'count', v: 43, c: 3e6 });
await until('a number again', () => [A, B, C, D].every((m) => m.out.count === 43));
console.log('accept: a member\'s wrong presence, message, shared and per-peer values kept out; initial meanwhile');

// Leaving: unmounting A shows at once, and its per-peer value goes with it.
A.root.unmount();
await until('the leave', () => [B, C, D].every((m) => m.out.peers.length === 3 && !(id(A) in m.out.cursors)), 5000);
await E.leave();
console.log('unmount: members see the leave, the per-peer value goes');

// usePeerwireRoom: null stays out, a room joins, another roomId leaves the first, a failed join reports it.
function Switch({ out }) {
  const [opts, setOpts] = useState(null);
  const state = usePeerwireRoom(opts);
  const peers = usePeers({ room: state.room });
  Object.assign(out, state, { peers, setOpts });
  return null;
}
const sw = {};
const swRoot = createRoot(document.body.appendChild(document.createElement('div')));
swRoot.render(h(Switch, { out: sw }));
await until('a first render', () => sw.setOpts);
await sleep(50);
if (sw.status !== 'idle' || sw.room !== null) fail(`null options: ${sw.status}`);
sw.setOpts({ appId, roomId, presence: { name: 'fy' } });
await until('the switch to join', () => sw.status === 'joined' && sw.peers.length === 3 && names(B).includes('fy'));
const first = sw.room;
sw.setOpts({ appId, roomId: `${roomId}-2`, presence: { name: 'fy' } });
await until('the switch to leave', () => !names(B).includes('fy') && sw.status === 'joined' && sw.room !== first);
if (sw.peers.length !== 0) fail('the second room has members');
sw.setOpts({ appId, roomId, wasm: '/no/such/peerwire_ffi.wasm' });
await until('a failed join', () => sw.status === 'error');
if (!(sw.error instanceof Error) || sw.room !== null) fail(`error state: ${sw.error}`);
sw.setOpts(null);
await until('idle again', () => sw.status === 'idle');
console.log('usePeerwireRoom: idle, join, switch rooms, error, idle');

for (const m of [B, C, D]) m.root.unmount();
swRoot.unmount();
await sleep(100);
console.log('ok');
process.exit(0);
