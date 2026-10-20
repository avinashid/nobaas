// Throughput benchmark for the JS glue and the wasm core: two Endpoints in one
// Node process, wired back-to-back (synchronous in-memory wire, no network).
// Reports messages/s and MB/s per lane and size, events/s through decodeEvent,
// and GC figures. Each case runs warm-up rounds on one connected pair, then
// reports the median of the timed rounds (min and max alongside).
//
//   npm run bench                              # from packages/js
//   node test/bench.mjs --runs=9 --warmup=3 --only=reliable --json
//   node --expose-gc test/bench.mjs            # adds heap growth per round
//
// MB/s counts payload bytes only, in decimal MB (1e6). Unreliable messages are
// capped at MAX_UNRELIABLE (1100 B) by the core, so 16 KiB and 256 KiB exist on
// the Reliable lane only. Not part of `npm test`: the numbers depend on the host.
//
// A/B, which is how a change is judged: the host is shared, so absolute numbers
// move with its load. Run main (A) and the branch (B) alternately in one session,
// so both see the same load. --lib loads src/nobaas.js and pkg/ from another
// checkout, so both sides run this same script:
//
//   git -C ~/projects/personal/nobaas worktree add /tmp/nb-main origin/main
//   /tmp/nb-main/packages/js/build.sh && ./packages/js/build.sh    # from the branch root
//   cd packages/js
//   for i in 1 2 3; do
//     node test/bench.mjs --lib=/tmp/nb-main/packages/js --json > /tmp/a$i.json
//     node test/bench.mjs --json > /tmp/b$i.json
//   done
//
// For each case, take B's median over A's median in each pair (a$i against b$i),
// and report the median of the three ratios, with the load average from each
// JSON. A ratio within A's own min..max spread is noise, not a change.
import os from 'node:os';
import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { PerformanceObserver } from 'node:perf_hooks';

const arg = (name, dflt) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : dflt;
};
const RUNS = Number(arg('runs', 7));
const WARMUP = Number(arg('warmup', 2));
const ONLY = arg('only', '');
const JSON_OUT = process.argv.includes('--json');
// The packages/js directory under test: this one, or another checkout's (A/B above).
const LIB = resolve(arg('lib', new URL('..', import.meta.url).pathname));
const wasm = arg('wasm', `${LIB}/pkg/nobaas_ffi.wasm`);
const { loadNobaas, Lane, EventKind, decodeEvent } = await import(pathToFileURL(`${LIB}/src/nobaas.js`));
// Unreliable sends per JS task: the per-link queue holds 256, so none are dropped.
const UNREL_BATCH = 256;

const nb = await loadNobaas(wasm);
const tick = () => new Promise((r) => setImmediate(r));
const median = (v) => [...v].sort((x, y) => x - y)[v.length >> 1];
const log = (s) => { if (!JSON_OUT) console.log(s); };

// GC pauses, collected for the whole process and sliced per round by time.
const gcs = [];
new PerformanceObserver((l) => gcs.push(...l.getEntries())).observe({ entryTypes: ['gc'] });

async function pair() {
  const a = nb.endpoint();
  const b = nb.endpoint();
  // Handles: a sees b as 2, b sees a as 1.
  a.onTransmit = (addr, bytes) => b.receive(1, bytes);
  b.onTransmit = (addr, bytes) => a.receive(2, bytes);
  a.connect(2, b.peerId);
  while (!a.isConnected(b.id)) await tick();
  return { a, b };
}

// One round: `count` messages of `size` bytes from a to b. Returns ms elapsed
// and messages delivered (Unreliable may lose some; Reliable must not).
async function stream({ a, b }, lane, size, count) {
  const msg = new Uint8Array(size).fill(7);
  let got = 0;
  let done;
  const all = new Promise((r) => (done = r));
  b.onEvent = (e) => {
    if (e.type === 'message' && ++got === count) done();
  };
  const t0 = performance.now();
  for (let i = 0; i < count; i++) {
    for (;;) {
      const r = a.send(b.id, msg, lane);
      if (r === 0) break;
      if (r !== -3) throw new Error(`send returned ${r}`);
      await tick();
    }
    if (lane === Lane.Unreliable && i % UNREL_BATCH === UNREL_BATCH - 1) await tick();
  }
  if (lane === Lane.Unreliable) {
    for (let idle = 0; got < count && idle < 20; idle++) await tick();
  } else {
    await all;
  }
  return { ms: performance.now() - t0, got };
}

async function round(fn) {
  if (globalThis.gc) globalThis.gc();
  const heap0 = process.memoryUsage().heapUsed;
  const t0 = performance.now();
  const r = await fn();
  const t1 = performance.now();
  return { ...r, t0, t1, heapMB: (process.memoryUsage().heapUsed - heap0) / 1e6 };
}

// The observer delivers entries late, so GC is matched to rounds by time once a case ends.
async function gcOf(rounds) {
  await new Promise((r) => setTimeout(r, 50));
  for (const r of rounds) {
    const g = gcs.filter((e) => e.startTime >= r.t0 && e.startTime <= r.t1);
    r.gcCount = g.length;
    r.gcMs = g.reduce((s, e) => s + e.duration, 0);
  }
}

const results = [];
async function report(name, rounds, extra) {
  await gcOf(rounds);
  const rate = rounds.map(extra.rate);
  const res = {
    name,
    ...extra.info,
    runs: rate.map((v) => Math.round(v)),
    median: Math.round(median(rate)),
    min: Math.round(Math.min(...rate)),
    max: Math.round(Math.max(...rate)),
    gcCount: median(rounds.map((r) => r.gcCount)),
    gcMs: +median(rounds.map((r) => r.gcMs)).toFixed(2),
  };
  if (extra.mb) res.mbPerSec = +median(rounds.map(extra.mb)).toFixed(2);
  if (extra.lost) res.lost = Math.max(...rounds.map(extra.lost));
  if (globalThis.gc) res.heapMB = +median(rounds.map((r) => r.heapMB)).toFixed(2);
  results.push(res);
  const mb = res.mbPerSec !== undefined ? `${res.mbPerSec.toFixed(2).padStart(9)} MB/s` : ''.padStart(14);
  const heap = res.heapMB !== undefined ? `  heap +${res.heapMB} MB` : '';
  const lost = res.lost ? `  lost ${res.lost}` : '';
  log(`${name.padEnd(26)}${String(res.median).padStart(10)} ${extra.unit}${mb}   [${res.min}..${res.max}]  gc ${res.gcCount} / ${res.gcMs} ms${heap}${lost}`);
}

const CASES = [
  ['reliable 64 B', Lane.Reliable, 64, 20000],
  ['reliable 16 KiB', Lane.Reliable, 16 << 10, 1000],
  ['reliable 256 KiB', Lane.Reliable, 256 << 10, 64],
  ['unreliable 64 B', Lane.Unreliable, 64, 20000],
  ['unreliable 1100 B (max)', Lane.Unreliable, 1100, 10000],
];

let commit = 'unknown';
try {
  commit = execSync('git rev-parse --short HEAD', { cwd: LIB, stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
} catch {}
const meta = {
  date: new Date().toISOString(),
  lib: LIB,
  commit,
  node: process.version,
  v8: process.versions.v8,
  platform: `${os.platform()} ${os.release()} ${os.arch()}`,
  cpu: `${os.cpus()[0]?.model ?? 'unknown'} x${os.cpus().length}`,
  memGB: +(os.totalmem() / 2 ** 30).toFixed(1),
  loadavg: os.loadavg().map((v) => +v.toFixed(2)),
  wasmBytes: readFileSync(wasm).length,
  runs: RUNS,
  warmup: WARMUP,
  exposeGc: !!globalThis.gc,
};
log(`nobaas JS bench: commit ${meta.commit}, load ${meta.loadavg[0]}, node ${meta.node}, ${meta.platform}, ${meta.cpu}, wasm ${meta.wasmBytes} B`);
// Other work on the host skews every number: compare runs taken on an idle machine.
if (meta.loadavg[0] > os.cpus().length / 2) log(`warning: load average ${meta.loadavg[0]} on ${os.cpus().length} cores; numbers will be noisy`);
log(`median of ${RUNS} rounds after ${WARMUP} warm-up; [min..max]; gc = collections / pause per round`);

for (const [name, lane, size, count] of CASES) {
  if (!name.includes(ONLY)) continue;
  const p = await pair();
  const rounds = [];
  for (let i = 0; i < WARMUP + RUNS; i++) {
    const r = await round(() => stream(p, lane, size, count));
    if (lane !== Lane.Unreliable && r.got !== count) throw new Error(`${name}: ${r.got} of ${count} delivered`);
    if (i >= WARMUP) rounds.push(r);
  }
  const retransmits = p.a.stats().retransmits;
  await report(name, rounds, {
    unit: 'msg/s',
    info: { lane: lane === Lane.Reliable ? 'reliable' : 'unreliable', size, count, retransmits },
    rate: (r) => (r.got * 1000) / r.ms,
    mb: (r) => (r.got * size) / 1e3 / r.ms,
    lost: lane === Lane.Unreliable ? (r) => count - r.got : null,
  });
  p.a.close();
  p.b.close();
}
for (const n of ['16 KiB', '256 KiB']) if (`unreliable ${n}`.includes(ONLY)) log(`${`unreliable ${n}`.padEnd(26)}       n/a (over MAX_UNRELIABLE = 1100 B: send returns -2)`);

// decodeEvent on its own: the per-event cost the host pays before onEvent.
function eventBuf(kind, tail) {
  const b = new Uint8Array(33 + tail.length);
  b[0] = kind;
  b.set(crypto.getRandomValues(new Uint8Array(32)), 1);
  b.set(tail, 33);
  return b;
}
const EVENTS = [
  ['decodeEvent message 64 B', eventBuf(EventKind.Message, [Lane.Reliable, ...new Uint8Array(64)])],
  ['decodeEvent connected', eventBuf(EventKind.Connected, [1, 0, 2, 0, 0, 0])],
];
const DECODES = 200000;
for (const [name, buf] of EVENTS) {
  if (!name.includes(ONLY)) continue;
  const rounds = [];
  let sink = 0;
  for (let i = 0; i < WARMUP + RUNS; i++) {
    const r = await round(async () => {
      const t0 = performance.now();
      for (let j = 0; j < DECODES; j++) sink += decodeEvent(buf).peer.length;
      return { ms: performance.now() - t0 };
    });
    if (i >= WARMUP) rounds.push(r);
  }
  if (sink === 0) throw new Error('decodeEvent returned nothing');
  await report(name, rounds, { unit: 'ev/s ', info: { count: DECODES }, rate: (r) => (DECODES * 1000) / r.ms });
}

if (JSON_OUT) console.log(JSON.stringify({ meta, results }, null, 2));
process.exit(0);
