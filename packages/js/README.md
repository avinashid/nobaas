# @peerwire/js

Peer-to-peer rooms for the browser and Node, with no server of our own:
peerwire-core compiled to a WebAssembly module with no imports
(`crates/peerwire-ffi`), a small JS host, WebRTC DataChannels as the datagram
transport and Nostr relays (or a BroadcastChannel) to find peers. No wasm-bindgen.

```
./packages/js/build.sh                         # -> packages/js/pkg/peerwire_ffi.wasm (~300 KB)
python3 -m http.server -d packages/js 8000     # any static server
```

## A chat in ten lines

<!-- readme-chat: test/readme.mjs runs this block in two browser tabs -->
```html
<ul id="log"></ul>
<form id="chat"><input id="text" autocomplete="off"><button>Send</button></form>
<script type="module">
  import { joinRoom } from '@peerwire/js';
  const room = await joinRoom({ appId: 'readme-chat', roomId: 'lobby' }); // signaling: 'nostr' to reach other machines
  const show = (who, text) => log.append(Object.assign(document.createElement('li'), { textContent: `${who}: ${text}` }));
  room.on('peer-join', (peer) => show(peer.slice(0, 6), 'joined'));
  room.on('message', (text, peer) => show(peer.slice(0, 6), text));
  chat.onsubmit = (e) => {
    e.preventDefault(); room.send(text.value);
    show('me', text.value); chat.reset();
  };
</script>
```

Open it in two tabs. With a bundler (Vite, webpack, esbuild) `@peerwire/js` resolves
from npm and the wasm is found next to the package. Without one, map it in the
page: `<script type="importmap">{"imports":{"@peerwire/js":"/path/to/packages/js/src/index.js"}}</script>`.

## joinRoom

```js
const room = await joinRoom({
  appId: 'my-app',          // rooms of different apps never meet
  roomId: randomRoomId(),   // the room's secret: share it to invite (see Closed rooms)
  signaling: 'local',       // 'local' (tabs of one browser, one Node process) | 'nostr' | custom factory
  presence: { name: 'Ana' }, // shared with every peer when it joins
  privacy: 'direct',        // or 'public-only': no LAN addresses in ICE (see Privacy below)
  fallback: 'auto',         // peers WebRTC can't link: 'auto' | 'peers' | 'off' (see Through any NAT)
});

room.on('peer-join', (peer, presence) => {});
room.on('peer-leave', (peer, reason) => {});    // 'left', 'closed', 'timeout', ...
room.on('presence', (peer, presence) => {});    // a peer called setPresence
room.on('message', (data, peer, { lane }) => {});

room.send('hi');                                // every joined peer
room.send({ x: 1, y: 2 }, { lane: 'unreliable' }); // latest-wins, one datagram at most
room.send(bytes, { to: peer });                 // one peer, or an array of them
room.setPresence({ name: 'Ana', typing: true });
room.peers; room.self; room.getPresence(peer); room.rtt(peer);
room.route(peer);                               // { kind: 'direct' (adopted?) | 'peer' (via) | 'signaling' }
await room.leave();                             // peers see 'peer-leave' at once
```

- **Data:** strings arrive as strings, `Uint8Array`/`ArrayBuffer` as `Uint8Array`,
  anything else as JSON.
- **Lanes:** `reliable` (default: ordered, retransmitted), `unreliable` (no
  retransmits, for positions and cursors), `urgent` (reliable, each fragment sent
  twice, ordered only among urgent messages so it overtakes queued `reliable`
  data; up to 64 KiB encoded, and `send` throws a `RangeError` above that).
  Reliable sends past the send window queue up instead of failing.
- **Presence** travels over the encrypted links, not through signalling. A peer
  counts as joined once its presence has arrived, so `peer-join` always has it.
- **Closed rooms:** the roomId is the room's secret. Relays see only a hash of it
  (the topic); every hello, WebRTC offer and link carries an HMAC under a key
  derived from it (`sha256("peerwire/room-key/<appId>/<roomId>")`, as in Rust). A peer
  is dialled only on a hello that proves the key, offers that don't prove it get no
  answer (and so none of your addresses), and a link that doesn't prove it within
  10 s is dropped without seeing presence or messages. A name anyone can guess
  (`'lobby'`) makes a public lobby; for a closed room use at least 128 random bits:
  `randomRoomId()`. Members are trusted alike: anyone given the roomId can join.
- **Signalling:** `'nostr'` publishes ephemeral events to `wss://relay.damus.io`,
  `wss://nos.lol` and `wss://relay.primal.net` (override with `relays`) and uses Google and Cloudflare STUN
  (override with `iceServers`). Relays see the room topic and the PeerIds in
  hellos, not the roomId; offers are sealed to the recipient. Each side sends one
  offer or answer with its candidates, once ICE gathering is complete or 300 ms
  after the first public (STUN) candidate (2 s at most), and any candidates found
  later in one `ice-batch`. Relays drop events, so an offer with no answer is sent
  again at 3 s and 6 s, and a repeated offer gets the same answer again.
  `transport.trace` reports each step (Net Lab shows it).
- **Node:** Node has no WebRTC. `signaling: 'local'` rooms then carry their
  datagrams over the BroadcastChannel (peers in one process or its workers). For
  real networks pass `RTCPeerConnection` from a WebRTC package such as
  node-datachannel's polyfill.
- **Identity:** `room.endpoint.secret()` returns 32 bytes; pass them back as
  `secret` to keep the same PeerId.

### Pull-style, for game engines

Hosts that handle callbacks poorly, such as Godot's web export through
`JavaScriptBridge`, can poll once a frame instead. Events are plain objects:
string ids, `ArrayBuffer` bytes, JSON values, and no class instances.

```js
const room = await joinRoom({ appId: 'my-game', roomId: 'match-1', queue: true });
// every frame:
for (const e of room.poll()) {   // or room.receive(): the next event, or null
  // { type: 'peer-join' | 'presence', peer, presence }
  // { type: 'peer-leave', peer, reason }
  // { type: 'message', peer, data, kind: 'text' | 'bytes' | 'json', lane }
}
room.sendTo('', 'x,y', 'unreliable');   // '' or null: everyone; else a peer id
```

`queue: true` keeps events from the moment you join. Without it, the queue starts
at the first `poll()` or `receive()`, so callback-only apps never build one up. A
started queue grows until you drain it. `demo/poll.html` runs this way.

Types are in `src/index.d.ts`.

## Files

| File | What it is |
|---|---|
| `src/index.js` | The package entry: `joinRoom` and everything below. |
| `src/room.js` | `joinRoom` and `Room`: mesh, presence, message framing, backpressure. |
| `src/proof.js` | Room key and HMAC proofs (hello, offer, link), compatible with Rust's; `randomRoomId()`. |
| `src/peerwire.js` | Loads the module; `Endpoint` (connect, send, events, stats, seal/open) and Nostr key helpers. Browser and Node. |
| `src/webrtc.js` | `WebRtcTransport`: one negotiated, unordered, no-retransmit DataChannel per peer. `BroadcastSignaling` for tabs of one browser. |
| `src/nostr.js` | `NostrSignaling` over public relays: ephemeral events, a throwaway key per session, payloads sealed to the recipient's PeerId. |
| `src/local.js` | `LocalTransport`: datagrams over the BroadcastChannel, for Node and tests. |
| `src/index.d.ts` | TypeScript types. |
| `demo/index.html` | Mesh chat on the low-level API: every tab links to every other. |
| `demo/poll.html` | Pull-style room: `poll()` and `sendTo()` once a frame, dots you move with the arrow keys. |
| `demo/arena.html` | Super-peer lockstep arena: `?role=super` in one tab, players in others; arrow keys move. |
| `demo/netlab.html` | Net Lab, a connection diagnostics page (not a game), mobile-friendly: join a room by any name you type (`office test` is the room `office-test`, shared as a `#room=` link, with recent rooms and a "make private" suffix), the global room, or a private one shared as a `#key=` link, and every member links to every other. An overview row sums up the room (peers, average and worst ping, packet loss, routes, WebRTC traffic, packets). Per peer: ping measured on the route in use (latest, min/avg/max, jitter, loss, with a sparkline), core and ICE round trips, WebRTC traffic, ICE restarts and port guesses, the route (direct, relayed by a member, or over signalling), the ICE candidate pair (IPv4/IPv6, UDP/TCP), the offer/answer exchange (times, resends, the addresses each carried), ICE connectivity checks sent, answered and received with a verdict on which way packets are blocked, time to link and the other side's NAT. Also this device's public addresses, IPv6 and a NAT mapping estimate, the signalling relays' state, chat, an event log with warnings and errors, and a JSON report. `?relays=` other Nostr relays; `?rtc=off` fails this tab's WebRTC links. `demo/crew-server.mjs` serves it at `/`. |
| `demo/crew.html` | Crew, a small Among Us-like game: the host is authoritative and relays for every player, so a player needs a link to the host only. Network panel: per-peer RTT and route (ICE path, or the fallback). `?turn=turn:HOST:PORT&tu=USER&tp=PASS` adds a TURN server; `?rtc=off` fails this tab's WebRTC links (testing the fallback). `demo/crew-server.mjs` serves it for hosting. |

Signalling is untrusted by design: whatever carries the offers, the Peerwire Noise
handshake over the DataChannel proves each side holds its PeerId.

## Privacy

A WebRTC peer sees the IP addresses in your ICE candidates and, once linked, the
one your packets come from. `new WebRtcTransport(ep, sig, { privacy })` picks what
it is given:

- `'direct'` (default): every candidate.
- `'public-only'`: no `host` candidates (LAN, loopback, mDNS `.local` names), in
  trickled ICE and in the SDP. Only what STUN reports goes out, so two tabs on one
  machine with no STUN server can't link this way.
- `'relay-only'` with `relay: <peer id>`: WebRTC runs with the relay only (also
  public-only); offers from anyone else are ignored and `dial()` to them throws.
  Reach other peers with `endpoint.connectVia(relay, peer)`: the endpoint is
  relay-only (`Endpoint.setRelayOnly`), so those links never punch to a direct
  path, and the relay is asked not to pass on your address. The relay sees both
  ends. There is no TURN: a peer not linked to the relay can't be reached.

## Trying it

- Same browser: open `demo/index.html` or the arena pages in several tabs.
- Different machines: add `?signal=nostr&room=<something only you know>` to every
  page. Peers meet through `wss://relay.damus.io`, `wss://nos.lol` and `wss://relay.primal.net` and use
  Google's public STUN. Peers behind symmetric NATs on both sides still meet: see
  below.

## Through any NAT

WebRTC can't link two peers that are both behind symmetric NAT (common on mobile
hotspots and carrier NAT) unless a TURN server is configured. Rather than requiring
one, `joinRoom` falls back per peer, on its own, with no setup on either side:

1. **Direct WebRTC**, as always (`room.route(peer).kind === 'direct'`). Two things help
   it through more NATs:
   - **Port prediction.** A browser behind a NAT that hands out ports in sequence sees it
     from its two STUN servers' answers (`natFromSdp`: one IPv4 address, two ports 1 to 8
     apart). Its offer, answer or late candidates then carry `nat: { ip, next, step }`. A
     peer that isn't behind a symmetric NAT itself adds 32 candidates at the ports around
     `next` (`predictedCandidates`), so its checks open its NAT to the port the other's
     will come from. Peers that don't know the field ignore it.
   - **ICE restart.** When a link that was open loses ICE (a Wi-Fi to mobile switch, a
     hotspot dropping), it isn't dropped: after 2 s of `disconnected`, or at once on
     `failed`, the dialling side renegotiates with new ICE credentials, so ICE gathers the
     new network's addresses and checks again; the other side asks it to. The DataChannel
     and the Peerwire session over it carry on, so the app sees no leave. At most 3 tries
     (2, 4, 8 s apart) until ICE connects again. `online` and `navigator.connection`
     changes restart links whose ICE is down at once. Each restart is an offer and an
     answer over signalling.
2. **Through another member.** When a link isn't open `fallbackMs` (8 s) after its
   offer, or its ICE fails, the dialling side dials the peer again through every member
   it has a direct link to, including members that link later. The core forwards for
   linked peers (`connect_via`, 256 KiB/s per peer) and sees only ciphertext (`'peer'`).
3. **Over the signalling relays.** If no member has answered within 1 s, Peerwire
   datagrams also go as signalling messages (a *pipe*). On Nostr each batch is sealed
   to the peer again (`'signaling'`). This works wherever the relays are reachable.
   It is slow, though: a relay round trip per batch, 40 ms batching, and at most 3
   batches a second for all of a client's pipes together. That is within what public
   relays allow one client. A relay that refuses events for its rate limit gets none
   for 30 s, doubling up to 10 min. The pause starts over at 30 s once the relay has
   gone 10 min without refusing.

The core keeps the first path that completes the handshake, and the room's proofs,
presence and messages run unchanged on it. A pipe opens only for a peer whose hello
proved the room key. `fallback: 'peers'` skips step 3; `'off'` keeps WebRTC only.
TURN in `iceServers` still works and is faster than step 3 when you have a server.

**A direct channel found later.** A DataChannel to the peer opened outside the room (a
browser birthday punch's, `src/birthday.js`, as Net Lab runs one) can take a fallback
link over. Both sides call `room.adoptChannel(peer, pc, dc)`. Before anything moves, the
two ends agree over the channel: each sends a one-byte marker until it hears the
other's, so a link never moves onto a channel only one side reads. A link over
signalling then sends its datagrams over the channel at once. A relayed link moves once
the endpoint's punch over the channel is answered (`endpoint.addCandidate`).
`room.route(peer)` then says `{ kind: 'direct', adopted: true }`. It resolves `false`,
and leaves the channel to the caller, if the link is direct already or the peer didn't
adopt within 5 s. When the channel closes, a link over signalling goes back at once.
A relayed one goes back to its member after the endpoint's 15 s direct timeout.

What the fallback reveals:
- **To the other members.** Every member the dialler is linked to learns that the two
  peers couldn't link directly. The member that relays sees the timing and volume of
  their traffic, never its content. A relaying member can also move the link onto its
  own path, which gives it no more power than dropping the traffic would.
- **To the public relays.** On a pipe, the Nostr relays see a steady stream between two
  tags for the whole session, where WebRTC sends them only a few signalling events.
  They see no content and, unlike WebRTC, the peer learns no IP address.

## Tests

Run these with Node and the Playwright install on this host:

- `node packages/js/test/node_pair.mjs`: two endpoints in Node, checks the glue.
- `node packages/js/test/privacy.mjs`: host-candidate filtering, and a relay-only
  endpoint linking through a relay and refusing a direct dial.
- `node packages/js/test/room_node.mjs`: `joinRoom` with three peers in Node: presence,
  message types, lanes, targeted sends, backpressure, leaving, a late joiner and
  the pull-style `poll()`/`receive()`/`sendTo()` path.
- `node packages/js/test/peer_id.mjs`: untrusted peer ids. A malformed or over-long id
  throws a `TypeError` before any copy into wasm memory, and every signalling layer
  drops a `from` that isn't 64 lowercase hex characters.
- `node packages/js/test/nostr_bounds.mjs`: a flood of 12,000 distinct Nostr events
  keeps the de-duplication set at 4096 ids, malformed events don't throw, and base64
  round-trips bodies up to 1 MB. No relays.
- `node packages/js/test/room_proof.mjs`: an attacker that knows only the room topic
  gets no dial, no answer to its offers and, on a link it dials itself, nothing but
  the link proof before it is dropped; the hello proof matches Rust's; a room that signs
  its own link proof after checking the peer's still joins on both sides.
- `node packages/js/test/fuzz.mjs`: seeded random and mutated input to the JS parsers (Nostr
  events and relay messages, hex and peer ids, room hello proofs, room frames): nothing throws
  out of a handler of untrusted input and no promise is left rejected.
- `node packages/js/test/webrtc_signals.mjs` (Chromium, loopback): malformed offers leave no
  link and no unhandled rejection, 300 random signals throw nothing, real offers are still
  answered, and each refused signal is counted in `WebRtcTransport.badSignals` (as Nostr's
  `stats.rejected` counts refused events). Without trickle, a lost first offer or first
  answer still links two transports (in about 3.2 s), candidates that overtake their
  description are held, and gathering stops 300 ms after the first public candidate.
- `node packages/js/test/nat_predict.mjs`: NAT type and prediction from server-reflexive
  candidates, the 32 predicted candidates, and a peer's prediction refused unless it is an
  IPv4 address and ports in range (nothing can be spliced into a candidate line).
- `node packages/js/test/ice_restart.mjs` (Chromium, loopback): an ICE restart by either side
  renews the ICE credentials while the DataChannel stays open and datagrams flow; ICE
  `disconnected` restarts after 2 s, `failed` at once; at most 3 while ICE stays down;
  `networkChanged()` touches only links whose ICE is down; an offer's prediction adds 32
  candidates, a malformed one none.
- `node packages/js/test/pump_rearm.mjs`: a transport that throws or refuses to send (a full
  DataChannel) costs datagrams, never the endpoint's timer: a reliable message sent during
  the outage still arrives after it, by retransmit.
- `node packages/js/test/endpoint_closed.mjs`: after `close()` no `Endpoint` method calls
  into wasm: a late datagram is dropped, `send`/`connect*` throw, queries return null.
- `node packages/js/test/scratch_buffers.mjs`: `send` and `receive` reuse the endpoint's
  buffers: messages arrive intact back to back and after wasm memory grows, with no
  allocation per message up to 16 KiB.
- `node packages/js/test/room_drain.mjs`: 6 MiB through a Room at once, over the send
  window: the outbox is flushed as ACKs arrive (`Endpoint.onDrain`), the timer is a fallback.
- `node packages/js/test/room_leave.mjs`: a room leaving with a full send window reaches
  its peers at once, before `leave()` returns; with the Bye lost, the core's close still does;
  the `pagehide` handler holds the page 20 ms after it, only if it had peers.
- `node packages/js/test/room_throttled.mjs --ab=<other packages/js>`: benchmark, not a test.
  Room send throughput when the sender's timers are throttled like a background tab's.
- `node packages/js/test/crew.mjs [tabs=4] [shot-prefix]`: plays `demo/crew.html` in four tabs: a kill,
  a report, a meeting with chat, the impostor voted out, then a second round won on tasks.
  `RTC_OFF=all` fails every WebRTC link, so the game runs over the signalling fallback;
  `RTC_OFF=1` fails tab 1's links only.
- `node packages/js/test/netlab_rooms.mjs [shot-prefix]`: Net Lab's named rooms: a typed name opens the room id
  it shows and goes in the address bar as `#room=`, the room link brings another tab into it (linked, pinging),
  another name is another room, the room is listed under recent rooms, and the report never holds its name.
- `node packages/js/test/netlab.mjs [tabs=3] [shot-prefix]`: opens `demo/netlab.html` in phone-sized tabs
  joined by one `#key=` link: every tab links to every other, pings come back, chat arrives, the
  report holds every peer and not the room key, and the page doesn't scroll sideways. `RTC_OFF=<i>`
  fails tab i's WebRTC links: it is still reached, by fallback, and pinged.
- `node packages/js/test/room_fallback.mjs`: a fake RTCPeerConnection blocks chosen
  pairs. A blocked pair links through a third member, or over signalling when there is
  none (a 100 KB message arrives whole), and stays apart with `fallback: 'peers'`/`'off'`.
  1,000 proven peers that each send one datagram over signalling leave at most 256 pipes.
- `node packages/js/test/room_adopt.mjs`: `room.adoptChannel` with the same fake. A link
  over signalling, or relayed by a member, moves onto a channel both ends adopt: 100 KB
  arrives with nothing over signalling, or the member relays nothing more. When the
  channel closes, the link goes back to its fallback route and carries on. A channel
  only one end adopts is refused after 5 s, and a direct link isn't moved.
- `node packages/js/test/netlab_carry.mjs [shot-prefix]`: Net Lab in two tabs that
  link over signalling (`?rtc=off`). A real loopback channel between them, handed in as
  a birthday punch's winner, carries the room link: the route goes direct, the log
  gives the channel's address, and chat goes over it. When it closes, the link goes back
  to signalling. Net Lab's transport hook passes a closed link's handle on to the room.
- `node packages/js/test/netlab_mic.mjs [shot-prefix]`: Net Lab's "Use microphone" button, with fake
  media and the browser's mDNS hiding on. Before, the local addresses are `.local`; after a grant the
  report says `granted`, the network test lists real local addresses and reads the same NAT, the room
  is joined again, and the other tab sees `mic` and real local addresses and links direct. The global
  room, with the grant stored before the tabs open or given while they're in it, gives the other tab
  no local address and still links, and a birthday punch's offer carries no candidate. A refused
  prompt reports `denied` and doesn't rejoin.
- `node packages/js/test/readme.mjs [shot-prefix]`: runs the README chat above in two
  tabs and checks a message gets across.
- `node packages/js/test/poll.mjs [tabs] [shot.png]`: the pull-style demo in tabs;
  every tab must see every other, and a closed tab must leave.
- `node packages/js/test/leave.mjs [--runs=10] [--stress=N] [--crash=1]`: closes a streaming
  tab and times the others' peer-leave (under 5 s each run); a crashed tab may take the 30 s
  idle timeout. `--stress=N` adds N busy loops for a loaded host.
- `node packages/js/test/browser.mjs [tabs] [shot.png]`: mesh of tabs, RTT and throughput.
- `node packages/js/test/arena.mjs [players] [secs] [shot.png|-]`: the arena. `LOSS=0.1`
  drops datagrams. Checks that every island agrees on its lockstep state.
- `node packages/js/test/nostr.mjs`: two isolated browser profiles meet over public
  Nostr relays and connect.
