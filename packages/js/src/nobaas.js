// JS glue for nobaas_ffi.wasm: the nobaas-core core with no imports.
// Works in browsers and Node (ES module). The host owns transports and time;
// see Endpoint below and webrtc.js for a DataChannel transport.

// Urgent: reliable and ordered with Reliable, each fragment sent twice; arrives as Reliable.
export const Lane = { Reliable: 0, Unreliable: 1, Urgent: 2 };
export const EventKind = { Connected: 1, Message: 2, Disconnected: 3, ConnectFailed: 4, PathChanged: 5, AddrObserved: 6 };
const REASONS = ['timeout', 'closed', 'replaced', 'violation'];
const MAX_DATAGRAM = 1232;

export async function loadNobaas(source) {
  let bytes;
  if (typeof source === 'string' || source instanceof URL) {
    if (typeof fetch === 'function' && !String(source).startsWith('file:') && !String(source).startsWith('/')) {
      bytes = await (await fetch(source)).arrayBuffer();
    } else {
      const { readFile } = await import('node:fs/promises');
      bytes = await readFile(source);
    }
  } else {
    bytes = source;
  }
  const { instance } = await WebAssembly.instantiate(bytes, {});
  return new Nobaas(instance.exports);
}

export const toHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
export const fromHex = (s) => Uint8Array.from(s.match(/../g), (h) => parseInt(h, 16));

class Nobaas {
  constructor(x) {
    this.x = x;
  }
  mem() {
    return new Uint8Array(this.x.memory.buffer);
  }
  // Copies bytes into a fresh buffer in wasm memory; caller frees.
  put(bytes) {
    const p = this.x.nb_alloc(bytes.length);
    this.mem().set(bytes, p);
    return p;
  }
  free(p, len) {
    this.x.nb_free(p, len);
  }
  // A fresh secp256k1 key for signing Nostr events: { secret, pubkey } (32 bytes each).
  nostrKeypair() {
    const x = this.x;
    for (;;) {
      const seed = this.put(crypto.getRandomValues(new Uint8Array(32)));
      const out = x.nb_alloc(64);
      const r = x.nb_nostr_keypair(seed, out, out + 32);
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
    const p = this.put(new Uint8Array([...secret, ...msg32, ...crypto.getRandomValues(new Uint8Array(32))]));
    const out = x.nb_alloc(64);
    x.nb_nostr_sign(p, p + 32, p + 64, out);
    const sig = this.mem().slice(out, out + 64);
    this.free(p, 96);
    this.free(out, 64);
    return sig;
  }
  // Creates an endpoint. `secret` (32 bytes) restores an identity.
  endpoint({ secret = null, shareAddresses = false } = {}) {
    return new Endpoint(this, secret, shareAddresses);
  }
}

const nowUs = () => BigInt(Math.floor((globalThis.performance ?? Date).now() * 1000));

export class Endpoint {
  constructor(nb, secret, shareAddresses) {
    this.nb = nb;
    const x = nb.x;
    const seed = crypto.getRandomValues(new Uint8Array(32));
    const sp = nb.put(seed);
    const kp = secret ? nb.put(secret) : 0;
    const out = x.nb_alloc(32);
    this.h = x.nb_endpoint_new(kp, sp, shareAddresses ? 1 : 0, out);
    this.peerId = nb.mem().slice(out, out + 32);
    this.id = toHex(this.peerId);
    nb.free(sp, 32);
    if (kp) nb.free(kp, 32);
    nb.free(out, 32);
    // Scratch buffers that live as long as the endpoint.
    this.peerBuf = x.nb_alloc(32);
    this.txBuf = x.nb_alloc(MAX_DATAGRAM);
    this.addrBuf = x.nb_alloc(4);
    this.evCap = 64 * 1024;
    this.evBuf = x.nb_alloc(this.evCap);
    this.onTransmit = () => {};
    this.onEvent = () => {};
    this.timer = null;
  }
  setPeer(id) {
    this.nb.mem().set(typeof id === 'string' ? fromHex(id) : id, this.peerBuf);
    return this.peerBuf;
  }
  secret() {
    const out = this.nb.x.nb_alloc(32);
    this.nb.x.nb_secret(this.h, out);
    const s = this.nb.mem().slice(out, out + 32);
    this.nb.free(out, 32);
    return s;
  }
  connect(addr, peer) {
    const r = this.nb.x.nb_connect(this.h, nowUs(), addr, this.setPeer(peer));
    this.pump();
    return r;
  }
  connectVia(relay, peer) {
    const x = this.nb.x;
    const rp = this.nb.put(typeof relay === 'string' ? fromHex(relay) : relay);
    const r = x.nb_connect_via(this.h, nowUs(), rp, this.setPeer(peer));
    this.nb.free(rp, 32);
    this.pump();
    return r;
  }
  stats() {
    const x = this.nb.x;
    const out = x.nb_alloc(128);
    x.nb_stats(this.h, out);
    const v = new BigUint64Array(this.nb.x.memory.buffer, out, 16);
    const names = ['txPackets', 'rxPackets', 'rxMalformed', 'rxUnknownSession', 'rxAuthFailed', 'rxReplayed', 'rxHandshakeDropped',
      'retransmits', 'unreliableDropped', 'roams', 'relayed', 'relayDropped', 'punchesSent', 'punchSucceeded', 'punchFailed', 'directFallbacks'];
    const r = Object.fromEntries(names.map((n, i) => [n, Number(v[i])]));
    this.nb.free(out, 128);
    return r;
  }
  // Encrypts to `peer`, authenticated as us (Noise X). See nobaas_core::sealed.
  seal(peer, bytes) {
    const x = this.nb.x;
    const p = this.nb.put(bytes);
    const cap = bytes.length + 128;
    const out = x.nb_alloc(cap);
    const n = x.nb_seal(this.h, this.setPeer(peer), p, bytes.length, out, cap);
    const r = n > 0 ? this.nb.mem().slice(out, out + n) : null;
    this.nb.free(p, bytes.length);
    this.nb.free(out, cap);
    return r;
  }
  // Returns { from, data } or null if it isn't for us or was tampered with.
  open(bytes) {
    const x = this.nb.x;
    const p = this.nb.put(bytes);
    const out = x.nb_alloc(bytes.length + 32);
    const n = x.nb_open(this.h, p, bytes.length, out, out + 32, bytes.length);
    const m = this.nb.mem();
    const r = n >= 0 ? { from: toHex(m.subarray(out, out + 32)), data: m.slice(out + 32, out + 32 + n) } : null;
    this.nb.free(p, bytes.length);
    this.nb.free(out, bytes.length + 32);
    return r;
  }
  isConnected(peer) {
    return this.nb.x.nb_is_connected(this.h, this.setPeer(peer)) === 1;
  }
  rttMs(peer) {
    const us = this.nb.x.nb_rtt_us(this.h, this.setPeer(peer));
    return us < 0n ? null : Number(us) / 1000;
  }
  // Returns 0, or a negative NB_ERR code (-1 not connected, -2 too large, -3 would block).
  send(peer, data, lane = Lane.Reliable) {
    const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : data;
    const p = this.nb.put(bytes);
    const r = this.nb.x.nb_send(this.h, this.setPeer(peer), lane, p, bytes.length);
    this.nb.free(p, bytes.length);
    this.schedule();
    return r;
  }
  // Feed a datagram that arrived on transport handle `addr`.
  receive(addr, bytes) {
    const p = this.nb.put(bytes);
    this.nb.x.nb_handle_datagram(this.h, nowUs(), addr, p, bytes.length);
    this.nb.free(p, bytes.length);
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
  // Drains datagrams and events, and re-arms the timer.
  pump() {
    const x = this.nb.x;
    const now = nowUs();
    for (;;) {
      const n = x.nb_poll_transmit(this.h, now, this.addrBuf, this.txBuf, MAX_DATAGRAM);
      if (n <= 0) break;
      const m = this.nb.mem();
      const addr = new DataView(m.buffer).getUint32(this.addrBuf, true);
      this.onTransmit(addr, m.slice(this.txBuf, this.txBuf + n));
    }
    for (;;) {
      let n = x.nb_poll_event(this.h, this.evBuf, this.evCap);
      if (n === 0) break;
      if (n < 0) {
        this.nb.free(this.evBuf, this.evCap);
        this.evCap = -n;
        this.evBuf = x.nb_alloc(this.evCap);
        continue;
      }
      this.onEvent(decodeEvent(this.nb.mem().slice(this.evBuf, this.evBuf + n)));
    }
    this.arm();
  }
  arm() {
    if (this.closed) return;
    const t = this.nb.x.nb_poll_timeout(this.h);
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    if (t < 0n) return;
    const delayMs = Math.max(0, Number(t - nowUs()) / 1000);
    this.timer = setTimeout(() => {
      this.timer = null;
      this.nb.x.nb_handle_timeout(this.h, nowUs());
      this.pump();
    }, delayMs);
  }
  close() {
    this.closed = true;
    if (this.timer) clearTimeout(this.timer);
    const x = this.nb.x;
    x.nb_endpoint_free(this.h);
    for (const [p, n] of [[this.peerBuf, 32], [this.txBuf, MAX_DATAGRAM], [this.addrBuf, 4], [this.evBuf, this.evCap]]) x.nb_free(p, n);
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
