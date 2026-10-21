// PERF-4: a Room send larger than the reliable send window (4 MiB) waits in the
// room's outbox. The outbox is flushed when the endpoint handles incoming datagrams
// (ACKs free window: Endpoint.onDrain), with a 2 ms timer only as the fallback.
// Sends 6 MiB in one go between two rooms in this process (BroadcastChannel and
// LocalTransport), checks every message arrives intact and in order, and that the
// drain hook, not the timer, moved the frames. Then the fallback alone (no hook)
// must still deliver another 6 MiB.
//
//   node packages/js/test/room_drain.mjs
import { joinRoom } from '../src/index.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (cond, ms) => {
  for (const t = Date.now(); !cond() && Date.now() - t < ms;) await sleep(10);
  return cond();
};

const opts = { appId: 'peerwire-test', roomId: `drain-${process.pid}` };
const [a, b] = await Promise.all([joinRoom(opts), joinRoom(opts)]);
check(await until(() => a.peers.length === 1 && b.peers.length === 1, 10000), 'two rooms joined');

// Frames each flush moved out of a's outbox, by who asked for the flush.
const moved = { drain: 0, timer: 0 };
let timerLeft = 0; // drain flushes that emptied the outbox but left the fallback timer armed
const queued = () => [...a.outbox.values()].reduce((n, q) => n + q.length, 0);
const flush = a.flush.bind(a);
a.flush = (from) => {
  const before = queued();
  flush(from);
  moved[from] += before - queued();
  if (from === 'drain' && before && !queued() && a.flushTimer) timerLeft++;
};

const got = [];
b.on('message', (data) => got.push(data));
const SIZE = 128 << 10;
const COUNT = 48; // 6 MiB, over the 4 MiB send window
const msg = (i) => new Uint8Array(SIZE).fill(i & 0xff).fill(0xa5, 0, 4);

async function burst(first, what) {
  got.length = 0;
  const t0 = performance.now();
  for (let i = 0; i < COUNT; i++) a.send(msg(first + i));
  const waited = queued();
  const done = await until(() => got.length === COUNT, 60000);
  const ms = performance.now() - t0;
  check(waited > 0, `${what}: the send window filled, ${waited} of ${COUNT} messages waited in the outbox`);
  check(done && got.every((d, i) => d.length === SIZE && d[4] === ((first + i) & 0xff) && d[SIZE - 1] === ((first + i) & 0xff)),
    `${what}: ${got.length} of ${COUNT} messages (${(COUNT * SIZE) / 2 ** 20} MiB) arrived intact and in order in ${ms.toFixed(0)} ms`);
  check(a.outbox.size === 0 && !a.flushTimer, `${what}: the outbox is empty and the fallback timer stopped`);
  return waited;
}

const waited = await burst(0, 'drain hook');
check(moved.drain === waited && moved.timer === 0, `the drain hook moved the waiting frames (drain ${moved.drain}, timer ${moved.timer})`);
check(timerLeft === 0, 'the drain that emptied the outbox stopped the fallback timer');

// Without the hook the 2 ms timer still empties the outbox, just later.
moved.drain = moved.timer = 0;
a.endpoint.onDrain = null;
const waited2 = await burst(100, 'timer fallback');
check(moved.timer === waited2 && moved.drain === 0, `with no drain hook the timer moved them (drain ${moved.drain}, timer ${moved.timer})`);

await Promise.all([a.leave(), b.leave()]);
console.log(failed ? `${failed} FAILED` : 'room drain: all passed');
process.exit(failed ? 1 : 0);
