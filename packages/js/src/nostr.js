// Signalling over public Nostr relays (NIP-01), with no server of our own.
//
// - Ephemeral events (kind 25059): relays forward them to live subscribers
//   and don't store them.
// - A fresh secp256k1 key per session signs events, so nothing links them to
//   the Nobaas identity or to other sessions.
// - Messages to a peer are sealed to its PeerId (Noise X: encrypted, and the
//   sender's PeerId authenticated), because WebRTC offers carry IP addresses.
//   They're tagged with a hash of the recipient's PeerId, not the PeerId.
// - Presence ('hello') is tagged with a hash of the room name and is readable
//   by anyone who knows the room name.
//
// Same interface as BroadcastSignaling: id, hello(meta), onHello, send(to, body),
// onMessage(fn), close().

import { isPeerId } from './nobaas.js';

const KIND = 25059;
export const DEFAULT_RELAYS = ['wss://relay.damus.io', 'wss://nos.lol'];

const enc = new TextEncoder();
const dec = new TextDecoder();
const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
const b64 = (b) => btoa(String.fromCharCode(...b));
const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const sha256 = async (bytes) => new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));

export class NostrSignaling {
  constructor(nb, endpoint, { room, relays = DEFAULT_RELAYS } = {}) {
    this.nb = nb;
    this.ep = endpoint;
    this.id = endpoint.id;
    this.room = room;
    this.key = nb.nostrKeypair();
    this.pubkey = hex(this.key.pubkey);
    this.handlers = [];
    this.onHello = () => {};
    this.seen = new Set();
    this.sockets = [];
    this.stats = { published: 0, received: 0, rejected: 0, relaysOpen: 0 };
    this.ready = this.start(relays);
  }

  async start(relays) {
    this.myTag = hex(await sha256(enc.encode(`nobaas/peer/${this.id}`)));
    this.roomTag = hex(await sha256(enc.encode(`nobaas/room/${this.room}`)));
    const sub = hex(crypto.getRandomValues(new Uint8Array(8)));
    const filter = { kinds: [KIND], '#p': [this.myTag, this.roomTag], since: Math.floor(Date.now() / 1000) - 30 };
    await Promise.all(relays.map((url) => new Promise((resolve) => {
      const ws = new WebSocket(url);
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
    const sig = this.nb.nostrSign(this.key.secret, id);
    const ev = { id: hex(id), pubkey: this.pubkey, created_at, kind: KIND, tags, content, sig: hex(sig) };
    const msg = JSON.stringify(['EVENT', ev]);
    for (const ws of this.sockets) if (ws.readyState === WebSocket.OPEN) ws.send(msg);
    this.stats.published++;
  }

  onRelay(url, raw) {
    let m;
    try {
      m = JSON.parse(raw);
    } catch {
      return;
    }
    if (m[0] === 'OK' && m[2] === false) {
      this.stats.rejected++;
      this.stats.lastRejection = `${url}: ${m[3]}`;
      return;
    }
    if (m[0] !== 'EVENT') return;
    const ev = m[2];
    if (!ev || ev.kind !== KIND || this.seen.has(ev.id) || ev.pubkey === this.pubkey) return;
    this.seen.add(ev.id);
    this.stats.received++;
    const tag = ev.tags.find((t) => t[0] === 'p')?.[1];
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
    await this.publish(hex(await sha256(enc.encode(`nobaas/peer/${to}`))), b64(sealed));
  }

  onMessage(fn) {
    this.handlers.push(fn);
  }

  close() {
    for (const ws of this.sockets) ws.close();
  }
}

