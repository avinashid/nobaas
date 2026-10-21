// PERF-2: Nostr signalling stays bounded under a flood. Anyone who knows the room
// name can publish events, so the de-duplication set must not grow without limit,
// and base64 must handle bodies far larger than a signalling message. No relays:
// events are fed straight into onRelay.
import { loadPeerwire } from '../src/peerwire.js';
import { BACKOFF_MS, MAX_BACKOFF_MS, RECONNECT_MS, NostrSignaling, MAX_SEEN, b64, unb64 } from '../src/nostr.js';

const wasm = process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname;
const pw = await loadPeerwire(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

const other = pw.endpoint();
const sig = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/flood', relays: [] });
await sig.ready;
const hellos = [];
sig.onHello = (from) => hellos.push(from);
const hello = b64(new TextEncoder().encode(JSON.stringify({ t: 'hello', from: other.id })));
// NIP-01 ids are 64 lowercase hex characters.
const hid = (i) => i.toString(16).padStart(64, '0');
const feed = (id, extra = {}) =>
  sig.onRelay('test', JSON.stringify(['EVENT', 's', { id, kind: 25059, pubkey: 'x', tags: [['p', sig.roomTag]], content: 'e30=', ...extra }]));

// 1. A flood of distinct ids keeps the set at the cap, forgetting the oldest first.
const N = 12000;
let peak = 0;
for (let i = 0; i < N; i++) {
  feed(hid(i));
  peak = Math.max(peak, sig.seen.size);
}
check(MAX_SEEN === 4096, `MAX_SEEN is 4096`);
check(peak <= MAX_SEEN && sig.seen.size === MAX_SEEN, `${N} distinct events: seen.size peaked at ${peak} (cap ${MAX_SEEN})`);
check(!sig.seen.has(hid(0)) && sig.seen.has(hid(N - 1)) && sig.seen.has(hid(N - MAX_SEEN)), 'the oldest ids are evicted, the newest kept');
check(sig.stats.received === N, `every distinct event was counted once (${sig.stats.received})`);

// 2. A recent duplicate is still dropped; a hello still gets through after the flood.
feed(hid(N - 1));
check(sig.stats.received === N, 'a recent duplicate id is still dropped');
feed(hid(1e9), { content: hello });
feed(hid(1e9), { content: hello });
check(hellos.length === 1 && hellos[0] === other.id, 'a hello after the flood is delivered once');

// 3. Malformed events neither throw nor enter the set.
const size = sig.seen.size;
const before = sig.stats.received;
let threw = false;
try {
  for (const id of [undefined, null, 7, { a: 1 }, ['x']]) feed(id);
  feed(hid(2e9), { tags: 'p' });
  feed(hid(3e9), { tags: [null, 5, 'p'] });
} catch {
  threw = true;
}
check(!threw && sig.stats.received === before + 2 && sig.seen.size === size, 'non-string ids are ignored; bad tags do not throw');

// 3b. Security PERF2-1: only a NIP-01 id (64 lowercase hex) is remembered, so a relay
// can't pin megabytes of id strings in `seen`.
const bad = ['a'.repeat(64 * 1024), 'ab'.repeat(33), 'ab'.repeat(31), 'AB'.repeat(32), 'zz'.repeat(32), '', `flood1`];
const heap = process.memoryUsage().heapUsed;
for (let i = 0; i < 4096; i++) feed(`${i.toString(16)}`.padStart(64 * 1024, 'f'));
for (const id of bad) feed(id);
check(sig.stats.received === before + 2 && sig.seen.size === size && [...sig.seen].every((id) => id.length === 64),
  `4096 events with 64 KiB ids and ${bad.length} non-hex or wrong-length ids are dropped, never remembered`);
check(process.memoryUsage().heapUsed - heap < 64 << 20, `heap grew ${((process.memoryUsage().heapUsed - heap) / 2 ** 20).toFixed(1)} MiB (unbounded: ~256 MiB)`);

// 3c. NB-014 (found by fuzz.mjs): a relay message that is JSON but not an array
// (`null` threw a TypeError out of the WebSocket handler) is ignored.
let scalarThrew = false;
try {
  for (const raw of ['null', '0', '1', '"EVENT"', 'true', 'false', '{}', '{"0":"EVENT"}']) sig.onRelay('test', raw);
} catch {
  scalarThrew = true;
}
check(!scalarThrew && sig.stats.received === before + 2, 'relay messages that are JSON but not arrays are ignored');
sig.close();

// 4. base64 round-trips a 100 KB and a 1 MB body (spread into fromCharCode fails at that size on some engines).
for (const n of [100 * 1024, 1 << 20]) {
  const bytes = crypto.getRandomValues(new Uint8Array(65536));
  const data = new Uint8Array(n).map((_, i) => bytes[i % bytes.length] ^ (i >> 16));
  const back = unb64(b64(data));
  check(back.length === n && back.every((x, i) => x === data[i]), `base64 round-trip of ${n} bytes`);
}
check(b64(new Uint8Array(0)) === '' && b64(new Uint8Array([0, 255, 128])) === 'AP+A', 'base64 of empty and short inputs');

// 5. A relay that refuses an event for its rate limit gets nothing for BACKOFF_MS, doubling;
//    refusals of events already sent during a pause don't double it again; others still get events.
{
  const s5 = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/backoff', relays: [] });
  await s5.ready;
  const sent = { a: 0, b: 0 };
  for (const name of ['a', 'b']) s5.sockets.push({ relayUrl: name, readyState: WebSocket.OPEN, send: () => sent[name]++, close() {} });
  await s5.publish(s5.roomTag, 'e30=');
  s5.onRelay('a', JSON.stringify(['OK', 'x', false, 'rate-limited: slow down']));
  s5.onRelay('a', JSON.stringify(['OK', 'y', false, 'banned: too many rate-limit violations, try again later']));
  await s5.publish(s5.roomTag, 'e30=');
  const first = s5.paused.get('a');
  check(sent.a === 1 && sent.b === 2 && first?.ms === BACKOFF_MS && s5.stats.backoffs === 1, `a rate-limited relay is paused once, the other still sent to (${JSON.stringify(sent)})`);
  s5.onRelay('b', JSON.stringify(['OK', 'z', false, 'invalid: bad signature']));
  check(!s5.paused.has('b'), 'other refusals do not pause a relay');
  first.until = Date.now() - 1; // the pause ran out
  await s5.publish(s5.roomTag, 'e30=');
  s5.onRelay('a', JSON.stringify(['OK', 'w', false, 'rate-limited: again']));
  check(sent.a === 2 && s5.paused.get('a').ms === BACKOFF_MS * 2, 'after the pause it is tried again, and a second limit doubles the pause');
  s5.paused.get('a').until = Date.now() - MAX_BACKOFF_MS - 1; // quiet for MAX_BACKOFF_MS since
  s5.onRelay('a', JSON.stringify(['OK', 'v', false, 'rate-limited: much later']));
  check(s5.paused.get('a').ms === BACKOFF_MS, 'a limit after a long quiet period starts the pause over');
  s5.sockets.length = 0;
}

// 6. A relay socket that closes (phone hotspot on, network switch, tab frozen) is redialled,
//    subscribes again and says so; a network change recycles open sockets; close() stops it.
{
  const Real = globalThis.WebSocket;
  const made = [];
  class Fake {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    constructor(url) {
      this.url = url;
      this.readyState = Fake.CONNECTING;
      this.sent = [];
      made.push(this);
    }
    open() {
      this.readyState = Fake.OPEN;
      this.onopen?.();
    }
    send(m) {
      this.sent.push(m);
    }
    close() {
      this.readyState = Fake.CLOSED;
      this.onclose?.();
    }
  }
  globalThis.WebSocket = Fake;
  const tick = (ms) => new Promise((r) => setTimeout(r, ms));
  const s6 = new NostrSignaling(pw, pw.endpoint(), { room: 'hotspot/reconnect', relays: ['wss://a', 'wss://b'] });
  await tick(20);
  made.forEach((w) => w.open());
  await s6.ready;
  const back = [];
  s6.onReconnect = (url) => back.push(url);
  check(s6.stats.relaysOpen === 2 && made.every((w) => w.sent.some((m) => m.startsWith('["REQ"'))), 'both relays open and subscribed');
  made[0].close(); // relay a drops
  check(s6.stats.relaysOpen === 1, `a closed relay is no longer counted open (${s6.stats.relaysOpen})`);
  await tick(RECONNECT_MS + 100);
  const a2 = made.at(-1);
  check(made.length === 3 && a2.url === 'wss://a', `relay a is redialled after ${RECONNECT_MS} ms (${made.length} sockets made)`);
  a2.close(); // fails again: the delay doubles
  await tick(RECONNECT_MS + 100);
  check(made.length === 3, 'a second failure waits longer');
  await tick(RECONNECT_MS + 100);
  const a3 = made.at(-1);
  a3.open();
  check(made.length === 4 && a3.sent.some((m) => m.startsWith('["REQ"')) && back.join() === 'wss://a' && s6.stats.reconnects === 1 && s6.stats.relaysOpen === 2,
    `the reopened relay subscribes again and onReconnect fires (${JSON.stringify(back)}, open ${s6.stats.relaysOpen})`);
  await s6.publish(s6.roomTag, 'e30=');
  check(a3.sent.some((m) => m.startsWith('["EVENT"')) && made[0].sent.every((m) => !m.startsWith('["EVENT"')), 'events go to the new socket, not the dead one');
  s6.redial(true); // a network change: open sockets may be on the old network
  check(made.length === 6 && s6.stats.relaysOpen === 0 && made.slice(-2).map((w) => w.url).sort().join() === 'wss://a,wss://b', 'a network change redials every relay at once');
  made.slice(-2).forEach((w) => w.open());
  check(s6.stats.relaysOpen === 2 && s6.sockets.length === 2, `after it, both are open again with one socket each (${s6.sockets.length})`);
  s6.close();
  await tick(RECONNECT_MS * 2 + 100);
  check(made.length === 6 && s6.retry.size === 0, 'close() stops the redialling');
  globalThis.WebSocket = Real;
}

console.log(failed ? `${failed} FAILED` : 'nostr bounds: all passed');
process.exit(failed ? 1 : 0);
