// Room proofs: knowing a room's name, not just its topic.
//
// The room topic (a hash of the name) is what signalling relays see, so it proves
// nothing. The room key is the same name hashed under another label, as in Rust
// (traversal.rs `room_key`): sha256("peerwire/room-key/<app>/<room>"). Peers prove
// they hold it with an HMAC-SHA256 under it, over a label and both ends' ids:
//
// - 'pw-hello' in a hello's meta.p, over sender id, role byte and meta.ts (the
//   sender's Unix time in ms, little-endian u64), exactly as Rust builds it;
// - 'pw-rtc-offer' in a sealed WebRTC offer, over sender and recipient ids;
// - 'pw-link' as the first room message on a Peerwire link, over sender and
//   recipient ids, so a link made any other way still has to prove the key.
//
// A room whose name can be guessed is a public lobby: use roomIds with at least
// 128 random bits (randomRoomId()).

import { toHex, fromHex } from './peerwire.js';

const enc = new TextEncoder();
const ROLES = { peer: 0, host: 1, client: 2 };
// Hellos older or newer than this (by the sender's clock) are dropped, as in Rust.
const FRESH_MS = 60000;

// 16 random bytes (128 bits), base64url: a roomId nobody can guess.
export function randomRoomId() {
  const b = crypto.getRandomValues(new Uint8Array(16));
  return btoa(String.fromCharCode(...b)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function concat(parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let i = 0;
  for (const p of parts) {
    out.set(p, i);
    i += p.length;
  }
  return out;
}

const id = (peer) => (typeof peer === 'string' ? fromHex(peer) : peer);

function u64le(n) {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n), true);
  return b;
}

export class RoomProof {
  // `name` is '<appId>/<roomId>'.
  static async create(name) {
    const raw = new Uint8Array(await crypto.subtle.digest('SHA-256', enc.encode(`peerwire/room-key/${name}`)));
    const key = await crypto.subtle.importKey('raw', raw, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify']);
    return new RoomProof(key);
  }

  constructor(key) {
    this.key = key;
  }

  async mac(label, parts) {
    return new Uint8Array(await crypto.subtle.sign('HMAC', this.key, concat([enc.encode(label), ...parts])));
  }

  // Constant-time check of a hex proof (WebCrypto's verify).
  async check(label, parts, proofHex) {
    if (typeof proofHex !== 'string' || !/^[0-9a-f]{64}$/.test(proofHex)) return false;
    return crypto.subtle.verify('HMAC', this.key, fromHex(proofHex), concat([enc.encode(label), ...parts]));
  }

  // The hello meta for `self`: { role, ts, p }.
  async hello(self, role = 'peer', ts = Date.now()) {
    return { role, ts, p: toHex(await this.mac('pw-hello', [id(self), [ROLES[role]], u64le(ts)])) };
  }

  async checkHello(from, meta, now = Date.now()) {
    if (typeof from !== 'string' || !/^[0-9a-f]{64}$/.test(from)) return false;
    const role = ROLES[meta?.role];
    const ts = meta?.ts;
    if (role === undefined || !Number.isSafeInteger(ts) || ts < 0 || Math.abs(now - ts) > FRESH_MS) return false;
    return this.check('pw-hello', [id(from), [role], u64le(ts)], meta.p);
  }

  // A proof from `from` to `to` under `label` ('pw-rtc-offer' or 'pw-link').
  async pair(label, from, to) {
    return toHex(await this.mac(label, [id(from), id(to)]));
  }

  checkPair(label, from, to, proofHex) {
    return this.check(label, [id(from), id(to)], proofHex);
  }
}
