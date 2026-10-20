// Throughput benchmark for the JS glue and the wasm core: two Endpoints in one
// Node process, wired back-to-back (synchronous in-memory wire, no network).
// Reports messages/s and MB/s per lane and size, events/s through decodeEvent,
// CPU time per message, and GC figures. Each case runs warm-up rounds on one
// connected pair, then reports the median of the timed rounds (min and max
// alongside). CPU time (process.cpuUsage, all threads) is what JS hot-path work
// changes, and other processes on the host barely move it; wall-clock msgs/s
// also includes timer waits (the core's 25 ms delayed ACK) and the host's load.
//
//   npm run bench                              # from packages/js
//   node test/bench.mjs --runs=9 --warmup=3 --only=reliable --json
//   node --expose-gc test/bench.mjs            # adds heap growth per round
//   node test/bench.mjs --lib=../other/packages/js   # bench another checkout
//
// MB/s counts payload bytes only, in decimal MB (1e6). Unreliable messages are
// capped at MAX_UNRELIABLE (1100 B) by the core, so 16 KiB and 256 KiB exist on
// the Reliable lane only. Not part of `npm test`: the numbers depend on the host.
//
// A/B, which is how a change is judged: the host is shared and its load moves
// absolute numbers by up to 2x between runs a minute apart, so neither old
// numbers nor whole runs taken one after the other compare well. --ab=<dir>
// loads a second packages/js (A, the baseline) into the same process and
// alternates rounds between A and this checkout (B), so each pair of rounds sees
// the same load. It prints B/A as the median of the per-pair ratios, with their
// min..max, for throughput (above 1 is faster) and for CPU per message (below 1
// is cheaper). A ratio whose range spans 1.0 is no measurable change.
//
//   git -C ~/projects/personal/nobaas worktree add /tmp/nb-main origin/main
//   /tmp/nb-main/packages/js/build.sh && ./packages/js/build.sh    # from the branch root
//   cd packages/js && node test/bench.mjs --ab=/tmp/nb-main/packages/js
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
// The packages/js directory under test: this one, or another checkout's.
const LIB = resolve(arg('lib', new URL('..', import.meta.url).pathname));
const AB = arg('ab', null);
// Unreliable sends per JS task: the per-link queue holds 256, so none are dropped.
const UNREL_BATCH = 256;

const tick = () => new Promise((r) => setImmediate(r));
const median = (v) => [...v].sort((x, y) => x - y)[v.length >> 1];
const log = (s) => { if (!JSON_OUT) console.log(s); };

// One packages/js: its glue module, a loaded wasm, and where it came from.
async function side(label, dir) {
  dir = resolve(dir);
  const mod = await import(pathToFileURL(`${dir}/src/nobaas.js`));
  const wasm = `${dir}/pkg/nobaas_ffi.wasm`;
  let commit = 'unknown';
  try {
    commit = execSync('git rev-parse --short HEAD', { cwd: dir, stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
  } catch {}
  return { label, lib: dir, commit, wasmBytes: readFileSync(wasm).length, mod, nb: await mod.loadNobaas(wasm) };
}
// Run order: A (baseline) first, then B; without --ab there is only B.
const sides = AB ? [await side('A', AB), await side('B', LIB)] : [await side('B', LIB)];
// Lane and EventKind numbers are wire constants, the same on both sides.
const { Lane, EventKind } = sides[0].mod;

// GC pauses, collected for the whole process and sliced per round by time.
const gcs = [];
new PerformanceObserver((l) => gcs.push(...l.getEntries())).observe({ entryTypes: ['gc'] });

async function pair(nb) {
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
  const cpu0 = process.cpuUsage();
  const t0 = performance.now();
  const r = await fn();
  const t1 = performance.now();
  const cpu = process.cpuUsage(cpu0);
  return { ...r, t0, t1, cpuMs: (cpu.user + cpu.system) / 1000, heapMB: (process.memoryUsage().heapUsed - heap0) / 1e6 };
}

// The observer delivers entries late, so GC is matched to rounds by time once a case ends.
function gcOf(rounds) {
  for (const r of rounds) {
    const g = gcs.filter((e) => e.startTime >= r.t0 && e.startTime <= r.t1);
    r.gcCount = g.length;
    r.gcMs = g.reduce((s, e) => s + e.duration, 0);
  }
}

const cpuUs = (c, r) => (r.cpuMs * 1000) / c.units(r);

function stats(c, rounds) {
  gcOf(rounds);
  const rate = rounds.map((r) => (c.units(r) * 1000) / r.ms);
  const s = {
    runs: rate.map((v) => Math.round(v)),
    median: Math.round(median(rate)),
    min: Math.round(Math.min(...rate)),
    max: Math.round(Math.max(...rate)),
    cpuUs: +median(rounds.map((r) => cpuUs(c, r))).toFixed(3),
    gcCount: median(rounds.map((r) => r.gcCount)),
    gcMs: +median(rounds.map((r) => r.gcMs)).toFixed(2),
  };
  if (c.mb) s.mbPerSec = +median(rounds.map(c.mb)).toFixed(2);
  if (c.lost) s.lost = Math.max(...rounds.map(c.lost));
  if (globalThis.gc) s.heapMB = +median(rounds.map((r) => r.heapMB)).toFixed(2);
  return s;
}

function line(name, c, s) {
  const mb = s.mbPerSec !== undefined ? `${s.mbPerSec.toFixed(2).padStart(9)} MB/s` : ''.padStart(14);
  const heap = s.heapMB !== undefined ? `  heap +${s.heapMB} MB` : '';
  const lost = s.lost ? `  lost ${s.lost}` : '';
  const cpu = `${s.cpuUs.toFixed(2).padStart(8)} µs cpu/${c.unit.startsWith('ev') ? 'ev ' : 'msg'}`;
  return `${name.padEnd(28)}${String(s.median).padStart(10)} ${c.unit}${mb}${cpu}   [${s.min}..${s.max}]  gc ${s.gcCount} / ${s.gcMs} ms${heap}${lost}`;
}

// Each case: setup(side) gives a state, run(state) one round, rate/mb turn a round into numbers.
const CASES = [];
for (const [name, lane, size, count] of [
  ['reliable 64 B', Lane.Reliable, 64, 20000],
  ['reliable 16 KiB', Lane.Reliable, 16 << 10, 1000],
  ['reliable 256 KiB', Lane.Reliable, 256 << 10, 64],
  ['unreliable 64 B', Lane.Unreliable, 64, 20000],
  ['unreliable 1100 B (max)', Lane.Unreliable, 1100, 10000],
]) {
  CASES.push({
    name,
    unit: 'msg/s',
    info: { lane: lane === Lane.Reliable ? 'reliable' : 'unreliable', size, count },
    setup: (s) => pair(s.nb),
    run: async (p) => {
      const r = await stream(p, lane, size, count);
      if (lane !== Lane.Unreliable && r.got !== count) throw new Error(`${name}: ${r.got} of ${count} delivered`);
      return r;
    },
    done: (p) => {
      const retransmits = p.a.stats().retransmits;
      p.a.close();
      p.b.close();
      return { retransmits };
    },
    units: (r) => r.got,
    mb: (r) => (r.got * size) / 1e3 / r.ms,
    lost: lane === Lane.Unreliable ? (r) => count - r.got : null,
  });
}

// decodeEvent on its own: the per-event cost the host pays before onEvent.
function eventBuf(kind, tail) {
  const b = new Uint8Array(33 + tail.length);
  b[0] = kind;
  b.set(crypto.getRandomValues(new Uint8Array(32)), 1);
  b.set(tail, 33);
  return b;
}
const DECODES = 200000;
for (const [name, buf] of [
  ['decodeEvent message 64 B', eventBuf(EventKind.Message, [Lane.Reliable, ...new Uint8Array(64)])],
  ['decodeEvent connected', eventBuf(EventKind.Connected, [1, 0, 2, 0, 0, 0])],
]) {
  CASES.push({
    name,
    unit: 'ev/s ',
    info: { count: DECODES },
    setup: (s) => s.mod.decodeEvent,
    run: async (decode) => {
      let sink = 0;
      const t0 = performance.now();
      for (let j = 0; j < DECODES; j++) sink += decode(buf).peer.length;
      if (sink === 0) throw new Error('decodeEvent returned nothing');
      return { ms: performance.now() - t0 };
    },
    done: () => ({}),
    units: () => DECODES,
  });
}

const meta = {
  date: new Date().toISOString(),
  sides: sides.map(({ label, lib, commit, wasmBytes }) => ({ label, lib, commit, wasmBytes })),
  node: process.version,
  v8: process.versions.v8,
  platform: `${os.platform()} ${os.release()} ${os.arch()}`,
  cpu: `${os.cpus()[0]?.model ?? 'unknown'} x${os.cpus().length}`,
  memGB: +(os.totalmem() / 2 ** 30).toFixed(1),
  loadavg: os.loadavg().map((v) => +v.toFixed(2)),
  runs: RUNS,
  warmup: WARMUP,
  exposeGc: !!globalThis.gc,
};
const desc = sides.map((s) => `${AB ? `${s.label} ` : ''}commit ${s.commit} wasm ${s.wasmBytes} B`).join(', ');
log(`nobaas JS bench: ${desc}; load ${meta.loadavg[0]}, node ${meta.node}, ${meta.platform}, ${meta.cpu}`);
if (meta.loadavg[0] > os.cpus().length / 2) log(`warning: load average ${meta.loadavg[0]} on ${os.cpus().length} cores; absolute numbers will be noisy (use --ab to compare)`);
log(`median of ${RUNS} rounds after ${WARMUP} warm-up; [min..max]; gc = collections / pause per round${AB ? '; A and B alternate rounds' : ''}`);

const results = [];
for (const c of CASES) {
  if (!c.name.includes(ONLY)) continue;
  const states = [];
  for (const s of sides) states.push(await c.setup(s));
  const rounds = sides.map(() => []);
  for (let i = 0; i < WARMUP + RUNS; i++) {
    // Swap who goes first each round, so neither side always follows the other.
    const order = i % 2 ? [...sides.keys()].reverse() : [...sides.keys()];
    for (const k of order) {
      const r = await round(() => c.run(states[k]));
      if (i >= WARMUP) rounds[k].push(r);
    }
  }
  const extra = states.map((st, k) => c.done(st, sides[k]));
  await new Promise((r) => setTimeout(r, 50)); // let the GC observer deliver
  const per = rounds.map((rs, k) => ({ ...stats(c, rs), ...extra[k] }));
  if (!AB) {
    results.push({ name: c.name, ...c.info, ...per[0] });
    log(line(c.name, c, per[0]));
    continue;
  }
  const sum = (v) => ({ median: +median(v).toFixed(3), min: +Math.min(...v).toFixed(3), max: +Math.max(...v).toFixed(3) });
  const ratio = sum(rounds[1].map((r, i) => (c.units(r) / r.ms) / (c.units(rounds[0][i]) / rounds[0][i].ms)));
  const cpuRatio = sum(rounds[1].map((r, i) => cpuUs(c, r) / cpuUs(c, rounds[0][i])));
  results.push({ name: c.name, ...c.info, a: per[0], b: per[1], ratio, cpuRatio });
  const f = (x) => `${x.median.toFixed(3)} [${x.min.toFixed(3)}..${x.max.toFixed(3)}]`;
  log(line(`${c.name} A`, c, per[0]));
  log(line(`${c.name} B`, c, per[1]));
  log(`${''.padEnd(28)}  B/A throughput ${f(ratio)}, cpu/${c.unit.startsWith('ev') ? 'ev' : 'msg'} ${f(cpuRatio)}`);
}
for (const n of ['16 KiB', '256 KiB']) if (`unreliable ${n}`.includes(ONLY)) log(`${`unreliable ${n}`.padEnd(28)}       n/a (over MAX_UNRELIABLE = 1100 B: send returns -2)`);

if (JSON_OUT) console.log(JSON.stringify({ meta, results }, null, 2));
process.exit(0);
