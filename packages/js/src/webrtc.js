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
// Same NAT: a peer whose server-reflexive (public) IPv4 is one of ours sits behind the
// same NAT, like a laptop on a phone's hotspot. Browsers find no direct path there
// unless they show their LAN addresses (mDNS hides them; Android Chrome lists only its
// mobile-data interface), so such a link counts as failed this soon instead. Strangers
// behind one carrier NAT match too, and seldom link direct either. ICE goes on checking:
// a DataChannel that opens after the fallback takes the link over (see dc.onopen).
export const RTC_SAME_NAT_FALLBACK_MS = 3000;
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
export const MAX_PIPE_PEERS = 256; // peers allowed to open a pipe to us
// Without trickle, an offer or answer goes once ICE gathering is complete, or
// GATHER_SETTLE_MS after the first public (srflx/relay) candidate, or at
// GATHER_MAX_MS. Candidates found later go in an 'ice-batch', the first LATE_ICE_MS
// after the description at the latest; a candidate found after that batch (a slow
// public address on a device that gathered mDNS first) is sent in a further batch,
// LATE_FLUSH_MS after it arrives, up to MAX_LATE candidates in all.
export const GATHER_SETTLE_MS = 300;
export const GATHER_MAX_MS = 2000;
export const LATE_ICE_MS = 3000;
export const LATE_FLUSH_MS = 400;
const MAX_LATE = 64;
// Signalling relays drop messages: an offer with no answer is sent again at these
// times after the first; an offer seen twice is answered with the same answer.
export const OFFER_RESEND_MS = [3000, 6000];
const MAX_ICE_BATCH = 32; // candidates in one 'ice-batch', and held before a remote description
// ICE restart, for a link whose DataChannel was open: once ICE has been
// 'disconnected' for RESTART_AFTER_MS, or at once on 'failed', the dialling side
// renegotiates with new ICE credentials (the other side asks it to, so offers never
// cross). At most MAX_RESTARTS in a row, RESTART_AFTER_MS doubling between them,
// counted afresh once ICE connects again. The Peerwire session keeps its handle.
export const RESTART_AFTER_MS = 2000;
export const MAX_RESTARTS = 3;
// Port prediction (as the native traversal does): a NAT whose two STUN mappings share
// an IP and are 1 to MAX_STEP ports apart hands ports out in sequence. A peer not
// behind a symmetric NAT itself adds GUESS_BEFORE + GUESS_AFTER + 1 candidates around
// the predicted port, so its checks open its NAT to the port the other's will come from.
export const MAX_STEP = 8;
export const GUESS_BEFORE = 4;
export const GUESS_AFTER = 27;
// adopt(): a DataChannel made outside the transport carries a link only once both
// ends adopt it. Each end sends CARRY every CARRY_EVERY_MS until it hears from the
// other, and answers a CARRY with CARRIED (which gets no answer), at most once per
// CARRY_EVERY_MS, so a flood of CARRY gets a trickle of replies. These are
// one-byte messages, which no Peerwire datagram is. Without a word from the far end
// in CARRY_WAIT_MS, the channel is left to the caller.
export const CARRY = 1;
export const CARRIED = 2;
export const CARRY_EVERY_MS = 200;
export const CARRY_WAIT_MS = 5000;

const CANDIDATE = /^(?:a=)?candidate:\S+ \d+ udp \d+ (\S+) (\d+) typ (\w+)/i;
const IPV4 = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;

// What the server-reflexive candidates in `sdp` (a description, or candidate lines)
// say about the NAT: 'symmetric' if one IPv4 address comes with two ports (two STUN
// servers saw two mappings), with { ip, next, step } when the ports are 1 to MAX_STEP
// apart (a sequential NAT: `next` is its likely next port); 'cone' for one port (a
// browser lists a mapping two servers agree on once, so one server answering looks
// the same); 'unknown' with no server-reflexive IPv4 candidate.
export function natFromSdp(sdp) {
  const ports = new Map(); // IPv4 -> Set of ports
  for (const l of typeof sdp === 'string' ? sdp.split(/\r?\n/) : []) {
    const m = CANDIDATE.exec(l.trim());
    if (!m || m[3] !== 'srflx' || !IPV4.test(m[1])) continue;
    if (!ports.has(m[1])) ports.set(m[1], new Set());
    ports.get(m[1]).add(Number(m[2]));
  }
  if (!ports.size) return { type: 'unknown' };
  for (const [ip, set] of ports) {
    if (set.size < 2) continue;
    const [a, b] = [...set].sort((x, y) => x - y);
    const step = b - a;
    const out = { type: 'symmetric' };
    if (set.size === 2 && step >= 1 && step <= MAX_STEP && b + step <= 65535) Object.assign(out, { ip, next: b + step, step });
    return out;
  }
  return { type: 'cone' };
}

// The server-reflexive IPv4 addresses in `text` (a description, or candidate lines).
export function srflxV4(text) {
  const out = [];
  for (const l of typeof text === 'string' ? text.split(/\r?\n/) : []) {
    const m = CANDIDATE.exec(l.trim());
    if (m && m[3] === 'srflx' && IPV4.test(m[1])) out.push(m[1]);
  }
  return out;
}

// A peer's prediction as sent in signalling, checked: { ip, next, step } or null.
export function checkPrediction(nat) {
  if (!nat || typeof nat !== 'object' || typeof nat.ip !== 'string' || !IPV4.test(nat.ip)) return null;
  const { next, step } = nat;
  if (!Number.isInteger(next) || !Number.isInteger(step) || step < 1 || step > MAX_STEP || next < 1 || next > 65535) return null;
  return { ip: nat.ip, next, step };
}

// The ICE candidates to add for a peer's prediction: its IP at ports
// next - GUESS_BEFORE·step to next + GUESS_AFTER·step, as low-priority srflx ones.
export function predictedCandidates(nat, sdpMid = '0') {
  const p = checkPrediction(nat);
  if (!p) return [];
  const out = [];
  for (let k = -GUESS_BEFORE; k <= GUESS_AFTER; k++) {
    const port = p.next + k * p.step;
    if (port < 1 || port > 65535) continue;
    const candidate = `candidate:pw${k + GUESS_BEFORE} 1 udp 1 ${p.ip} ${port} typ srflx raddr 0.0.0.0 rport 0`;
    out.push({ candidate, sdpMid, sdpMLineIndex: 0 });
  }
  return out;
}

// Counts the ICE candidates in an SDP by type and family: host candidates with an
// mDNS name (.local) are `mdns`, as only the same LAN can resolve them.
export function candidateSummary(sdp) {
  const out = { host4: 0, host6: 0, mdns: 0, srflx4: 0, srflx6: 0, prflx: 0, relay: 0 };
  if (typeof sdp !== 'string') return out;
  for (const l of sdp.split(/\r?\n/)) {
    const m = /^a=candidate:\S+ \d+ \S+ \d+ (\S+) \d+ typ (\w+)/.exec(l);
    if (!m) continue;
    const fam = m[1].includes(':') ? 6 : 4;
    if (m[2] === 'host') out[m[1].endsWith('.local') ? 'mdns' : `host${fam}`]++;
    else if (m[2] === 'srflx') out[`srflx${fam}`]++;
    else if (m[2] in out) out[m[2]]++;
  }
  return out;
}

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
  // `onPeer(peer, state, handle)`: 'open' and 'closed' for a DataChannel (with its
  //   link's endpoint handle), and 'failed' once when one isn't open `fallbackMs`
  //   after its offer or answer (`sameNatFallbackMs` for a peer behind our public
  //   IPv4), or ICE failed.
  // `pipes`: carry datagrams over signalling for peers allowed with allowPipe().
  // `trace(peer, what, detail)`, settable later: diagnostics of each link's
  //   signalling ('offer-sent', 'offer-received', 'answer-sent', 'answer-received',
  //   'ice-batch-sent', 'ice-batch-received'; detail has candidateSummary counts),
  //   of ICE ('ice-state', 'candidate-error') and 'same-nat' ({ ip, ms }: the peer
  //   has our public IPv4, so the link falls back after `ms`).
  constructor(endpoint, signaling, { iceServers = [], onPeer = () => {}, loss = 0, trickle = true, privacy = 'direct', relay = null, offerProof = null, pipes = false, fallbackMs = RTC_FALLBACK_MS, sameNatFallbackMs = RTC_SAME_NAT_FALLBACK_MS, RTCPeerConnection = globalThis.RTCPeerConnection } = {}) {
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
    this.resent = 0; // offers and answers sent again (OFFER_RESEND_MS)
    this.restarts = 0; // ICE restarts begun (as the dialling side)
    this.trace = null;
    this.sig = signaling;
    this.iceServers = iceServers;
    this.onPeer = onPeer;
    this.links = new Map(); // handle -> { pc, dc, peer }
    this.byPeer = new Map(); // peer hex -> handle
    this.nextHandle = 1;
    this.fallbackMs = fallbackMs;
    this.sameNatMs = Math.min(sameNatFallbackMs, fallbackMs);
    this.pipesOn = pipes;
    this.pipes = new Map(); // handle -> pipe (see pipe())
    this.pipeByPeer = new Map(); // peer hex -> handle
    this.pipeAllowed = new Set(); // peers whose datagrams over signalling we take
    this.pipeTokens = PIPE_BURST; // the rate budget all pipes share
    this.pipeRefilled = Date.now();
    // Batches sent; datagrams received, dropped (queue full), and stale (from a peer whose
    // own DataChannel carries its datagrams now: sent before it opened).
    this.piped = { sent: 0, received: 0, dropped: 0, stale: 0 };
    this.carriers = new Map(); // peer hex -> adopted channel (see adopt())
    this.carried = new Map(); // endpoint handle -> the adopted channel carrying it now
    endpoint.onTransmit = (handle, bytes) => {
      if (loss && Math.random() < loss) {
        this.dropped++;
        return;
      }
      const c = this.carried.get(handle);
      if (c && c.dc.readyState === 'open') return this.dcSend(c.dc, bytes);
      const pipe = this.pipes.get(handle);
      if (pipe) {
        // The peer's own DataChannel opened after its link fell back to this pipe: the
        // datagrams go over the channel, and the far end's core, hearing them there,
        // moves the link onto it (as a direct path moves to a new address), then ours.
        const own = this.links.get(this.byPeer.get(pipe.peer));
        return own?.dc.readyState === 'open' ? this.dcSend(own.dc, bytes) : this.pipeSend(pipe, bytes);
      }
      const l = this.links.get(handle);
      if (l && l.dc.readyState === 'open') this.dcSend(l.dc, bytes);
    };
    // Signals are untrusted: one that fails must not become an unhandled rejection
    // (which ends a Node process).
    signaling.onMessage((from, msg) => void this.onSignal(from, msg).catch(() => void this.badSignals++));
  }

  dcSend(dc, bytes) {
    // A full SCTP buffer means the path is congested: drop like UDP would, and
    // let the core's congestion control back off. dc.send throws when it's full.
    if (dc.bufferedAmount > MAX_BUFFERED) return void this.congested++;
    try {
      dc.send(bytes);
    } catch {
      this.congested++;
    }
  }

  link(peer) {
    let h = this.byPeer.get(peer);
    if (h !== undefined) return this.links.get(h);
    h = this.nextHandle++;
    const pc = new this.Pc({ iceServers: this.iceServers });
    const dc = pc.createDataChannel('peerwire', { negotiated: true, id: 0, ordered: false, maxRetransmits: 0 });
    dc.binaryType = 'arraybuffer';
    // Without trickle: `described` once the offer or answer is out, `late` holds the
    // candidates found after it, `held` the peer's that came before its description.
    // `mine` and `theirs`: the public IPv4s each side's candidates name (see sameNat);
    // `heard` once the DataChannel has carried a datagram from the peer.
    const l = { pc, dc, peer, handle: h, dialer: false, timer: null, watchedAt: 0, failed: false, described: false, late: [], lateTimer: null,
      held: [], offerMsg: null, offerSdp: null, answerMsg: null, resends: [],
      lateSent: 0, opened: false, restarts: 0, restartTimer: null, guessed: null,
      mine: new Set(), theirs: new Set(), sameNat: false, heard: false };
    dc.onopen = () => {
      clearTimeout(l.timer);
      l.opened = true;
      // Opened after the room fell back (ICE went on checking): a link relayed by a
      // member punches over it and moves there; a pipe's datagrams go over it (see
      // onTransmit). Else the dialler's handshake goes over it, joining a dial under way.
      if (this.ep.isConnected(peer)) this.ep.addCandidate(h, peer);
      else if (l.dialer) this.ep.connect(h, peer);
      this.onPeer(peer, 'open');
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'failed') this.failed(l);
    };
    dc.onclose = () => this.onPeer(peer, 'closed', h);
    // Binary datagrams only: a string message would become a length in new Uint8Array(string).
    dc.onmessage = (e) => {
      if (!(e.data instanceof ArrayBuffer) || e.data.byteLength > MAX_DATAGRAM) return;
      l.heard = true;
      this.ep.receive(h, new Uint8Array(e.data));
    };
    pc.onicecandidate = (e) => {
      const c = e.candidate;
      if (c) this.sameNat(l, l.mine, c.candidate);
      if (c && this.privacy !== 'direct' && / typ host( |$)/.test(c.candidate)) return;
      if (this.trickle) return void (c && this.signal(peer, { kind: 'ice', candidate: c.toJSON() }));
      if (!l.described) return; // still to go in the description
      if (!c) return void this.sendLate(l); // gathering complete: send what is buffered
      if (l.lateSent + l.late.length >= MAX_LATE) return; // bounded
      l.late.push(c.toJSON());
      // The first batch waits for gathering to settle (describe()'s LATE_ICE_MS timer);
      // a candidate found after that batch is sent LATE_FLUSH_MS later, so a slow public
      // address (a device that gathered mDNS first) still reaches the peer.
      if (l.lateSent > 0 && !l.lateTimer) l.lateTimer = setTimeout(() => this.sendLate(l), LATE_FLUSH_MS);
    };
    if (typeof pc.addEventListener === 'function') {
      pc.addEventListener('iceconnectionstatechange', () => {
        this.tr(peer, 'ice-state', { state: pc.iceConnectionState });
        this.iceChanged(l);
      });
      pc.addEventListener('icecandidateerror', (e) => this.tr(peer, 'candidate-error', { url: e.url, code: e.errorCode, text: e.errorText, address: e.address }));
    }
    this.links.set(h, l);
    this.byPeer.set(peer, h);
    return l;
  }

  // Starts the clock on a link once its offer or answer is out; `again` re-arms it from
  // the same start, for a link just found to be behind our NAT.
  watch(l, again = false) {
    if (!again) l.watchedAt = Date.now();
    clearTimeout(l.timer);
    const ms = l.sameNat ? this.sameNatMs : this.fallbackMs;
    if (l.dc.readyState !== 'open') l.timer = setTimeout(() => this.failed(l), Math.max(0, l.watchedAt + ms - Date.now()));
  }

  // Notes the public IPv4s that candidates in `text` name, ours or the peer's (`set`),
  // and once the two sides share one, shortens the link's clock to sameNatMs (see
  // RTC_SAME_NAT_FALLBACK_MS). Called for every description and candidate either way.
  sameNat(l, set, text) {
    for (const ip of srflxV4(text)) if (set.size < MAX_LATE) set.add(ip);
    if (l.sameNat || l.opened || l.failed) return;
    const ip = [...l.mine].find((a) => l.theirs.has(a));
    if (!ip) return;
    l.sameNat = true;
    this.tr(l.peer, 'same-nat', { ip, ms: this.sameNatMs });
    if (l.watchedAt) this.watch(l, true);
  }

  failed(l) {
    clearTimeout(l.timer);
    if (l.opened) return this.restartSoon(l, 0); // was linked: try to get it back
    if (l.failed || l.dc.readyState === 'open' || this.links.get(l.handle) !== l) return;
    l.failed = true;
    this.onPeer(l.peer, 'failed');
  }

  // ICE state of a link whose DataChannel was open: connected again resets the
  // restart count; 'disconnected' restarts after a grace, 'failed' at once.
  iceChanged(l) {
    if (!l.opened || this.links.get(l.handle) !== l) return;
    const s = l.pc.iceConnectionState;
    if (s === 'connected' || s === 'completed') {
      clearTimeout(l.restartTimer);
      l.restartTimer = null;
      l.restarts = 0;
    } else if (s === 'disconnected') this.restartSoon(l, RESTART_AFTER_MS);
    else if (s === 'failed') this.restartSoon(l, 0);
  }

  // Restarts ICE on `l` in `ms`, unless one is already due (a sooner one wins).
  restartSoon(l, ms) {
    if (l.restartTimer && ms > 0) return;
    clearTimeout(l.restartTimer);
    l.restartTimer = setTimeout(() => {
      l.restartTimer = null;
      this.restartIce(l, 'ice');
    }, ms);
  }

  // The network changed (a network switch, or back online): links that were open but
  // whose ICE isn't connected now restart at once, rather than after the grace.
  networkChanged() {
    for (const l of this.links.values()) {
      const s = l.pc.iceConnectionState;
      if (l.opened && s !== 'connected' && s !== 'completed') this.restartSoon(l, 0);
    }
  }

  // Renegotiates `l` with new ICE credentials, so ICE gathers again (new local
  // addresses after a network change) and checks every pair afresh. The DataChannel
  // and the Peerwire session over it carry on. Only the dialling side offers; the
  // other asks it to with a 'restart' signal.
  async restartIce(l, reason) {
    if (!l.opened || this.links.get(l.handle) !== l || l.restarts >= MAX_RESTARTS) return;
    clearTimeout(l.restartTimer);
    l.restarts++;
    const again = RESTART_AFTER_MS * 2 ** l.restarts; // if this one doesn't connect either
    l.restartTimer = setTimeout(() => {
      l.restartTimer = null;
      const s = l.pc.iceConnectionState;
      if (s !== 'connected' && s !== 'completed') this.restartIce(l, 'retry');
    }, again);
    this.tr(l.peer, 'ice-restart', { n: l.restarts, reason, dialer: l.dialer });
    if (!l.dialer) return this.signal(l.peer, { kind: 'restart' });
    this.restarts++;
    try {
      await l.pc.setLocalDescription(await l.pc.createOffer({ iceRestart: true }));
    } catch {
      return; // mid-negotiation or closed: the retry timer tries again
    }
    Object.assign(l, { described: false, late: [], lateSent: 0 });
    clearTimeout(l.lateTimer);
    await this.gathered(l.pc);
    const p = this.offerProof ? await this.offerProof.sign(l.peer) : undefined;
    if (this.links.get(l.handle) !== l) return;
    l.offerMsg = { kind: 'offer', sdp: this.describe(l), p, restart: true, ...this.prediction(l) };
    this.signal(l.peer, l.offerMsg);
    this.tr(l.peer, 'offer-sent', { ...candidateSummary(l.offerMsg.sdp.sdp), restart: true });
    this.resendOffer(l);
  }

  // Whether `l` sent an offer and waits for its answer.
  awaiting(l) {
    const st = l.pc.signalingState;
    return st === undefined ? !l.pc.remoteDescription : st === 'have-local-offer';
  }

  // No answer yet: the offer (or the answer) was lost on the way; send it again.
  resendOffer(l) {
    for (const t of l.resends) clearTimeout(t);
    const msg = l.offerMsg;
    l.resends = OFFER_RESEND_MS.map((ms) => setTimeout(() => {
      if (this.links.get(l.handle) !== l || l.offerMsg !== msg || !this.awaiting(l) || l.failed) return;
      if (!msg.restart && l.dc.readyState === 'open') return;
      this.resent++;
      this.signal(l.peer, msg);
      this.tr(l.peer, 'offer-sent', { ...candidateSummary(msg.sdp.sdp), resend: true, restart: !!msg.restart });
    }, ms));
  }

  // Our NAT's port prediction, for a message carrying our description: { nat } when
  // our server-reflexive candidates show a sequential symmetric NAT, else nothing.
  prediction(l, extra = []) {
    const sdp = [l.pc.localDescription?.sdp ?? '', ...extra.map((c) => `a=${c.candidate}`)].join('\n');
    const nat = natFromSdp(sdp);
    return nat.next ? { nat: { ip: nat.ip, next: nat.next, step: nat.step } } : {};
  }

  // A peer's prediction came: unless our own NAT is symmetric (each probe would cost
  // it a port, and its own mapping isn't the one the peer saw), add candidates at the
  // ports around it, once per prediction.
  async guess(l, nat) {
    const p = checkPrediction(nat);
    if (!p || !l.pc.remoteDescription || this.privacy === 'relay-only') return;
    const key = `${p.ip}:${p.next}:${p.step}`;
    if (l.guessed === key || natFromSdp(l.pc.localDescription?.sdp).type === 'symmetric') return;
    l.guessed = key;
    const mid = /^a=mid:(\S+)/m.exec(l.pc.remoteDescription.sdp ?? '')?.[1] ?? '0';
    const cands = predictedCandidates(p, mid);
    this.tr(l.peer, 'guesses', { ip: p.ip, next: p.next, step: p.step, n: cands.length });
    await this.addCandidates(l, cands);
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

  // Sends a signal; a closed or failing signalling channel loses it, like a relay would.
  signal(peer, msg) {
    try {
      Promise.resolve(this.sig.send(peer, msg)).catch(() => {});
    } catch {
      // lost
    }
  }

  tr(peer, what, detail) {
    try {
      this.trace?.(peer, what, detail);
    } catch {
      // diagnostics never break a link
    }
  }

  // Takes the description to send (and starts collecting late candidates).
  describe(l) {
    const d = this.description(l.pc);
    l.described = true;
    this.sameNat(l, l.mine, l.pc.localDescription?.sdp);
    if (!this.trickle && l.pc.iceGatheringState !== 'complete') l.lateTimer = setTimeout(() => this.sendLate(l), LATE_ICE_MS);
    return d;
  }

  // Sends the late candidates buffered in `l.late` as an 'ice-batch', clearing it.
  // Called when gathering completes, when the first batch is due (LATE_ICE_MS), and
  // on the debounce after a later candidate, so candidates that arrive after the
  // first batch are sent too, not lost. At most MAX_ICE_BATCH per batch (the receiver
  // refuses more); the rest go next tick.
  sendLate(l) {
    clearTimeout(l.lateTimer);
    l.lateTimer = null;
    if (this.links.get(l.handle) !== l || !l.late.length) return;
    const batch = l.late.splice(0, MAX_ICE_BATCH);
    l.lateSent += batch.length;
    this.signal(l.peer, { kind: 'ice-batch', c: batch, ...this.prediction(l, batch) });
    this.tr(l.peer, 'ice-batch-sent', { ...candidateSummary(batch.map((c) => `a=${c.candidate}`).join('\n')), more: l.late.length > 0 });
    if (l.late.length) l.lateTimer = setTimeout(() => this.sendLate(l), 0);
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
    if (this.links.get(l.handle) !== l) return; // dropped meanwhile
    l.offerMsg = { kind: 'offer', sdp: this.describe(l), p, ...this.prediction(l) };
    this.signal(peer, l.offerMsg);
    this.tr(peer, 'offer-sent', candidateSummary(l.offerMsg.sdp.sdp));
    this.watch(l);
    this.resendOffer(l);
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
      const sdp = typeof msg.sdp?.sdp === 'string' ? msg.sdp.sdp : null;
      if (!fresh) {
        // The same offer again: our answer was lost (or is still being made). Send it again.
        const l = this.links.get(this.byPeer.get(from));
        if (sdp !== null && sdp === l.offerSdp) {
          if (l.answerMsg) {
            this.resent++;
            this.signal(from, l.answerMsg);
            this.tr(from, 'answer-sent', { ...candidateSummary(l.answerMsg.sdp.sdp), resend: true });
          }
          return;
        }
      }
      const l = this.link(from);
      l.offerSdp = sdp; // a copy that comes while this one is answered is ignored
      try {
        await l.pc.setRemoteDescription(msg.sdp);
        await l.pc.setLocalDescription(await l.pc.createAnswer());
      } catch {
        // A malformed offer: the link it opened is dropped, so it can't block a later one.
        if (fresh && this.links.get(l.handle) === l) this.drop(from);
        return void this.badSignals++;
      }
      this.tr(from, 'offer-received', { ...candidateSummary(sdp), restart: !fresh });
      this.sameNat(l, l.theirs, sdp);
      await this.addHeld(l);
      if (!fresh) Object.assign(l, { described: false, late: [], lateSent: 0 });
      await this.gathered(l.pc);
      if (this.links.get(l.handle) !== l) return; // dropped meanwhile
      l.answerMsg = { kind: 'answer', sdp: this.describe(l), ...this.prediction(l) };
      this.signal(from, l.answerMsg);
      this.tr(from, 'answer-sent', { ...candidateSummary(l.answerMsg.sdp.sdp), restart: !fresh });
      if (fresh) this.watch(l);
      await this.guess(l, msg.nat);
    } else if (msg.kind === 'answer') {
      const l = this.link(from);
      if (!this.awaiting(l)) return; // a copy of one already applied
      try {
        await l.pc.setRemoteDescription(msg.sdp);
      } catch {
        // Malformed: ignored, the dial times out or a resent offer gets a good one.
        return;
      }
      this.tr(from, 'answer-received', candidateSummary(msg.sdp?.sdp));
      this.sameNat(l, l.theirs, msg.sdp?.sdp);
      await this.addHeld(l);
      await this.guess(l, msg.nat);
    } else if (msg.kind === 'restart') {
      // The other side lost ICE: as the dialling side, renegotiate (within the limit).
      const l = this.links.get(this.byPeer.get(from));
      if (l.dialer) this.restartIce(l, 'asked');
    } else if (msg.kind === 'ice' || msg.kind === 'ice-batch') {
      const cands = msg.kind === 'ice' ? [msg.candidate] : msg.c;
      if (!Array.isArray(cands) || cands.length > MAX_ICE_BATCH) return void this.badSignals++;
      const l = this.link(from);
      if (msg.kind === 'ice-batch') this.tr(from, 'ice-batch-received', candidateSummary(cands.map((c) => `a=${c?.candidate}`).join('\n')));
      this.sameNat(l, l.theirs, cands.map((c) => (typeof c?.candidate === 'string' ? c.candidate : '')).join('\n'));
      // Candidates can overtake the description they belong to: hold them until it comes.
      if (!l.pc.remoteDescription) return void l.held.push(...cands.slice(0, MAX_ICE_BATCH - l.held.length));
      await this.addCandidates(l, cands);
      if (msg.kind === 'ice-batch') await this.guess(l, msg.nat);
    }
  }

  async addHeld(l) {
    const held = l.held.splice(0);
    await this.addCandidates(l, held);
  }

  async addCandidates(l, cands) {
    for (const c of cands) {
      try {
        await l.pc.addIceCandidate(c);
      } catch {
        // malformed, or for another description: the others may do
      }
    }
  }

  // Without trickle, waits for ICE gathering: until it is complete, GATHER_SETTLE_MS
  // after the first public candidate, or GATHER_MAX_MS.
  gathered(pc) {
    if (this.trickle || pc.iceGatheringState === 'complete') return Promise.resolve();
    return new Promise((resolve) => {
      let settle = null;
      const done = () => {
        clearTimeout(settle);
        clearTimeout(max);
        pc.removeEventListener('icegatheringstatechange', onState);
        pc.removeEventListener('icecandidate', onCandidate);
        resolve();
      };
      const onState = () => pc.iceGatheringState === 'complete' && done();
      const onCandidate = (e) => {
        if (!settle && e.candidate && / typ (srflx|relay)( |$)/.test(e.candidate.candidate)) settle = setTimeout(done, GATHER_SETTLE_MS);
      };
      const max = setTimeout(done, GATHER_MAX_MS);
      pc.addEventListener('icegatheringstatechange', onState);
      pc.addEventListener('icecandidate', onCandidate);
    });
  }

  has(peer) {
    return this.byPeer.has(peer);
  }

  // Lets `peer` (one that proved the room key) send us datagrams over signalling.
  allowPipe(peer) {
    if (!this.pipesOn || this.pipeAllowed.has(peer)) return;
    this.pipeAllowed.add(peer);
    if (this.pipeAllowed.size <= MAX_PIPE_PEERS) return;
    const old = this.pipeAllowed.values().next().value;
    this.pipeAllowed.delete(old);
    // Its pipe goes too, unless a session uses it: pipes stay bounded by the allowed set and our links.
    if (!this.ep.isConnected(old)) this.dropPipe(old);
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
    // The peer's own DataChannel opened late and carries its datagrams now: these were
    // sent before, and taking them would move the link back onto the pipe for a while.
    const own = this.links.get(this.byPeer.get(from));
    if (own?.heard && own.dc.readyState === 'open') return void (this.piped.stale += d.length);
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

  // Carries `peer`'s link over `dc`, an open DataChannel to `peer` made outside this
  // transport (`pc` its connection; a birthday punch's, see birthday.js), once the far
  // end adopts its end too (see CARRY), so a link never moves onto a channel only one
  // side reads. With `pipe`, the datagrams of `peer`'s pipe go over `dc` instead of
  // signalling, and the endpoint sees no new address; otherwise `dc` gets a handle of
  // its own, for endpoint.addCandidate. Resolves to that handle, or -1 (the channel
  // left to the caller) if the far end didn't adopt within CARRY_WAIT_MS or `dc`
  // closed first. Once carrying, `dc` closing or `pc` failing ends it, as do
  // drop(peer), close() and a newer adopt(peer); ending it closes `pc`. onPeer(peer,
  // 'carrier') fires when carrying starts and when it ends.
  adopt(peer, pc, dc, { pipe = false } = {}) {
    const ph = pipe ? this.pipeByPeer.get(peer) : undefined;
    if (!isPeerId(peer) || dc?.readyState !== 'open' || (pipe && ph === undefined)) return Promise.resolve(-1);
    this.dropCarrier(peer);
    const handle = this.nextHandle++;
    const c = { peer, pc, dc, handle, addr: pipe ? ph : handle, live: false, end: null, replied: -Infinity };
    this.carriers.set(peer, c);
    dc.binaryType = 'arraybuffer';
    const say = (b) => this.dcSend(dc, Uint8Array.of(b)); // bounded like any datagram; closing: onclose follows
    return new Promise((resolve) => {
      const every = setInterval(() => say(CARRY), CARRY_EVERY_MS);
      const wait = setTimeout(() => c.end(), CARRY_WAIT_MS);
      // Stops waiting: carrying from now on, or (no `live`) given back to the caller.
      const settle = (live) => {
        clearInterval(every);
        clearTimeout(wait);
        c.end = null;
        if (!live) {
          dc.onmessage = dc.onclose = null;
          if (this.carriers.get(peer) === c) this.carriers.delete(peer);
          return resolve(-1);
        }
        c.live = true;
        this.carried.set(c.addr, c);
        dc.onclose = () => this.dropCarrier(peer, c);
        pc.onconnectionstatechange = () => ['failed', 'closed'].includes(pc.connectionState) && this.dropCarrier(peer, c);
        resolve(handle);
        this.onPeer(peer, 'carrier');
      };
      c.end = () => settle(false);
      dc.onclose = c.end;
      dc.onmessage = (e) => {
        if (!(e.data instanceof ArrayBuffer) || e.data.byteLength > MAX_DATAGRAM || this.carriers.get(peer) !== c) return;
        const b = new Uint8Array(e.data);
        // Anything from the far end means it adopted its end too.
        if (!c.live) settle(true);
        if (b.length !== 1) return this.ep.receive(c.addr, b);
        if (b[0] === CARRY && Date.now() - c.replied >= CARRY_EVERY_MS) {
          c.replied = Date.now();
          say(CARRIED);
        }
      };
      say(CARRY);
    });
  }

  // Whether endpoint handle `h` goes over an adopted channel now.
  carries(h) {
    return this.carried.has(h);
  }

  // The adopted channel carrying `peer`'s link now, { pc, dc }, or null.
  carrier(peer) {
    const c = this.carriers.get(peer);
    return c?.live ? { pc: c.pc, dc: c.dc } : null;
  }

  // Ends `peer`'s adopted channel (only if it is still `c`): one still waiting is
  // given back to its caller; one carrying is closed, and its link goes back to the
  // route it had (a pipe at once; a relayed link once the endpoint sees the direct
  // path gone). `quiet`: no onPeer.
  dropCarrier(peer, c = this.carriers.get(peer), quiet = false) {
    if (!c || this.carriers.get(peer) !== c) return;
    if (c.end) return c.end();
    this.carriers.delete(peer);
    this.carried.delete(c.addr);
    c.live = false;
    c.dc.onmessage = c.dc.onclose = null;
    c.pc.onconnectionstatechange = null;
    try {
      c.pc.close();
    } catch {
      // already closed
    }
    if (!quiet) this.onPeer(peer, 'carrier');
  }

  // Forgets the WebRTC link to `peer`, so a later dial or offer starts afresh.
  dropRtc(peer) {
    const h = this.byPeer.get(peer);
    if (h === undefined) return;
    this.shut(this.links.get(h));
    this.links.delete(h);
    this.byPeer.delete(peer);
  }

  shut(l) {
    clearTimeout(l.timer);
    clearTimeout(l.lateTimer);
    clearTimeout(l.restartTimer);
    l.opened = false; // no restart for a link we closed
    for (const t of l.resends) clearTimeout(t);
    l.dc.onclose = null;
    l.pc.onconnectionstatechange = null;
    l.pc.close();
  }

  // Forgets every path to `peer`: its WebRTC link, its pipe and an adopted channel.
  drop(peer) {
    this.dropRtc(peer);
    this.dropPipe(peer);
    this.dropCarrier(peer, undefined, true);
  }

  dropPipe(peer) {
    const h = this.pipeByPeer.get(peer);
    if (h === undefined) return;
    clearTimeout(this.pipes.get(h).timer);
    this.pipes.delete(h);
    this.pipeByPeer.delete(peer);
  }

  close() {
    this.checking.clear();
    for (const peer of [...this.carriers.keys()]) this.dropCarrier(peer, undefined, true);
    for (const p of this.pipes.values()) clearTimeout(p.timer);
    this.pipes.clear();
    this.pipeByPeer.clear();
    for (const l of this.links.values()) this.shut(l);
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
