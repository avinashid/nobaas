// PERF-2: Nostr signalling stays bounded under a flood. Anyone who knows the room
// name can publish events, so the de-duplication set must not grow without limit,
// and base64 must handle bodies far larger than a signalling message. No relays:
// events are fed straight into onRelay.
import { loadNobaas } from '../src/nobaas.js';
import { NostrSignaling, MAX_SEEN, b64, unb64 } from '../src/nostr.js';

const wasm = process.argv[2] ?? new URL('../pkg/nobaas_ffi.wasm', import.meta.url).pathname;
const nb = await loadNobaas(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

const other = nb.endpoint();
const sig = new NostrSignaling(nb, nb.endpoint(), { room: 'perf2/flood', relays: [] });
await sig.ready;
const hellos = [];
sig.onHello = (from) => hellos.push(from);
const hello = b64(new TextEncoder().encode(JSON.stringify({ t: 'hello', from: other.id })));
const feed = (id, extra = {}) =>
  sig.onRelay('test', JSON.stringify(['EVENT', 's', { id, kind: 25059, pubkey: 'x', tags: [['p', sig.roomTag]], content: 'e30=', ...extra }]));

// 1. A flood of distinct ids keeps the set at the cap, forgetting the oldest first.
const N = 12000;
let peak = 0;
for (let i = 0; i < N; i++) {
  feed(`flood${i}`);
  peak = Math.max(peak, sig.seen.size);
}
check(MAX_SEEN === 4096, `MAX_SEEN is 4096`);
check(peak <= MAX_SEEN && sig.seen.size === MAX_SEEN, `${N} distinct events: seen.size peaked at ${peak} (cap ${MAX_SEEN})`);
check(!sig.seen.has('flood0') && sig.seen.has(`flood${N - 1}`) && sig.seen.has(`flood${N - MAX_SEEN}`), 'the oldest ids are evicted, the newest kept');
check(sig.stats.received === N, `every distinct event was counted once (${sig.stats.received})`);

// 2. A recent duplicate is still dropped; a hello still gets through after the flood.
feed(`flood${N - 1}`);
check(sig.stats.received === N, 'a recent duplicate id is still dropped');
feed('hello1', { content: hello });
feed('hello1', { content: hello });
check(hellos.length === 1 && hellos[0] === other.id, 'a hello after the flood is delivered once');

// 3. Malformed events neither throw nor enter the set.
const size = sig.seen.size;
const before = sig.stats.received;
let threw = false;
try {
  for (const id of [undefined, null, 7, { a: 1 }, ['x']]) feed(id);
  feed('notags', { tags: 'p' });
  feed('badtag', { tags: [null, 5, 'p'] });
} catch {
  threw = true;
}
check(!threw && sig.stats.received === before + 2 && sig.seen.size === size, 'non-string ids are ignored; bad tags do not throw');
sig.close();

// 4. base64 round-trips a 100 KB and a 1 MB body (spread into fromCharCode fails at that size on some engines).
for (const n of [100 * 1024, 1 << 20]) {
  const bytes = crypto.getRandomValues(new Uint8Array(65536));
  const data = new Uint8Array(n).map((_, i) => bytes[i % bytes.length] ^ (i >> 16));
  const back = unb64(b64(data));
  check(back.length === n && back.every((x, i) => x === data[i]), `base64 round-trip of ${n} bytes`);
}
check(b64(new Uint8Array(0)) === '' && b64(new Uint8Array([0, 255, 128])) === 'AP+A', 'base64 of empty and short inputs');

console.log(failed ? `${failed} FAILED` : 'nostr bounds: all passed');
process.exit(failed ? 1 : 0);
