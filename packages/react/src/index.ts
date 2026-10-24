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
export { MAX_KEYS, MAX_KEY_LENGTH, SETTLE_MS } from './store.js';
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

// Every hook below works on `room` when it is given (null: no room), else on the nearest RoomProvider's.
function useTarget(room: Room | null | undefined): Room | null {
  const state = useContext(RoomContext);
  return room === undefined ? (state?.room ?? null) : room;
}

/** The other members, oldest first, with their presence. */
export function usePeers<P = Presence>(room?: Room | null): readonly Peer<P>[] {
  const r = useTarget(room);
  const store = r ? storeOf(r) : null;
  const subscribe = useCallback((fn: () => void) => (store ? store.subscribe('peers', fn) : () => {}), [store]);
  return useSyncExternalStore(
    store ? subscribe : noSubscribe,
    () => (store ? store.peers() : NO_PEERS),
    () => NO_PEERS,
  ) as readonly Peer<P>[];
}

/** Our presence and a setter: like useState, and every member sees the change. */
export function usePresence<P = Presence>(room?: Room | null): [P | undefined, SetState<P>] {
  const r = useTarget(room);
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
 * `message` skips the messages this package sends for shared and per-peer state.
 */
export function useRoomEvent<E extends keyof RoomEvents>(event: E, handler: RoomEvents[E], room?: Room | null): void {
  const r = useTarget(room);
  const latest = useRef(handler);
  useInsertionEffect(() => {
    latest.current = handler;
  });
  useEffect(() => {
    if (!r) return;
    const fn = (...args: unknown[]) => {
      if (event === 'message' && isInternal(args[0])) return;
      (latest.current as (...args: unknown[]) => void)(...args);
    };
    return r.on(event, fn as RoomEvents[E]);
  }, [r, event]);
}

export interface UseMessagesResult<T> {
  /** Messages since this component mounted (or the room joined), oldest first, at most `limit`. */
  messages: readonly Message<T>[];
  /** `room.send`, and the message is added to `messages` with `mine: true`. Returns how many peers it went to. */
  send: (data: T, options?: SendOptions) => number;
  clear: () => void;
}

/** The room's messages as a list, for a chat or an event log: the newest `limit` (default 200) are kept. */
export function useMessages<T = any>(limit = 200, room?: Room | null): UseMessagesResult<T> {
  const r = useTarget(room);
  const [messages, setMessages] = useState<readonly Message<T>[]>([]);
  const keep = Math.max(1, limit);
  // A new limit applies from the next message; only a new room empties the list.
  const kept = useRef(keep);
  useInsertionEffect(() => {
    kept.current = keep;
  });
  const add = useCallback((m: Message<T>) => setMessages((list) => [...list, m].slice(-kept.current)), []);
  useEffect(() => {
    setMessages((list) => (list.length ? [] : list));
    if (!r) return;
    return r.on('message', (data, peer, info) => {
      if (!isInternal(data)) add({ id: ++nextId, peer, data, lane: info.lane, kind: info.kind, at: Date.now(), mine: false });
    });
  }, [r, add]);
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
 */
export function useSharedState<T>(key: string, initial: T, room?: Room | null): [T, SetState<T>] {
  const r = useTarget(room);
  const store = r ? storeOf(r) : null;
  const first = useRef(initial);
  useInsertionEffect(() => {
    first.current = initial;
  });
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
      const prev = cur ? (cur.v as T) : first.current;
      store.setShared(key, typeof next === 'function' ? (next as (prev: T) => T)(prev) : next);
    },
    [store, key],
  );
  return [entry ? (entry.v as T) : initial, set];
}

/**
 * Each member's latest value for `key`, such as a cursor or a controller's input, and a setter for ours. Values
 * go on the unreliable lane as they change, then once reliably when they settle (SETTLE_MS); a member that
 * leaves drops out, and one that joins gets ours. Ours is not in the record. Values go as JSON.
 */
export function usePeerState<T>(key: string, room?: Room | null): [Readonly<Record<PeerId, T>>, (value: T) => void] {
  const r = useTarget(room);
  const store = r ? storeOf(r) : null;
  const subscribe = useCallback(
    (fn: () => void) => {
      if (!store) return () => {};
      store.start();
      return store.subscribe(`p:${key}`, fn);
    },
    [store, key],
  );
  const values = useSyncExternalStore(
    store ? subscribe : noSubscribe,
    () => (store ? store.getLive(key) : NO_VALUES),
    () => NO_VALUES,
  ) as Readonly<Record<PeerId, T>>;
  const set = useCallback((value: T) => store?.setLive(key, value), [store, key]);
  return [values, set];
}
