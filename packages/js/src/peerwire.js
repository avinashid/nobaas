// JS glue for peerwire_ffi.wasm: the peerwire-core core with no imports.
// Works in browsers and Node (ES module). The host owns transports and time;
// see Endpoint below and webrtc.js for a DataChannel transport.

// Urgent: reliable, ordered among urgent messages only (overtakes queued Reliable), each fragment
// sent twice; arrives as Reliable.
export const Lane = { Reliable: 0, Unreliable: 1, Urgent: 2 };
export const EventKind = { Connected: 1, Message: 2, Disconnected: 3, ConnectFailed: 4, PathChanged: 5, AddrObserved: 6 };
const REASONS = ['timeout', 'closed', 'replaced', 'violation'];
// The largest datagram the core sends or accepts; transports drop anything bigger.
export const MAX_DATAGRAM = 1232;
const WOULD_BLOCK = -3;
// Each endpoint keeps a send buffer this size; bigger messages get a buffer of their own.
const SEND_SCRATCH = 16 * 1024;
const encoder = new TextEncoder();
const IN_NODE = typeof globalThis.process?.versions?.node === 'string';

// Only Node reads from disk: file: URLs and absolute paths. Anything else is fetched, and in a
// browser that includes root-relative paths such as the '/assets/…wasm' bundlers emit.
export async function loadPeerwire(source) {
  let bytes;
  if (typeof source === 'string' || source instanceof URL) {
    const s = String(source);
    if (IN_NODE && (s.startsWith('file:') || s.startsWith('/'))) {
      const { readFile } = await import('node:fs/promises');
      bytes = await readFile(s.startsWith('file:') ? new URL(s) : s);
    } else {
      const res = await fetch(source);
      if (!res.ok) throw new Error(`loadPeerwire: ${s}: HTTP ${res.status}`);
      bytes = await res.arrayBuffer();
    }
  } else {
    bytes = source;
  }
  const { instance } = await WebAssembly.instantiate(bytes, {});
  return new Peerwire(instance.exports);
}

// Byte -> two hex characters, and character code -> nibble (0x100: not a hex digit).
const HEX = Array.from({ length: 256 }, (_, i) => i.toString(16).padStart(2, '0'));
const NIBBLE = new Uint16Array(128).fill(0x100);
for (let i = 0; i < 16; i++) {
  NIBBLE[i.toString(16).charCodeAt(0)] = i;
  NIBBLE[i.toString(16).toUpperCase().charCodeAt(0)] = i;
}
const nibble = (s, i) => {
  const c = s.charCodeAt(i);
  return c < 128 ? NIBBLE[c] : 0x100;
};

export function toHex(b) {
  let s = '';
  for (let i = 0; i < b.length; i++) s += HEX[b[i]];
  return s;
}
// Pairs of hex digits as bytes; a trailing odd character is ignored. Anything
// that isn't hex takes the old parseInt path, so such input reads as it always did.
export function fromHex(s) {
  const out = new Uint8Array(s.length >> 1);
  let bad = 0;
  for (let i = 0; i < out.length; i++) {
    const hi = nibble(s, 2 * i);
    const lo = nibble(s, 2 * i + 1);
    bad |= hi | lo;
    out[i] = (hi << 4) | lo;
  }
  if (bad & 0x100) return Uint8Array.from(s.match(/../g), (h) => parseInt(h, 16));
  return out;
}

// A PeerId as text: 64 lowercase hex characters. Ids from signalling are
// untrusted; check them with this before using them.
export const isPeerId = (s) => typeof s === 'string' && /^[0-9a-f]{64}$/.test(s);

// Exactly `n` bytes, or a TypeError: every fixed-size copy into wasm memory
// goes through here, so nothing can write past its buffer.
function exact(b, n, what) {
  if (!(b instanceof Uint8Array) || b.length !== n) throw new TypeError(`${what} must be ${n} bytes`);
  return b;
}

// A PeerId (32 bytes, or 64 lowercase hex characters) as bytes; throws otherwise.
export function peerBytes(id) {
  if (typeof id === 'string') {
    if (!isPeerId(id)) throw new TypeError('peer id must be 64 lowercase hex characters or 32 bytes');
    return fromHex(id);
  }
  return exact(id, 32, 'peer id');
}

function bytesOf(b) {
  if (b instanceof Uint8Array) return b;
  if (b instanceof ArrayBuffer) return new Uint8Array(b);
  if (ArrayBuffer.isView(b)) return new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
  throw new TypeError('expected bytes (Uint8Array, ArrayBuffer or a typed array view)');
}

class Peerwire {
  constructor(x) {
    this.x = x;
    this.view = new Uint8Array(x.memory.buffer);
  }
  // A view of wasm memory. Growing memory replaces memory.buffer (the old one is
  // detached, length 0), so the view is rebuilt whenever the buffer changed: pointers
  // into wasm memory outlive a grow, views don't. Never keep a view across a wasm call.
  mem() {
    if (this.view.buffer !== this.x.memory.buffer) this.view = new Uint8Array(this.x.memory.buffer);
    return this.view;
  }
  // Copies bytes into a fresh buffer in wasm memory, sized from the bytes
  // themselves; caller frees `bytes.length`.
  put(bytes) {
    bytes = bytesOf(bytes);
    const p = this.x.pw_alloc(bytes.length);
    this.mem().set(bytes, p);
    return p;
  }
  free(p, len) {
    this.x.pw_free(p, len);
  }
  // A fresh secp256k1 key for signing Nostr events: { secret, pubkey } (32 bytes each).
  nostrKeypair() {
    const x = this.x;
    for (;;) {
      const seed = this.put(crypto.getRandomValues(new Uint8Array(32)));
      const out = x.pw_alloc(64);
      const r = x.pw_nostr_keypair(seed, out, out + 32);
      const m = this.mem();
      const k = { secret: m.slice(out, out + 32), pubkey: m.slice(out + 32, out + 64) };
      this.free(seed, 32);
      this.free(out, 64);
      if (r === 0) return k;
    }
  }
  // BIP-340 signature of a 32-byte message.
  nostrSign(secret, msg32) {
    const x = this.x;
    exact(secret, 32, 'nostr secret');
    exact(msg32, 32, 'nostr message');
    const p = this.put(new Uint8Array([...secret, ...msg32, ...crypto.getRandomValues(new Uint8Array(32))]));
    const out = x.pw_alloc(64);
    x.pw_nostr_sign(p, p + 32, p + 64, out);
    const sig = this.mem().slice(out, out + 64);
    this.free(p, 96);
    this.free(out, 64);
    return sig;
  }
  // Creates an endpoint. `secret` (32 bytes) restores an identity. `relayOnly`:
  // see Endpoint.setRelayOnly.
  endpoint({ secret = null, shareAddresses = false, relayOnly = false } = {}) {
    const ep = new Endpoint(this, secret, shareAddresses);
    if (relayOnly) ep.setRelayOnly(true);
    return ep;
  }
}

const nowUs = () => BigInt(Math.floor((globalThis.performance ?? Date).now() * 1000));

export class Endpoint {
  constructor(pw, secret, shareAddresses) {
    this.pw = pw;
    const x = pw.x;
    const seed = crypto.getRandomValues(new Uint8Array(32));
    // The wasm reads 32 bytes from the secret pointer.
    const kp = secret ? pw.put(exact(secret, 32, 'secret')) : 0;
    const sp = pw.put(seed);
    const out = x.pw_alloc(32);
    this.h = x.pw_endpoint_new(kp, sp, shareAddresses ? 1 : 0, out);
    this.peerId = pw.mem().slice(out, out + 32);
    this.id = toHex(this.peerId);
    pw.free(sp, 32);
    if (kp) pw.free(kp, 32);
    pw.free(out, 32);
    // Scratch buffers that live as long as the endpoint.
    this.peerBuf = x.pw_alloc(32);
    this.peerHex = null; // the id peerBuf holds, when it was given as text
    this.txBuf = x.pw_alloc(MAX_DATAGRAM);
    this.rxBuf = x.pw_alloc(MAX_DATAGRAM);
    this.sendBuf = x.pw_alloc(SEND_SCRATCH);
    this.addrBuf = x.pw_alloc(4);
    this.evCap = 64 * 1024;
    this.evBuf = x.pw_alloc(this.evCap);
    this.onTransmit = () => {};
    this.onEvent = () => {};
    // Called at the end of a pump that handled incoming datagrams: ACKs may have
    // freed send window, so a sender that got -3 (would block) can try again.
    this.onDrain = null;
    this.received = false;
    this.kept = null; // { data, p, n }: see sendRetry
    this.timer = null;
    this.closed = false;
    this.transmitErrors = 0; // datagrams lost because onTransmit threw
  }
  // close() frees the wasm handle and buffers, so nothing may reach them afterwards:
  // send and connect* throw, receive, addCandidate and setRelayOnly do nothing, queries return null or false
  // (stats() returns the counters as they were at close).
  live(what) {
    if (this.closed) throw new Error(`${what}: endpoint closed`);
  }
  // Throws on anything but a valid PeerId, before touching wasm memory. Only this
  // writes peerBuf, so the same id as text again (strings can't change) needs no copy.
  setPeer(id) {
    if (typeof id === 'string' && id === this.peerHex) return this.peerBuf;
    this.peerHex = null;
    this.pw.mem().set(peerBytes(id), this.peerBuf);
    if (typeof id === 'string') this.peerHex = id;
    return this.peerBuf;
  }
  secret() {
    if (this.closed) return null;
    const out = this.pw.x.pw_alloc(32);
    this.pw.x.pw_secret(this.h, out);
    const s = this.pw.mem().slice(out, out + 32);
    this.pw.free(out, 32);
    return s;
  }
  connect(addr, peer) {
    this.live('connect');
    const r = this.pw.x.pw_connect(this.h, nowUs(), addr, this.setPeer(peer));
    this.pump();
    return r;
  }
  // Relay-only: links made through a relay (connectVia) stay relayed and never
  // punch to a direct path, dials through a relay alone send nothing direct (a
  // direct dial later given a relay may still complete direct), relays forward
  // our packets without our address, and a direct handshake is answered only
  // from a peer we are dialling directly.
  setRelayOnly(on) {
    if (this.closed) return;
    this.pw.x.pw_set_relay_only(this.h, on ? 1 : 0);
  }
  connectVia(relay, peer) {
    this.live('connectVia');
    const x = this.pw.x;
    const pp = this.setPeer(peer);
    const rp = this.pw.put(peerBytes(relay));
    const r = x.pw_connect_via(this.h, nowUs(), rp, pp);
    this.pw.free(rp, 32);
    this.pump();
    return r;
  }
  // A direct address `addr` for a relayed (or dialling) `peer`: the endpoint punches
  // to it and moves the link there once the peer answers from it. Does nothing for a
  // peer linked directly or unknown, while relay-only for one linked or being dialled
  // through a relay (nothing is sent to `addr`), or after close().
  addCandidate(addr, peer) {
    if (this.closed) return;
    this.pw.x.pw_add_candidate(this.h, nowUs(), addr, this.setPeer(peer));
    this.pump();
  }
  // Ends the session with `peer` and sends it a sealed close at once (this pumps), so it
  // sees 'disconnected' now rather than after its idle timeout. No local event; does
  // nothing for a peer not connected, or after close().
  disconnect(peer) {
    if (this.closed) return;
    this.pw.x.pw_disconnect(this.h, this.setPeer(peer));
    this.pump();
  }
  stats() {
    if (this.closed) return { ...this.finalStats };
    const x = this.pw.x;
    const out = x.pw_alloc(128);
    x.pw_stats(this.h, out);
    const v = new BigUint64Array(this.pw.x.memory.buffer, out, 16);
    const names = ['txPackets', 'rxPackets', 'rxMalformed', 'rxUnknownSession', 'rxAuthFailed', 'rxReplayed', 'rxHandshakeDropped',
      'retransmits', 'unreliableDropped', 'roams', 'relayed', 'relayDropped', 'punchesSent', 'punchSucceeded', 'punchFailed', 'directFallbacks'];
    const r = Object.fromEntries(names.map((n, i) => [n, Number(v[i])]));
    this.pw.free(out, 128);
    return r;
  }
  // Encrypts to `peer`, authenticated as us (Noise X). See peerwire_core::sealed.
  seal(peer, bytes) {
    if (this.closed) return null;
    const x = this.pw.x;
    const pp = this.setPeer(peer);
    bytes = bytesOf(bytes);
    const p = this.pw.put(bytes);
    const cap = bytes.length + 128;
    const out = x.pw_alloc(cap);
    const n = x.pw_seal(this.h, pp, p, bytes.length, out, cap);
    const r = n > 0 ? this.pw.mem().slice(out, out + n) : null;
    this.pw.free(p, bytes.length);
    this.pw.free(out, cap);
    return r;
  }
  // Returns { from, data } or null if it isn't for us or was tampered with.
  open(bytes) {
    if (this.closed) return null;
    const x = this.pw.x;
    bytes = bytesOf(bytes);
    const p = this.pw.put(bytes);
    const out = x.pw_alloc(bytes.length + 32);
    const n = x.pw_open(this.h, p, bytes.length, out, out + 32, bytes.length);
    const m = this.pw.mem();
    const r = n >= 0 ? { from: toHex(m.subarray(out, out + 32)), data: m.slice(out + 32, out + 32 + n) } : null;
    this.pw.free(p, bytes.length);
    this.pw.free(out, bytes.length + 32);
    return r;
  }
  isConnected(peer) {
    if (this.closed) return false;
    return this.pw.x.pw_is_connected(this.h, this.setPeer(peer)) === 1;
  }
  rttMs(peer) {
    if (this.closed) return null;
    const us = this.pw.x.pw_rtt_us(this.h, this.setPeer(peer));
    return us < 0n ? null : Number(us) / 1000;
  }
  // Returns 0, or a negative PW_ERR code (-1 not connected, -2 too large, -3 would block).
  // Throws after close(): a send on a closed endpoint is a bug in the app, not a lost packet.
  send(peer, data, lane = Lane.Reliable) {
    this.live('send');
    const pp = this.setPeer(peer);
    const r = this.sendBytes(pp, lane, data, false);
    this.schedule();
    return r;
  }
  // send() for a caller that tries the same message again after -3 (would block), as
  // Room does with its outbox: a message over the 16 KiB send buffer then stays in
  // wasm memory between tries instead of being copied in on every try. `data` is
  // matched by identity, so it must not change between tries. One message is kept
  // at a time, until it is sent, another one replaces it, or close().
  sendRetry(peer, data, lane = Lane.Reliable) {
    this.live('send');
    const pp = this.setPeer(peer);
    const r = this.sendBytes(pp, lane, data, true);
    // A message that would block queued nothing: no pump for it (a drain retries on every ACK).
    if (r !== WOULD_BLOCK) this.schedule();
    return r;
  }
  unkeep() {
    if (!this.kept) return;
    this.pw.free(this.kept.p, this.kept.n);
    this.kept = null;
  }
  // Messages up to SEND_SCRATCH go through the endpoint's send buffer: strings are
  // encoded straight into it (encodeInto stops short when one doesn't fit), bytes
  // copied in. Bigger ones get a buffer of their own.
  sendBytes(pp, lane, data, keep) {
    const x = this.pw.x;
    if (keep && this.kept?.data === data) return this.sendKept(pp, lane);
    let bytes;
    if (typeof data === 'string') {
      const { read, written } = encoder.encodeInto(data, this.pw.mem().subarray(this.sendBuf, this.sendBuf + SEND_SCRATCH));
      if (read === data.length) return x.pw_send(this.h, pp, lane, this.sendBuf, written);
      bytes = encoder.encode(data);
    } else {
      bytes = bytesOf(data);
      if (bytes.length <= SEND_SCRATCH) {
        this.pw.mem().set(bytes, this.sendBuf);
        return x.pw_send(this.h, pp, lane, this.sendBuf, bytes.length);
      }
    }
    if (keep) {
      this.unkeep();
      this.kept = { data, p: this.pw.put(bytes), n: bytes.length };
      return this.sendKept(pp, lane);
    }
    const p = this.pw.put(bytes);
    const r = x.pw_send(this.h, pp, lane, p, bytes.length);
    this.pw.free(p, bytes.length);
    return r;
  }
  sendKept(pp, lane) {
    const r = this.pw.x.pw_send(this.h, pp, lane, this.kept.p, this.kept.n);
    if (r !== WOULD_BLOCK) this.unkeep();
    return r;
  }
  // Feed a datagram that arrived on transport handle `addr`. Over MAX_DATAGRAM is never ours: dropped.
  // After close() a late datagram is normal and is dropped too.
  receive(addr, bytes) {
    if (this.closed) return;
    bytes = bytesOf(bytes);
    if (bytes.length > MAX_DATAGRAM) return;
    this.pw.mem().set(bytes, this.rxBuf);
    this.pw.x.pw_handle_datagram(this.h, nowUs(), addr, this.rxBuf, bytes.length);
    this.received = true;
    this.schedule();
  }
  // Sends made in the same JS task share packets: flush once, after the task.
  schedule() {
    if (this.scheduled) return;
    this.scheduled = true;
    queueMicrotask(() => {
      this.scheduled = false;
      if (!this.closed) this.pump();
    });
  }
  // Drains datagrams and events, and always re-arms the timer: a transport or
  // handler that throws must not leave the endpoint without retransmits or keepalives.
  // A callback may close() the endpoint: stop at once, its handle and buffers are freed.
  pump() {
    if (this.closed) return;
    const x = this.pw.x;
    const now = nowUs();
    try {
      for (;;) {
        const n = x.pw_poll_transmit(this.h, now, this.addrBuf, this.txBuf, MAX_DATAGRAM);
        if (n <= 0) break;
        const m = this.pw.mem();
        const a = this.addrBuf;
        const addr = (m[a] | (m[a + 1] << 8) | (m[a + 2] << 16) | (m[a + 3] << 24)) >>> 0;
        try {
          this.onTransmit(addr, m.slice(this.txBuf, this.txBuf + n));
        } catch {
          // A datagram the transport couldn't take is a lost datagram; the core retransmits.
          this.transmitErrors++;
        }
        if (this.closed) return;
      }
      for (;;) {
        let n = x.pw_poll_event(this.h, this.evBuf, this.evCap);
        if (n === 0) break;
        if (n < 0) {
          this.pw.free(this.evBuf, this.evCap);
          this.evCap = -n;
          this.evBuf = x.pw_alloc(this.evCap);
          continue;
        }
        this.onEvent(decodeEvent(this.pw.mem().slice(this.evBuf, this.evBuf + n)));
        if (this.closed) return;
      }
      if (this.received) {
        this.received = false;
        this.onDrain?.();
      }
    } finally {
      this.arm();
    }
  }
  arm() {
    if (this.closed) return;
    const t = this.pw.x.pw_poll_timeout(this.h);
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    if (t < 0n) return;
    const delayMs = Math.max(0, Number(t - nowUs()) / 1000);
    this.timer = setTimeout(() => {
      this.timer = null;
      if (this.closed) return;
      this.pw.x.pw_handle_timeout(this.h, nowUs());
      this.pump(); // re-arms, whatever the transport or handlers do
    }, delayMs);
  }
  close() {
    if (this.closed) return; // a second close would free the handle twice
    this.finalStats = this.stats();
    this.unkeep();
    this.closed = true;
    if (this.timer) clearTimeout(this.timer);
    const x = this.pw.x;
    x.pw_endpoint_free(this.h);
    for (const [p, n] of [[this.peerBuf, 32], [this.txBuf, MAX_DATAGRAM], [this.rxBuf, MAX_DATAGRAM],
      [this.sendBuf, SEND_SCRATCH], [this.addrBuf, 4], [this.evBuf, this.evCap]]) x.pw_free(p, n);
  }
}

function decodePath(b, o) {
  if (b[o] === 0) return { direct: new DataView(b.buffer, b.byteOffset).getUint32(o + 1, true) };
  return { via: toHex(b.subarray(o + 1, o + 33)) };
}

export function decodeEvent(b) {
  const kind = b[0];
  const peer = toHex(b.subarray(1, 33));
  switch (kind) {
    case EventKind.Connected:
      return { type: 'connected', peer, initiator: b[33] === 1, path: decodePath(b, 34) };
    case EventKind.Message:
      return { type: 'message', peer, lane: b[33], data: b.subarray(34) };
    case EventKind.Disconnected:
      return { type: 'disconnected', peer, reason: REASONS[b[33]] };
    case EventKind.ConnectFailed:
      return { type: 'connectFailed', peer };
    case EventKind.PathChanged:
      return { type: 'pathChanged', peer, path: decodePath(b, 33) };
    case EventKind.AddrObserved:
      return { type: 'addrObserved', peer, addr: new DataView(b.buffer, b.byteOffset).getUint32(33, true) };
    default:
      return { type: 'unknown', kind };
  }
}
