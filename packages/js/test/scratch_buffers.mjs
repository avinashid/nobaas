// PERF-3: send() and receive() go through buffers the endpoint keeps for its life
// (rxBuf, and a 16 KiB sendBuf with the old per-message buffer as the fallback), and
// toHex/fromHex use lookup tables. This checks that every message still arrives intact
// when the buffers are reused back to back, across the size boundaries and after wasm
// memory grows (which detaches every view of the old memory), that the hot path makes
// no allocation, and that the hex helpers agree with the code they replaced.
import { loadNobaas, Lane, toHex, fromHex } from '../src/nobaas.js';

const wasm = process.argv[2] ?? new URL('../pkg/nobaas_ffi.wasm', import.meta.url).pathname;
const nb = await loadNobaas(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const enc = new TextEncoder();
const same = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

// Counts nb_alloc calls an endpoint makes.
const spy = (ep) => {
  ep.allocs = 0;
  ep.nb = Object.assign(Object.create(nb), {
    x: Object.fromEntries(Object.entries(nb.x).map(([k, v]) => [k,
      typeof v === 'function' && k === 'nb_alloc' ? (...args) => (ep.allocs++, v(...args)) : v])),
  });
  return ep;
};
const wire = (from, to, addr) => (from.onTransmit = (_, d) => setImmediate(() => to.receive(addr, d)));

const a = spy(nb.endpoint());
const b = spy(nb.endpoint());
wire(a, b, 1);
wire(b, a, 2);
const got = [];
b.onEvent = (e) => e.type === 'message' && got.push(e.data);
a.connect(2, b.peerId);
for (let i = 0; i < 200 && !(a.isConnected(b.id) && b.isConnected(a.id)); i++) await sleep(5);
check(a.isConnected(b.id) && b.isConnected(a.id), 'pair connected');

// Sends `msgs` in one task (each one overwrites the send buffer), waits for all of them.
async function roundTrip(msgs, what) {
  got.length = 0;
  for (const m of msgs) check(a.send(b.id, m, Lane.Reliable) === 0, `${what}: send ${typeof m === 'string' ? `string ${m.length}` : `bytes ${m.length}`} accepted`);
  for (let i = 0; i < 400 && got.length < msgs.length; i++) await sleep(5);
  const want = msgs.map((m) => (typeof m === 'string' ? enc.encode(m) : m));
  check(got.length === want.length && want.every((w, i) => same(got[i], w)), `${what}: ${want.length} messages arrive intact and in order`);
}
const bytes = (n, seed) => Uint8Array.from({ length: n }, (_, i) => (i * 31 + seed) & 0xff);

// Bytes on both sides of the 16 KiB send buffer and of one datagram.
await roundTrip([bytes(0, 1), bytes(1, 2), bytes(1232, 3), bytes(16 * 1024, 4), bytes(16 * 1024 + 1, 5), bytes(3, 6)], 'bytes');
// Strings: 3-byte characters that just fit (16383 B) and just miss (16386 B, encodeInto
// stops short), a lone surrogate (encoded as U+FFFD), and short ones after a long one.
await roundTrip(['€'.repeat(5461), '€'.repeat(5462), 'a\uD800b', '', 'short', 'x'.repeat(16 * 1024), 'x'.repeat(16 * 1024 + 1), 'tail'], 'strings');

// A big send makes a buffer of its own; wasm memory may grow. Grow it by hand too: every
// view taken before is detached now, and the scratch buffers must still be used right.
const pagesBefore = nb.x.memory.buffer.byteLength;
nb.x.memory.grow(4);
check(nb.x.memory.buffer.byteLength === pagesBefore + 4 * 65536, 'wasm memory grew by 4 pages');
await roundTrip([bytes(200, 7), 'after grow', bytes(16 * 1024, 8)], 'after grow');
await roundTrip([bytes(300 * 1024, 9), 'after a big send', bytes(500, 10)], 'big send');

// The hot path allocates nothing: small sends and every datagram received.
await sleep(100);
const allocsA = a.allocs;
const allocsB = b.allocs;
await roundTrip(Array.from({ length: 50 }, (_, i) => (i % 2 ? `message ${i}` : bytes(100 + i * 300, i))), 'hot path');
check(a.allocs === allocsA && b.allocs === allocsB, `no nb_alloc for sends up to 16 KiB or for received datagrams (${a.allocs - allocsA}, ${b.allocs - allocsB})`);

// Hex: every byte, both cases, and the old parseInt reading of anything else.
const oldFromHex = (s) => Uint8Array.from(s.match(/../g), (h) => parseInt(h, 16));
const all = Uint8Array.from({ length: 256 }, (_, i) => i);
const allHex = toHex(all);
check(allHex === Array.from(all, (x) => x.toString(16).padStart(2, '0')).join(''), 'toHex matches the old code for every byte');
check(same(fromHex(allHex), all) && same(fromHex(allHex.toUpperCase()), all), 'fromHex reads every byte, lower and upper case');
check(toHex(new Uint8Array(0)) === '' && fromHex('abc').length === 1 && fromHex('abc')[0] === 0xab, 'empty input, and an odd trailing character is ignored');
const odd = ['zz', '1g', 'g1', '0x10', ' 1', '1 ', 'éé', 'abšc'];
check(odd.every((s) => same(fromHex(s), oldFromHex(s))), `fromHex reads non-hex input as the old code did (${odd.map((s) => toHex(fromHex(s))).join(' ')})`);

a.close();
b.close();
console.log(failed ? `${failed} FAILED` : 'scratch buffers: all passed');
process.exit(failed ? 1 : 0);
