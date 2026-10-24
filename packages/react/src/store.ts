// What the hooks keep per Room: its member list, our presence, shared keys (last writer wins) and each peer's
// latest value per key. One store per Room, made on first use. Shared and per-peer state travel as JSON
// messages with a `$peerwire` field, which useMessages and useRoomEvent hide.
//
// Every member is trusted alike, as in the room itself: any member can write any shared key. What a member
// sends is still untrusted input, so it is checked and capped here before it reaches the app.
import { isPeerId } from '@peerwire/js';
import type { PeerId, Presence, Room } from '@peerwire/js';

/** Shared keys a store keeps, and per-peer keys: a new key past this from a peer is ignored. */
export const MAX_KEYS = 1024;
/** The longest key, in UTF-16 code units. */
export const MAX_KEY_LENGTH = 256;
/** A per-peer value goes on the unreliable lane, then once more on the reliable one when it has been still
 * this long, so the last value of a burst always arrives. */
export const SETTLE_MS = 250;
/** The largest value, as JSON text: one from a member past this is ignored, and setting one throws. */
export const MAX_VALUE_LENGTH = 64 * 1024;
/** What a store holds of shared and per-peer values in all, as JSON text: past this, a member's new value is
 * ignored. With MAX_KEYS, it bounds the memory members can make a store use. */
export const MAX_STORE_LENGTH = 16 * 1024 * 1024;
/** Catch-ups a store answers per member while it stays, so a member can't make it send its state over and over. */
const MAX_ANSWERS = 4;
/** Entries per catch-up message. */
const CHUNK = 64;

export interface Peer<P = Presence> {
  id: PeerId;
  presence: P;
}

// `len`: the value's length as JSON text.
type Shared = { v: unknown; c: number; w: PeerId; len: number };
type Live = { v: unknown; n: number; len: number };
type Entry = unknown[];

const EMPTY: Readonly<Record<PeerId, unknown>> = Object.freeze({});

/** True for the messages this package sends for its own state. */
export const isInternal = (data: unknown): boolean =>
  typeof data === 'object' && data !== null && typeof (data as { $peerwire?: unknown }).$peerwire === 'string';

const validKey = (k: unknown): k is string => typeof k === 'string' && k.length <= MAX_KEY_LENGTH;
const counter = (n: unknown): n is number => Number.isSafeInteger(n) && (n as number) >= 1;
// What a peer will see: values go as JSON, so ours are kept as JSON gives them back.
function asJson(value: unknown): { v: unknown; len: number } {
  const text = JSON.stringify(value === undefined ? null : value);
  if (text.length > MAX_VALUE_LENGTH) throw new RangeError(`value of ${text.length} characters as JSON; at most ${MAX_VALUE_LENGTH}`);
  return { v: JSON.parse(text), len: text.length };
}
// The length of a value a member sent (already parsed from JSON, so it serializes).
const jsonLength = (v: unknown): number => JSON.stringify(v).length;

export class RoomStore {
  readonly room: Room;
  private shared = new Map<string, Shared>();
  private live = new Map<string, Map<PeerId, Live>>();
  private liveViews = new Map<string, Readonly<Record<PeerId, unknown>>>();
  private mine = new Map<string, Live>();
  private settling = new Map<string, ReturnType<typeof setTimeout>>();
  private listeners = new Map<string, Set<() => void>>();
  private peerList: readonly Peer[] | null = null;
  private answers = new Map<PeerId, number>();
  private held = 0;
  private clock = 0;
  private seq = 0;
  private started = false;
  private closed = false;

  constructor(room: Room) {
    this.room = room;
    const peersChanged = () => {
      this.peerList = null;
      this.notify('peers');
    };
    room.on('peer-join', (peer) => {
      peersChanged();
      if (this.started) this.sync(peer, true);
    });
    room.on('presence', peersChanged);
    room.on('peer-leave', (peer) => {
      peersChanged();
      this.dropPeer(peer);
    });
    room.on('message', (data, peer) => {
      if (isInternal(data)) this.receive(peer, data);
    });
  }

  /** Starts shared and per-peer state: catches up with every member, now and as they join. */
  start(): void {
    if (this.started || this.closed) return;
    this.started = true;
    for (const peer of this.room.peers) this.sync(peer, true);
  }

  /** The room is being left: no more sends or timers. */
  close(): void {
    this.closed = true;
    for (const t of this.settling.values()) clearTimeout(t);
    this.settling.clear();
  }

  subscribe(channel: string, fn: () => void): () => void {
    let set = this.listeners.get(channel);
    if (!set) this.listeners.set(channel, (set = new Set()));
    set.add(fn);
    return () => {
      set.delete(fn);
      if (!set.size) this.listeners.delete(channel);
    };
  }

  private notify(channel: string): void {
    for (const fn of this.listeners.get(channel) ?? []) fn();
  }

  // Members.

  peers(): readonly Peer[] {
    return (this.peerList ??= Object.freeze(this.room.peers.map((id) => ({ id, presence: this.room.getPresence(id) }))));
  }

  presence(): Presence | undefined {
    return this.room.getPresence();
  }

  setPresence(presence: Presence): void {
    this.room.setPresence(presence);
    this.notify('self');
  }

  // Shared keys: one value per key for the whole room. A write carries a Lamport clock; the higher clock wins,
  // then the higher writer id, so every member settles on the same value whatever order writes arrive in.

  getShared(key: string): { v: unknown } | undefined {
    return this.shared.get(key);
  }

  setShared(key: string, value: unknown): void {
    if (!validKey(key)) throw new RangeError(`shared state key longer than ${MAX_KEY_LENGTH}`);
    const c = Math.max(this.clock, this.shared.get(key)?.c ?? 0) + 1;
    if (!counter(c)) throw new RangeError('shared state clock exhausted');
    const { v, len } = asJson(value);
    // Sent first: a value too large to send throws before anything changes here.
    this.send({ $peerwire: 'set', k: key, v, c });
    this.clock = c;
    this.held += len - (this.shared.get(key)?.len ?? 0);
    this.shared.set(key, { v, c, w: this.room.self, len });
    this.notify(`s:${key}`);
  }

  // Whether a member's value of `len` can replace one of `was` within MAX_VALUE_LENGTH and MAX_STORE_LENGTH.
  private fits(len: number, was = 0): boolean {
    return len <= MAX_VALUE_LENGTH && this.held - was + len <= MAX_STORE_LENGTH;
  }

  private merge(k: unknown, v: unknown, c: unknown, w: PeerId): void {
    if (!validKey(k) || !counter(c)) return;
    const cur = this.shared.get(k);
    if (cur ? c < cur.c || (c === cur.c && w <= cur.w) : this.shared.size >= MAX_KEYS) return;
    const value = v ?? null;
    const len = jsonLength(value);
    if (!this.fits(len, cur?.len)) return;
    this.held += len - (cur?.len ?? 0);
    this.shared.set(k, { v: value, c, w, len });
    this.clock = Math.max(this.clock, c);
    this.notify(`s:${k}`);
  }

  // Per-peer values: each member's latest value per key, such as a cursor. Numbered per sender, so a datagram
  // that arrives late never overwrites a newer value.

  getLive(key: string): Readonly<Record<PeerId, unknown>> {
    let view = this.liveViews.get(key);
    if (!view) {
      const byPeer = this.live.get(key);
      if (!byPeer) return EMPTY;
      const o: Record<PeerId, unknown> = {};
      for (const [peer, e] of byPeer) o[peer] = e.v;
      this.liveViews.set(key, (view = Object.freeze(o)));
    }
    return view;
  }

  setLive(key: string, value: unknown): void {
    if (!validKey(key)) throw new RangeError(`per-peer state key longer than ${MAX_KEY_LENGTH}`);
    if (this.closed) return;
    const e = { ...asJson(value), n: ++this.seq };
    const m = { $peerwire: 'live', k: key, v: e.v, n: e.n };
    this.mine.set(key, e);
    clearTimeout(this.settling.get(key));
    try {
      this.room.send(m, { lane: 'unreliable' });
    } catch (err) {
      // Over one datagram: reliable only.
      if (!(err instanceof RangeError)) throw err;
      this.settling.delete(key);
      return this.send(m);
    }
    this.settling.set(
      key,
      setTimeout(() => {
        this.settling.delete(key);
        try {
          if (this.mine.get(key) === e) this.send(m);
        } catch {
          // Left the room meanwhile, outside the hooks.
        }
      }, SETTLE_MS),
    );
  }

  private mergeLive(peer: PeerId, k: unknown, v: unknown, n: unknown): void {
    if (!validKey(k) || !counter(n)) return;
    let byPeer = this.live.get(k);
    if (!byPeer) {
      if (this.live.size >= MAX_KEYS) return;
      this.live.set(k, (byPeer = new Map()));
    }
    const cur = byPeer.get(peer);
    if (cur && n <= cur.n) return;
    const value = v ?? null;
    const len = jsonLength(value);
    if (!this.fits(len, cur?.len)) {
      if (!byPeer.size) this.live.delete(k);
      return;
    }
    this.held += len - (cur?.len ?? 0);
    byPeer.set(peer, { v: value, n, len });
    this.liveViews.delete(k);
    this.notify(`p:${k}`);
  }

  private dropPeer(peer: PeerId): void {
    this.answers.delete(peer);
    for (const [k, byPeer] of this.live) {
      const e = byPeer.get(peer);
      if (!e) continue;
      byPeer.delete(peer);
      this.held -= e.len;
      if (!byPeer.size) this.live.delete(k);
      this.liveViews.delete(k);
      this.notify(`p:${k}`);
    }
  }

  // Catching up. A member that joins, or starts its store, sends every member what it has and asks for
  // theirs (`ask`); the answer carries what the asker lacks. Both sides ask on `peer-join`, and the one whose
  // message arrives before its presence (dropped there) still gets an answer to the other's.

  private sync(peer: PeerId, ask: boolean): void {
    const s: Entry[] = [...this.shared].map(([k, e]) => [k, e.v, e.c, e.w]);
    const p: Entry[] = [...this.mine].map(([k, e]) => [k, e.v, e.n]);
    const msgs: { s: Entry[]; p: Entry[] }[] = [];
    for (let i = 0; i < s.length; i += CHUNK) msgs.push({ s: s.slice(i, i + CHUNK), p: [] });
    for (let i = 0; i < p.length; i += CHUNK) msgs.push({ s: [], p: p.slice(i, i + CHUNK) });
    if (!msgs.length) {
      if (!ask) return;
      msgs.push({ s: [], p: [] });
    }
    msgs.forEach((m, i) => this.sendSync(peer, m, ask && i === msgs.length - 1));
  }

  private sendSync(peer: PeerId, m: { s: Entry[]; p: Entry[] }, ask: boolean): void {
    try {
      this.send({ $peerwire: 'sync', ask, ...m }, peer);
    } catch (err) {
      if (!(err instanceof RangeError)) throw err;
      // Too large together: one entry per message. One that is too large alone can't be sent at all.
      const one = [...m.s.map((e) => ({ s: [e], p: [] })), ...m.p.map((e) => ({ s: [], p: [e] }))];
      if (one.length > 1) one.forEach((o, i) => this.sendSync(peer, o, ask && i === one.length - 1));
    }
  }

  private receive(peer: PeerId, m: Record<string, unknown>): void {
    if (m.$peerwire === 'set') this.merge(m.k, m.v, m.c, peer);
    else if (m.$peerwire === 'live') this.mergeLive(peer, m.k, m.v, m.n);
    else if (m.$peerwire === 'sync') {
      const { s, p } = m;
      if (!Array.isArray(s) || !Array.isArray(p) || s.length > MAX_KEYS || p.length > MAX_KEYS) return;
      for (const e of s) if (Array.isArray(e) && e.length === 4 && isPeerId(e[3])) this.merge(e[0], e[1], e[2], e[3]);
      for (const e of p) if (Array.isArray(e) && e.length === 3) this.mergeLive(peer, e[0], e[1], e[2]);
      const answered = this.answers.get(peer) ?? 0;
      if (m.ask === true && this.started && answered < MAX_ANSWERS) {
        this.answers.set(peer, answered + 1);
        this.sync(peer, false);
      }
    }
  }

  // Sends to every member, or to `to`; nothing once closed.
  private send(m: object, to?: PeerId): void {
    if (!this.closed) this.room.send(m, to === undefined ? undefined : { to });
  }
}

const stores = new WeakMap<Room, RoomStore>();

/** The store for `room`, made on first use. */
export function storeOf(room: Room): RoomStore {
  let s = stores.get(room);
  if (!s) stores.set(room, (s = new RoomStore(room)));
  return s;
}

/** Ends `room`'s store, if it has one, as the room is left. */
export function closeStore(room: Room): void {
  stores.get(room)?.close();
}
