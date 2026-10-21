// WebRTC DataChannel transport for a Peerwire Endpoint.
//
// Each remote peer gets an RTCPeerConnection with one negotiated DataChannel
// (unordered, no retransmits): a datagram pipe. Its local handle number is the
// endpoint's address for that peer. Reliability, ordering and authentication
// come from Peerwire on top; the signalling channel is untrusted, because the Noise
// handshake over the DataChannel proves the remote holds the PeerId it claims.
//
// Signalling adapters implement { id, send(to, msg), onMessage(fn), close() };
// messages are JSON-able objects. See BroadcastSignaling below.
//
// When WebRTC can't link two peers (symmetric NAT on both sides, no TURN), the
// transport can carry Peerwire datagrams over the signalling channel itself: a
// *pipe*, one more handle the endpoint dials like any address. Datagrams are
// already Noise-encrypted; Nostr signalling seals each batch to the peer again.
// Pipes are slow (a relay round trip per batch) and rate-capped, a last resort.

import { isPeerId, MAX_DATAGRAM } from './peerwire.js';
import { b64, unb64 } from './nostr.js';

// Above this many bytes queued on a DataChannel, new datagrams are dropped (PERF-1).
export const MAX_BUFFERED = 1024 * 1024;
// A DataChannel not open this long after the offer or answer counts as failed.
export const RTC_FALLBACK_MS = 8000;
// Pipes: datagrams are batched for PIPE_FLUSH_MS into one signalling message of at
// most PIPE_BATCH bytes. All pipes together send PIPE_RATE messages a second (burst
// PIPE_BURST), within what public relays allow one client, and batch more while they
// wait; past PIPE_QUEUE bytes waiting on a pipe, the oldest is dropped.
export const PIPE_FLUSH_MS = 40;
export const PIPE_BATCH = 16 * 1024;
export const PIPE_RATE = 3;
export const PIPE_BURST = 6;
export const PIPE_QUEUE = 64 * 1024;
const MAX_PIPE_ITEMS = 64; // datagrams accepted in one incoming batch
const MAX_PIPE_PEERS = 256; // peers allowed to open a pipe to us

// Removes `host` candidates (LAN, loopback, mDNS .local names) from an SDP, keeping
// server-reflexive (public, from STUN) and relay ones.
export function withoutHostCandidates(sdp) {
  return sdp.split('\r\n').filter((l) => !(l.startsWith('a=candidate:') && / typ host( |$)/.test(l))).join('\r\n');
}

export class WebRtcTransport {
  // `loss` drops that fraction of outgoing datagrams, for testing.
  // `trickle: false` sends one offer/answer with every candidate once ICE
  // gathering finishes (fewer signalling messages, for relays like Nostr).
  // `RTCPeerConnection` lets Node pass in an implementation (node-datachannel).
  // `privacy`: which addresses peers are given in ICE candidates.
  //   'direct' (default): all of them.
  //   'public-only': no `host` candidates (LAN, loopback): only what STUN sees.
  //   'relay-only': a WebRTC link to `relay` (a peer id, public-only) and to no one
  //     else; reach others with endpoint.connectVia(relay, peer), which stays
  //     relayed. Browsers have no relay but a peer: with no TURN server there is
  //     no relay-only path to a peer that isn't linked to `relay`.
  // `offerProof`: { sign(to), check(from, p) }, both async. Offers carry
  //   `p: await sign(to)`, and an offer is answered (with our addresses) only if
  //   check(from, p) holds. joinRoom passes a room-key proof (see proof.js).
  // `onPeer(peer, state)`: 'open' and 'closed' for a DataChannel, and 'failed' once
  //   when one isn't open `fallbackMs` after its offer or answer (or ICE failed).
  // `pipes`: carry datagrams over signalling for peers allowed with allowPipe().
  constructor(endpoint, signaling, { iceServers = [], onPeer = () => {}, loss = 0, trickle = true, privacy = 'direct', relay = null, offerProof = null, pipes = false, fallbackMs = RTC_FALLBACK_MS, RTCPeerConnection = globalThis.RTCPeerConnection } = {}) {
    if (!['direct', 'public-only', 'relay-only'].includes(privacy)) throw new Error(`unknown privacy mode: ${privacy}`);
    if (privacy === 'relay-only' && !isPeerId(relay)) throw new TypeError("privacy 'relay-only' needs a relay peer id");
    this.ep = endpoint;
    this.Pc = RTCPeerConnection;
    this.privacy = privacy;
    this.relay = relay;
    this.offerProof = offerProof;
    this.checking = new Map(); // peer hex -> signals that came while its offer was checked
    if (privacy === 'relay-only') endpoint.setRelayOnly(true);
    this.trickle = trickle;
    this.dropped = 0; // datagrams dropped by the simulated `loss`
    this.congested = 0; // datagrams dropped because the DataChannel was full or refused them
    this.badSignals = 0; // signals refused as malformed or unproven (see onSignal)
    this.sig = signaling;
    this.iceServers = iceServers;
    this.onPeer = onPeer;
    this.links = new Map(); // handle -> { pc, dc, peer }
    this.byPeer = new Map(); // peer hex -> handle
    this.nextHandle = 1;
    this.fallbackMs = fallbackMs;
    this.pipesOn = pipes;
    this.pipes = new Map(); // handle -> pipe (see pipe())
    this.pipeByPeer = new Map(); // peer hex -> handle
    this.pipeAllowed = new Set(); // peers whose datagrams over signalling we take
    this.pipeTokens = PIPE_BURST; // the rate budget all pipes share
    this.pipeRefilled = Date.now();
    this.piped = { sent: 0, received: 0, dropped: 0 }; // batches sent; datagrams received and dropped
    endpoint.onTransmit = (handle, bytes) => {
      if (loss && Math.random() < loss) {
        this.dropped++;
        return;
      }
      const pipe = this.pipes.get(handle);
      if (pipe) return this.pipeSend(pipe, bytes);
      const l = this.links.get(handle);
      if (!l || l.dc.readyState !== 'open') return;
      // A full SCTP buffer means the path is congested: drop like UDP would, and
      // let the core's congestion control back off. dc.send throws when it's full.
      if (l.dc.bufferedAmount > MAX_BUFFERED) return void this.congested++;
      try {
        l.dc.send(bytes);
      } catch {
        this.congested++;
      }
    };
    // Signals are untrusted: one that fails must not become an unhandled rejection
    // (which ends a Node process).
    signaling.onMessage((from, msg) => void this.onSignal(from, msg).catch(() => void this.badSignals++));
  }

  link(peer) {
    let h = this.byPeer.get(peer);
    if (h !== undefined) return this.links.get(h);
    h = this.nextHandle++;
    const pc = new this.Pc({ iceServers: this.iceServers });
    const dc = pc.createDataChannel('peerwire', { negotiated: true, id: 0, ordered: false, maxRetransmits: 0 });
    dc.binaryType = 'arraybuffer';
    const l = { pc, dc, peer, handle: h, dialer: false, timer: null, failed: false };
    dc.onopen = () => {
      clearTimeout(l.timer);
      if (l.dialer) this.ep.connect(h, peer);
      this.onPeer(peer, 'open');
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'failed') this.failed(l);
    };
    dc.onclose = () => this.onPeer(peer, 'closed');
    // Binary datagrams only: a string message would become a length in new Uint8Array(string).
    dc.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer && e.data.byteLength <= MAX_DATAGRAM) this.ep.receive(h, new Uint8Array(e.data));
    };
    pc.onicecandidate = (e) => {
      if (!e.candidate || !this.trickle) return;
      if (this.privacy !== 'direct' && / typ host( |$)/.test(e.candidate.candidate)) return;
      this.sig.send(peer, { kind: 'ice', candidate: e.candidate.toJSON() });
    };
    this.links.set(h, l);
    this.byPeer.set(peer, h);
    return l;
  }

  // Starts the clock on a link once its offer or answer is out.
  watch(l) {
    clearTimeout(l.timer);
    if (l.dc.readyState !== 'open') l.timer = setTimeout(() => this.failed(l), this.fallbackMs);
  }

  failed(l) {
    clearTimeout(l.timer);
    if (l.failed || l.dc.readyState === 'open' || this.links.get(l.handle) !== l) return;
    l.failed = true;
    this.onPeer(l.peer, 'failed');
  }

  // Whether WebRTC (which hands the peer our addresses) may run with `peer`.
  allowed(peer) {
    return this.privacy !== 'relay-only' || peer === this.relay;
  }

  // The local description to send, with host candidates left out unless 'direct'.
  description(pc) {
    const d = pc.localDescription.toJSON();
    return this.privacy === 'direct' ? d : { type: d.type, sdp: withoutHostCandidates(d.sdp) };
  }

  // Opens a DataChannel to `peer` and, once open, runs the Peerwire handshake.
  async dial(peer) {
    if (!isPeerId(peer)) throw new TypeError('dial: not a peer id (64 lowercase hex characters)');
    if (!this.allowed(peer)) throw new Error('relay-only: reach this peer with endpoint.connectVia(relay, peer)');
    if (this.byPeer.has(peer)) return;
    const l = this.link(peer);
    l.dialer = true;
    await l.pc.setLocalDescription(await l.pc.createOffer());
    await this.gathered(l.pc);
    const p = this.offerProof ? await this.offerProof.sign(peer) : undefined;
    this.sig.send(peer, { kind: 'offer', sdp: this.description(l.pc), p });
    this.watch(l);
  }

  async onSignal(from, msg) {
    // Signalling is untrusted: a `from` that isn't a PeerId never gets a link.
    if (!isPeerId(from) || !msg) return void this.badSignals++;
    if (!this.allowed(from)) return;
    if (msg.kind === 'dg') return this.pipeReceive(from, msg.d);
    const held = this.checking.get(from);
    if (held) return void held.push(msg);
    if (msg.kind === 'offer' && this.offerProof && !this.byPeer.has(from)) {
      // Hold what follows (trickled candidates) until the proof is checked.
      this.checking.set(from, []);
      const ok = await this.offerProof.check(from, msg.p).catch(() => false);
      const rest = this.checking.get(from) ?? [];
      this.checking.delete(from);
      if (!ok) return void this.badSignals++;
      await this.apply(from, msg);
      for (const m of rest) await this.onSignal(from, m);
      return;
    }
    await this.apply(from, msg);
  }

  async apply(from, msg) {
    // Only an offer opens a link; answers and candidates for a dropped one are stale.
    if (msg.kind !== 'offer' && !this.byPeer.has(from)) return;
    if (msg.kind === 'offer') {
      const fresh = !this.byPeer.has(from);
      const l = this.link(from);
      try {
        await l.pc.setRemoteDescription(msg.sdp);
        await l.pc.setLocalDescription(await l.pc.createAnswer());
      } catch {
        // A malformed offer: the link it opened is dropped, so it can't block a later one.
        if (fresh && this.links.get(l.handle) === l) this.drop(from);
        return void this.badSignals++;
      }
      await this.gathered(l.pc);
      this.sig.send(from, { kind: 'answer', sdp: this.description(l.pc) });
      this.watch(l);
    } else if (msg.kind === 'answer') {
      const l = this.link(from);
      try {
        await l.pc.setRemoteDescription(msg.sdp);
      } catch {
        // Malformed, or a copy for a link already answered: ignored, the dial times out or stands.
      }
    } else if (msg.kind === 'ice') {
      const l = this.link(from);
      try {
        await l.pc.addIceCandidate(msg.candidate);
      } catch (e) {
        // Candidates can race the remote description; the next ones will do.
      }
    }
  }

  // Without trickle, waits (up to 2 s) for ICE gathering to finish.
  gathered(pc) {
    if (this.trickle || pc.iceGatheringState === 'complete') return Promise.resolve();
    return new Promise((resolve) => {
      const done = () => {
        if (pc.iceGatheringState === 'complete') resolve();
      };
      pc.addEventListener('icegatheringstatechange', done);
      setTimeout(resolve, 2000);
    });
  }

  has(peer) {
    return this.byPeer.has(peer);
  }

  // Lets `peer` (one that proved the room key) send us datagrams over signalling.
  allowPipe(peer) {
    if (!this.pipesOn || this.pipeAllowed.has(peer)) return;
    this.pipeAllowed.add(peer);
    if (this.pipeAllowed.size > MAX_PIPE_PEERS) this.pipeAllowed.delete(this.pipeAllowed.values().next().value);
  }

  isPipe(handle) {
    return this.pipes.has(handle);
  }

  // The endpoint address that reaches `peer` over signalling, or null if pipes are off.
  pipe(peer) {
    if (!this.pipesOn || !isPeerId(peer)) return null;
    let h = this.pipeByPeer.get(peer);
    if (h !== undefined) return h;
    h = this.nextHandle++;
    this.pipes.set(h, { peer, handle: h, q: [], bytes: 0, timer: null });
    this.pipeByPeer.set(peer, h);
    return h;
  }

  pipeSend(p, bytes) {
    p.q.push([b64(bytes), bytes.length]);
    p.bytes += bytes.length;
    // Like a full UDP buffer: drop the oldest and let congestion control back off.
    while (p.bytes > PIPE_QUEUE && p.q.length > 1) {
      p.bytes -= p.q.shift()[1];
      this.piped.dropped++;
    }
    this.pipeFlushSoon(p, PIPE_FLUSH_MS);
  }

  pipeFlushSoon(p, ms) {
    if (!p.timer) p.timer = setTimeout(() => this.pipeFlush(p), ms);
  }

  pipeFlush(p) {
    p.timer = null;
    if (this.pipes.get(p.handle) !== p || !p.q.length) return;
    const now = Date.now();
    this.pipeTokens = Math.min(PIPE_BURST, this.pipeTokens + ((now - this.pipeRefilled) / 1000) * PIPE_RATE);
    this.pipeRefilled = now;
    if (this.pipeTokens < 1) return this.pipeFlushSoon(p, Math.ceil(((1 - this.pipeTokens) * 1000) / PIPE_RATE));
    this.pipeTokens -= 1;
    const d = [];
    let n = 0;
    while (p.q.length && d.length < MAX_PIPE_ITEMS && (n === 0 || n + p.q[0][1] <= PIPE_BATCH)) {
      const [s, len] = p.q.shift();
      n += len;
      d.push(s);
    }
    p.bytes -= n;
    this.piped.sent++;
    try {
      Promise.resolve(this.sig.send(p.peer, { kind: 'dg', d })).catch(() => {});
    } catch {
      // A closed signalling channel: the datagrams are lost, as on a dead path.
    }
    if (p.q.length) this.pipeFlushSoon(p, PIPE_FLUSH_MS);
  }

  pipeReceive(from, d) {
    if (!this.pipeAllowed.has(from) || !Array.isArray(d) || d.length > MAX_PIPE_ITEMS) return void this.badSignals++;
    const h = this.pipe(from);
    for (const s of d) {
      if (typeof s !== 'string' || s.length > (MAX_DATAGRAM * 4) / 3 + 4) return void this.badSignals++;
      let bytes;
      try {
        bytes = unb64(s);
      } catch {
        return void this.badSignals++;
      }
      this.piped.received++;
      this.ep.receive(h, bytes);
    }
  }

  // Forgets the WebRTC link to `peer`, so a later dial or offer starts afresh.
  dropRtc(peer) {
    const h = this.byPeer.get(peer);
    if (h === undefined) return;
    const l = this.links.get(h);
    clearTimeout(l.timer);
    l.dc.onclose = null;
    l.pc.onconnectionstatechange = null;
    l.pc.close();
    this.links.delete(h);
    this.byPeer.delete(peer);
  }

  // Forgets every path to `peer`: its WebRTC link and its pipe.
  drop(peer) {
    this.dropRtc(peer);
    const h = this.pipeByPeer.get(peer);
    if (h === undefined) return;
    clearTimeout(this.pipes.get(h).timer);
    this.pipes.delete(h);
    this.pipeByPeer.delete(peer);
  }

  close() {
    this.checking.clear();
    for (const p of this.pipes.values()) clearTimeout(p.timer);
    this.pipes.clear();
    this.pipeByPeer.clear();
    for (const l of this.links.values()) {
      clearTimeout(l.timer);
      l.dc.onclose = null;
      l.pc.onconnectionstatechange = null;
      l.pc.close();
    }
    this.links.clear();
    this.byPeer.clear();
    this.sig.close();
  }
}

// Signalling between tabs of one browser on one machine (same origin), for
// local tests and demos. Presence is announced with 'hello'; the peer with the
// smaller id dials, so only one offer is made per pair.
export class BroadcastSignaling {
  constructor(id, room = 'peerwire-signal') {
    this.id = id;
    this.ch = new BroadcastChannel(room);
    this.handlers = [];
    this.onHello = () => {};
    this.ch.onmessage = (e) => {
      const m = e.data;
      // Any script on the origin can post here: drop anything without a PeerId `from`.
      if (!m || !isPeerId(m.from) || m.from === this.id) return;
      if (m.kind === 'hello') return this.onHello(m.from, m.meta);
      if (m.to !== this.id) return;
      for (const h of this.handlers) h(m.from, m.body);
    };
  }
  // `meta` is free-form presence data (a role, a room nickname).
  hello(meta) {
    this.ch.postMessage({ kind: 'hello', from: this.id, meta });
  }
  send(to, body) {
    if (!isPeerId(to)) throw new TypeError('send: `to` must be a peer id (64 lowercase hex characters)');
    this.ch.postMessage({ kind: 'msg', from: this.id, to, body });
  }
  onMessage(fn) {
    this.handlers.push(fn);
  }
  close() {
    this.ch.close();
  }
}
