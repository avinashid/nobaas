// What the hooks keep per Room: its member list, our presence, shared keys (last writer wins) and each peer's
// latest value per key. One store per Room, made on first use. Shared and per-peer state travel as JSON
// messages with a `$peerwire` field, which useMessages and useRoomEvent hide.
//
// Every member is trusted alike, as in the room itself: any member can write any shared key. What a member
// sends is still untrusted input, so it is checked and capped here before it reaches the app.
import { isPeerId } from '@peerwire/js';
import type { PeerId, Presence, Room } from '@peerwire/js';

/** Shared keys a store keeps. */
export const MAX_KEYS = 1024;
/** The longest key, in UTF-16 code units. */
export const MAX_KEY_LENGTH = 256;
/** A per-peer value goes on the unreliable lane, then once more on the reliable one when it has been still
 * this long, so the last value of a burst always arrives. */
export const SETTLE_MS = 250;
/** The largest value, as JSON text: one from a member past this is ignored, and setting one throws. */
export const MAX_VALUE_LENGTH = 64 * 1024;
/** What a store holds of shared values in all, as JSON text. */
export const MAX_STORE_LENGTH = 1024 * 1024;
/** Per-peer keys each member can have, ours included. */
export const MAX_PEER_KEYS = 64;
/** What each member's per-peer values can take in all, as JSON text, ours included. */
export const MAX_PEER_LENGTH = 256 * 1024;
/** How far ahead of ours another member's clock can be, in milliseconds: a shared write stamped later than
 * that is ignored. */
export const MAX_CLOCK_DRIFT = 5 * 60 * 1000;
/** Catch-up asks a store answers per member while it stays. */
const MAX_ANSWERS = 4;
/** Entries per catch-up message: values, then versions in a digest. */
const CHUNK = 64;
const DIGEST_CHUNK = 256;

export interface Peer<P = unknown> {
  id: PeerId;
  presence: P;
}

// A shared value and its version: the clock `c`, then the writer `w`, orders writes. `len`: the value's length
// as JSON text.
type Shared = { v: unknown; c: number; w: PeerId; len: number };
type Version = { c: number; w: PeerId };
type Live = { v: unknown; n: number; len: number };
type Entry = unknown[];
type Parts = { s?: Entry[]; p?: Entry[]; h?: Entry[] };
// One member's catch-up with us while it stays: the entries we have sent it (a newer one goes again), the
// versions it has told us it holds since its last ask, whether it holds one we lack, whether it has answered
// us, and the asks we have answered.
type Catchup = {
  sentShared: Map<string, Shared>;
  sentLive: Map<string, Live>;
  told: Map<string, Version>;
  more: boolean;
  answered: boolean;
  answers: number;
};

const EMPTY: Readonly<Record<PeerId, unknown>> = Object.freeze({});

/** True for the messages this package sends for its own state. */
export const isInternal = (data: unknown): boolean =>
  typeof data === 'object' && data !== null && typeof (data as { $peerwire?: unknown }).$peerwire === 'string';

const validKey = (k: unknown): k is string => typeof k === 'string' && k.length <= MAX_KEY_LENGTH;
const counter = (n: unknown): n is number => Number.isSafeInteger(n) && (n as number) >= 1;
// A member's clock for a shared write: milliseconds, no further ahead of ours than MAX_CLOCK_DRIFT.
const stamp = (c: unknown): c is number => counter(c) && c <= Date.now() + MAX_CLOCK_DRIFT;
// Whether version `a` wins over `b`: the later clock, then the higher writer id. Every member compares alike.
const newer = (a: Version, b: Version): boolean => a.c > b.c || (a.c === b.c && a.w > b.w);
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
  private sharedLength = 0;
  private live = new Map<string, Map<PeerId, Live>>();
  private liveViews = new Map<string, Readonly<Record<PeerId, unknown>>>();
  private liveUse = new Map<PeerId, { keys: number; len: number }>();
  private mine = new Map<string, Live>();
  private mineLength = 0;
  private settling = new Map<string, ReturnType<typeof setTimeout>>();
  private listeners = new Map<string, Set<() => void>>();
  private peerList: readonly Peer<Presence>[] | null = null;
  private catchups = new Map<PeerId, Catchup>();
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
      if (this.started) this.hello(peer);
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
    for (const peer of this.room.peers) this.hello(peer);
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

  peers(): readonly Peer<Presence>[] {
    return (this.peerList ??= Object.freeze(this.room.peers.map((id) => ({ id, presence: this.room.getPresence(id) }))));
  }

  presence(): Presence | undefined {
    return this.room.getPresence();
  }

  setPresence(presence: Presence): void {
    this.room.setPresence(presence);
    this.notify('self');
  }

  // Shared keys: one value per key for the whole room. A write carries a hybrid clock, wall time in milliseconds
  // or one past the value it replaces if that is later. The later clock wins, then the higher writer id, so
  // every member settles on the same value whatever order writes arrive in. A clock further ahead of ours than
  // MAX_CLOCK_DRIFT is ignored, so no member can stamp a key out of reach.

  getShared(key: string): { v: unknown } | undefined {
    return this.shared.get(key);
  }

  setShared(key: string, value: unknown): void {
    if (!validKey(key)) throw new RangeError(`shared state key longer than ${MAX_KEY_LENGTH}`);
    const cur = this.shared.get(key);
    const c = Math.max(Date.now(), (cur?.c ?? 0) + 1);
    const { v, len } = asJson(value);
    if (!this.makeRoom(key, len, cur, true)) {
      throw new RangeError(`shared state is full: ${MAX_KEYS} keys or ${MAX_STORE_LENGTH} characters as JSON`);
    }
    this.send({ $peerwire: 'set', k: key, v, c });
    this.putShared(key, { v, c, w: this.room.self, len }, cur);
  }

  private merge(k: unknown, v: unknown, c: unknown, w: PeerId): void {
    if (!validKey(k) || !stamp(c)) return;
    const cur = this.shared.get(k);
    if (cur && !newer({ c, w }, cur)) return;
    const value = v ?? null;
    const len = jsonLength(value);
    if (!this.makeRoom(k, len, cur, this.listeners.has(`s:${k}`))) return;
    this.putShared(k, { v: value, c, w, len }, cur);
  }

  private putShared(key: string, e: Shared, cur: Shared | undefined): void {
    this.sharedLength += e.len - (cur?.len ?? 0);
    this.shared.set(key, e);
    this.notify(`s:${key}`);
  }

  // Whether a value of `len` can take `key` in place of `cur` within MAX_KEYS and MAX_STORE_LENGTH. When the
  // store is full, a key the app uses (a hook reads it, or we write it) pushes out keys no hook reads, oldest
  // first, so a member can't fill the store to keep the app's own keys out.
  private makeRoom(key: string, len: number, cur: Shared | undefined, used: boolean): boolean {
    if (len > MAX_VALUE_LENGTH) return false;
    let keys = this.shared.size + (cur ? 0 : 1) - MAX_KEYS;
    let chars = this.sharedLength - (cur?.len ?? 0) + len - MAX_STORE_LENGTH;
    if (keys <= 0 && chars <= 0) return true;
    if (!used) return false;
    const out: string[] = [];
    for (const [k, e] of this.shared) {
      if (keys <= 0 && chars <= 0) break;
      if (k === key || this.listeners.has(`s:${k}`)) continue;
      out.push(k);
      keys--;
      chars -= e.len;
    }
    if (keys > 0 || chars > 0) return false;
    for (const k of out) {
      this.sharedLength -= this.shared.get(k)!.len;
      this.shared.delete(k);
      for (const cu of this.catchups.values()) cu.sentShared.delete(k);
    }
    return true;
  }

  // Per-peer values: each member's latest value per key, such as a cursor. Numbered per sender, so a datagram
  // that arrives late never overwrites a newer value. Each member has its own quota, so one can't crowd out
  // the others.

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
    const cur = this.mine.get(key);
    if (!cur && this.mine.size >= MAX_PEER_KEYS) throw new RangeError(`at most ${MAX_PEER_KEYS} per-peer keys`);
    const e = { ...asJson(value), n: ++this.seq };
    if (this.mineLength - (cur?.len ?? 0) + e.len > MAX_PEER_LENGTH) {
      throw new RangeError(`per-peer values of more than ${MAX_PEER_LENGTH} characters as JSON in all`);
    }
    const m = { $peerwire: 'live', k: key, v: e.v, n: e.n };
    this.mineLength += e.len - (cur?.len ?? 0);
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
    const cur = byPeer?.get(peer);
    if (cur && n <= cur.n) return;
    const use = this.liveUse.get(peer) ?? { keys: 0, len: 0 };
    const value = v ?? null;
    const len = jsonLength(value);
    if (len > MAX_VALUE_LENGTH || use.len - (cur?.len ?? 0) + len > MAX_PEER_LENGTH) return;
    if (!cur && use.keys >= MAX_PEER_KEYS) return;
    if (!cur) use.keys++;
    use.len += len - (cur?.len ?? 0);
    this.liveUse.set(peer, use);
    if (!byPeer) this.live.set(k, (byPeer = new Map()));
    byPeer.set(peer, { v: value, n, len });
    this.liveViews.delete(k);
    this.notify(`p:${k}`);
  }

  private dropPeer(peer: PeerId): void {
    this.catchups.delete(peer);
    this.liveUse.delete(peer);
    for (const [k, byPeer] of this.live) {
      if (!byPeer.delete(peer)) continue;
      if (!byPeer.size) this.live.delete(k);
      this.liveViews.delete(k);
      this.notify(`p:${k}`);
    }
  }

  // Catching up. When a member joins, or our store starts, we send it a digest of the shared versions we hold
  // and ask. An ask is answered with the shared values its digest lacks and our per-peer values, leaving out
  // any already sent to that member, so a join costs each value once. The answer asks back when the asker
  // holds a version we lack or hasn't answered us yet: room.js can lose the first message to a newcomer (it
  // lists the peer before our presence goes), and that may have been our ask. An answer never is: it follows
  // an ask, so our presence has gone by then.

  private catchup(peer: PeerId): Catchup {
    let cu = this.catchups.get(peer);
    if (!cu) {
      cu = { sentShared: new Map(), sentLive: new Map(), told: new Map(), more: false, answered: false, answers: 0 };
      this.catchups.set(peer, cu);
    }
    return cu;
  }

  private digest(): Entry[] {
    return [...this.shared].map(([k, e]) => [k, e.c, e.w]);
  }

  private hello(peer: PeerId): void {
    this.sendSync(peer, { h: this.digest() }, true, false);
  }

  // The digest when it asks back holds only the versions whose values it doesn't carry: the asker takes those
  // as told, like a digest.
  private answer(peer: PeerId, cu: Catchup): void {
    const s: Entry[] = [];
    const h: Entry[] = [];
    for (const [k, e] of this.shared) {
      const t = cu.told.get(k);
      if (cu.sentShared.get(k) === e || (t && !newer(e, t))) {
        h.push([k, e.c, e.w]);
        continue;
      }
      s.push([k, e.v, e.c, e.w]);
      cu.sentShared.set(k, e);
    }
    const p: Entry[] = [];
    for (const [k, e] of this.mine) {
      if (cu.sentLive.get(k) === e) continue;
      p.push([k, e.v, e.n]);
      cu.sentLive.set(k, e);
    }
    const ask = cu.more || !cu.answered;
    cu.told.clear();
    cu.more = false;
    this.sendSync(peer, ask ? { s, p, h } : { s, p }, ask, true);
  }

  // A version the member holds, from its digest or a value it sent: the answer to it leaves out what isn't
  // newer. A version from its digest that we lack makes the answer ask back.
  private tell(cu: Catchup, k: unknown, c: unknown, w: PeerId, digest: boolean): void {
    if (!validKey(k) || !stamp(c)) return;
    const cur = this.shared.get(k);
    if (!cur || newer({ c, w }, cur)) {
      if (digest) cu.more = true;
    } else if (cu.told.size < MAX_KEYS || cu.told.has(k)) {
      cu.told.set(k, { c, w });
    }
  }

  // Sends `parts` to `peer` in messages of at most CHUNK values or DIGEST_CHUNK versions, `ask` on the last.
  // `re` marks an answer.
  private sendSync(peer: PeerId, parts: Parts, ask: boolean, re: boolean): void {
    const msgs: Parts[] = [];
    for (const [f, size] of [['s', CHUNK], ['p', CHUNK], ['h', DIGEST_CHUNK]] as const) {
      const list = parts[f] ?? [];
      for (let i = 0; i < list.length; i += size) msgs.push({ [f]: list.slice(i, i + size) });
    }
    if (!msgs.length) {
      if (!ask) return;
      msgs.push({});
    }
    msgs.forEach((m, i) => this.sendPart(peer, m, ask && i === msgs.length - 1, re));
  }

  private sendPart(peer: PeerId, m: Parts, ask: boolean, re: boolean): void {
    try {
      this.send({ $peerwire: 'sync', ask, re, ...m }, peer);
    } catch (err) {
      if (!(err instanceof RangeError)) throw err;
      // Too large together: one entry per message. One that is too large alone can't be sent at all.
      const one: Parts[] = Object.entries(m).flatMap(([f, list]: [string, Entry[]]) => list.map((e) => ({ [f]: [e] })));
      if (one.length > 1) one.forEach((o, i) => this.sendPart(peer, o, ask && i === one.length - 1, re));
    }
  }

  private receive(peer: PeerId, m: Record<string, unknown>): void {
    if (m.$peerwire === 'set') this.merge(m.k, m.v, m.c, peer);
    else if (m.$peerwire === 'live') this.mergeLive(peer, m.k, m.v, m.n);
    else if (m.$peerwire === 'sync') {
      const { s = [], p = [], h = [] } = m;
      if (![s, p, h].every((a) => Array.isArray(a) && a.length <= MAX_KEYS)) return;
      const cu = this.catchup(peer);
      if (m.re === true) cu.answered = true;
      for (const e of s as unknown[]) {
        if (!Array.isArray(e) || e.length !== 4 || !isPeerId(e[3])) continue;
        this.merge(e[0], e[1], e[2], e[3]);
        this.tell(cu, e[0], e[2], e[3], false);
      }
      for (const e of p as unknown[]) if (Array.isArray(e) && e.length === 3) this.mergeLive(peer, e[0], e[1], e[2]);
      for (const e of h as unknown[]) if (Array.isArray(e) && e.length === 3 && isPeerId(e[2])) this.tell(cu, e[0], e[1], e[2], true);
      if (m.ask === true && this.started && cu.answers < MAX_ANSWERS) {
        cu.answers++;
        this.answer(peer, cu);
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
