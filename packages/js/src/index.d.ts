// Types for @nobaas/js.

/** A peer's id: its 32-byte Nobaas PeerId as 64 hex characters. */
export type PeerId = string;

/** Free-form data a peer shares with the room (a nickname, a colour); must survive JSON. */
export type Presence = any;

/** `reliable`: ordered, retransmitted. `unreliable`: latest-wins datagrams, one packet at most.
 * `urgent`: reliable, each fragment sent twice; arrives as `reliable`. */
export type LaneName = 'reliable' | 'unreliable' | 'urgent';

/** What `room.send` accepts. Strings arrive as strings, bytes as a Uint8Array, anything else as parsed JSON. */
export type RoomData = string | Uint8Array | ArrayBuffer | ArrayBufferView | number | boolean | null | object;

export interface SignalingAdapter {
  readonly id: string;
  /** Resolves once the adapter can publish (relays connected). */
  readonly ready?: Promise<unknown>;
  /** Called with each peer that announces itself in the room. */
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): void | Promise<void>;
  send(to: PeerId, body: unknown): void | Promise<void>;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}

export interface SignalingContext {
  endpoint: Endpoint;
  nobaas: Nobaas;
  /** `${appId}/${roomId}` */
  room: string;
}

export interface JoinRoomOptions {
  /** Names your app; rooms of different apps never meet. */
  appId: string;
  roomId: string;
  /**
   * How peers find each other.
   * - `'local'` (default): BroadcastChannel; tabs of one browser, or one Node process.
   * - `'nostr'`: public Nostr relays (`wss://relay.damus.io`, `wss://nos.lol`), across the internet.
   * - a factory returning your own adapter.
   */
  signaling?: 'local' | 'nostr' | ((ctx: SignalingContext) => SignalingAdapter);
  /** Shared with every peer when it joins; change it with `room.setPresence`. Default `{}`. */
  presence?: Presence;
  /** Queue events for `room.poll()`/`room.receive()` from the start; otherwise the first call starts the queue. */
  queue?: boolean;
  /** 32 bytes restoring an identity from `room.endpoint.secret()`; a fresh one by default. */
  secret?: Uint8Array;
  /** Where to load `nobaas_ffi.wasm` from: a URL, a file path (Node) or the bytes. Default: next to this package. */
  wasm?: string | URL | BufferSource;
  /** Nostr relays, for `signaling: 'nostr'`. */
  relays?: string[];
  /** Default: none for `'local'`, Google and Cloudflare STUN otherwise. */
  iceServers?: RTCIceServer[];
  /** A WebRTC implementation for runtimes without one (Node: node-datachannel's polyfill).
   * Without WebRTC, `'local'` rooms send datagrams over the BroadcastChannel. */
  RTCPeerConnection?: typeof globalThis.RTCPeerConnection;
}

export interface MessageInfo {
  lane: 'reliable' | 'unreliable';
  /** How it was sent: a string, bytes, or any other value as JSON. */
  kind: 'text' | 'bytes' | 'json';
}

/** A queued event from `room.poll()`/`room.receive()`: plain values only, for hosts such as Godot's JavaScriptBridge. */
export type RoomEvent =
  | { type: 'peer-join'; peer: PeerId; presence: Presence }
  | { type: 'presence'; peer: PeerId; presence: Presence }
  | { type: 'peer-leave'; peer: PeerId; reason: string }
  | { type: 'message'; peer: PeerId; data: string | ArrayBuffer | any; kind: 'text' | 'bytes' | 'json'; lane: 'reliable' | 'unreliable' };

export interface RoomEvents {
  /** A peer connected and shared its presence. */
  'peer-join': (peer: PeerId, presence: Presence) => void;
  /** A peer left, timed out or lost its connection. */
  'peer-leave': (peer: PeerId, reason: 'left' | 'closed' | 'failed' | 'timeout' | 'replaced' | 'violation') => void;
  /** A joined peer changed its presence. */
  presence: (peer: PeerId, presence: Presence) => void;
  message: (data: any, peer: PeerId, info: MessageInfo) => void;
}

export interface SendOptions {
  /** One peer or several; default every joined peer. */
  to?: PeerId | PeerId[];
  /** Default `'reliable'`. */
  lane?: LaneName | number;
}

export declare class Room {
  /** Our own PeerId. */
  readonly self: PeerId;
  /** Joined peers, not counting us. */
  readonly peers: PeerId[];
  /** The Nobaas endpoint underneath, for stats, sealing and identity. */
  readonly endpoint: Endpoint;
  readonly signaling: SignalingAdapter;
  /** Subscribes; returns a function that unsubscribes. */
  on<E extends keyof RoomEvents>(event: E, fn: RoomEvents[E]): () => void;
  off<E extends keyof RoomEvents>(event: E, fn: RoomEvents[E]): void;
  /** Sends to every joined peer, or to `to`. Returns how many peers it was queued for.
   * Throws RangeError if the message can't fit the lane (unreliable: one datagram). */
  send(data: RoomData, options?: SendOptions): number;
  /** `send` with positional arguments; `peer` '' or null sends to everyone. */
  sendTo(peer: PeerId | '' | null, data: RoomData, lane?: LaneName | number): number;
  /** Every queued event, oldest first, and clears the queue. The first call starts queueing. */
  poll(): RoomEvent[];
  /** The next queued event, or null. The first call starts queueing. */
  receive(): RoomEvent | null;
  /** Our presence, or a joined peer's. */
  getPresence(peer?: PeerId): Presence | undefined;
  setPresence(presence: Presence): void;
  /** Smoothed round-trip time to a peer in ms, or null before the first sample. */
  rtt(peer: PeerId): number | null;
  stats(): EndpointStats;
  /** Tells every peer we're going, then closes the links. Also runs on `pagehide`. */
  leave(): Promise<void>;
}

/** Joins a room: `const room = await joinRoom({ appId: 'my-app', roomId: 'lobby' })`. */
export declare function joinRoom(options: JoinRoomOptions): Promise<Room>;

export declare const DEFAULT_ICE_SERVERS: RTCIceServer[];
export declare const DEFAULT_RELAYS: string[];

// Lower layers.

export declare const Lane: { readonly Reliable: 0; readonly Unreliable: 1; readonly Urgent: 2 };
export declare const EventKind: {
  readonly Connected: 1;
  readonly Message: 2;
  readonly Disconnected: 3;
  readonly ConnectFailed: 4;
  readonly PathChanged: 5;
  readonly AddrObserved: 6;
};

export type Path = { direct: number } | { via: PeerId };

export type EndpointEvent =
  | { type: 'connected'; peer: PeerId; initiator: boolean; path: Path }
  | { type: 'message'; peer: PeerId; lane: number; data: Uint8Array }
  | { type: 'disconnected'; peer: PeerId; reason: 'timeout' | 'closed' | 'replaced' | 'violation' }
  | { type: 'connectFailed'; peer: PeerId }
  | { type: 'pathChanged'; peer: PeerId; path: Path }
  | { type: 'addrObserved'; peer: PeerId; addr: number }
  | { type: 'unknown'; kind: number };

export interface EndpointStats {
  txPackets: number;
  rxPackets: number;
  rxMalformed: number;
  rxUnknownSession: number;
  rxAuthFailed: number;
  rxReplayed: number;
  rxHandshakeDropped: number;
  retransmits: number;
  unreliableDropped: number;
  roams: number;
  relayed: number;
  relayDropped: number;
  punchesSent: number;
  punchSucceeded: number;
  punchFailed: number;
  directFallbacks: number;
}

export interface Nobaas {
  nostrKeypair(): { secret: Uint8Array; pubkey: Uint8Array };
  nostrSign(secret: Uint8Array, msg32: Uint8Array): Uint8Array;
  endpoint(options?: { secret?: Uint8Array | null; shareAddresses?: boolean }): Endpoint;
}

export declare function loadNobaas(source: string | URL | BufferSource): Promise<Nobaas>;

export declare class Endpoint {
  readonly peerId: Uint8Array;
  readonly id: PeerId;
  /** Called with (transport handle, datagram) for each datagram to send. */
  onTransmit: (addr: number, bytes: Uint8Array) => void;
  onEvent: (event: EndpointEvent) => void;
  secret(): Uint8Array;
  connect(addr: number, peer: PeerId | Uint8Array): number;
  connectVia(relay: PeerId | Uint8Array, peer: PeerId | Uint8Array): number;
  stats(): EndpointStats;
  seal(peer: PeerId | Uint8Array, bytes: Uint8Array): Uint8Array | null;
  open(bytes: Uint8Array): { from: PeerId; data: Uint8Array } | null;
  isConnected(peer: PeerId | Uint8Array): boolean;
  rttMs(peer: PeerId | Uint8Array): number | null;
  /** 0, or a negative NB_ERR code (-1 not connected, -2 too large, -3 would block). */
  send(peer: PeerId | Uint8Array, data: string | Uint8Array, lane?: number): number;
  receive(addr: number, bytes: Uint8Array): void;
  pump(): void;
  close(): void;
}

export declare function decodeEvent(bytes: Uint8Array): EndpointEvent;
export declare function toHex(bytes: Uint8Array): string;
export declare function fromHex(hex: string): Uint8Array;

export interface Transport {
  dial(peer: PeerId): void | Promise<void>;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class WebRtcTransport implements Transport {
  constructor(
    endpoint: Endpoint,
    signaling: SignalingAdapter,
    options?: {
      iceServers?: RTCIceServer[];
      onPeer?: (peer: PeerId, state: 'open' | 'closed') => void;
      loss?: number;
      trickle?: boolean;
      RTCPeerConnection?: typeof globalThis.RTCPeerConnection;
    },
  );
  dial(peer: PeerId): Promise<void>;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class LocalTransport implements Transport {
  constructor(endpoint: Endpoint, signaling: BroadcastSignaling);
  dial(peer: PeerId): void;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class BroadcastSignaling implements SignalingAdapter {
  constructor(id: string, room?: string);
  readonly id: string;
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): void;
  send(to: PeerId, body: unknown): void;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}

export declare class NostrSignaling implements SignalingAdapter {
  constructor(nb: Nobaas, endpoint: Endpoint, options: { room: string; relays?: string[] });
  readonly id: string;
  readonly ready: Promise<void>;
  readonly stats: { published: number; received: number; rejected: number; relaysOpen: number; lastRejection?: string };
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): Promise<void>;
  send(to: PeerId, body: unknown): Promise<void>;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}
