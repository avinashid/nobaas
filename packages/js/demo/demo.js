import { loadNobaas, Lane } from '../nobaas.js';
import { WebRtcTransport, BroadcastSignaling } from '../webrtc.js';
import { NostrSignaling } from '../nostr.js';

const params = new URLSearchParams(location.search);
const room = params.get('room') ?? 'demo';
const nb = await loadNobaas(new URL('../pkg/nobaas_ffi.wasm', import.meta.url));
const ep = nb.endpoint();
// ?signal=nostr: public Nostr relays + public STUN, so tabs on different
// machines can meet. Default: BroadcastChannel (same browser only).
const nostr = params.get('signal') === 'nostr';
const sig = nostr ? new NostrSignaling(nb, ep, { room: `demo/${room}` }) : new BroadcastSignaling(ep.id, `nobaas-${room}`);
const ice = nostr ? { iceServers: [{ urls: 'stun:stun.l.google.com:19302' }], trickle: false } : {};
const peers = new Map(); // id -> { path, connectedAt }
const received = new Map(); // id -> count of bench messages
const pongs = [];
const $ = (id) => document.getElementById(id);
const short = (id) => id.slice(0, 12);
const log = (s) => {
  const d = $('log');
  d.textContent += `${new Date().toISOString().slice(11, 23)} ${s}\n`;
  d.scrollTop = d.scrollHeight;
};
const render = () => {
  $('peers').innerHTML = [...peers].map(([id, p]) => {
    const rtt = ep.rttMs(id);
    return `<tr><td><code>${short(id)}</code></td><td>${p.path}</td><td>${rtt === null ? '–' : rtt.toFixed(2) + ' ms'}</td></tr>`;
  }).join('');
};
const transport = new WebRtcTransport(ep, sig, { ...ice, onPeer: (id, s) => log(`datachannel ${short(id)} ${s}`) });
const dec = new TextDecoder();

ep.onEvent = (e) => {
  if (e.type === 'connected') {
    peers.set(e.peer, { path: e.path.direct !== undefined ? `datachannel #${e.path.direct}` : `via ${short(e.path.via)}`, connectedAt: performance.now() });
    log(`Nobaas link up with ${short(e.peer)} (${e.initiator ? 'dialled' : 'answered'})`);
  } else if (e.type === 'disconnected') {
    peers.delete(e.peer);
    log(`Nobaas link to ${short(e.peer)} closed: ${e.reason}`);
  } else if (e.type === 'message') {
    const tag = e.data[0];
    if (tag === 0x42) {
      received.set(e.peer, (received.get(e.peer) ?? 0) + 1);
    } else if (tag === 0x50) {
      ep.send(e.peer, Uint8Array.of(0x51, ...e.data.subarray(1)), e.lane);
    } else if (tag === 0x51) {
      const t = new DataView(e.data.buffer, e.data.byteOffset + 1).getFloat64(0, true);
      pongs.push(performance.now() - t);
    } else {
      log(`${short(e.peer)}: ${dec.decode(e.data)}`);
    }
  }
  render();
};
sig.onHello = (other) => {
  if (!transport.byPeer.has(other)) {
    sig.hello(); // let the newcomer learn about us too
    if (ep.id < other) transport.dial(other);
  }
};
$('me').textContent = short(ep.id);
$('room').textContent = room;
$('chat').onsubmit = (ev) => {
  ev.preventDefault();
  for (const id of peers.keys()) ep.send(id, $('text').value);
  log(`me: ${$('text').value}`);
  $('text').value = '';
};
setInterval(render, 1000);
sig.hello();
log(`ready; announcing on ${nostr ? 'Nostr relays' : 'the room channel'}`);

// Hooks for automated tests.
window.demo = {
  id: ep.id,
  peers: () => [...peers.keys()],
  rtt: (id) => ep.rttMs(id),
  stats: () => ep.stats(),
  signaling: () => sig.stats ?? null,
  paths: () => Object.fromEntries([...transport.links.values()].map((l) => [l.peer, l.pc.connectionState])),
  buffered: () => [...transport.links.values()].map((l) => l.dc.bufferedAmount),
  received: (id) => received.get(id) ?? 0,
  // Sends n messages of `size` bytes; resolves when all are queued.
  async bench(id, n, size, lane = Lane.Reliable) {
    const m = new Uint8Array(size);
    m[0] = 0x42;
    for (let i = 0; i < n; i++) {
      while (ep.send(id, m, lane) === -3) await new Promise((r) => setTimeout(r, 1));
    }
  },
  // App-level round trips over the Nobaas link.
  async ping(id, n, lane = Lane.Reliable) {
    pongs.length = 0;
    for (let i = 0; i < n; i++) {
      const m = new Uint8Array(9);
      m[0] = 0x50;
      new DataView(m.buffer).setFloat64(1, performance.now(), true);
      ep.send(id, m, lane);
      await new Promise((r) => setTimeout(r, 10));
    }
    await new Promise((r) => setTimeout(r, 200));
    return [...pongs].sort((a, b) => a - b);
  },
};
