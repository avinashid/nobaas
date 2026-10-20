// Room send throughput when the sender's timers are throttled, as in a background tab
// (Chrome runs a hidden tab's timers at most once a second). A simulation: the sender
// runs in a worker whose setTimeout fires no sooner than --clamp ms, the receiver runs
// unthrottled in the main thread, and the rooms meet over BroadcastChannel
// (LocalTransport). With --ab=<dir>, senders from that packages/js (A) and this one
// (B) alternate bursts of --mib MiB in 128 KiB messages; the receiver is always B.
// Not part of the gate: the numbers depend on the host.
//
//   node test/room_throttled.mjs --ab=/tmp/nb-main/packages/js --mib=12 --runs=5 --clamp=1000
import { Worker, isMainThread, parentPort, workerData } from 'node:worker_threads';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
const MSG = 128 << 10;
if (!isMainThread) {
  const { dir, roomId, clamp } = workerData;
  const st = globalThis.setTimeout;
  globalThis.setTimeout = (f, ms = 0, ...a) => st(f, Math.max(ms, clamp), ...a);
  const { joinRoom } = await import(pathToFileURL(`${dir}/src/index.js`));
  const room = await joinRoom({ appId: 'bg', roomId });
  const msg = new Uint8Array(MSG).fill(7);
  parentPort.on('message', (m) => {
    if (m === 'leave') return room.leave().then(() => process.exit(0));
    for (let i = 0; i < m; i++) room.send(msg);
  });
  parentPort.postMessage('ready');
} else {
  const arg = (name, dflt) => process.argv.find((a) => a.startsWith(`--${name}=`))?.slice(name.length + 3) ?? dflt;
  const MIB = Number(arg('mib', 12));
  const RUNS = Number(arg('runs', 5));
  const CLAMP = Number(arg('clamp', 1000));
  const AB = arg('ab', null);
  const COUNT = (MIB << 20) / MSG;
  const here = resolve(new URL('..', import.meta.url).pathname);
  const { joinRoom } = await import(pathToFileURL(`${here}/src/index.js`));
  const sides = {};
  for (const [s, dir] of [...(AB ? [['A', resolve(AB)]] : []), ['B', here]]) {
    const roomId = `bg-${s}-${process.pid}`;
    const rx = await joinRoom({ appId: 'bg', roomId });
    const w = new Worker(new URL(import.meta.url), { workerData: { dir, roomId, clamp: CLAMP } });
    await new Promise((r) => w.once('message', r));
    while (rx.peers.length !== 1) await new Promise((r) => setTimeout(r, 10));
    sides[s] = { rx, w };
  }
  const burst = async ({ rx, w }) => {
    let got = 0; let done; const all = new Promise((r) => (done = r));
    const off = rx.on('message', () => ++got === COUNT && done());
    const t0 = performance.now();
    w.postMessage(COUNT);
    await all; off();
    return performance.now() - t0;
  };
  const ms = Object.fromEntries(Object.keys(sides).map((s) => [s, []]));
  // The first round of each side warms up; the order swaps every round.
  for (let i = 0; i < RUNS + 1; i++) for (const s of i % 2 ? Object.keys(sides).reverse() : Object.keys(sides)) {
    const t = await burst(sides[s]);
    if (i > 0) ms[s].push(t);
  }
  const med = (v) => [...v].sort((p, q) => p - q)[v.length >> 1];
  for (const s of Object.keys(ms)) console.log(`${s}: ${MIB} MiB with sender timers clamped to ${CLAMP} ms: median ${med(ms[s]).toFixed(0)} ms (${((MIB * 1.048576) / med(ms[s]) * 1000).toFixed(2)} MB/s) [${ms[s].map((t) => t.toFixed(0)).join(' ')}]`);
  if (AB) console.log(`B/A throughput ${(med(ms.A) / med(ms.B)).toFixed(2)}`);
  for (const { rx, w } of Object.values(sides)) { w.postMessage('leave'); await rx.leave(); }
  process.exit(0);
}
