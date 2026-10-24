// When a room tries WebRTC again with the peers it reaches only by fallback (relayed by
// a member, or over signalling): see Room.retryDirect. A retry dials the peer afresh
// with a new RTCPeerConnection (WebRtcTransport.retry); if its DataChannel opens, the
// link moves onto it, the same session, and route() says 'direct'.
//
// The dialling side retries each such peer on a backoff: firstMs after it went on the
// fallback, then twice as long each time up to maxMs, each ±jitter. A trigger (the app,
// a network change, a microphone or camera grant, the other side asking) makes peers
// due at once. Retries go over the signalling relays, as pipes do, so a room keeps them
// rare: one in flight at a time, starts at least gapMs apart, peers in turn; the other
// side's requests are heeded once per askMs per peer.

export class DirectRetries {
  static firstMs = 20000;
  static maxMs = 300000;
  static gapMs = 2000;
  static askMs = 20000;
  static jitter = 0.2;

  // `start(peer)` begins a retry: true while one is in flight (until done(peer)), false
  // if there is none to wait for (the peer was only asked to, or couldn't be retried).
  // `holdMs`: a retry in flight that never reports back stops holding the slot then.
  // (A browser's setTimeout throws when called as a method of anything but the window.)
  constructor(start, { holdMs = 30000, now = Date.now, rand = Math.random, setTimer = (f, ms) => setTimeout(f, ms), clearTimer = (t) => clearTimeout(t) } = {}) {
    Object.assign(this, { start, holdMs, now, rand, setTimer, clearTimer });
    this.peers = new Map(); // peer on a fallback -> { n: retries begun, timer, auto, askedAt, again }
    this.due = new Set(); // peers waiting for the slot, in turn
    this.busy = null; // the peer whose retry is in flight
    this.hold = null;
    this.next = -Infinity; // the earliest the next retry may start
    this.wake = null;
    this.closed = false;
  }

  // Whether `peer` is on a fallback (`on`) or not (direct, or not linked). `auto`: retry
  // it on the backoff (the dialling side); otherwise only when triggered.
  track(peer, on, auto) {
    if (!on) return this.forget(peer);
    if (this.closed || this.peers.has(peer)) return;
    const s = { n: 0, timer: null, auto, askedAt: -Infinity, again: false };
    this.peers.set(peer, s);
    this.arm(peer, s);
  }

  // How long a peer waits after `n` retries: firstMs·2ⁿ up to maxMs, ±jitter.
  delay(n) {
    const C = DirectRetries;
    return Math.round(Math.min(C.firstMs * 2 ** n, C.maxMs) * (1 + C.jitter * (2 * this.rand() - 1)));
  }

  arm(peer, s) {
    this.clearTimer(s.timer);
    s.timer = s.auto ? this.setTimer(() => this.soon(peer), this.delay(s.n)) : null;
  }

  // Makes `peer`, or every peer on a fallback, due now; one whose retry is in flight goes
  // again after it. Returns how many peers that is.
  soon(peer) {
    if (this.closed) return 0;
    const list = peer === undefined ? [...this.peers.keys()] : this.peers.has(peer) ? [peer] : [];
    for (const p of list) {
      const s = this.peers.get(p);
      this.clearTimer(s.timer);
      s.timer = null;
      if (p === this.busy) s.again = true;
      else this.due.add(p);
    }
    this.pump();
    return list.length;
  }

  // The other side asked for a retry of `peer`.
  ask(peer) {
    const s = this.peers.get(peer);
    if (!s || this.now() - s.askedAt < DirectRetries.askMs) return 0;
    s.askedAt = this.now();
    return this.soon(peer);
  }

  // The retry of `peer` ended: its channel opened, or it failed.
  done(peer) {
    if (this.busy !== peer) return;
    this.release();
    const s = this.peers.get(peer);
    if (s?.again) {
      s.again = false;
      this.due.add(peer);
    } else if (s) this.arm(peer, s);
    this.pump();
  }

  forget(peer) {
    this.clearTimer(this.peers.get(peer)?.timer);
    this.peers.delete(peer);
    this.due.delete(peer);
    if (this.busy !== peer) return;
    this.release();
    this.pump();
  }

  release() {
    this.clearTimer(this.hold);
    this.hold = null;
    this.busy = null;
  }

  // Starts the next due peer's retry, once the slot is free and the gap is over.
  pump() {
    if (this.closed || this.busy !== null || this.wake || !this.due.size) return;
    const wait = this.next - this.now();
    if (wait > 0) {
      this.wake = this.setTimer(() => {
        this.wake = null;
        this.pump();
      }, wait);
      return;
    }
    const [peer] = this.due;
    this.due.delete(peer);
    const s = this.peers.get(peer);
    s.n++;
    this.next = this.now() + DirectRetries.gapMs * (1 + DirectRetries.jitter * this.rand());
    this.busy = peer;
    if (this.start(peer) && this.busy === peer) {
      this.hold = this.setTimer(() => this.done(peer), this.holdMs);
      return;
    }
    if (this.busy === peer) this.busy = null;
    if (this.peers.get(peer) === s) this.arm(peer, s);
    this.pump();
  }

  close() {
    this.closed = true;
    for (const s of this.peers.values()) this.clearTimer(s.timer);
    this.peers.clear();
    this.due.clear();
    this.clearTimer(this.wake);
    this.release();
  }
}
