'use client';
// @peerwire/react: React hooks for Peerwire rooms. Join with <RoomProvider> (or usePeerwireRoom), then read and
// write the room from any component below it. Rooms are joined in effects only, so this renders on a server too.
import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useEffect,
  useInsertionEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from 'react';
import type { ReactElement, ReactNode } from 'react';
import { joinRoom } from '@peerwire/js';
import type { JoinRoomOptions, MessageInfo, PeerId, Presence, Room, RoomData, RoomEvents, SendOptions } from '@peerwire/js';
import { closeStore, isInternal, storeOf } from './store.js';
import type { Peer } from './store.js';

export { randomRoomId } from '@peerwire/js';
export type { JoinRoomOptions, MessageInfo, PeerId, Presence, Room, RoomEvents, SendOptions } from '@peerwire/js';
export { MAX_KEYS, MAX_KEY_LENGTH, MAX_STORE_LENGTH, MAX_VALUE_LENGTH, SETTLE_MS } from './store.js';
export type { Peer } from './store.js';

/** `idle`: no options, so no room. `joining`, then `joined` or `error`. */
export type RoomStatus = 'idle' | 'joining' | 'joined' | 'error';

export interface RoomState {
  /** The joined room, or null until it is joined. */
  room: Room | null;
  status: RoomStatus;
  /** Why joining failed, with status `error`. */
  error: Error | null;
}

/** A message from useMessages. */
export interface Message<T = any> {
  /** Unique within the page, for React keys. */
  id: number;
  /** Who sent it: a member, or `room.self` for ours. */
  peer: PeerId;
  /** As sent: a string, a Uint8Array, or a JSON value. From another member it is untrusted input. */
  data: T;
  lane: MessageInfo['lane'];
  kind: MessageInfo['kind'];
  /** `Date.now()` when it arrived or was sent. */
  at: number;
  /** True for a message we sent with useMessages' `send`. */
  mine: boolean;
}

export type SetState<T> = (next: T | ((prev: T) => T)) => void;

const IDLE: RoomState = { room: null, status: 'idle', error: null };
const JOINING: RoomState = { room: null, status: 'joining', error: null };
const NO_PEERS: readonly Peer[] = Object.freeze([]);
const NO_VALUES: Readonly<Record<PeerId, any>> = Object.freeze({});
const noSubscribe = () => () => {};
let nextId = 0;

const RoomContext = createContext<RoomState | null>(null);

// When to join again: any option but `presence` changing its JSON. Functions and classes (a signalling factory,
// RTCPeerConnection) don't count and are taken from the render that joins; bytes count by content up to 1 KiB
// (a `secret`), larger ones (a `wasm` module) by identity.
const buffers = new WeakMap<object, number>();
let nextBuffer = 0;
function optionsKey(options: JoinRoomOptions): string {
  return JSON.stringify(options, function (this: unknown, k: string, v: unknown) {
    if (this === options && k === 'presence') return undefined;
    if (typeof v === 'function') return 'function';
    if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) {
      const bytes = v instanceof ArrayBuffer ? new Uint8Array(v) : new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
      if (bytes.length <= 1024) return `bytes:${Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}`;
      let id = buffers.get(v);
      if (id === undefined) buffers.set(v, (id = ++nextBuffer));
      return `buffer:${id}`;
    }
    return v;
  });
}

/**
 * Joins a room while the component is mounted, and leaves it on unmount or when an option changes (all but
 * `presence`, which is sent to the room instead). Pass null to stay out of any room. StrictMode-safe: a join
 * that finishes after its effect was cleaned up is left at once.
 */
export function usePeerwireRoom(options: JoinRoomOptions | null | undefined): RoomState {
  const key = options ? optionsKey(options) : null;
  const latest = useRef(options);
  useInsertionEffect(() => {
    latest.current = options;
  });
  const [state, setState] = useState<RoomState>(() => (options ? JOINING : IDLE));

  useEffect(() => {
    const opts = latest.current;
    if (!opts) {
      setState(IDLE);
      return;
    }
    let gone = false;
    let joined: Room | null = null;
    setState(JOINING);
    joinRoom(opts).then(
      (room) => {
        if (gone) return void room.leave();
        joined = room;
        // The presence may have changed while joining.
        const p = latest.current?.presence;
        if (p !== undefined && JSON.stringify(p) !== JSON.stringify(room.getPresence())) storeOf(room).setPresence(p);
        setState({ room, status: 'joined', error: null });
      },
      (e: unknown) => {
        if (!gone) setState({ room: null, status: 'error', error: e instanceof Error ? e : new Error(String(e)) });
      },
    );
    return () => {
      gone = true;
      if (joined) {
        closeStore(joined);
        void joined.leave();
      }
    };
  }, [key]);

  const presence = options?.presence === undefined ? undefined : JSON.stringify(options.presence);
  const { room } = state;
  useEffect(() => {
    if (room && presence !== undefined && presence !== JSON.stringify(room.getPresence())) {
      storeOf(room).setPresence(JSON.parse(presence));
    }
  }, [room, presence]);

  return state;
}

export interface RoomProviderProps extends JoinRoomOptions {
  children?: ReactNode;
}

/** Joins the room its props name, for the hooks in the components below it. */
export function RoomProvider({ children, ...options }: RoomProviderProps): ReactElement {
  const state = usePeerwireRoom(options);
  return createElement(RoomContext.Provider, { value: state }, children);
}

/** The nearest RoomProvider's room, status and error. Throws outside a RoomProvider. */
export function useRoom(): RoomState {
  const state = useContext(RoomContext);
  if (!state) throw new Error('useRoom must be used inside a <RoomProvider>');
  return state;
}

/** Which room a hook works on: `room` when given (null: none), else the nearest RoomProvider's. */
export interface RoomOption {
  room?: Room | null;
}

/**
 * A check of a value another member sent, which TypeScript also takes as the value's type, e.g.
 * `(v) => typeof v === 'string'`. Members can send any JSON, so render nothing a check hasn't passed: an object
 * where React expects text throws, and an app with no error boundary unmounts and leaves the room.
 */
export type Accept<T> = ((value: unknown) => value is T) | ((value: unknown) => boolean);

// Runs an app's check on what a member sent. One that throws counts as failed: it runs during render, so a throw
// would unmount the app.
function passes<A extends unknown[]>(check: (...args: A) => boolean, ...args: A): boolean {
  try {
    return check(...args) === true;
  } catch {
    return false;
  }
}

function useTarget(room: Room | null | undefined): Room | null {
  const state = useContext(RoomContext);
  return room === undefined ? (state?.room ?? null) : room;
}

// The latest value passed, for callbacks that shouldn't change identity with it.
function useLatest<V>(value: V): { readonly current: V } {
  const ref = useRef(value);
  useInsertionEffect(() => {
    ref.current = value;
  });
  return ref;
}

// useSharedState's check when none is given: a value of the same kind as `initial`.
function sameKind(initial: unknown): (value: unknown) => boolean {
  if (initial === null || initial === undefined) return () => true;
  if (Array.isArray(initial)) return Array.isArray;
  const t = typeof initial;
  if (t === 'object') return (v) => typeof v === 'object' && v !== null && !Array.isArray(v);
  return (v) => typeof v === t;
}

/**
 * The other members, oldest first, with their presence. With `accept`, a member whose presence fails it is left
 * out, so `presence` always has the type `accept` checks.
 */
export function usePeers<P = Presence>(options: { accept?: Accept<P> } & RoomOption = {}): readonly Peer<P>[] {
  const r = useTarget(options.room);
  const store = r ? storeOf(r) : null;
  const accept = useLatest(options.accept);
  const subscribe = useCallback((fn: () => void) => (store ? store.subscribe('peers', fn) : () => {}), [store]);
  const all = useSyncExternalStore(
    store ? subscribe : noSubscribe,
    () => (store ? store.peers() : NO_PEERS),
    () => NO_PEERS,
  );
  return useMemo(() => {
    const ok = accept.current;
    return (ok ? all.filter((p) => passes(ok, p.presence)) : all) as readonly Peer<P>[];
  }, [all, accept]);
}

/** Our presence and a setter: like useState, and every member sees the change. */
export function usePresence<P = Presence>(options: RoomOption = {}): [P | undefined, SetState<P>] {
  const r = useTarget(options.room);
  const store = r ? storeOf(r) : null;
  const subscribe = useCallback((fn: () => void) => (store ? store.subscribe('self', fn) : () => {}), [store]);
  const presence = useSyncExternalStore(
    store ? subscribe : noSubscribe,
    () => store?.presence(),
    () => undefined,
  ) as P | undefined;
  const set = useCallback<SetState<P>>(
    (next) => {
      if (!store) return;
      const prev = store.presence() as P;
      store.setPresence(typeof next === 'function' ? (next as (prev: P) => P)(prev) : next);
    },
    [store],
  );
  return [presence, set];
}

/**
 * Calls `handler` for each room event, always the latest handler passed, so it can be an inline function.
 * `message` skips the messages this package sends for shared and per-peer state. Arguments come from other
 * members as sent: check them.
 */
export function useRoomEvent<E extends keyof RoomEvents>(event: E, handler: RoomEvents[E], options: RoomOption = {}): void {
  const r = useTarget(options.room);
  const latest = useLatest(handler);
  useEffect(() => {
    if (!r) return;
    const fn = (...args: unknown[]) => {
      if (event === 'message' && isInternal(args[0])) return;
      (latest.current as (...args: unknown[]) => void)(...args);
    };
    return r.on(event, fn as RoomEvents[E]);
  }, [r, event, latest]);
}

export interface UseMessagesOptions<T> extends RoomOption {
  /** How many to keep, newest last. Default 200. */
  limit?: number;
  /** Keep only messages that pass; `info` says how each was sent. Without it, every message is kept as sent. */
  accept?: ((data: unknown, info: MessageInfo) => data is T) | ((data: unknown, info: MessageInfo) => boolean);
}

export interface UseMessagesResult<T> {
  /** Messages since this component mounted (or the room joined), oldest first, at most `limit`. */
  messages: readonly Message<T>[];
  /** `room.send`, and the message is added to `messages` with `mine: true`. Returns how many peers it went to. */
  send: (data: T, options?: SendOptions) => number;
  clear: () => void;
}

/** The room's messages as a list, for a chat or an event log. */
export function useMessages<T = any>(options: UseMessagesOptions<T> = {}): UseMessagesResult<T> {
  const r = useTarget(options.room);
  const [messages, setMessages] = useState<readonly Message<T>[]>([]);
  // A new limit or check applies from the next message; only a new room empties the list.
  const keep = useLatest(Math.max(1, options.limit ?? 200));
  const accept = useLatest(options.accept);
  const add = useCallback((m: Message<T>) => setMessages((list) => [...list, m].slice(-keep.current)), [keep]);
  useEffect(() => {
    setMessages((list) => (list.length ? [] : list));
    if (!r) return;
    return r.on('message', (data, peer, info) => {
      if (isInternal(data) || (accept.current && !passes(accept.current, data, info))) return;
      add({ id: ++nextId, peer, data, lane: info.lane, kind: info.kind, at: Date.now(), mine: false });
    });
  }, [r, add, accept]);
  const send = useCallback(
    (data: T, options?: SendOptions) => {
      if (!r) return 0;
      const n = r.send(data as unknown as RoomData, options);
      const kind = typeof data === 'string' ? 'text' : data instanceof ArrayBuffer || ArrayBuffer.isView(data) ? 'bytes' : 'json';
      const lane = options?.lane === 'unreliable' || options?.lane === 1 ? 'unreliable' : 'reliable';
      add({ id: ++nextId, peer: r.self, data, lane, kind, at: Date.now(), mine: true });
      return n;
    },
    [r, add],
  );
  const clear = useCallback(() => setMessages([]), []);
  return { messages, send, clear };
}

/**
 * State shared by the whole room: like useState, but every member that uses `key` sees the same value, and a
 * member that joins later catches up. The last write wins (by a logical clock, so all members agree). Values go
 * as JSON. Until the room is joined the value is `initial` and setting it does nothing.
 *
 * A value that fails `accept` reads as `initial`. Without `accept`, a value must be of the same kind as `initial`:
 * the same `typeof`, an array for an array, a non-array object for an object; anything for null.
 */
export function useSharedState<T>(key: string, initial: T, options: { accept?: Accept<T> } & RoomOption = {}): [T, SetState<T>] {
  const r = useTarget(options.room);
  const store = r ? storeOf(r) : null;
  const ok = options.accept ?? sameKind(initial);
  const latest = useLatest({ initial, ok });
  const subscribe = useCallback(
    (fn: () => void) => {
      if (!store) return () => {};
      store.start();
      return store.subscribe(`s:${key}`, fn);
    },
    [store, key],
  );
  const entry = useSyncExternalStore(
    store ? subscribe : noSubscribe,
    () => store?.getShared(key),
    () => undefined,
  );
  const set = useCallback<SetState<T>>(
    (next) => {
      if (!store) return;
      const cur = store.getShared(key);
      const prev = cur && passes(latest.current.ok, cur.v) ? (cur.v as T) : latest.current.initial;
      store.setShared(key, typeof next === 'function' ? (next as (prev: T) => T)(prev) : next);
    },
    [store, key, latest],
  );
  return [entry && passes(ok, entry.v) ? (entry.v as T) : initial, set];
}

/**
 * Each member's latest value for `key`, such as a cursor or a controller's input, and a setter for ours. Values
 * go on the unreliable lane as they change, then once reliably when they settle (SETTLE_MS); a member that
 * leaves drops out, and one that joins gets ours. Ours is not in the record. Values go as JSON. With `accept`, a
 * value that fails it is left out.
 */
export function usePeerState<T = any>(
  key: string,
  options: { accept?: Accept<T> } & RoomOption = {},
): [Readonly<Record<PeerId, T>>, (value: T) => void] {
  const r = useTarget(options.room);
  const store = r ? storeOf(r) : null;
  const accept = useLatest(options.accept);
  const subscribe = useCallback(
    (fn: () => void) => {
      if (!store) return () => {};
      store.start();
      return store.subscribe(`p:${key}`, fn);
    },
    [store, key],
  );
  const all = useSyncExternalStore(
    store ? subscribe : noSubscribe,
    () => (store ? store.getLive(key) : NO_VALUES),
    () => NO_VALUES,
  );
  const values = useMemo(() => {
    const ok = accept.current;
    if (!ok) return all;
    const kept: Record<PeerId, unknown> = {};
    for (const [peer, v] of Object.entries(all)) if (passes(ok, v)) kept[peer] = v;
    return Object.freeze(kept);
  }, [all, accept]) as Readonly<Record<PeerId, T>>;
  const set = useCallback((value: T) => store?.setLive(key, value), [store, key]);
  return [values, set];
}
