// joinRoom: the high-level API. A room is a full mesh of Peerwire links between
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
// Rooms are closed by their name (proof.js): hellos, WebRTC offers and the first
// message on every link carry an HMAC under the room key, which the topic relays
// see doesn't give away. A peer is dialled only on a hello that proves the key,
// an offer is answered only if it proves it, and a link that doesn't prove it
// within PROOF_MS is dropped, never told our presence. Use roomIds with at least
// 128 random bits (randomRoomId()): a guessable name is a public lobby.
//
// When WebRTC can't link two members (symmetric NAT on both sides, and no TURN in
// iceServers), the room falls back on its own, per peer: the dialer dials the
// peer through every member it is linked to directly (the core relays for linked
// peers) and over the signalling channel itself (a pipe, see webrtc.js), at once,
// and the first path to answer wins. Every path carries the same Noise session,
// so relays and signalling only ever see sealed datagrams. room.route(peer) says
// which path a peer is on; a link over the pipe moves to a member that links to
// both later, once the core sees it relay both ways, and back to the pipe if that
// member goes. `fallback: 'peers'` skips the pipe, 'off' both. A peer
// behind our own public IPv4 (a laptop on a phone's hotspot) falls back after
// sameNatFallbackMs, not fallbackMs. The link's own DataChannel opening later takes
// the link back (webrtc.js), and one found by other means (a birthday punch,
// birthday.js) can take a fallback link over: room.adoptChannel(). Meanwhile the room
// tries WebRTC again now and then, and on a network change or a microphone or camera
// grant (retry.js); a retry whose DataChannel opens takes the link over the same way.
//
// Hosts that can't take callbacks well (Godot's JavaScriptBridge) pull instead:
// room.poll() / room.receive() return queued events as plain objects (strings,
// numbers, ArrayBuffers), and room.sendTo(peer, data, lane) takes no options.

import { loadPeerwire, Lane, toHex, fromHex, isPeerId } from './peerwire.js';
import { WebRtcTransport, BroadcastSignaling } from './webrtc.js';
import { NostrSignaling } from './nostr.js';
import { LocalTransport } from './local.js';
import { RoomProof } from './proof.js';
import { DirectRetries } from './retry.js';

export const DEFAULT_ICE_SERVERS = [{ urls: 'stun:stun.l.google.com:19302' }, { urls: 'stun:stun.cloudflare.com:3478' }];
const LANES = { reliable: Lane.Reliable, unreliable: Lane.Unreliable, urgent: Lane.Urgent };
// Nudge carries nothing (peers ignore tags they don't know): see the 'open' handler.
const T = { Bytes: 0, Text: 1, Json: 2, Presence: 3, Bye: 4, Proof: 5, Nudge: 6 };
// Presence is re-announced this often, so a lost hello only delays a join.
const HELLO_EVERY_MS = 15000;
// A link must prove the room key this soon after it connects.
const PROOF_MS = 10000;
// In a fallback, members get this long to relay a peer before signalling, the
// slowest path, joins the dial.
const PEER_FIRST_MS = 1000;
// Frames held from one peer while its link proof is checked.
const MAX_HELD = 256;
// How long a pagehide handler that said goodbye holds the page. The browser may end
// the renderer as soon as the handler returns, before its WebRTC thread has put the
// Bye on the wire; holding the main thread a few ms lets that thread send it.
const PAGEHIDE_GRACE_MS = 20;
const NOT_CONNECTED = -1;
const TOO_LARGE = -2;
const WOULD_BLOCK = -3;

const enc = new TextEncoder();
const dec = new TextDecoder();
const modules = new Map(); // wasm source -> Promise<Peerwire>, shared by rooms

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
  fallback = 'auto',
  fallbackMs,
  sameNatFallbackMs,
  retryOnGrant = true,
  RTCPeerConnection = globalThis.RTCPeerConnection,
} = {}) {
  if (!appId || !roomId) throw new TypeError('joinRoom needs an appId and a roomId');
  // A room dials every member, so relay-only (one WebRTC link, to the relay) can't apply.
  if (privacy !== 'direct' && privacy !== 'public-only') throw new TypeError(`joinRoom privacy must be 'direct' or 'public-only', not ${privacy}`);
  if (!['auto', 'peers', 'off'].includes(fallback)) throw new TypeError(`joinRoom fallback must be 'auto', 'peers' or 'off', not ${fallback}`);
  const src = wasm ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url);
  const key = String(src);
  if (!modules.has(key)) modules.set(key, loadPeerwire(src).catch((e) => (modules.delete(key), Promise.reject(e))));
  const pw = await modules.get(key);
  const endpoint = pw.endpoint({ secret });
  const name = `${appId}/${roomId}`;
  const proof = await RoomProof.create(name);
  let sig;
  if (signaling === 'local') sig = new BroadcastSignaling(endpoint.id, `peerwire/${name}`);
  else if (signaling === 'nostr') sig = new NostrSignaling(pw, endpoint, { room: name, relays });
  else if (typeof signaling === 'function') sig = signaling({ endpoint, peerwire: pw, room: name });
  else throw new TypeError(`unknown signaling ${signaling}: use 'local', 'nostr' or a factory`);
  let transport;
  if (RTCPeerConnection) {
    // Remote peers need STUN to find a path; tabs of one browser don't.
    const ice = iceServers ?? (signaling === 'local' ? [] : DEFAULT_ICE_SERVERS);
    // Relays charge per event: one offer and one answer, each with every candidate.
    const offerProof = {
      sign: (to) => proof.pair('pw-rtc-offer', endpoint.id, to),
      check: (from, p) => proof.checkPair('pw-rtc-offer', from, endpoint.id, p),
    };
    const pipes = fallback === 'auto';
    transport = (onPeer) => new WebRtcTransport(endpoint, sig, { iceServers: ice, trickle: signaling === 'local', onPeer, privacy, offerProof, pipes, fallbackMs, sameNatFallbackMs, RTCPeerConnection });
  } else if (signaling === 'local') {
    transport = () => new LocalTransport(endpoint, sig);
  } else {
    endpoint.close();
    sig.close();
    throw new Error('no WebRTC in this runtime: pass RTCPeerConnection (e.g. from node-datachannel) or use signaling: \'local\'');
  }
  // A grant shows local addresses only to new connections, and only 'direct' offers them.
  const room = new Room(endpoint, sig, transport, presence, proof, { fallback, grants: privacy === 'direct' && retryOnGrant });
  if (queue) room.queue = [];
  await sig.ready;
  await room.hello();
  return room;
}

export class Room {
  static proofMs = PROOF_MS; // tests shorten it
  static peerFirstMs = PEER_FIRST_MS;
  constructor(endpoint, signaling, makeTransport, presence, proof, { fallback = 'off', grants = false } = {}) {
    this.endpoint = endpoint;
    this.signaling = signaling;
    this.self = endpoint.id;
    this.me = presence;
    this.linked = new Set(); // peers with a Peerwire session
    this.members = new Map(); // joined peers -> presence
    this.handlers = new Map(); // event -> Set of fns
    this.outbox = new Map(); // peer -> [frame, lane] waiting for send window
    this.left = false;
    this.queue = null; // events for poll()/receive(), once asked for
    this.greeted = new Set(); // peers whose hello we answered
    this.proof = proof;
    this.proven = new Set(); // linked peers that proved the room key
    this.proving = new Map(); // peer -> frames held while its proof is checked
    this.proofSent = new Map(); // linked peer -> resolves once our link proof is written
    this.deadlines = new Map(); // peer -> timer that drops it unless it proves the key
    this.fallback = fallback;
    this.routes = new Map(); // linked peer -> { kind: 'direct' | 'peer' | 'signaling', via?, adopted? }
    this.paths = new Map(); // linked peer -> the endpoint's path to it
    this.fallingBack = new Set(); // peers we dial by fallback since WebRTC failed
    this.transport = makeTransport((peer, state, handle) => {
      if (state === 'closed') this.rtcClosed(peer, handle);
      else if (state === 'failed') this.rtcFailed(peer);
      else if (state === 'carrier') this.reroute(peer);
      else if (state === 'open' || state === 'retry-failed') this.retries.done(peer);
      // A link over signalling moves onto a channel that opened late (a retry's, say) with
      // its first datagram there: one now (its ACK moves the far end's too), not at the next
      // keepalive, up to 10 s on.
      if (state === 'open' && this.routes.get(peer)?.kind === 'signaling' && this.proven.has(peer)) this.write(peer, Uint8Array.of(T.Nudge), Lane.Reliable);
      else if (state === 'retry') this.retries.ask(peer);
    });
    // Peers on a fallback: WebRTC is tried again now and then (see retryDirect).
    this.retries = new DirectRetries((peer) => this.retryNow(peer), { holdMs: (this.transport.fallbackMs ?? 8000) + 10000 });
    this.unwatch = []; // removes the permission listeners
    if (grants) this.watchGrants();
    endpoint.onEvent = (e) => this.onEvent(e);
    // ACKs free send window: flush the outbox as they arrive (the timer is a fallback).
    endpoint.onDrain = () => this.outbox.size && this.flush('drain');
    signaling.onHello = (peer, meta) => this.onHello(peer, meta);
    // A relay back after a network change: announce us now, not at the next interval.
    if ('onReconnect' in signaling) signaling.onReconnect = () => this.hello();
    this.helloTimer = setInterval(() => this.hello(), HELLO_EVERY_MS);
    this.onPageHide = () => {
      const linked = this.linked.size;
      this.leave();
      // A wait that blocks: no timer or promise would run before the page goes.
      for (const end = performance.now() + PAGEHIDE_GRACE_MS; linked && performance.now() < end;);
    };
    globalThis.addEventListener?.('pagehide', this.onPageHide);
    // A network switch: links whose ICE dropped restart it now (only those, so a new
    // speed estimate, which fires 'change' too, costs nothing), and peers on a fallback
    // are retried (only on a new network: 'online', or a new connection type).
    this.netType = globalThis.navigator?.connection?.type;
    this.onNetwork = (e) => {
      this.transport.networkChanged?.();
      const type = globalThis.navigator?.connection?.type;
      if (e?.type === 'online' || type !== this.netType) this.retries.soon();
      this.netType = type;
    };
    globalThis.addEventListener?.('online', this.onNetwork);
    globalThis.navigator?.connection?.addEventListener?.('change', this.onNetwork);
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
    for (const p of this.proven) this.write(p, f, Lane.Reliable);
  }

  // How `peer` is reached: { kind: 'direct' } (its own WebRTC link), { kind: 'direct',
  // adopted: true } (a channel handed to adoptChannel), { kind: 'peer', via } (relayed
  // by member `via`), { kind: 'signaling' } (over the signalling relays), or null while
  // not linked.
  route(peer) {
    return this.routes.get(peer) ?? null;
  }

  // Tries WebRTC again, now, with `peer` (or every peer) that the room reaches only by
  // fallback: a new connection, so one made after a microphone or camera grant offers the
  // local addresses (privacy 'direct'), and one after a network change its addresses.
  // If its DataChannel opens, the link moves onto it (route() says 'direct'), the same
  // session; if not, nothing changes. Retries take turns: one at a time per room, at
  // least 2 s apart. Returns how many peers are to be retried.
  retryDirect(peer) {
    return this.left ? 0 : this.retries.soon(peer);
  }

  // Begins a retry with `peer`: the dialling side dials it afresh, the other asks it to.
  retryNow(peer) {
    if (this.left || !this.linked.has(peer)) return false;
    if (this.self < peer) return !!this.transport.retry?.(peer);
    this.transport.askRetry?.(peer);
    return false;
  }

  // Browsers show their local addresses only to connections made after a microphone or
  // camera grant, so on one, peers on a fallback are retried.
  watchGrants() {
    for (const name of ['microphone', 'camera']) {
      (async () => {
        const st = await globalThis.navigator?.permissions?.query?.({ name });
        if (!st?.addEventListener || this.left) return;
        const on = () => st.state === 'granted' && this.retries.soon();
        st.addEventListener('change', on);
        this.unwatch.push(() => st.removeEventListener('change', on));
      })().catch(() => {}); // a permission this browser can't query
    }
  }

  // Moves `peer`'s link onto `dc`, an open DataChannel to it made outside the room
  // (`pc` its connection; a birthday punch's, say), when the room reaches `peer` only by
  // fallback (relayed by a member, or over signalling) and the peer adopts its end too.
  // Resolves true once the channel carries the link: a link over signalling moves at
  // once; a relayed one once the endpoint's punch over the channel is answered. route()
  // then says { kind: 'direct', adopted: true }. Resolves false, leaving the channel to
  // the caller, if the link is direct already or the peer didn't adopt in time. When
  // the channel closes, the link goes back to its fallback route.
  async adoptChannel(peer, pc, dc) {
    const r = this.routes.get(peer);
    if (this.left || !r || r.kind === 'direct' || !this.transport.adopt) return false;
    const pipe = r.kind === 'signaling';
    const h = await this.transport.adopt(peer, pc, dc, { pipe });
    if (h < 0) return false;
    if (!pipe) this.endpoint.addCandidate(h, peer);
    return true;
  }

  rtt(peer) {
    return this.endpoint.rttMs(peer);
  }

  stats() {
    return this.endpoint.stats();
  }

  // Says goodbye to every peer, so they see 'peer-leave' at once, then closes. All of it
  // happens before this returns, with no timer or await, because on pagehide the page
  // may be gone before either runs. The Bye goes on the unreliable lane, which no full
  // send window or unacknowledged reliable data holds back, and the core's sealed close
  // follows it; each is handed to the transport (dc.send) by the pump in the same call.
  async leave() {
    if (this.left) return;
    this.left = true;
    clearInterval(this.helloTimer);
    clearTimeout(this.flushTimer);
    globalThis.removeEventListener?.('pagehide', this.onPageHide);
    globalThis.removeEventListener?.('online', this.onNetwork);
    globalThis.navigator?.connection?.removeEventListener?.('change', this.onNetwork);
    this.retries.close();
    for (const off of this.unwatch) off();
    const bye = Uint8Array.of(T.Bye);
    for (const p of this.linked) this.endpoint.send(p, bye, Lane.Unreliable);
    this.endpoint.pump();
    for (const p of this.linked) this.endpoint.disconnect(p);
    this.outbox.clear();
    this.transport.close();
    this.endpoint.close();
    this.members.clear();
    this.linked.clear();
    this.proven.clear();
    this.proving.clear();
    this.proofSent.clear();
    for (const t of this.deadlines.values()) clearTimeout(t);
    this.deadlines.clear();
    this.routes.clear();
    this.paths.clear();
    this.fallingBack.clear();
  }

  // Announces us on the room topic, with a proof of the room key.
  async hello() {
    if (!this.left) this.signaling.hello(await this.proof.hello(this.self));
  }

  async onHello(peer, meta) {
    // A custom signalling adapter may pass anything along: only PeerIds are dialled.
    // `meta.pw` marks a Rust UDP peer, which WebRTC can't dial.
    if (this.left || !isPeerId(peer) || peer === this.self || meta?.pw) return;
    // Knowing the topic isn't enough: only hellos that prove the room key count.
    if (!(await this.proof.checkHello(peer, meta))) return;
    // A member may reach us over signalling once WebRTC fails, whichever side dials.
    this.transport.allowPipe?.(peer);
    if (this.left || this.transport.has(peer) || this.fallingBack.has(peer) || this.endpoint.isConnected(peer)) return;
    // Answer a newcomer's hello once, so it learns about us too.
    if (!this.greeted.has(peer)) {
      this.greeted.add(peer);
      this.hello();
    }
    // One side dials: the smaller id.
    if (this.self < peer) this.transport.dial(peer);
  }

  onEvent(e) {
    if (this.left) return;
    if (e.type === 'connected') {
      const peer = e.peer;
      this.linked.add(peer);
      this.paths.set(peer, e.path);
      this.reroute(peer);
      this.fallingBack.delete(peer);
      // Prove the room key first; presence follows once the peer has proved it too.
      this.proofSent.set(peer, this.proof.pair('pw-link', this.self, peer).then((p) => {
        if (this.linked.has(peer)) this.write(peer, frame(T.Proof, fromHex(p)), Lane.Reliable);
      }));
      clearTimeout(this.deadlines.get(peer));
      this.deadlines.set(peer, setTimeout(() => {
        if (!this.proven.has(peer)) this.lost(peer, 'unproven');
      }, Room.proofMs));
    } else if (e.type === 'message') {
      this.onMessage(e.peer, e.data, e.lane);
    } else if (e.type === 'pathChanged') {
      if (this.linked.has(e.peer)) {
        this.paths.set(e.peer, e.path);
        this.reroute(e.peer);
      }
    } else if (e.type === 'disconnected') {
      this.lost(e.peer, e.reason);
    } else if (e.type === 'connectFailed') {
      this.lost(e.peer, 'failed');
    }
  }

  onMessage(peer, data, lane) {
    const tag = data[0];
    const body = data.subarray(1);
    if (!this.proven.has(peer)) return this.unproven(peer, tag, body, data, lane);
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

  // A frame from a peer that hasn't proved the room key: only its proof (or a
  // goodbye) is acted on, and what follows the proof waits for the check.
  unproven(peer, tag, body, data, lane) {
    const held = this.proving.get(peer);
    if (held) {
      if (held.length >= MAX_HELD) return this.lost(peer, 'unproven');
      return void held.push([data, lane]);
    }
    if (tag === T.Bye) return this.lost(peer, 'left');
    if (tag !== T.Proof || !this.linked.has(peer)) return;
    const rest = [];
    this.proving.set(peer, rest);
    this.proof.checkPair('pw-link', peer, this.self, toHex(body)).then((ok) => {
      // Lost (and maybe linked again) meanwhile: this check is stale.
      if (this.left || this.proving.get(peer) !== rest) return;
      this.proving.delete(peer);
      if (!ok) return this.lost(peer, 'unproven');
      clearTimeout(this.deadlines.get(peer));
      this.deadlines.delete(peer);
      this.proven.add(peer);
      this.relayPending(peer);
      this.offerRelays();
      // Our presence goes after our own proof, or the peer drops it as unproven and never
      // lists us: signing that proof can finish after this check (WebCrypto keeps no order).
      const sent = this.proofSent.get(peer);
      sent.then(() => {
        if (this.proven.has(peer) && this.proofSent.get(peer) === sent) this.write(peer, frame(T.Presence, enc.encode(JSON.stringify(this.me))), Lane.Reliable);
      });
      for (const [d, l] of rest) this.onMessage(peer, d, l);
    });
  }

  routeOf(path) {
    if (path.via) return { kind: 'peer', via: path.via };
    if (this.transport.carries?.(path.direct)) return { kind: 'direct', adopted: true };
    return { kind: this.transport.isPipe?.(path.direct) ? 'signaling' : 'direct' };
  }

  // The route to `peer` on `path`. The core learns whether a direct path is the slow
  // signalling pipe, which a member relay beats (offerRelays).
  setRoute(peer, path) {
    const r = this.routeOf(path);
    this.routes.set(peer, r);
    this.retries.track(peer, r.kind !== 'direct', this.self < peer); // the dialling side keeps the backoff
    if (path.direct !== undefined) this.endpoint.setSlow(peer, r.kind === 'signaling' ? path.direct : null);
  }

  // The route to a linked `peer` again: its path changed, or an adopted channel took
  // its link or gave it back.
  reroute(peer) {
    const path = this.paths.get(peer);
    if (!path || !this.linked.has(peer)) return;
    this.setRoute(peer, path);
    this.offerRelays();
  }

  // Every proven member linked to us directly is offered as a relay for every proven
  // peer we reach over signalling: the core probes through it and moves the link there
  // once the peer answers, which it does only if it would use that member too.
  offerRelays() {
    const members = [...this.routes].filter(([p, r]) => r.kind === 'direct' && this.proven.has(p)).map(([p]) => p);
    for (const [p, r] of this.routes) {
      if (r.kind === 'signaling' && this.proven.has(p)) for (const m of members) this.endpoint.addRelay(m, p);
    }
  }

  // A DataChannel closed: the peer is lost if that was its path, or if no fallback
  // is under way; otherwise only the WebRTC link goes. `handle`, the closed link's: a
  // link that isn't the peer's path now (an old one, while the path still names a
  // relayed channel that just ended, say) never drops the peer (NB48b-5).
  rtcClosed(peer, handle) {
    const r = this.routes.get(peer);
    const stale = handle !== undefined && this.paths.get(peer)?.direct !== handle;
    if (r ? r.kind !== 'direct' || r.adopted || stale : this.fallingBack.has(peer)) return this.transport.dropRtc(peer);
    this.lost(peer, 'closed');
  }

  // WebRTC couldn't link `peer`. The side that dials (the smaller id) dials it again
  // through every member linked to us directly (and any that links later) and, unless
  // one answers within PEER_FIRST_MS, over signalling too; the core keeps the first
  // path to answer. The other side just answers.
  rtcFailed(peer) {
    if (this.left || this.fallback === 'off' || this.self > peer || this.endpoint.isConnected(peer)) return;
    const vias = [...this.routes].filter(([p, r]) => p !== peer && r.kind === 'direct' && this.proven.has(p)).map(([p]) => p);
    const pipe = this.transport.pipe?.(peer);
    // Nothing else to try: drop the dead link, so a later hello dials WebRTC again.
    if (!vias.length && pipe == null) return this.lost(peer, 'failed');
    this.fallingBack.add(peer);
    for (const v of vias) this.endpoint.connectVia(v, peer);
    if (pipe == null) return;
    setTimeout(() => !this.left && this.fallingBack.has(peer) && this.endpoint.connect(pipe, peer), Room.peerFirstMs);
  }

  // A member just linked directly and proved the key: it can relay the peers we are
  // still dialling by fallback (the core adds a path to a dial under way).
  relayPending(member) {
    if (this.routes.get(member)?.kind !== 'direct') return;
    for (const p of this.fallingBack) if (p !== member && !this.endpoint.isConnected(p)) this.endpoint.connectVia(member, p);
  }

  lost(peer, reason) {
    this.transport.drop(peer);
    this.linked.delete(peer);
    this.proven.delete(peer);
    this.proving.delete(peer);
    this.proofSent.delete(peer);
    clearTimeout(this.deadlines.get(peer));
    this.deadlines.delete(peer);
    this.outbox.delete(peer);
    this.greeted.delete(peer);
    this.routes.delete(peer);
    this.paths.delete(peer);
    this.fallingBack.delete(peer);
    this.retries.forget(peer);
    // Its sealed close may never come now that its transport is gone: end the session
    // here, so links it relayed go back to signalling at once.
    if (reason === 'left') this.endpoint.disconnect(peer);
    if (this.members.delete(peer)) this.emit('peer-leave', peer, reason);
  }

  write(peer, f, lane) {
    const q = this.outbox.get(peer);
    if (q) return void q.push([f, lane]);
    const r = this.endpoint.sendRetry(peer, f, lane);
    if (r === WOULD_BLOCK) {
      this.outbox.set(peer, [[f, lane]]);
      this.flushSoon();
    } else if (r === TOO_LARGE) {
      throw new RangeError(`message of ${f.length - 1} bytes is too large for this lane`);
    } else if (r < 0 && r !== NOT_CONNECTED) {
      throw new Error(`send failed (${r})`);
    }
  }

  // Sends what the send window takes, oldest first. `from` says who asked: 'drain'
  // (the endpoint handled incoming datagrams, ACKs among them) or 'timer'.
  flush(from) {
    if (this.left) return;
    for (const [peer, q] of this.outbox) {
      while (q.length && this.endpoint.sendRetry(peer, q[0][0], q[0][1]) !== WOULD_BLOCK) q.shift();
      if (!q.length) this.outbox.delete(peer);
    }
    if (!this.outbox.size) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
  }

  // The fallback while the outbox holds frames, should no datagram arrive to drain it.
  flushSoon() {
    if (this.flushTimer) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = null;
      this.flush('timer');
      if (this.outbox.size && !this.left) this.flushSoon();
    }, 2);
  }
}
