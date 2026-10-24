// Signalling over public Nostr relays (NIP-01), with no server of our own.
//
// - Ephemeral events (kind 25059): relays forward them to live subscribers
//   and don't store them.
// - A fresh secp256k1 key per session signs events, so nothing links them to
//   the Peerwire identity or to other sessions.
// - Messages to a peer are sealed to its PeerId (Noise X: encrypted, and the
//   sender's PeerId authenticated), because WebRTC offers carry IP addresses.
//   They're tagged with a hash of the recipient's PeerId, not the PeerId.
// - Presence ('hello') is tagged with a hash of the room name. Its body (which
//   carries our long-term PeerId) is encrypted with AES-GCM under a key derived
//   from the room name (sha256("peerwire/room-enc/<room>")), so a relay operator
//   or anyone who only sees the traffic learns nothing: only someone who already
//   knows the room name can read who is in it (SEC-2). Knowing the name is the
//   same bar as computing the topic tag, so this keeps presence confidential to
//   room members without changing who can join.
//
// Same interface as BroadcastSignaling: id, hello(meta), onHello, send(to, body),
// onMessage(fn), close().

import { isPeerId } from './peerwire.js';

const KIND = 25059;
const MAX_CONTENT = 64 * 1024; // event content we are willing to decode (base64 characters)
export const MAX_SEEN = 4096; // event ids remembered for de-duplication; the oldest is forgotten first
const EVENT_ID = /^[0-9a-f]{64}$/; // NIP-01: the event hash in lowercase hex, so `seen` stays ~256 KiB
// Three: a relay may rate-limit or ban an IP that several devices share (a phone hotspot).
export const DEFAULT_RELAYS = ['wss://nos.lol', 'wss://relay.primal.net', 'wss://nostr.mom'];
// A relay that refuses an event for its rate limit (NIP-01 'rate-limited:', or
// 'blocked:'/'banned') gets nothing from us for this long, doubling to the cap.
export const BACKOFF_MS = 30000;
export const MAX_BACKOFF_MS = 600000;
const LIMITED = /^(rate-limited|blocked)|banned|rate.?limit/i;
// A relay socket that closes (the phone switched networks or turned on its hotspot,
// Android froze the tab, the relay dropped us) is dialled again after this long,
// doubling to the cap; a network change or the page showing again redials it at
// once, and going online redials every relay.
export const RECONNECT_MS = 1000;
export const MAX_RECONNECT_MS = 30000;
// A socket still connecting this long when the network changes is given up and redialled.
const CONNECT_TIMEOUT_MS = 5000;

const enc = new TextEncoder();
const dec = new TextDecoder();
const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
// Chunked: spreading a large array into fromCharCode overflows the engine's argument limit.
export const b64 = (b) => {
  let s = '';
  for (let i = 0; i < b.length; i += 8192) s += String.fromCharCode.apply(null, b.subarray(i, i + 8192));
  return btoa(s);
};
export const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const sha256 = async (bytes) => new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));

export class NostrSignaling {
  constructor(pw, endpoint, { room, relays = DEFAULT_RELAYS } = {}) {
    this.pw = pw;
    this.ep = endpoint;
    this.id = endpoint.id;
    this.room = room;
    this.key = pw.nostrKeypair();
    this.pubkey = hex(this.key.pubkey);
    this.handlers = [];
    this.onHello = () => {};
    this.seen = new Set();
    this.sockets = [];
    this.paused = new Map(); // relay url -> { until, ms }: backing off its rate limit
    this.stats = { published: 0, received: 0, rejected: 0, relaysOpen: 0, reconnects: 0 };
    this.closed = false;
    this.retry = new Map(); // relay url -> { ms, timer }: redial after a close
    this.onReconnect = () => {}; // (url): a relay opened again after a close
    this.ready = this.start(relays);
  }

  async start(relays) {
    this.myTag = hex(await sha256(enc.encode(`peerwire/peer/${this.id}`)));
    this.roomTag = hex(await sha256(enc.encode(`peerwire/room/${this.room}`)));
    // Presence bodies are encrypted under this key so only someone who knows the
    // room name can read them (SEC-2); the topic tag above is a hash of the name,
    // so the name itself can't be recovered from the traffic.
    this.encKey = await crypto.subtle.importKey(
      'raw', await sha256(enc.encode(`peerwire/room-enc/${this.room}`)),
      { name: 'AES-GCM' }, false, ['encrypt', 'decrypt']);
    this.sub = hex(crypto.getRandomValues(new Uint8Array(8)));
    // Only going online recycles open sockets: navigator.connection's 'change' also
    // fires when Chrome's speed estimate moves (rtt 450 -> 300 ms), and redialling every
    // relay on each of those got an IP banned for too many connections.
    this.wake = (e) => {
      if (e?.type === 'visibilitychange' && globalThis.document?.visibilityState !== 'visible') return;
      this.redial(e?.type === 'online');
    };
    globalThis.addEventListener?.('online', this.wake);
    globalThis.document?.addEventListener?.('visibilitychange', this.wake);
    globalThis.navigator?.connection?.addEventListener?.('change', this.wake);
    await Promise.all(relays.map((url) => new Promise((resolve) => {
      this.dial(url, resolve);
      setTimeout(resolve, 5000);
    })));
  }

  // Opens (or reopens) the socket to `url`; `opened` runs once it opens or fails.
  dial(url, opened = () => {}) {
    if (this.closed) return;
    const again = this.sockets.some((s) => s.relayUrl === url);
    this.sockets = this.sockets.filter((s) => s.relayUrl !== url);
    const ws = new WebSocket(url);
    ws.relayUrl = url;
    ws.onopen = () => {
      this.stats.relaysOpen++;
      ws.counted = true;
      this.retry.delete(url);
      // Since a little before now: what was published while we were away has gone.
      const filter = { kinds: [KIND], '#p': [this.myTag, this.roomTag], since: Math.floor(Date.now() / 1000) - 30 };
      ws.send(JSON.stringify(['REQ', this.sub, filter]));
      opened();
      if (again) {
        this.stats.reconnects++;
        this.onReconnect(url);
      }
    };
    ws.onerror = () => opened();
    ws.onclose = () => this.lostSocket(ws);
    ws.onmessage = (m) => void this.onRelay(url, m.data).catch(() => {});
    ws.dialled = Date.now();
    this.sockets.push(ws);
  }

  lostSocket(ws) {
    if (ws.counted) this.stats.relaysOpen--;
    ws.counted = false;
    if (this.closed || !this.sockets.includes(ws) || this.retry.get(ws.relayUrl)?.timer) return;
    const ms = this.retry.get(ws.relayUrl)?.ms ?? RECONNECT_MS;
    const timer = setTimeout(() => {
      this.retry.set(ws.relayUrl, { ms: Math.min(ms * 2, MAX_RECONNECT_MS), timer: null });
      this.dial(ws.relayUrl);
    }, ms);
    this.retry.set(ws.relayUrl, { ms, timer });
  }

  // The network came back or changed, or the page is showing again: every relay that
  // isn't open is redialled now. On going online (`all`), open sockets are too,
  // since their TCP connection may be on the old network and take minutes to time out.
  redial(all) {
    if (this.closed) return;
    for (const ws of [...this.sockets]) {
      const open = ws.readyState === WebSocket.OPEN;
      const stuck = ws.readyState === WebSocket.CONNECTING && Date.now() - ws.dialled < CONNECT_TIMEOUT_MS;
      if ((open && !all) || stuck) continue;
      clearTimeout(this.retry.get(ws.relayUrl)?.timer);
      this.retry.delete(ws.relayUrl);
      if (ws.counted) this.stats.relaysOpen--;
      ws.counted = false;
      ws.onclose = null;
      ws.onmessage = null;
      try {
        ws.close();
      } catch {}
      this.dial(ws.relayUrl);
    }
  }

  async publish(tag, content) {
    await this.ready;
    const created_at = Math.floor(Date.now() / 1000);
    const tags = [['p', tag]];
    const id = await sha256(enc.encode(JSON.stringify([0, this.pubkey, created_at, KIND, tags, content])));
    const sig = this.pw.nostrSign(this.key.secret, id);
    const ev = { id: hex(id), pubkey: this.pubkey, created_at, kind: KIND, tags, content, sig: hex(sig) };
    const msg = JSON.stringify(['EVENT', ev]);
    const now = Date.now();
    for (const ws of this.sockets) {
      if (ws.readyState === WebSocket.OPEN && !(this.paused.get(ws.relayUrl)?.until > now)) ws.send(msg);
    }
    this.stats.published++;
  }

  async onRelay(url, raw) {
    let m;
    try {
      m = JSON.parse(raw);
    } catch {
      return;
    }
    // Relay messages are arrays; `null` and other JSON values are ignored, not dereferenced.
    if (!Array.isArray(m)) return;
    if (m[0] === 'OK' && m[2] === false) {
      this.stats.rejected++;
      this.stats.lastRejection = `${url}: ${m[3]}`;
      if (LIMITED.test(String(m[3]))) {
        const p = this.paused.get(url);
        // Events already on their way get refused too: count one per pause.
        if (p?.until > Date.now()) return;
        // A relay quiet for MAX_BACKOFF_MS since its last pause starts over at BACKOFF_MS.
        const ms = p && Date.now() - p.until < MAX_BACKOFF_MS ? Math.min(p.ms * 2, MAX_BACKOFF_MS) : BACKOFF_MS;
        this.paused.set(url, { until: Date.now() + ms, ms });
        this.stats.backoffs = (this.stats.backoffs ?? 0) + 1;
      }
      return;
    }
    if (m[0] !== 'EVENT') return;
    const ev = m[2];
    if (!ev || ev.kind !== KIND || typeof ev.id !== 'string' || !EVENT_ID.test(ev.id) || this.seen.has(ev.id) || ev.pubkey === this.pubkey) return;
    // Anyone who knows the room name can publish, so the set is bounded (a Set iterates in insertion order).
    this.seen.add(ev.id);
    if (this.seen.size > MAX_SEEN) this.seen.delete(this.seen.values().next().value);
    this.stats.received++;
    if (!Array.isArray(ev.tags)) return;
    const tag = ev.tags.find((t) => Array.isArray(t) && t[0] === 'p')?.[1];
    // Signalling bodies are small; a huge event would only cost us wasm memory.
    if (typeof ev.content !== 'string' || ev.content.length > MAX_CONTENT) return;
    try {
      if (tag === this.roomTag) {
        // Presence is encrypted to the room: a body we can't open isn't ours to read.
        const h = await this.openRoom(ev.content);
        // Anyone who knows the room name can publish a hello: check `from` is a PeerId.
        if (h && h.t === 'hello' && isPeerId(h.from) && h.from !== this.id) this.onHello(h.from, h.meta);
      } else if (tag === this.myTag) {
        const opened = this.ep.open(unb64(ev.content));
        if (!opened) return; // not for us, or tampered: ignore
        const body = JSON.parse(dec.decode(opened.data));
        for (const h of this.handlers) h(opened.from, body);
      }
    } catch {
      // malformed content from someone else's client: ignore
    }
  }

  // Encrypts an object for the room: base64 of nonce (12 bytes) || AES-GCM ciphertext.
  async sealRoom(obj) {
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ct = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, this.encKey, enc.encode(JSON.stringify(obj))));
    const out = new Uint8Array(iv.length + ct.length);
    out.set(iv);
    out.set(ct, iv.length);
    return b64(out);
  }

  // Decrypts a room-sealed body back to its object, or null if it isn't one for
  // this room (another room's tag collision, old plaintext, or a forgery).
  async openRoom(content) {
    if (typeof content !== 'string') return null;
    let buf;
    try {
      buf = unb64(content);
    } catch {
      return null;
    }
    if (buf.length < 12 + 16) return null; // too short for a nonce and a GCM tag
    try {
      const pt = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: buf.subarray(0, 12) }, this.encKey, buf.subarray(12));
      return JSON.parse(dec.decode(new Uint8Array(pt)));
    } catch {
      return null;
    }
  }

  async hello(meta) {
    await this.ready; // the room tag and key are computed asynchronously
    this.publish(this.roomTag, await this.sealRoom({ t: 'hello', from: this.id, meta }));
  }

  async send(to, body) {
    if (!isPeerId(to)) throw new TypeError('send: `to` must be a peer id (64 lowercase hex characters)');
    const sealed = this.ep.seal(to, enc.encode(JSON.stringify(body)));
    if (!sealed) return;
    await this.publish(hex(await sha256(enc.encode(`peerwire/peer/${to}`))), b64(sealed));
  }

  onMessage(fn) {
    this.handlers.push(fn);
  }

  close() {
    this.closed = true;
    for (const r of this.retry.values()) clearTimeout(r.timer);
    this.retry.clear();
    globalThis.removeEventListener?.('online', this.wake);
    globalThis.document?.removeEventListener?.('visibilitychange', this.wake);
    globalThis.navigator?.connection?.removeEventListener?.('change', this.wake);
    for (const ws of this.sockets) ws.close();
  }
}

