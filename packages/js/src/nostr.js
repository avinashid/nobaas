// Signalling over public Nostr relays (NIP-01), with no server of our own.
//
// - Ephemeral events (kind 25059): relays forward them to live subscribers
//   and don't store them.
// - A fresh secp256k1 key per session signs events, so nothing links them to
//   the Peerwire identity or to other sessions.
// - Messages to a peer are sealed to its PeerId (Noise X: encrypted, and the
//   sender's PeerId authenticated), because WebRTC offers carry IP addresses.
//   They're tagged with a hash of the recipient's PeerId, not the PeerId.
// - Presence ('hello') is tagged with a hash of the room name and is readable
//   by anyone who knows the room name.
//
// Same interface as BroadcastSignaling: id, hello(meta), onHello, send(to, body),
// onMessage(fn), close().

import { isPeerId } from './peerwire.js';

const KIND = 25059;
const MAX_CONTENT = 64 * 1024; // event content we are willing to decode (base64 characters)
export const MAX_SEEN = 4096; // event ids remembered for de-duplication; the oldest is forgotten first
const EVENT_ID = /^[0-9a-f]{64}$/; // NIP-01: the event hash in lowercase hex, so `seen` stays ~256 KiB
export const DEFAULT_RELAYS = ['wss://relay.damus.io', 'wss://nos.lol'];
// A relay that refuses an event for its rate limit (NIP-01 'rate-limited:', or
// 'blocked:'/'banned') gets nothing from us for this long, doubling to the cap.
export const BACKOFF_MS = 30000;
export const MAX_BACKOFF_MS = 600000;
const LIMITED = /^(rate-limited|blocked)|banned|rate.?limit/i;

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
    this.stats = { published: 0, received: 0, rejected: 0, relaysOpen: 0 };
    this.ready = this.start(relays);
  }

  async start(relays) {
    this.myTag = hex(await sha256(enc.encode(`peerwire/peer/${this.id}`)));
    this.roomTag = hex(await sha256(enc.encode(`peerwire/room/${this.room}`)));
    const sub = hex(crypto.getRandomValues(new Uint8Array(8)));
    const filter = { kinds: [KIND], '#p': [this.myTag, this.roomTag], since: Math.floor(Date.now() / 1000) - 30 };
    await Promise.all(relays.map((url) => new Promise((resolve) => {
      const ws = new WebSocket(url);
      ws.relayUrl = url;
      ws.onopen = () => {
        this.stats.relaysOpen++;
        ws.send(JSON.stringify(['REQ', sub, filter]));
        resolve();
      };
      ws.onerror = () => resolve();
      ws.onmessage = (m) => this.onRelay(url, m.data);
      this.sockets.push(ws);
      setTimeout(resolve, 5000);
    })));
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

  onRelay(url, raw) {
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
        const h = JSON.parse(dec.decode(unb64(ev.content)));
        // Anyone who knows the room name can publish a hello: check `from` is a PeerId.
        if (h.t === 'hello' && isPeerId(h.from) && h.from !== this.id) this.onHello(h.from, h.meta);
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

  async hello(meta) {
    await this.ready; // the room tag is computed asynchronously
    this.publish(this.roomTag, b64(enc.encode(JSON.stringify({ t: 'hello', from: this.id, meta }))));
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
    for (const ws of this.sockets) ws.close();
  }
}

