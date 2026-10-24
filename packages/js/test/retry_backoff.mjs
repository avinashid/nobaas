// DirectRetries (src/retry.js), the schedule of a room's WebRTC retries, on a fake clock:
// - backoff: one peer's retries come 20, 40, 80, 160 s apart, then every 5 min, each
//   within ±20 % of that;
// - budget: peers made due together take turns, one retry in flight at a time and
//   starts at least 2 s apart; a retry that never reports back frees the slot after
//   holdMs; a peer triggered while its retry is in flight goes again after it;
// - the other side's requests are heeded once per 20 s; a peer that isn't the
//   dialling side's (auto off) is retried only when triggered; forget and close stop it;
// - a 20-peer room whose retries all fail, over an hour: never two at once, at most a
//   few starts a minute, and about one per peer per 5 min once at the cap.
//
//   node packages/js/test/retry_backoff.mjs
import { DirectRetries } from '../src/retry.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};

// A fake clock: timers run in order as advance() moves time on.
let now = 0;
let timers = [];
let ids = 0;
const clock = {
  now: () => now,
  setTimer: (fn, ms) => {
    const t = { at: now + Math.max(0, ms), fn, id: ++ids };
    timers.push(t);
    return t;
  },
  clearTimer: (t) => {
    if (t) timers = timers.filter((x) => x !== t);
  },
};
const advance = (ms) => {
  const end = now + ms;
  for (;;) {
    timers.sort((a, b) => a.at - b.at || a.id - b.id);
    const t = timers[0];
    if (!t || t.at > end) break;
    timers.shift();
    now = t.at;
    t.fn();
  }
  now = end;
};
const reset = () => {
  now = 0;
  timers = [];
};

// A scheduler whose retries each fail `takes` ms after they start (done() then), or never
// report back (takes: null); `started` lists [time, peer].
function make({ takes = 100, rand = () => 0.5, holdMs = 30000, inFlight = true } = {}) {
  const started = [];
  let busy = 0;
  let most = 0;
  const r = new DirectRetries((peer) => {
    started.push([now, peer]);
    if (!inFlight) return false;
    most = Math.max(most, ++busy);
    if (takes !== null) clock.setTimer(() => (busy--, r.done(peer)), takes);
    return true;
  }, { holdMs, rand, ...clock });
  return { r, started, most: () => most, release: () => busy-- };
}
const P = (i) => i.toString(16).padStart(64, '0');

// 1. Backoff, no jitter (rand 0.5): 20, 40, 80, 160 s apart, then 5 min.
{
  reset();
  const { r, started } = make();
  r.track(P(1), true, true);
  advance(1500000);
  const gaps = started.map(([t], i) => (i ? t - started[i - 1][0] - 100 : t));
  const want = [20000, 40000, 80000, 160000, 300000, 300000, 300000];
  if (want.some((w, i) => gaps[i] !== w)) fail(`backoff ${gaps.slice(0, 7).join(', ')}, want ${want.join(', ')}`);
  r.close();
  console.log(`backoff: retries at ${started.slice(0, 7).map(([t]) => `${t / 1000}s`).join(', ')}`);
}

// Jitter: every delay within ±20 % of its base, both ends reached.
{
  const r = new DirectRetries(() => false, { ...clock });
  let lo = Infinity;
  let hi = 0;
  for (let n = 0; n < 12; n++) {
    const base = Math.min(20000 * 2 ** n, 300000);
    for (const x of [0, 0.25, 0.5, 0.999999, Math.random()]) {
      r.rand = () => x;
      const d = r.delay(n) / base;
      if (d < 0.8 - 1e-6 || d > 1.2 + 1e-6) fail(`delay(${n}) with rand ${x}: ${d} of the base`);
      lo = Math.min(lo, d);
      hi = Math.max(hi, d);
    }
  }
  if (lo > 0.801 || hi < 1.199) fail(`jitter spans ${lo}..${hi}, want 0.8..1.2`);
  console.log(`jitter: delays between ${lo.toFixed(2)} and ${hi.toFixed(2)} of the base`);
}

// 2. Five peers triggered at once: in turn, one in flight, starts at least 2 s apart; a
// retry that takes longer than the gap holds the next back.
{
  reset();
  const { r, started, most } = make({ takes: 500 });
  for (let i = 1; i <= 5; i++) r.track(P(i), true, true);
  if (r.soon() !== 5) fail('soon() should count 5 peers');
  advance(15000);
  const order = started.map(([, p]) => p);
  if (order.join() !== [1, 2, 3, 4, 5].map(P).join()) fail(`not in turn: ${order.map((p) => p.slice(-1))}`);
  const apart = started.slice(1).map(([t], i) => t - started[i][0]);
  if (most() !== 1 || apart.some((d) => d < 2000)) fail(`in flight ${most()}, starts ${apart.join(', ')} ms apart`);
  r.close();
  reset();
  const slow = make({ takes: 5000 });
  for (let i = 1; i <= 3; i++) slow.r.track(P(i), true, true);
  slow.r.soon();
  advance(20000);
  const t = slow.started.map(([x]) => x);
  if (t.join() !== '0,5000,10000' || slow.most() !== 1) fail(`slow retries started at ${t.join(', ')}, in flight ${slow.most()}`);
  slow.r.close();
  console.log(`budget: 5 peers in turn, one at a time, ${Math.min(...apart)} ms apart at least; 5 s retries start 5 s apart`);
}

// 3. A retry that never reports back frees the slot after holdMs.
{
  reset();
  const { r, started } = make({ takes: null, holdMs: 10000 });
  r.track(P(1), true, true);
  r.track(P(2), true, true);
  r.soon();
  advance(9999);
  if (started.length !== 1) fail(`${started.length} started before the hold ended`);
  advance(1);
  if (started.length !== 2 || started[1][0] !== 10000) fail(`the second started at ${started[1]?.[0]}, want 10000`);
  r.close();
  console.log('hold: a retry that never ends frees the slot after holdMs');
}

// 4. A trigger while the peer's own retry is in flight: once more after it, never two.
{
  reset();
  const { r, started, most } = make({ takes: 3000 });
  r.track(P(1), true, true);
  r.soon(P(1));
  advance(1000);
  r.soon(P(1));
  r.soon(P(1));
  advance(10000);
  if (started.map(([t]) => t).join() !== '0,3000' || most() !== 1) fail(`started at ${started.map(([t]) => t)}`);
  r.close();
  console.log('again: a peer triggered during its retry goes once more after it');
}

// 5. The other side's requests: once per 20 s per peer. Unknown peers: nothing.
{
  reset();
  const { r, started } = make();
  r.track(P(1), true, false);
  if (r.ask(P(9)) !== 0) fail('a request for a peer not on a fallback counted');
  r.ask(P(1));
  advance(5000);
  r.ask(P(1));
  advance(5000);
  r.ask(P(1));
  advance(11000);
  r.ask(P(1));
  advance(5000);
  if (started.map(([t]) => t).join() !== '0,21000') fail(`requests heeded at ${started.map(([t]) => t)}, want 0 and 21000`);
  r.close();
  console.log('asked: requests heeded once per 20 s; one for a peer not on a fallback ignored');
}

// 6. No backoff for a peer that isn't ours to dial, and none when start() has nothing in
// flight (it only asked): the backoff of a dialling peer still goes on.
{
  reset();
  const { r, started } = make();
  r.track(P(1), true, false);
  advance(600000);
  if (started.length) fail('a peer with auto off was retried on a timer');
  r.soon(P(1));
  advance(1);
  if (started.length !== 1) fail('a trigger did not retry a peer with auto off');
  r.close();
  reset();
  const asked = make({ inFlight: false });
  asked.r.track(P(2), true, true);
  advance(65000);
  if (asked.started.map(([t]) => t).join() !== '20000,60000') fail(`no-flight retries at ${asked.started.map(([t]) => t)}`);
  asked.r.close();
  console.log('auto: only the dialling side keeps the backoff; a start with nothing in flight keeps it going');
}

// 7. forget() while in flight frees the slot and stops the peer; close() stops all.
{
  reset();
  const { r, started } = make({ takes: null });
  r.track(P(1), true, true);
  r.track(P(2), true, true);
  r.soon();
  advance(100);
  r.track(P(1), false); // went direct
  advance(2500);
  if (started.map(([, p]) => p.slice(-1)).join() !== '1,2') fail(`after forget: ${started.map(([t, p]) => `${t}:${p.slice(-1)}`)}`);
  r.close();
  advance(3600000);
  if (started.length !== 2 || timers.length) fail(`after close: ${started.length} started, ${timers.length} timers left`);
  console.log('forget: a peer gone direct frees the slot at once; close leaves no timer');
}

// 8. A 20-peer room on the pipe whose retries all fail after 8 s (fallbackMs), real
// jitter, for an hour: never two at once, a few starts a minute at most.
{
  reset();
  const { r, started, most } = make({ takes: 8000, rand: Math.random });
  for (let i = 1; i <= 20; i++) r.track(P(i), true, true);
  advance(3600000);
  let busiest = 0;
  for (let i = 0, j = 0; i < started.length; i++) {
    while (started[i][0] - started[j][0] >= 60000) j++;
    busiest = Math.max(busiest, i - j + 1);
  }
  const last = started.filter(([t]) => t >= 3000000).length; // the last 10 min, all at the cap
  if (most() !== 1 || busiest > 8) fail(`20 peers: ${most()} in flight at once, ${busiest} starts in a minute`);
  if (last > 20 * 2 * 1.3 || last < 20 * 2 * 0.7) fail(`20 peers at the cap: ${last} retries in 10 min, want about 40`);
  r.close();
  console.log(`20 peers, all failing, 1 h: ${started.length} retries, at most ${busiest} in a minute, never two at once; ${last} in the last 10 min (about 1 per peer per 5 min)`);
}

console.log('retry backoff: all passed');
