// joinRoom: the high-level API. A room is a full mesh of Nobaas links between
// everyone who joined the same appId and roomId, found through a signalling
// adapter and carried over WebRTC DataChannels.
//
//   const room = await joinRoom({ appId: 'my-app', roomId: 'lobby' });
//   room.on('message', (data, peer) => ...);
//   room.send('hello');
//
// Every room message starts with one tag byte (see T). Presence travels over
// the encrypted links, never through signalling: a peer counts as joined once
// its presence has arrived, so 'peer-join' always carries it.
//
// Hosts that can't take callbacks well (Godot's JavaScriptBridge) pull instead:
// room.poll() / room.receive() return queued events as plain objects (strings,
// numbers, ArrayBuffers), and room.sendTo(peer, data, lane) takes no options.

import { loadNobaas, Lane } from './nobaas.js';
import { WebRtcTransport, BroadcastSignaling } from './webrtc.js';
import { NostrSignaling } from './nostr.js';
import { LocalTransport } from './local.js';

export const DEFAULT_ICE_SERVERS = [{ urls: 'stun:stun.l.google.com:19302' }, { urls: 'stun:stun.cloudflare.com:3478' }];
const LANES = { reliable: Lane.Reliable, unreliable: Lane.Unreliable, urgent: Lane.Urgent };
const T = { Bytes: 0, Text: 1, Json: 2, Presence: 3, Bye: 4 };
// Presence is re-announced this often, so a lost hello only delays a join.
const HELLO_EVERY_MS = 15000;
const NOT_CONNECTED = -1;
const TOO_LARGE = -2;
const WOULD_BLOCK = -3;

const enc = new TextEncoder();
const dec = new TextDecoder();
const modules = new Map(); // wasm source -> Promise<Nobaas>, shared by rooms

function frame(tag, body) {
  const b = new Uint8Array(body.length + 1);
  b[0] = tag;
  b.set(body, 1);
  return b;
}

function encode(data) {
  if (typeof data === 'string') return frame(T.Text, enc.encode(data));
  if (data instanceof Uint8Array) return frame(T.Bytes, data);
  if (data instanceof ArrayBuffer) return frame(T.Bytes, new Uint8Array(data));
  if (ArrayBuffer.isView(data)) return frame(T.Bytes, new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
  return frame(T.Json, enc.encode(JSON.stringify(data)));
}

function laneOf(lane) {
  const n = typeof lane === 'number' ? lane : LANES[lane];
  if (n === undefined) throw new TypeError(`unknown lane ${lane}: use 'reliable', 'unreliable' or 'urgent'`);
  return n;
}

// A queued event: only strings, numbers, plain JSON values and ArrayBuffers.
function plainEvent(type, args) {
  if (type === 'message') {
    const [data, peer, { kind, lane }] = args;
    return { type, peer, data: data instanceof Uint8Array ? data.slice().buffer : data, kind, lane };
  }
  const [peer, x] = args;
  if (type === 'peer-leave') return { type, peer, reason: x };
  return { type, peer, presence: x };
}

export async function joinRoom({
  appId,
  roomId,
  signaling = 'local',
  presence = {},
  queue = false,
  secret,
  wasm,
  relays,
  iceServers,
  privacy = 'direct',
  RTCPeerConnection = globalThis.RTCPeerConnection,
} = {}) {
  if (!appId || !roomId) throw new TypeError('joinRoom needs an appId and a roomId');
  // A room dials every member, so relay-only (one WebRTC link, to the relay) can't apply.
  if (privacy !== 'direct' && privacy !== 'public-only') throw new TypeError(`joinRoom privacy must be 'direct' or 'public-only', not ${privacy}`);
  const src = wasm ?? new URL('../pkg/nobaas_ffi.wasm', import.meta.url);
  const key = String(src);
  if (!modules.has(key)) modules.set(key, loadNobaas(src).catch((e) => (modules.delete(key), Promise.reject(e))));
  const nb = await modules.get(key);
  const endpoint = nb.endpoint({ secret });
  const name = `${appId}/${roomId}`;
  let sig;
  if (signaling === 'local') sig = new BroadcastSignaling(endpoint.id, `nobaas/${name}`);
  else if (signaling === 'nostr') sig = new NostrSignaling(nb, endpoint, { room: name, relays });
  else if (typeof signaling === 'function') sig = signaling({ endpoint, nobaas: nb, room: name });
  else throw new TypeError(`unknown signaling ${signaling}: use 'local', 'nostr' or a factory`);
  let transport;
  if (RTCPeerConnection) {
    // Remote peers need STUN to find a path; tabs of one browser don't.
    const ice = iceServers ?? (signaling === 'local' ? [] : DEFAULT_ICE_SERVERS);
    // Relays charge per event: one offer and one answer, each with every candidate.
    transport = (onPeer) => new WebRtcTransport(endpoint, sig, { iceServers: ice, trickle: signaling === 'local', onPeer, privacy, RTCPeerConnection });
  } else if (signaling === 'local') {
    transport = () => new LocalTransport(endpoint, sig);
  } else {
    endpoint.close();
    sig.close();
    throw new Error('no WebRTC in this runtime: pass RTCPeerConnection (e.g. from node-datachannel) or use signaling: \'local\'');
  }
  const room = new Room(endpoint, sig, transport, presence);
  if (queue) room.queue = [];
  await sig.ready;
  sig.hello();
  return room;
}

export class Room {
  constructor(endpoint, signaling, makeTransport, presence) {
    this.endpoint = endpoint;
    this.signaling = signaling;
    this.self = endpoint.id;
    this.me = presence;
    this.linked = new Set(); // peers with a Nobaas session
    this.members = new Map(); // joined peers -> presence
    this.handlers = new Map(); // event -> Set of fns
    this.outbox = new Map(); // peer -> [frame, lane] waiting for send window
    this.left = false;
    this.queue = null; // events for poll()/receive(), once asked for
    this.greeted = new Set(); // peers whose hello we answered
    this.transport = makeTransport((peer, state) => {
      if (state === 'closed') this.lost(peer, 'closed');
    });
    endpoint.onEvent = (e) => this.onEvent(e);
    signaling.onHello = (peer) => this.onHello(peer);
    this.helloTimer = setInterval(() => signaling.hello(), HELLO_EVERY_MS);
    this.onPageHide = () => this.leave();
    globalThis.addEventListener?.('pagehide', this.onPageHide);
  }

  get peers() {
    return [...this.members.keys()];
  }

  on(event, fn) {
    if (!this.handlers.has(event)) this.handlers.set(event, new Set());
    this.handlers.get(event).add(fn);
    return () => this.off(event, fn);
  }

  off(event, fn) {
    this.handlers.get(event)?.delete(fn);
  }

  emit(event, ...args) {
    if (this.queue) this.queue.push(plainEvent(event, args));
    for (const fn of this.handlers.get(event) ?? []) {
      try {
        fn(...args);
      } catch (e) {
        // A throwing handler must not stall the endpoint: report it on its own.
        setTimeout(() => {
          throw e;
        });
      }
    }
  }

  // Sends to every joined peer, or to `to` (one id or several). Returns how
  // many peers it was queued for.
  send(data, { to, lane = 'reliable' } = {}) {
    if (this.left) throw new Error('left the room');
    const l = laneOf(lane);
    const f = encode(data);
    const targets = to === undefined ? this.peers : [to].flat().filter((p) => this.members.has(p));
    for (const p of targets) this.write(p, f, l);
    return targets.length;
  }

  // send() without an options object: `peer` '' or null means everyone.
  sendTo(peer, data, lane = 'reliable') {
    return this.send(data, peer ? { to: peer, lane } : { lane });
  }

  // Every event since the last call, oldest first. The first call starts the
  // queue (or pass `queue: true` to joinRoom to catch events from the start).
  poll() {
    if (!this.queue) this.queue = [];
    const q = this.queue;
    this.queue = [];
    return q;
  }

  // The next event, or null.
  receive() {
    if (!this.queue) this.queue = [];
    return this.queue.shift() ?? null;
  }

  getPresence(peer = this.self) {
    return peer === this.self ? this.me : this.members.get(peer);
  }

  setPresence(presence) {
    this.me = presence;
    const f = frame(T.Presence, enc.encode(JSON.stringify(presence)));
    for (const p of this.linked) this.write(p, f, Lane.Reliable);
  }

  rtt(peer) {
    return this.endpoint.rttMs(peer);
  }

  stats() {
    return this.endpoint.stats();
  }

  // Says goodbye to every peer, so they see 'peer-leave' at once, then closes.
  async leave() {
    if (this.left) return;
    this.left = true;
    clearInterval(this.helloTimer);
    globalThis.removeEventListener?.('pagehide', this.onPageHide);
    for (const p of this.linked) this.endpoint.send(p, Uint8Array.of(T.Bye), Lane.Reliable);
    this.endpoint.pump();
    await new Promise((r) => setTimeout(r, 50));
    this.transport.close();
    this.endpoint.close();
    this.members.clear();
    this.linked.clear();
  }

  onHello(peer) {
    if (this.left || this.transport.has(peer)) return;
    // Answer a newcomer's hello once, so it learns about us too.
    if (!this.greeted.has(peer)) {
      this.greeted.add(peer);
      this.signaling.hello();
    }
    // One side dials: the smaller id.
    if (this.self < peer) this.transport.dial(peer);
  }

  onEvent(e) {
    if (this.left) return;
    if (e.type === 'connected') {
      this.linked.add(e.peer);
      this.write(e.peer, frame(T.Presence, enc.encode(JSON.stringify(this.me))), Lane.Reliable);
    } else if (e.type === 'message') {
      this.onMessage(e.peer, e.data, e.lane);
    } else if (e.type === 'disconnected') {
      this.lost(e.peer, e.reason);
    } else if (e.type === 'connectFailed') {
      this.lost(e.peer, 'failed');
    }
  }

  onMessage(peer, data, lane) {
    const tag = data[0];
    const body = data.subarray(1);
    if (tag === T.Presence) {
      let p;
      try {
        p = JSON.parse(dec.decode(body));
      } catch {
        return;
      }
      const joined = this.members.has(peer);
      this.members.set(peer, p);
      this.emit(joined ? 'presence' : 'peer-join', peer, p);
      return;
    }
    if (tag === T.Bye) return this.lost(peer, 'left');
    // Before its presence, a peer hasn't joined: only unreliable data can race it.
    if (!this.members.has(peer)) return;
    let value;
    if (tag === T.Bytes) value = body;
    else if (tag === T.Text) value = dec.decode(body);
    else if (tag === T.Json) {
      try {
        value = JSON.parse(dec.decode(body));
      } catch {
        return;
      }
    } else return;
    const kind = tag === T.Bytes ? 'bytes' : tag === T.Text ? 'text' : 'json';
    this.emit('message', value, peer, { lane: lane === Lane.Unreliable ? 'unreliable' : 'reliable', kind });
  }

  lost(peer, reason) {
    this.transport.drop(peer);
    this.linked.delete(peer);
    this.outbox.delete(peer);
    this.greeted.delete(peer);
    if (this.members.delete(peer)) this.emit('peer-leave', peer, reason);
  }

  write(peer, f, lane) {
    const q = this.outbox.get(peer);
    if (q) return void q.push([f, lane]);
    const r = this.endpoint.send(peer, f, lane);
    if (r === WOULD_BLOCK) {
      this.outbox.set(peer, [[f, lane]]);
      this.flushSoon();
    } else if (r === TOO_LARGE) {
      throw new RangeError(`message of ${f.length - 1} bytes is too large for this lane`);
    } else if (r < 0 && r !== NOT_CONNECTED) {
      throw new Error(`send failed (${r})`);
    }
  }

  flushSoon() {
    if (this.flushTimer) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = null;
      if (this.left) return;
      for (const [peer, q] of this.outbox) {
        while (q.length && this.endpoint.send(peer, q[0][0], q[0][1]) !== WOULD_BLOCK) q.shift();
        if (!q.length) this.outbox.delete(peer);
      }
      if (this.outbox.size) this.flushSoon();
    }, 2);
  }
}
