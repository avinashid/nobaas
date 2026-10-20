// Browser arena over Nobaas: one super-peer tab (?role=super) and any number of
// player tabs. The same protocol as nobaas-sim/examples/arena.rs:
//   player -> super  INPUT: this tick's input plus the 7 before (unreliable)
//   super -> player  BUNDLE: the island's latest inputs, every tick (unreliable)
//   super -> player  WELCOME / ROSTER (reliable): id, island members, join ticks
//   NACK (reliable) for a gap redundancy didn't cover, both ways: players ask
//   the super-peer (which answers from its per-member history), and the
//   super-peer asks a player for its own inputs it never received
// Players run input-only lockstep with integer physics, so every member of an
// island computes the same state; checkpoint hashes let a test compare them.

import { loadNobaas, Lane } from '../nobaas.js';
import { WebRtcTransport, BroadcastSignaling } from '../webrtc.js';
import { NostrSignaling } from '../nostr.js';

const HZ = 60;
const TICK_MS = 1000 / HZ;
const ISLAND = 12;
const REDUNDANCY = 8;
const ARENA = 480;
const JOIN_DELAY = 30; // ticks between being admitted and the first input
const UNKNOWN = 0xff; // input byte for a tick the super-peer doesn't have
const MAX_RUN = 64; // ticks per bundle entry
const params = new URLSearchParams(location.search);
const role = params.get('role') === 'super' ? 'super' : 'player';
const room = params.get('room') ?? 'arena';
const bot = params.get('bot') !== '0';
const drawing = params.get('draw') !== '0';

const nb = await loadNobaas(new URL('../pkg/nobaas_ffi.wasm', import.meta.url));
const ep = nb.endpoint();
// ?signal=nostr lets players on other machines join (public relays + STUN).
const nostr = params.get('signal') === 'nostr';
const sig = nostr ? new NostrSignaling(nb, ep, { room: `arena/${room}` }) : new BroadcastSignaling(ep.id, `nobaas-arena-${room}`);
const transport = new WebRtcTransport(ep, sig, {
  loss: Number(params.get('loss') ?? 0),
  ...(nostr ? { iceServers: [{ urls: 'stun:stun.l.google.com:19302' }], trickle: false } : {}),
});
const $ = (id) => document.getElementById(id);
$('role').textContent = role;
$('me').textContent = ep.id.slice(0, 12);

const u16 = (v) => [v & 255, v >> 8];
const u32 = (v) => [v & 255, (v >> 8) & 255, (v >> 16) & 255, (v >>> 24) & 255];
const rd16 = (b, o) => b[o] | (b[o + 1] << 8);
const rd32 = (b, o) => (b[o] | (b[o + 1] << 8) | (b[o + 2] << 16) | (b[o + 3] << 24)) >>> 0;

function botInput(pid, tick) {
  const phase = Math.floor((tick + pid * 7) / 30);
  let h = Math.imul(pid + 1, 0x9e3779b1) ^ Math.imul(phase + 1, 0x85ebca6b);
  h ^= h >>> 15;
  h = Math.imul(h, 0x2c1b3c6d) >>> 0;
  return [0x01, 0x02, 0x04, 0x08, 0x05, 0x09, 0x06, 0x0a][h % 8] | (h % 5 === 0 ? 0x10 : 0);
}

function spawn(pid) {
  return { x: 20 + ((pid * 97) % (ARENA - 40)), y: 20 + ((pid * 53) % (ARENA - 40)) };
}

function step(s, bits) {
  const v = bits & 0x10 ? 4 : 2;
  if (bits & 1) s.x += v;
  if (bits & 2) s.x -= v;
  if (bits & 4) s.y += v;
  if (bits & 8) s.y -= v;
  s.x = Math.max(0, Math.min(ARENA, s.x));
  s.y = Math.max(0, Math.min(ARENA, s.y));
}

if (role === 'super') runSuper();
else runPlayer();

function runSuper() {
  const epoch = Date.now() + 500;
  const tickNow = () => Math.floor((Date.now() - epoch) / TICK_MS);
  const players = new Map(); // peer -> { pid, island, joinTick }
  const byPid = new Map(); // pid -> peer
  // Every member's inputs by tick: bursts and reordering can't lose any.
  const hist = new Map(); // pid -> { bits: Map(tick -> input), newest, firstUnsent }
  const entry = (pid, from, to) => {
    const h = hist.get(pid);
    const n = to - from + 1;
    const out = [...u16(pid), ...u32(to), n];
    for (let k = 0; k < n; k++) out.push(h.bits.get(to - k) ?? UNKNOWN);
    return out;
  };
  let nextPid = 0;
  // Roster version per island. Bundles carry it, so a member that sees a newer
  // version than its roster waits for the update before confirming any tick.
  const versions = new Map();
  sig.onHello = () => sig.hello({ role: 'super' });
  sig.hello({ role: 'super' });
  ep.onEvent = (e) => {
    if (e.type === 'connected') {
      const pid = nextPid++;
      const p = { pid, island: Math.floor(pid / ISLAND), joinTick: Math.max(0, tickNow()) + JOIN_DELAY };
      // A late joiner needs the island's state at its join tick from a member.
      const existing = members(p.island).sort((a, b) => a[1].pid - b[1].pid);
      players.set(e.peer, p);
      byPid.set(pid, e.peer);
      const w = new Uint8Array(new ArrayBuffer(16));
      w[0] = 0x57;
      new DataView(w.buffer).setFloat64(1, epoch, true);
      w.set([...u16(pid), ...u32(p.joinTick), existing.length ? 1 : 0], 9);
      ep.send(e.peer, w, Lane.Reliable);
      versions.set(p.island, (versions.get(p.island) ?? 0) + 1);
      roster(p.island);
      if (existing.length) ep.send(existing[0][0], Uint8Array.from([0x53, ...u16(pid), ...u32(p.joinTick)]), Lane.Reliable);
    } else if (e.type === 'disconnected') {
      players.delete(e.peer);
    } else if (e.type === 'message' && e.data[0] === 0x54) {
      const to = byPid.get(rd16(e.data, 1));
      if (to) ep.send(to, e.data, Lane.Reliable);
    } else if (e.type === 'message' && e.data[0] === 0x49) {
      const pid = rd16(e.data, 1);
      const tk = rd32(e.data, 3);
      const join = players.get(e.peer)?.joinTick ?? 0;
      if (!hist.has(pid)) hist.set(pid, { bits: new Map(), newest: -1, firstUnsent: join, contig: join, lastNack: 0 });
      const h = hist.get(pid);
      for (let k = 0; k < e.data[7]; k++) if (tk >= k) h.bits.set(tk - k, e.data[8 + k]);
      h.newest = Math.max(h.newest, tk);
      while (h.bits.has(h.contig)) h.contig++;
      for (const t of h.bits.keys()) if (t < h.newest - 1024) h.bits.delete(t);
      // A hole older than the redundancy window won't fill by itself: ask the player.
      if (h.contig < h.newest - REDUNDANCY && performance.now() - h.lastNack > 150) {
        let to = h.contig;
        while (to < h.newest && !h.bits.has(to + 1)) to++;
        ep.send(e.peer, Uint8Array.from([0x4e, ...u16(pid), ...u32(h.contig), ...u32(to)]), Lane.Reliable);
        h.lastNack = performance.now();
        uplinkNacks++;
      }
    } else if (e.type === 'message' && e.data[0] === 0x4e) {
      // NACK pid from to: answer from history, in runs of MAX_RUN.
      const pid = rd16(e.data, 1);
      const [from, to] = [rd32(e.data, 3), rd32(e.data, 7)];
      if (!hist.has(pid)) return;
      for (let a = from; a <= to; a += MAX_RUN) {
        const b = Math.min(to, a + MAX_RUN - 1);
        const p = players.get(e.peer);
        ep.send(e.peer, Uint8Array.from([0x42, 1, ...u16(versions.get(p?.island) ?? 0), ...entry(pid, a, b)]), Lane.Reliable);
      }
      repairs++;
    }
  };
  function members(island) {
    return [...players.entries()].filter(([, p]) => p.island === island);
  }
  function roster(island) {
    const ms = members(island);
    const r = [0x52, ...u16(versions.get(island) ?? 0), ms.length];
    for (const [, p] of ms) r.push(...u16(p.pid), ...u32(p.joinTick));
    for (const [peer] of ms) ep.send(peer, Uint8Array.from(r), Lane.Reliable);
  }
  let sentBundles = 0;
  let repairs = 0;
  let uplinkNacks = 0;
  const t0 = performance.now();
  function tick() {
    const t = tickNow();
    const islands = new Set([...players.values()].map((p) => p.island));
    for (const k of islands) {
      const ms = members(k);
      const parts = [];
      for (const [, p] of ms) {
        const h = hist.get(p.pid);
        if (!h || h.newest < 0 || (h.newest + REDUNDANCY <= t && h.firstUnsent > h.newest)) continue;
        // Everything not yet sent, plus the last REDUNDANCY ticks again.
        const from = Math.max(Math.min(h.firstUnsent, h.newest - REDUNDANCY + 1), h.newest - MAX_RUN + 1, 0);
        parts.push(entry(p.pid, from, h.newest));
        h.firstUnsent = h.newest + 1;
      }
      if (!parts.length) continue;
      const out = Uint8Array.from([0x42, parts.length, ...u16(versions.get(k) ?? 0), ...parts.flat()]);
      for (const [peer] of ms) ep.send(peer, out, Lane.Unreliable);
      sentBundles += ms.length;
    }
    setTimeout(tick, Math.max(0, epoch + (t + 1) * TICK_MS - Date.now()));
  }
  setTimeout(tick, Math.max(0, epoch - Date.now()));
  setInterval(() => {
    const s = ep.stats();
    $('hud').textContent = `super-peer: repairs ${repairs}, uplink nacks ${uplinkNacks}; ${players.size} players in ${new Set([...players.values()].map((p) => p.island)).size} islands; bundles/s ${(sentBundles / ((performance.now() - t0) / 1000)).toFixed(0)}; packets out ${s.txPackets}`;
  }, 500);
  window.arena = { role, id: ep.id, players: () => players.size, stats: () => ep.stats(), repairs: () => repairs, uplinkNacks: () => uplinkNacks };
}

function runPlayer() {
  let superPeer = null;
  let me = null; // { pid, joinTick, epoch }
  let roster = new Map(); // pid -> joinTick
  let rosterVersion = -1;
  let seenVersion = 0;
  const known = new Map(); // pid -> Map(tick -> bits)
  const contiguous = new Map(); // pid -> next tick not yet known
  let confirmed = 0; // all ticks < confirmed are known for the whole island
  let simTick = 0;
  let base = 0; // first tick this player simulates (its join, or the snapshot's tick)
  let waitingSnapshot = false;
  const snapRequests = []; // { pid, tick } to serve
  const snapshotAt = (tick) => {
    const out = [0x54, 0, 0, ...u32(tick), state.size];
    for (const [pid, st] of [...state].sort((a, b) => a[0] - b[0])) out.push(...u16(pid), ...u16(st.x), ...u16(st.y));
    return out;
  };
  const state = new Map(); // pid -> {x, y} at simTick
  const hashes = new Map(); // tick -> hash, every 60 ticks
  const lags = [];
  const keys = new Set();
  addEventListener('keydown', (e) => keys.add(e.key));
  addEventListener('keyup', (e) => keys.delete(e.key));

  sig.onHello = (other, meta) => {
    if (meta?.role === 'super' && !superPeer) {
      superPeer = other;
      transport.dial(other);
    }
  };
  sig.hello({ role: 'player' });
  const tickTime = (t) => me.epoch + t * TICK_MS;

  const newest = new Map(); // pid -> highest tick known
  const lastNack = new Map(); // pid -> time of the last repair request
  let nacks = 0;
  function learn(pid, tk, bits) {
    if (bits === UNKNOWN) return;
    newest.set(pid, Math.max(newest.get(pid) ?? -1, tk));
    if (!known.has(pid)) known.set(pid, new Map());
    known.get(pid).set(tk, bits);
    let c = contiguous.get(pid) ?? Math.max(roster.get(pid) ?? tk, base);
    const m = known.get(pid);
    while (m.has(c)) c++;
    contiguous.set(pid, c);
  }
  // Asks the super-peer for gaps that redundancy didn't cover.
  function repair() {
    const now = performance.now();
    for (const [pid, join] of roster) {
      if (pid === me.pid) continue;
      const c = Math.max(contiguous.get(pid) ?? Math.max(join, base), join);
      const top = newest.get(pid) ?? -1;
      if (top - c < REDUNDANCY || now - (lastNack.get(pid) ?? 0) < 150) continue;
      let to = c;
      while (to < top && !known.get(pid)?.has(to + 1)) to++;
      ep.send(superPeer, Uint8Array.from([0x4e, ...u16(pid), ...u32(c), ...u32(to)]), Lane.Reliable);
      lastNack.set(pid, now);
      nacks++;
    }
  }
  function advance() {
    if (!me || roster.size === 0 || waitingSnapshot) return;
    repair();
    if (seenVersion > rosterVersion) return; // a membership change is on its way
    let c = Infinity;
    for (const [pid, join] of roster) c = Math.min(c, Math.max(contiguous.get(pid) ?? Math.max(join, base), join));
    if (!Number.isFinite(c) || c <= confirmed) return;
    const now = Date.now();
    for (let t = confirmed; t < c; t++) if (t >= me.joinTick + HZ) lags.push(now - tickTime(t));
    if (lags.length > 20000) lags.splice(0, lags.length - 20000);
    confirmed = c;
    // Run the lockstep simulation up to the confirmed tick.
    for (; simTick < confirmed; simTick++) {
      for (let i = snapRequests.length - 1; i >= 0; i--) {
        if (snapRequests[i].tick !== simTick) continue;
        const out = snapshotAt(simTick);
        out.splice(1, 2, ...u16(snapRequests[i].pid));
        ep.send(superPeer, Uint8Array.from(out), Lane.Reliable);
        snapRequests.splice(i, 1);
      }
      for (const [pid, join] of roster) {
        if (simTick < join) continue;
        if (simTick === join) state.set(pid, spawn(pid));
        step(state.get(pid), known.get(pid).get(simTick));
      }
      if ((simTick + 1) % HZ === 0) {
        let h = 2166136261;
        for (const pid of [...roster.keys()].sort((a, b) => a - b)) {
          const s = state.get(pid);
          if (!s) continue;
          for (const v of [pid, s.x, s.y]) h = Math.imul(h ^ v, 16777619) >>> 0;
        }
        hashes.set(simTick + 1, h);
      }
    }
    // Forget inputs nobody needs any more.
    for (const m of known.values()) for (const t of m.keys()) if (t < confirmed - REDUNDANCY * 4) m.delete(t);
  }

  ep.onEvent = (e) => {
    if (e.type !== 'message') return;
    const b = e.data;
    if (b[0] === 0x57) {
      me = { epoch: new DataView(b.buffer, b.byteOffset).getFloat64(1, true), pid: rd16(b, 9), joinTick: rd32(b, 11) };
      confirmed = simTick = base = me.joinTick;
      waitingSnapshot = b[15] === 1;
      startTicking();
    } else if (b[0] === 0x4e && me && rd16(b, 1) === me.pid) {
      // The super-peer lost some of our inputs: resend them, reliably.
      const [from, to] = [rd32(b, 3), rd32(b, 7)];
      for (let a = from; a <= to; a += MAX_RUN) {
        const top = Math.min(to, a + MAX_RUN - 1);
        const m = [0x49, ...u16(me.pid), ...u32(top), top - a + 1];
        for (let t = top; t >= a; t--) m.push(mine.get(t) ?? 0);
        ep.send(superPeer, Uint8Array.from(m), Lane.Reliable);
      }
    } else if (b[0] === 0x53) {
      snapRequests.push({ pid: rd16(b, 1), tick: rd32(b, 3) });
      advance();
    } else if (b[0] === 0x54) {
      const tick = rd32(b, 3);
      state.clear();
      for (let i = 0; i < b[7]; i++) {
        const o = 8 + i * 6;
        state.set(rd16(b, o), { x: rd16(b, o + 2), y: rd16(b, o + 4) });
      }
      confirmed = simTick = base = tick;
      for (const pid of contiguous.keys()) {
        let c = Math.max(contiguous.get(pid), tick);
        while (known.get(pid)?.has(c)) c++;
        contiguous.set(pid, c);
      }
      waitingSnapshot = false;
      advance();
    } else if (b[0] === 0x52) {
      const ver = rd16(b, 1);
      if (ver <= rosterVersion) return;
      const r = new Map();
      for (let i = 0; i < b[3]; i++) r.set(rd16(b, 4 + i * 6), rd32(b, 6 + i * 6));
      // A newcomer joins at a future tick, so the confirmed past stays valid.
      roster = r;
      rosterVersion = ver;
      seenVersion = Math.max(seenVersion, ver);
      advance();
    } else if (b[0] === 0x42) {
      seenVersion = Math.max(seenVersion, rd16(b, 2));
      let o = 4;
      for (let i = 0; i < b[1]; i++) {
        const pid = rd16(b, o);
        const tk = rd32(b, o + 2);
        const n = b[o + 6];
        for (let k = 0; k < n; k++) if (tk >= k) learn(pid, tk - k, b[o + 7 + k]);
        o += 7 + n;
      }
      advance();
    }
  };

  const mine = new Map(); // tick -> own input, kept for repair requests
  function myInput(t) {
    if (!bot || keys.size) {
      let bits = 0;
      if (keys.has('ArrowRight')) bits |= 1;
      if (keys.has('ArrowLeft')) bits |= 2;
      if (keys.has('ArrowDown')) bits |= 4;
      if (keys.has('ArrowUp')) bits |= 8;
      if (keys.has(' ')) bits |= 0x10;
      return bits;
    }
    return botInput(me.pid, t);
  }
  function startTicking() {
    // Start at the join tick even if the welcome arrived late: the island
    // cannot confirm any tick this player has no input for, so ticks that
    // already passed are sent now, as idle input.
    let t = me.joinTick;
    function tick() {
      const now = Math.floor((Date.now() - me.epoch) / TICK_MS);
      for (; t <= now; t++) {
        mine.set(t, t < now - 1 ? 0 : myInput(t));
        mine.delete(t - 1024);
        learn(me.pid, t, mine.get(t));
        const n = Math.min(REDUNDANCY, t - me.joinTick + 1);
        const m = [0x49, ...u16(me.pid), ...u32(t), n];
        for (let k = 0; k < n; k++) m.push(mine.get(t - k));
        ep.send(superPeer, Uint8Array.from(m), Lane.Unreliable);
      }
      advance();
      setTimeout(tick, Math.max(0, tickTime(t) - Date.now()));
    }
    tick();
  }

  const ctx = $('view').getContext('2d');
  function draw() {
    ctx.fillStyle = '#10141a';
    ctx.fillRect(0, 0, ARENA, ARENA);
    for (const [pid, s] of state) {
      ctx.fillStyle = `hsl(${(pid * 47) % 360} 80% 60%)`;
      ctx.beginPath();
      ctx.arc(s.x, s.y, pid === me?.pid ? 9 : 6, 0, Math.PI * 2);
      ctx.fill();
    }
    requestAnimationFrame(draw);
  }
  if (drawing) requestAnimationFrame(draw);
  const pct = (q) => {
    const v = [...lags].sort((a, b) => a - b);
    return v.length ? v[Math.min(v.length - 1, Math.round((v.length - 1) * q))] : NaN;
  };
  setInterval(() => {
    if (!me) return;
    const now = Math.floor((Date.now() - me.epoch) / TICK_MS);
    $('hud').textContent = `player ${me.pid}, island ${Math.floor(me.pid / ISLAND)} with ${roster.size} players; confirmed tick ${confirmed} (now ${now}, rollback depth ${now - confirmed}); confirm lag p50 ${pct(0.5).toFixed(1)} ms, p99 ${pct(0.99).toFixed(1)} ms; RTT ${ep.rttMs(superPeer)?.toFixed(1)} ms`;
  }, 250);
  window.arena = {
    role,
    id: ep.id,
    pid: () => me?.pid,
    joined: () => !!me && roster.size > 0,
    island: () => (me ? Math.floor(me.pid / ISLAND) : null),
    rosterSize: () => roster.size,
    waiting: () => waitingSnapshot,
    nacks: () => nacks,
    debug: () => [...roster].map(([pid, join]) => [pid, join, contiguous.get(pid) ?? null, known.get(pid)?.size ?? 0]),
    rtt: () => ep.rttMs(superPeer),
    confirmed: () => confirmed,
    hashes: () => Object.fromEntries(hashes),
    lag: () => ({ p50: pct(0.5), p99: pct(0.99), n: lags.length }),
  };
}
