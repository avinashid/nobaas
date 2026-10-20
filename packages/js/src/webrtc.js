// WebRTC DataChannel transport for a Nobaas Endpoint.
//
// Each remote peer gets an RTCPeerConnection with one negotiated DataChannel
// (unordered, no retransmits): a datagram pipe. Its local handle number is the
// endpoint's address for that peer. Reliability, ordering and authentication
// come from Nobaas on top; the signalling channel is untrusted, because the Noise
// handshake over the DataChannel proves the remote holds the PeerId it claims.
//
// Signalling adapters implement { id, send(to, msg), onMessage(fn), close() };
// messages are JSON-able objects. See BroadcastSignaling below.

export class WebRtcTransport {
  // `loss` drops that fraction of outgoing datagrams, for testing.
  // `trickle: false` sends one offer/answer with every candidate once ICE
  // gathering finishes (fewer signalling messages, for relays like Nostr).
  // `RTCPeerConnection` lets Node pass in an implementation (node-datachannel).
  constructor(endpoint, signaling, { iceServers = [], onPeer = () => {}, loss = 0, trickle = true, RTCPeerConnection = globalThis.RTCPeerConnection } = {}) {
    this.ep = endpoint;
    this.Pc = RTCPeerConnection;
    this.trickle = trickle;
    this.dropped = 0;
    this.sig = signaling;
    this.iceServers = iceServers;
    this.onPeer = onPeer;
    this.links = new Map(); // handle -> { pc, dc, peer }
    this.byPeer = new Map(); // peer hex -> handle
    this.nextHandle = 1;
    endpoint.onTransmit = (handle, bytes) => {
      const l = this.links.get(handle);
      if (loss && Math.random() < loss) {
        this.dropped++;
        return;
      }
      if (l && l.dc.readyState === 'open') l.dc.send(bytes);
    };
    signaling.onMessage((from, msg) => this.onSignal(from, msg));
  }

  link(peer) {
    let h = this.byPeer.get(peer);
    if (h !== undefined) return this.links.get(h);
    h = this.nextHandle++;
    const pc = new this.Pc({ iceServers: this.iceServers });
    const dc = pc.createDataChannel('nobaas', { negotiated: true, id: 0, ordered: false, maxRetransmits: 0 });
    dc.binaryType = 'arraybuffer';
    const l = { pc, dc, peer, handle: h, dialer: false };
    dc.onopen = () => {
      if (l.dialer) this.ep.connect(h, peer);
      this.onPeer(peer, 'open');
    };
    dc.onclose = () => this.onPeer(peer, 'closed');
    dc.onmessage = (e) => this.ep.receive(h, new Uint8Array(e.data));
    pc.onicecandidate = (e) => {
      if (e.candidate && this.trickle) this.sig.send(peer, { kind: 'ice', candidate: e.candidate.toJSON() });
    };
    this.links.set(h, l);
    this.byPeer.set(peer, h);
    return l;
  }

  // Opens a DataChannel to `peer` and, once open, runs the Nobaas handshake.
  async dial(peer) {
    if (this.byPeer.has(peer)) return;
    const l = this.link(peer);
    l.dialer = true;
    await l.pc.setLocalDescription(await l.pc.createOffer());
    await this.gathered(l.pc);
    this.sig.send(peer, { kind: 'offer', sdp: l.pc.localDescription.toJSON() });
  }

  async onSignal(from, msg) {
    // Only an offer opens a link; answers and candidates for a dropped one are stale.
    if (msg.kind !== 'offer' && !this.byPeer.has(from)) return;
    if (msg.kind === 'offer') {
      const l = this.link(from);
      await l.pc.setRemoteDescription(msg.sdp);
      await l.pc.setLocalDescription(await l.pc.createAnswer());
      await this.gathered(l.pc);
      this.sig.send(from, { kind: 'answer', sdp: l.pc.localDescription.toJSON() });
    } else if (msg.kind === 'answer') {
      const l = this.link(from);
      await l.pc.setRemoteDescription(msg.sdp);
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

  // Forgets the link to `peer`, so a later dial or offer starts afresh.
  drop(peer) {
    const h = this.byPeer.get(peer);
    if (h === undefined) return;
    const l = this.links.get(h);
    l.dc.onclose = null;
    l.pc.close();
    this.links.delete(h);
    this.byPeer.delete(peer);
  }

  close() {
    for (const l of this.links.values()) {
      l.dc.onclose = null;
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
  constructor(id, room = 'nobaas-signal') {
    this.id = id;
    this.ch = new BroadcastChannel(room);
    this.handlers = [];
    this.onHello = () => {};
    this.ch.onmessage = (e) => {
      const m = e.data;
      if (m.from === this.id) return;
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
    this.ch.postMessage({ kind: 'msg', from: this.id, to, body });
  }
  onMessage(fn) {
    this.handlers.push(fn);
  }
  close() {
    this.ch.close();
  }
}
