// @peerwire/react in Node: React components rendered with react-dom into jsdom, members of one room over
// BroadcastChannel (signaling 'local'). Checks server rendering (initial values, no room), joining under StrictMode with no ghost member, presence both
// ways, messages and their limit, shared state (concurrent writes converge, a late joiner catches up), per-peer
// values, leaving, switching rooms, a failed join, and a member that sends malformed or flooding state messages.
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
  const [me, setMe] = usePresence();
  const { messages, send } = useMessages(5);
  const [count, setCount] = useSharedState('count', 0);
  const [late] = useSharedState('k1099', 'none');
  const [cursors, setCursor] = usePeerState('cursor');
  const [tick, setTick] = useState(0);
  // A new handler every render: the latest one must be called.
  useRoomEvent('message', (data) => out.raw.push([tick, data]));
  out.maxPeers = Math.max(out.maxPeers ?? 0, peers.length);
  Object.assign(out, { room, status, peers, me, messages, count, late, cursors, setMe, send, setCount, setCursor, setTick, tick });
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
// Keys past MAX_KEYS (1024) are ignored: 'count' and 'k1099' make 1100 offered; 'k1099' must not take.
for (let i = 0; i < 1100; i++) E.send({ $peerwire: 'set', k: `k${i}`, v: i, c: 1 });
E.send({ $peerwire: 'live', k: 'cursor', v: { x: 8 }, n: 6 });
await until('the flood', () => A.out.cursors[E.self]?.x === 8);
if (A.out.late !== 'none') fail(`key ${lib.MAX_KEYS + 1} was kept: ${A.out.late}`);
E.send({ $peerwire: 'set', k: 'count', v: 42, c: 1e6 });
await until('a valid write from a raw member', () => [A, B, C, D].every((m) => m.out.count === 42));
console.log(`malformed state messages ignored, keys capped at ${lib.MAX_KEYS}, a valid write from outside React taken`);

// Leaving: unmounting A shows at once, and its per-peer value goes with it.
A.root.unmount();
await until('the leave', () => [B, C, D].every((m) => m.out.peers.length === 3 && !(id(A) in m.out.cursors)), 5000);
await E.leave();
console.log('unmount: members see the leave, the per-peer value goes');

// usePeerwireRoom: null stays out, a room joins, another roomId leaves the first, a failed join reports it.
function Switch({ out }) {
  const [opts, setOpts] = useState(null);
  const state = usePeerwireRoom(opts);
  const peers = usePeers(state.room);
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
