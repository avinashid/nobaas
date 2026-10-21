// PERF1-2: close() frees the endpoint's wasm handle and buffers, so after it no public
// method may call into wasm with them: a late datagram from a peer's still-open
// DataChannel would otherwise run on freed memory shared with every other endpoint.
// send and connect* throw, receive and setRelayOnly do nothing, queries return
// null or false, and stats() returns the counters as they were at close.
import { loadPeerwire, Lane, MAX_DATAGRAM } from '../src/peerwire.js';

const wasm = process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname;
const pw = await loadPeerwire(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Counts wasm calls an endpoint makes after its close(), by name: any is a use of its freed handle.
const spy = (ep) => {
  ep.afterClose = [];
  ep.freed = []; // [pointer, length] of each pw_free after close
  ep.pw = Object.assign(Object.create(pw), {
    x: Object.fromEntries(Object.entries(pw.x).map(([k, v]) => [k,
      typeof v === 'function' ? (...args) => {
        if (ep.closed) {
          ep.afterClose.push(k);
          if (k === 'pw_free') ep.freed.push(`${args[0]}:${args[1]}`);
        }
        return v(...args);
      } : v])),
  });
  return ep;
};
// Datagrams sent in one task arrive together in a later one.
const wire = (from, to, addr) => (from.onTransmit = (_, d) => setImmediate(() => to.receive(addr, d)));

const a = spy(pw.endpoint());
const b = spy(pw.endpoint());
wire(a, b, 1);
wire(b, a, 2);
a.connect(2, b.peerId);
for (let i = 0; i < 200 && !(a.isConnected(b.id) && b.isConnected(a.id)); i++) await sleep(5);
check(a.isConnected(b.id) && b.isConnected(a.id), 'pair connected');
b.send(a.id, 'hi', Lane.Reliable);
await sleep(50);
const before = b.stats();
const secret = b.secret();
const sealed = a.seal(b.id, new Uint8Array([1, 2, 3]));
check(before.txPackets > 0 && secret?.length === 32 && sealed !== null, 'stats, secret and seal work before close');

b.close();
const bufs = [[b.peerBuf, 32], [b.txBuf, MAX_DATAGRAM], [b.rxBuf, MAX_DATAGRAM], [b.sendBuf, 16 * 1024],
  [b.addrBuf, 4], [b.evBuf, b.evCap]].map(([p, n]) => `${p}:${n}`);
check(b.closed === true && b.afterClose.join() === `pw_endpoint_free,${bufs.map(() => 'pw_free').join()}`
  && b.freed.slice().sort().join() === bufs.slice().sort().join() && new Set(bufs).size === bufs.length,
  `close() frees the handle and its ${bufs.length} buffers, nothing else (${b.afterClose.join(', ')})`);
b.afterClose = [];

// The repro from the card: a's sends still reach b's wire, which calls b.receive().
for (let i = 0; i < 5; i++) a.send(b.id, `late ${i}`, Lane.Reliable);
await sleep(300);
b.receive(1, new Uint8Array(100));

const throws = (fn, what) => {
  try {
    fn();
  } catch (e) {
    return check(e instanceof Error && e.message === `${what}: endpoint closed`, `${what} throws "${e.message}"`);
  }
  check(false, `${what} throws`);
};
throws(() => b.send(a.id, 'x'), 'send');
throws(() => b.sendRetry(a.id, 'x'), 'send');
throws(() => b.connect(2, a.peerId), 'connect');
throws(() => b.connectVia(a.id, a.peerId), 'connectVia');
check(b.secret() === null, 'secret() is null');
check(b.seal(a.id, new Uint8Array([1])) === null, 'seal() is null');
check(b.open(sealed) === null, 'open() is null');
check(b.isConnected(a.id) === false, 'isConnected() is false');
check(b.rttMs(a.id) === null, 'rttMs() is null');
check(b.setRelayOnly(true) === undefined, 'setRelayOnly() does nothing');
check(b.disconnect(a.id) === undefined, 'disconnect() does nothing');
const after = b.stats();
check(Object.keys(after).join() === Object.keys(before).join() && after.txPackets >= before.txPackets && after.rxPackets >= before.rxPackets,
  `stats() returns the counters at close (txPackets ${after.txPackets})`);
after.txPackets = -1;
check(b.stats().txPackets !== -1, 'stats() after close is a copy');
b.pump();
b.schedule();
b.arm();
b.close();
await sleep(50);
check(b.afterClose.length === 0, `no wasm call after close() from any method (${b.afterClose.length}: ${[...new Set(b.afterClose)].join(', ')})`);

// The other endpoint, sharing the module, is unharmed and sees b go.
const c = pw.endpoint();
wire(a, c, 3);
wire(c, a, 2);
const got = new Promise((r) => (c.onEvent = (e) => e.type === 'message' && r(new TextDecoder().decode(e.data))));
a.connect(3, c.peerId);
for (let i = 0; i < 200 && !a.isConnected(c.id); i++) await sleep(5);
a.send(c.id, 'still fine', Lane.Reliable);
check((await Promise.race([got, sleep(1000)])) === 'still fine', 'a fresh link on the shared module still works');
a.close();
c.close();

console.log(failed ? `${failed} FAILED` : 'endpoint closed: all passed');
process.exit(failed ? 1 : 0);
