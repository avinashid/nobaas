// Datagrams over the signalling channel itself (BroadcastSignaling), for hosts
// without WebRTC (Node, workers) and for tests. Same interface as
// WebRtcTransport: dial, has, drop, close. The Nobaas handshake and lanes run on
// top as usual. Every datagram is posted to the whole channel, so this suits a
// few peers in one process or one browser, not a network.

import { isPeerId } from './nobaas.js';

export class LocalTransport {
  constructor(endpoint, signaling) {
    this.ep = endpoint;
    this.sig = signaling;
    this.byPeer = new Map(); // peer hex -> handle
    this.byHandle = new Map(); // handle -> peer hex
    this.nextHandle = 1;
    endpoint.onTransmit = (handle, bytes) => {
      const peer = this.byHandle.get(handle);
      if (peer) signaling.send(peer, { kind: 'dg', bytes });
    };
    signaling.onMessage((from, msg) => {
      if (isPeerId(from) && msg.kind === 'dg' && msg.bytes instanceof Uint8Array) this.ep.receive(this.handle(from), msg.bytes);
    });
  }

  handle(peer) {
    let h = this.byPeer.get(peer);
    if (h === undefined) {
      h = this.nextHandle++;
      this.byPeer.set(peer, h);
      this.byHandle.set(h, peer);
    }
    return h;
  }

  dial(peer) {
    if (!isPeerId(peer)) throw new TypeError('dial: not a peer id (64 lowercase hex characters)');
    if (!this.byPeer.has(peer)) this.ep.connect(this.handle(peer), peer);
  }

  has(peer) {
    return this.byPeer.has(peer);
  }

  drop(peer) {
    const h = this.byPeer.get(peer);
    if (h === undefined) return;
    this.byPeer.delete(peer);
    this.byHandle.delete(h);
  }

  close() {
    this.byPeer.clear();
    this.byHandle.clear();
    this.sig.close();
  }
}
