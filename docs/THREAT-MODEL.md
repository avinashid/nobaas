# Threat model

Scope of this document: `peerwire-core` (handshake, wire format, lanes, relay, NAT
traversal, STUN, invites, sealed messages), the C ABI (`peerwire-ffi`), the
JavaScript glue and transport (`packages/js`), and the Nostr / BroadcastChannel
signaling adapters. It records what Peerwire defends against, what it does not, and
the findings of the first security review (M5, NB-013).

Status: pre-1.0, no external audit. See [SECURITY.md](../SECURITY.md).

## What Peerwire protects

1. **Peer authentication.** A `PeerId` is an Ed25519 public key. A link is
   `Noise_IK_25519_ChaChaPoly_BLAKE2s`, whose static key is the X25519 form of
   that identity, and the initiator sends its Ed25519 PeerId inside HS1 bound to
   the handshake transcript. A completed handshake proves the remote holds the
   secret for the PeerId you dialled. No signaling relay, peer relay or NAT in
   the path can impersonate a peer or man-in-the-middle a link.
2. **Link confidentiality and integrity.** After the handshake, every data
   packet is AEAD-sealed (ChaCha20-Poly1305) under the session keys. A relay
   carrying a `Via` link sees only ciphertext.
3. **Replay of data packets.** Each link has a 2048-wide sliding nonce window
   (WireGuard-style); replayed or too-old data packets are dropped before
   decryption is trusted.
4. **Signaling payload secrecy.** Offers (which carry IP candidates) are sent as
   sealed messages (`Noise_X`): encrypted to the recipient PeerId and
   authenticating the sender's PeerId. A tampered or forged offer fails to open.

## Trust boundaries

| Boundary | Trusted? | Notes |
|---|---|---|
| Signaling relay (Nostr, BroadcastChannel) | No | May read, drop, reorder, replay or inject any signaling message. Cannot forge a sealed payload or impersonate a PeerId. |
| Peer relay (`FWD`/`RELAYED`) | No | Sees only ciphertext; rate-limited; forwards only to already-connected peers. |
| NAT / on-path network | No | Noise authenticates the endpoints; cannot MITM. Can drop/delay. |
| Remote peer | Partially | Authenticated, but may be malicious: send malformed frames, flood, probe. Parsers and resource caps are the defence. |
| Host application / C caller | Yes | The C ABI trusts its caller to pass valid pointers and lengths (see SEC-8). |

## Attacker capabilities considered

- Off-path attacker who can spoof source addresses but not observe replies.
- On-path attacker who observes and modifies datagrams.
- A malicious or curious signaling relay, or any party who learns a room name or
  a target PeerId.
- A connected, authenticated peer behaving maliciously.

Out of scope: compromise of the host OS or its CSPRNG; side channels in the
underlying crypto crates (`snow`, `ed25519-dalek`, `k256`); traffic analysis by a
global passive adversary; malicious application code above the API.

---

## Findings

Severity uses likelihood × impact for this software's threat model. Each finding
that needs a change is tracked as an NB card.

**Status as of main `7b716a2`, with the NB-044…NB-048 branches reviewed on 2026-10-23.**

| Finding | Status |
|---|---|
| SEC-1 (High) | **Fixed** in NB-021 (`ccc3b79`), with its review findings N21-1 and N21-2 |
| SEC-2 (Medium) | Narrowed by NB-031/NB-037: hellos need the room key; PeerId still readable by name holders |
| SEC-3 (Medium) | Fixed for rooms (NB-031 Rust, NB-037 JS); open for a bare `WebRtcTransport` |
| SEC-4…SEC-8 | Open (NB-024…NB-028) |
| SEC-9…SEC-11 | Met by NB-020/NB-030 and `docs/GODOT.md` (Security); NB-029 audit and N29-1 fix |
| SEC-12 (Info) | Accepted, no card |
| N38-1…N38-3 (port mapping) | **Fixed** in NB-038 (`71d83b2`) and NB-041 (`0dfdbc9`) |
| NPM-1 (Low) | **Fixed** by `4cffbf9` (merged `14f463a`): no port mapping under `RelayOnly` |
| NPM-2 (Low) | **Fixed** in `docs/GODOT.md` (`9be92d5`); the flood risk itself is SEC-4 |
| NPM-3 (Low) | **Fixed** by `6f947f3`: a map request in flight at stop gets one delete |
| NB44-1 (Medium) | **Fixed** by `40e42db` (per-IP punch cap), re-checked 2026-10-23 ([below](#hard-nat-punching-nb-044-nb-045-nb-046-nb-048--nb44-1nb48-2)) |
| NB48-1 (Medium) | Open: the Net Lab fleet, same section |
| NB44-2, NB44-3, NB48-2, NB46-1 (Low) | Open, same section |
| NB45-I1, NB44-I2, NB46-I1, NB44b-I1 (Info) | Optional hardening or accepted, same section |
| Earlier reviews' bounds and residuals | Recorded in [Bounds from reviews](#bounds-and-residuals-from-reviews-nb-014nb-043) |

### SEC-1 — Unvalidated peer-id length corrupts the wasm heap (High) — NB-021

**Status: fixed** in NB-021 (merged `ccc3b79`). Every fixed-size copy into wasm
memory checks its length first (`peerBytes`, `exact`), every variable-size one
takes bytes only (`bytesOf`), and the signaling adapters (`nostr.js`, `webrtc.js`,
`local.js`, `room.js`) drop a `from` that is not 64 lowercase hex characters. The
reproduction below now throws a `TypeError` and the module stays healthy
(`packages/js/test/peer_id.mjs`, in the gate). The C ABI peer-length argument is
a separate defence-in-depth card.

`packages/js/peerwire.js` `Endpoint.setPeer(id)` copies `id` into the fixed 32-byte
`peerBuf` with `mem().set(..., this.peerBuf)` and performs no length check;
`seal()` and `open()`/`put()` have the same shape. An over-long id writes past the
buffer into adjacent wasm linear memory, corrupting other endpoint state and the
allocator; the module then traps (`unreachable`) or behaves unpredictably.

This is reachable from untrusted input: the demos and `webrtc.js` pass a
signaling peer identifier (`from` of a Nostr/BroadcastChannel `hello`, or a `to`
for `seal`) straight into `dial()` → `seal()` → `setPeer()`. A hostile relay or
peer that publishes a `hello` with an over-long `from` can drive the overflow in
any client that auto-dials newcomers (as `demo/demo.js` and `demo/arena.js` do).

**Reproduction.** Load the wasm, call `ep.seal('ab'.repeat(4096), ...)`: the copy
runs past `peerBuf` and a subsequent `nb.endpoint()` allocation traps the module.

**Fix.** Validate length at the boundary before every wasm copy:
- `setPeer`/`seal`/`open`: require a 32-byte id (or a 64-hex-char string) and
  reject otherwise;
- in `nostr.js`/`webrtc.js`, validate that a signaling `from`/`to` is 64 lowercase
  hex chars before using it as a PeerId.
Defence in depth: the C ABI could take an explicit peer-length argument, but the
JS-side check is the necessary fix.

The review of the fix found two more unbounded inputs, both fixed in the same
merge:

- **N21-1 (Medium) — fixed.** `webrtc.js` passed DataChannel messages to
  `new Uint8Array(e.data)`. `binaryType = 'arraybuffer'` covers binary messages
  only, so a peer could send a *string* such as `"2147483648"`, which became a
  length: one 10-byte message made the tab allocate 2 GB and aborted the module
  in `pw_alloc` (`"100000000"` grew wasm memory to ~101 MB, never returned). It
  needed only an open DataChannel, before the Noise handshake. Now `onmessage`
  accepts only an `ArrayBuffer` of at most `MAX_DATAGRAM` (1232) bytes, and
  `Endpoint.receive` drops anything larger before allocating, which also covers
  `local.js` and custom transports.
- **N21-2 (Low) — fixed.** `nostr.js` decoded and opened event content of any
  size, so a hostile relay could push multi-MB events, each costing about twice
  its size in wasm memory. Content that is not a string or is over 64 KiB is now
  skipped before decoding.

### SEC-2 — Room `hello` discloses the long-term PeerId in cleartext (Medium) — NB-022

`nostr.js` `hello()` publishes `{ t:'hello', from:<full PeerId>, meta }` as plain
base64 under a tag derived only from the room name. Anyone who knows the room name
(the room tag is `sha256("peerwire/room/<room>")`, computable by anyone with the
name) can read every member's long-term Ed25519 PeerId, correlate it with the
ephemeral Nostr pubkey, the `created_at` timestamp and the relay set, and track
that identity across rooms and time. This defeats the "fresh key per session"
unlinkability the adapter otherwise aims for.

**Fix.** Don't broadcast the raw PeerId in a room-wide, unencrypted hello. Options,
in order of preference: (a) gossip presence only to peers already known
out-of-band and seal it; (b) include only an ephemeral, per-session handle in the
hello and exchange the real PeerId inside the sealed channel; (c) at minimum
document that room membership reveals every member's identity to anyone who knows
the room name, so room names must be treated as secrets.

**Status: narrowed, open (NB-022).** Since NB-031 (Rust) and NB-037 (JS) a room
key, separate from the topic, is derived from the name, and a hello is acted on
only with its HMAC proof, so someone without the name can no longer get dialled.
But the JS hello still carries `from` in plain base64, and the topic is an
unsalted hash of the name, so anyone who knows or guesses the name still reads
every member's PeerId. Option (c) is done: the docs say private room names need
at least 128 random bits (`randomRoomId()` in JS). Options (a)/(b) are not.

### SEC-3 — Sealed-offer probe reveals IP candidates to anyone who knows your PeerId (Medium) — NB-023

Because a valid offer only requires the target's PeerId (a public value), any
party who learns a PeerId can seal an offer to it over a public relay and receive
a WebRTC *answer* — which contains the target's ICE candidates (local and
server-reflexive IP addresses). The Noise handshake still protects against
impersonation, but the metadata (presence + IP) leaks before any mutual
authentication of intent. Combined with SEC-2, a passive observer of a room can
deanonymize every member's IP.

**Fix.** Treat an inbound offer as a request, not a command: rate-limit answers
per source, and gate candidate disclosure on some proof the peer belongs in the
room (a room-scoped shared secret, or an allow-list the app supplies). Document
the residual "known PeerId ⇒ discoverable IP" exposure for apps that publish
PeerIds widely.

**Status: fixed for rooms, open elsewhere (NB-023).** In Rust (NB-031), room
offers are acted on only with a verified room-key MAC, and a plain offer only
from a peer we invited. In JS (NB-037), `joinRoom` gives `WebRtcTransport` an
`offerProof`, and an offer without a valid room proof gets no answer, so no
candidates. A `WebRtcTransport` built directly without `offerProof` still answers
any sealed offer, so the finding stays open for that use.

### SEC-4 — Half-open handshake table exhaustion (Medium) — NB-024

`Config::max_pending_handshakes` (default 4096) caps half-open responder links,
and the cap is checked before the expensive Noise build, so CPU per flood packet
is bounded once the table is full and memory is bounded (~a few MB, entries expire
after `handshake_timeout`, 10 s). However, there is no address-validation cookie:
an attacker sending ~4096 HS1s per 10 s with spoofed or varied `(sender_idx,
ephemeral)` keeps the table saturated, so legitimate inbound handshakes are
dropped for the duration — a connection-establishment denial of service against a
public/always-on peer. HS2 is roughly HS1-sized, so reflection amplification is
~1×.

**Fix (hardening).** Add a stateless retry/cookie step (QUIC Retry / WireGuard
cookie style): on the first HS1 from an unvalidated address, reply with a keyed
cookie and allocate no state until the address echoes it. This moves the cost to
address-spoofers and keeps the table for validated peers. Until then, operators of
always-on peers should size `max_pending_handshakes` and `handshake_timeout` for
their exposure.

### SEC-5 — Deterministic, unsalted Nostr peer tag enables presence correlation (Low–Medium) — NB-025

The per-peer tag is `sha256("peerwire/peer/<PeerId>")`, a fixed function of the
public PeerId. Anyone who knows a target's PeerId can compute the tag and
subscribe to it on the public relays to learn whenever that identity is addressed,
and to correlate activity across sessions and rooms. (SEC-2 hands out the PeerIds;
even without it, PeerIds are shared in invites.)

**Fix.** Derive the routing tag from a rotating, room- or epoch-scoped secret
rather than from the bare PeerId, so the tag is unlinkable to anyone who only
knows the public PeerId. Accept that two peers must share that secret (they share
a room already).

### SEC-6 — Sealed signaling messages have no replay protection (Low) — NB-026

`sealed::open` intentionally provides no anti-replay (documented in `sealed.rs`:
"callers that care put a timestamp or nonce inside"). `nostr.js` adds none beyond
a per-session `seen` set of event ids, which does not survive a reconnect and does
not stop a relay from re-delivering an old sealed offer with a fresh event id. An
observer can replay a captured sealed offer later; the effect is limited (it
triggers a redundant answer / ICE churn, feeding SEC-3) but non-zero.

**Fix.** Put a `created_at`/nonce inside the sealed body and reject stale or
duplicate (sender, nonce) at the adapter. The Nostr `created_at` is unauthenticated
(it is outside the sealed blob) and must not be trusted for this.

### SEC-7 — No in-session rekeying / forward secrecy refresh (Low) — NB-027

A link keeps one set of transport keys for its whole lifetime (nonce is a u64
counter; exhaustion is not a practical concern). There is no periodic rekey, so
compromise of a live session's keys exposes all traffic on that session, past
(if recorded) and future, until it is torn down. A reconnect performs a fresh
handshake with new ephemeral keys, which bounds exposure in practice.

**Fix (future).** Offer an optional rekey (new ephemeral DH) on a byte/time
threshold for long-lived sessions. Document the current property meanwhile.

### SEC-8 — C ABI trusts caller pointers and lengths (Low, by contract) — NB-028

Several `pw_*` functions dereference the endpoint handle and in/out buffers
without null or length checks (e.g. `pw_secret`, `pw_send`, `pw_stats`), and
`bytes(ptr, len)` with a null `ptr` and non-zero `len` is undefined behaviour.
This is the documented C-ABI contract and the JS glue always upholds it, but a
native (Dart/Swift/C) caller that violates it gets memory unsafety rather than an
error code.

**Fix.** Document the contract explicitly in the header/readme, and add null
checks on the handle and on buffers where `len > 0` as cheap defence in depth.

### SEC-12 — An invite holder can confirm a guessed link by naming it as a relay (Info) — no card

Since NB-033 an invite holder the node never dialled learns none of its linked
PeerIds. But relay fallback dials a waiting sender `Via` every relay it listed
that the node is linked to, when the offer arrives (`on_offer`) or when that link
comes up later (`handle_event`, `Connected`). So a holder can list a guessed
PeerId X as a relay: if a relayed handshake from the node arrives through X, the
node is linked to X.

This predates NB-033 (it only extends the check to links that come up later) and
is inherent to relay fallback. It is bounded: at most 8 guesses (`MAX_RELAYS`) per
offer acted on, under the offer rate limits; each guess answers only "linked to X
or not" and enumerates nothing; and X must actually relay to the holder, so X has
to be linked to it. No fix planned; a stronger design (naming only links that
agreed to relay) would be a separate card.

### Router port mapping (NB-038, Godot switch) — N38-1…N38-3, NPM-1…NPM-3

`Builder::port_mapping(true)` (Godot: `set_port_mapping(true)`), off by default,
asks the LAN gateway (PCP, then NAT-PMP, then UPnP-IGD) to forward the node's UDP
port. The mapped address is offered like a public one. This changes the attack
surface in three ways:

- **Unsolicited reachability.** While mapped, anyone on the internet can send to
  the port, with no NAT filtering in front. It reaches only the Noise endpoint,
  but that endpoint has no handshake-flood cookies yet, so SEC-4 applies directly.
  It also shows up in port scans. `docs/GODOT.md` says this and advises asking
  the player first (NPM-2).
- **The gateway and the LAN are trusted for the address only.** Any device on the
  LAN can answer the requests. A rogue responder can report a false address,
  which peers would then dial. It can't get into a session: no one finishes a
  handshake without the peer's key. Accepted residual.
- **Leftover mappings.** After a crash the router keeps the mapping until its
  lease ends (2 h), or until someone deletes it if the router only allows
  permanent UPnP leases. Documented.

Findings:

- **N38-1 (fixed, NB-038 `5b52eb5`).** A dripping or stalled UPnP gateway could
  hold the mapping thread and the delete on drop. Now each HTTP exchange has one
  3 s deadline, and drop waits at most 7 s.
- **N38-2 (fixed, NB-038 `5b52eb5`).** A PCP or NAT-PMP answer with lifetime 0
  was taken as a permanent mapping; it is now a refusal.
- **N38-3 (fixed, NB-041 `bf769e9`).** A UPnP chain stopped half-way kept
  sending; the worker now checks stop before each HTTP exchange.
- **NPM-1 (Low, fixed by `4cffbf9`, merged `14f463a`).** Under `Privacy::RelayOnly` the mapped
  address is never given out, yet the port was still opened. Now a relay-only
  node asks the router nothing (state `RelayOnly`). Switching to relay-only
  deletes the mapping without blocking, and switching out maps again after the
  old delete has gone out. A Godot relay-only host still maps, because it gives
  out its public addresses as under `PublicOnly`.
- **NPM-2 (Low, fixed in `docs/GODOT.md`, `9be92d5`).** The doc said the port
  "reaches only the Noise endpoint" without the SEC-4 flood caveat.
- **NPM-3 (Low, fixed by `6f947f3`).** `Mapper::stop()` deleted only a lease it
  held. A PCP or NAT-PMP map request still unanswered when the worker stopped
  (drop right after bind, or `set_privacy(RelayOnly)` right after a map request)
  got no delete, so the router's late grant stayed open for its lease (2 h). Now
  a stop with no lease and a map request out sends one lifetime-0 delete: PCP
  with the same nonce and no suggested address (RFC 6887 §15), NAT-PMP with
  external port 0 (RFC 6886 §3.4). With a lease held, only `release()` deletes.

### Hard-NAT punching (NB-044, NB-045, NB-046, NB-048) — NB44-1…NB48-2

Reviewed on 2026-10-23: native `backend/NB-045-lockstep-prediction` (`3a6c8b8`,
NB-044 birthday punch and NB-045 lockstep) and browser
`ui/NB-048-browser-birthday` (`5ee351e`, NB-046 ICE restart and port prediction,
NB-047 Net Lab rooms, NB-048 browser birthday punch). Mechanisms and costs are in
`docs/NAT-TRAVERSAL.md` (Birthday punching, Lockstep prediction, Reflection).

These features make a node send many packets to an address a remote party names.
The security question is who can name that address and how much traffic follows.

| Trigger | Who can trigger it | Traffic per try | Limits |
|---|---|---|---|
| Native spray (an offer saying `Symmetric` with no prediction, to a cone node) | A room member, an invite holder, or a peer the node dialled | Up to 2048 HS1s of 133 B to ports ≥ 1024 of the offer's first address (public IPv4): 272,916 B over 3.8 s | 5 min per peer; per node, a burst of 4, then 1 per 30 s |
| Native open (an offer saying `Cone`, to a random-port symmetric node) | Same | 256 sockets for 10 s, each sending 1 HS1 to the offer's address (about 34 KB) | Same budget; at most 512 extra sockets open at once |
| Native lockstep sweep (both sides predict) | Same | Up to 96 HS1s (about 13 KB) to predicted ports of the offer's IP, and up to 96 of the node's own NAT ports | Same budget |
| Browser port guesses (NB-046) | A linked or dialling WebRTC peer | 32 extra remote candidates per new `{ ip, next, step }` | Chrome's ICE pacing (about 16 checks/s per connection) |
| Browser fleet and spray (NB-048, Net Lab only) | Any member of the Net Lab room, including the open global room | Fleet: 256 connections checking every candidate in the peer's offer. Spray: 1024 checks to the IP the peer names | 70 s per run, no cooldown |

The core stayed sans-IO and deterministic: `Endpoint::spray`, `sweep` and `opener`
use the endpoint's seeded RNG and the `now` the host passes in. Sockets, the 5 ms
poll and routing live in `peerwire::Node`. `fuzz.rs` checks that a spray visits each
port at most once and respects `MAX_SPRAY`.

**NB44-1 — The birthday spray is a reflection vector with about 270× byte
amplification (Medium).** A sealed offer costs the sender well under 1 KB. When the
node acts on it, the node sends 272,916 B to ports of an IP the sender chose. The IP
only has to be public IPv4 and dialable, and it isn't checked against anything the
peer proved. The 5-minute cooldown is per PeerId, and a room-key holder can make up
any number of PeerIds, so only the node budget limits it: 4 sprays, all at one IP if
the sender wishes.

- Repro (unit test on the branch, `punch_case`): 4 offers from 4 PeerIds naming
  `198.51.100.9` make one cone node send 8,208 datagrams (1,091,664 B) to that IP
  within 6 s; 8 PeerIds send no more (budget).
- Sustained, the limit is about 9 KB/s per node, towards any victims. Every cone
  member of a public room can be recruited, one offer each.
- Fix: a cooldown per destination IP across all PeerIds (for example, one planned
  punch per IP at a time and at most 2 per 5 min), so all four sprays can't land on
  one victim. Correct the "no amplification beyond the count" line in
  NAT-TRAVERSAL.md. Test: 4 PeerIds offering one IP start 1 spray.
- **Status: fixed** by `40e42db` (NB-044b). `start_plans` allows one punch per
  destination IP at a time (20 s) and at most 2 per 5 min across all PeerIds. The
  check runs before the node token, and covers spray, sweep and open. Re-check probe:
  4 or 30 PeerIds offering one IP every 1 s or every 20 s for 15 min. Per 5-min
  window, that IP got at most 4,096 spray probes (2 sprays, 545,832 B) from a cone
  node, or 2 fleets (512 HS1s) from a symmetric one. Before the fix it was about 14
  sprays (3.8 MB) per 5 min.
- Residual: the cap is per node. The first spray is still about 270× the offer, and
  N room members recruited with one offer each still reflect N times the
  per-node bound at one victim. Recruiting needs the room key or an invite.

**NB44b-I1 — The per-IP cap can be spent by a room member (Info, accepted).** A
member can name a real peer's public IP in offers from minted PeerIds, which uses
up that IP's 2 slots. Birthday punches towards it then stop for 5 min and fall back
to the relay. Two genuine peers behind one carrier-NAT IP get at most 2 punches per
5 min between them. A plan the cap blocks is dropped, not deferred, until that
peer's next offer. These are availability costs only, and the cap is worth them.

**NB44-2 — Extra sockets take any sender as a route, before authentication
(Low).** `Node::read_extra` makes the first extra socket that hears from an address
that address's route, and refreshes the socket's `last_recv` on every datagram. It
does this before `Endpoint::handle_datagram` has authenticated anything.

- Repro (node test on the branch): junk from about 57,000 loopback source ports to
  one extra socket left 24,496 entries in `routes`. The socket stayed open 1.2 s past
  `until`, and stays open for as long as junk keeps arriving.
- Who can reach an extra socket:
  - the offer's address, through a port-restricted NAT;
  - any host on the LAN;
  - the internet, if the node is bound dual-stack on a host with global IPv6 and
    no inbound firewall (the ports can be found by a UDP scan).
- A sender that can spoof the source address of a peer the node already talks to
  can move the node's egress to that peer onto the extra socket, which breaks the
  path through the peer's NAT.
- The punch target alone gets an HS1 from every socket's NAT mapping. One datagram
  per socket per minute (under `ROUTE_IDLE`) then keeps all of them open
  indefinitely: up to 512 fds (NB44-3) and the 5 ms poll (NB44-I2) for as long as it
  likes. This is why NB44-2 should land before native builds ship.
- Fix: set a route only for an address the endpoint has accepted, that is a session
  path or dial target equal to `from` after `handle_datagram`. Refresh `last_recv`
  only from that address, and cap `routes` at `MAX_EXTRA_SOCKETS`. Test: junk from N
  ports leaves `routes` empty, and the sockets close at `until`.

**NB44-3 — Local resources a remote peer can spend (Low).**
- **File descriptors.** An offer saying `Cone` makes a random-port symmetric node
  open 256 sockets for 10 s, up to 512 at once. The default soft `RLIMIT_NOFILE` is
  256 for macOS apps (the launchd default) and 1024 on Linux desktops.
  `open_extra` stops at the first failed bind, so the process can sit at its limit
  for 10 s: other opens in the host
  fail with `EMFILE` (a Godot resource load, the Nostr websocket reconnect), and the
  punch silently opens fewer sockets.
- **Ports.** Each try takes 256 ports on a carrier NAT, and many carrier NATs give a
  subscriber only 512 to 2048. The NAT keeps them for its UDP timeout after the
  sockets close.
- **Connection tracking.** A spray adds up to 2048 entries to the cone side's
  router (already in NAT-TRAVERSAL.md).
- Fix:
  - On the first failed bind, close a reserve (for example 64 of the sockets just
    opened).
  - Run one open at a time (256).
  - Give open plans their own budget of 1 per 2 min, longer than common UDP mapping
    timeouts.
  - The NB44-1 per-IP cooldown covers repeats.
  - Document in GODOT.md that macOS hosts (and mobile ones, check the limit) should
    raise `RLIMIT_NOFILE`.
- Found by reading the code; no probe was run.

**NB45-I1 — Sweep ports can be below 1024 (Info).** The sender's prediction (`next`,
`step`) chooses the ports of a lockstep sweep and of port guesses. Unlike the spray,
they aren't kept at 1024 and up, so 96 HS1s can reach well-known ports (53, 123,
161) of the offered IP. An HS1 is not a valid request for those protocols, so
nothing is amplified. Fix (optional): filter ports below 1024 as the spray does.

**NB44-I2 — The 5 ms wake-up lasts as long as a punched link (Info, not
security).** `Node::next_timeout` is capped at 5 ms while any extra socket is open,
and a route socket stays open while its link lives, so a link made by a birthday
punch keeps the host waking 200 times a second. This costs battery on phones.
Suggested: wait on the extra sockets' readiness, or poll less often once only routes
are left.

**NB48-1 — Net Lab: any room member can make a visitor's browser run a 256-connection
fleet against addresses of the member's choosing, with no click, repeatedly
(Medium).** The trigger is two messages: `{t:'bday', step:'start',
yourRole:'hard'}`, then `{t:'bday', step:'offer', sdp}`. With them, `onBirthday`
generates a certificate and calls `openFleet` with `FLEET_SIZE` (256), applying the
attacker's offer, candidates included, to every connection. Every connection then
runs ICE checks to every candidate in that offer.

- Repro (`packages/js/test/sec_fleet_probe.mjs`, Chromium, loopback; scratch only):
  an offer with 20 injected candidates drew 16 STUN checks/s from 1 connection,
  515/s from 32 and 2,095/s from 128. That scales linearly, so about 4,000/s from
  256, for the 70 s timeout. The candidates can name any IP, including the
  visitor's LAN.
- There is no cooldown: `step:'abort'` closes the fleet, and a new `start` opens
  another at once. Each round also costs the visitor 256 connections and a key
  generation, which is heavy on a phone.
- The global room is open to anyone who loads Net Lab.
- Fix:
  - Library: `openFleet` strips the offer's candidates and takes one validated
    public IPv4 address for the easy side, added to each connection with
    `addIceCandidate`, so the fleet checks one address, as the native open does.
    Cap `size` at `FLEET_SIZE`.
  - Net Lab: a remote `start` runs only after a local click. One fleet at a time,
    one per peer per 5 min, and `abort` doesn't reset the wait.
  - Test: an offer with N candidates leaves at most one remote candidate on each
    fleet connection.

**NB48-2 — The easy side sprays whatever IPv4 address the peer names (Low).** In Net
Lab's `answer` step, `data.ip` goes to `applyAnswer` after `sprayCandidates` checks
only its format. That sends 1024 ICE checks, with retransmits, at about 16/s to
ports 1024 and up of any IPv4 address, including the visitor's own router or LAN
hosts, or loopback. Fix: accept only a public IPv4 address (not RFC 1918, loopback,
100.64/10, link-local or multicast) that matches a server-reflexive address in the
peer's own signalling.

**NB46-1 — A peer can keep the dialling side publishing offers (Low).** A `restart`
signal makes the dialling side create and publish an ICE-restart offer, resent up to
twice. `MAX_RESTARTS` (3) starts over every time ICE reconnects. So a peer that
answers each restart and lets ICE reconnect can make us publish about 3 offers
(1–2 KB of SDP each) per round, without end.

Independently, and already true on main, every distinct offer from a linked peer is
answered, and every copy of the last offer gets the answer resent, with no per-peer
limit. The publishes land on our Nostr relays, which rate-limit and ban busy IPs:
relay.damus.io banned a shared hotspot IP in testing (see `86119f4`). Losing the
relays cuts signalling for the whole room.

Fix: honour `restart` only while our own ICE is not `connected` or `completed`, and
allow at most 3 restarts per minute per link whether or not ICE reconnects. Put the
answers and resends we publish to one peer under a budget (for example a burst of 6,
then 1 per 5 s). Found by reading the code; no probe was run.

**NB46-I1 — Port guesses aim at any IPv4 address (Info).** `checkPrediction` checks
only the format of `nat.ip`. It doesn't have to be the peer's server-reflexive IP, or
public. Each new `{ ip, next, step }` adds 32 candidates, and every `ice-batch` can
carry a new one. Remote candidates per link have no total cap, only 32 per message
(this predates NB-046). The bound is Chrome's ICE pacing, which is WebRTC's own
security model for remote candidates (RFC 8826), but the cost is cheap to tighten.
Fix: require `nat.ip` to match one of the peer's server-reflexive addresses, take one
prediction per description, and cap remote candidates per link (for example at 128).

### Bounds and residuals from reviews (NB-014…NB-043)

This section records what each earlier review established about resource bounds,
and what it accepted. Values are from main `7b716a2`.

**Core sessions**
- **Receive memory per link.**
  - Reliable lane: `RECV_WINDOW` 1024 fragments of `FRAGMENT` 1100 B.
  - Urgent lane: `URGENT_RECV_WINDOW` 64 fragments, with messages capped at
    `MAX_URGENT` (64 KiB).
  - Fragments longer than `FRAGMENT`, and reassembly past `max_message` (1 MiB by
    default), are refused.
  - The bound tested in `fuzz.rs` is about 2.31 MB per link (2.17 MB reliable, 0.13
    MB urgent). `docs/DOCKER.md` sizes `PEERWIRE_MAX_PEERS` from it: 256 peers can pin
    about 0.6 GB.
- **Retransmission.**
  - RTO is 20 ms to 5 s; backoff `rto·2^min(tries-1, 8)`, at most 10 s.
  - After `MAX_TRIES` (16) the link is dead.
  - ACKs report a 64-fragment SACK bitmap. Since PERF-10, a fragment beyond the
    bitmap's reach doesn't time out while the hole below it is repaired.
- **Congestion control.**
  - A loss halves the window (`LOSS_CUT`).
  - Queueing delay above 25 ms cuts it to 0.85 (`DELAY_CUT`).
  - A loss with under 5 ms of queueing cuts it to 0.8 (`RANDOM_LOSS_CUT`, Veno);
    PERF-10 deepens that to 0.5 on a timeout in the same round trip.
  - The minimum window is `MIN_CWND` (4).
  - Congestion state is per link, so a peer that games it affects only its own link.
- **Links.**
  - `max_links` is unlimited by default, and the `peerwire-peer` server sets it to
    256.
  - Half-open handshakes don't count towards `max_links`.
  - An HS1 is refused before the DH when half-open handshakes reach
    `max_pending_handshakes` (4096) or links reach `max_links`.
  - An unannounced responder past the cap is dropped at its first data packet.
  - Handshake floods are still SEC-4 (cookies, NB-024).
- **Relay.**
  - 256 KiB/s with a 64 KiB burst per peer, and a 256-packet queue.
  - The total (`relay_total_rate`) is unlimited by default, and `peerwire-peer`
    sets it to 8 MiB/s.

**JS endpoint and rooms**
- **Lifetime.**
  - `leave()` sends `Bye` to each linked peer on the unreliable lane, then
    disconnects them.
  - On `pagehide` it waits at most 20 ms for that.
  - A received `Bye` is inside the Noise session, so it only ever drops the
    sender's own link. Rooms accept it before the room proof, which is harmless for
    the same reason.
- **Sends.** `sendRetry` keeps at most one message above the 16 KiB scratch buffer
  between tries. The room's outbox has no cap. It holds only what the local
  application sends, so that is application policy.
- **Signals.**
  - Malformed or unproven signals are counted in `badSignals` (a counter only, with
    no action).
  - An `ice` or `ice-batch` message holds at most 32 candidates, and at most 32 are
    held before the description.
  - Residuals:
    - No total cap on remote candidates per link (NB46-I1).
    - No cap on signals held while an offer's proof is checked (one async check, so
      the window is short).
- **Fallback pipes (NB-043).**
  - `MAX_PIPE_PEERS` 256, evicted oldest first. Since `a283a01` an evicted permission
    drops its pipe unless a session uses it (NB43-1).
  - Outgoing pipes share 3 messages/s (burst 6), in batches of 16 KiB.
  - Each pipe's queue is 64 KiB, dropping the oldest.
  - Incoming batches are capped at 64 items but not rate-limited.
  - Residuals:
    - **NB43-I1:** a relaying member can roam a link onto its own pipe, because a
      direct-to-direct roam needs no path validation.
    - **NB43-I2:** an insider holding the room key can churn the pipe permissions,
      take the shared budget, and send 64 HS1s per batch. Card NB-043b; it supports
      SEC-4.
    - **NB43-I3:** the Nostr backoff never decayed; fixed in `a283a01`.
    - **NB43-I4:** relays see session-long timing and volume of piped links, and
      every direct member learns that a pair failed WebRTC.

**Signalling**
- **JS Nostr.**
  - Content is capped at 64 KiB.
  - 4096 event ids are remembered (oldest forgotten), and each must be 64 hex
    characters.
  - A relay that rate-limits us is paused from 30 s, doubling to 10 min, and the
    pause starts over after 10 quiet minutes.
  - Closed sockets are redialled from 1 s, doubling to 30 s.
  - Residual: JS checks neither the event id hash nor the signature, so a relay can
    present one event under many ids. Offers are sealed and hellos proven, and
    replays fall under SEC-6, so the cost is duplicate work. Rust checks the id.
- **Rust Nostr.**
  - Websocket messages and frames are capped at 128 KiB.
  - The inbox holds 1024 events, then drops.
  - 4096 event ids are remembered.
  - At most 64 publishes are queued while a relay is down.

**Addresses we give out**
- **NB-041.** Offers to a relay peer name no links. Residual: under `Direct`, a relay
  peer sees our LAN addresses; `PublicOnly` hides them.
- **NB-042.**
  - **NB42-I1 and NB42-I2:** mapped `::ffff:` addresses compare as IPv4, and NAT64
    `64:ff9b::/96` is judged by its IPv4; fixed in NB-042b (`1529b54`).
  - **NB42-I3:** Godot always binds `[::]`, so a host with global IPv6 and no inbound
    firewall is reachable unsolicited. That makes SEC-4 more urgent.
  - **Residual NB42b-I1:** `is_public` doesn't unwrap NAT64, so a NAT64 address
    embedding a private IPv4 still gets a guess window.
  - **Residual NB42b-I2:** the local-use prefix `64:ff9b:1::/48` isn't covered.
- **`73aa866` (private interface addresses).** A socket bound to any interface also
  offers up to 3 RFC 1918 interface addresses. Container and VM bridges are skipped
  by name, but VPN tunnels (`wg*`, `tun*`, `utun*`, `ppp*`) are not. So under
  `Direct` a peer can learn, for example, a corporate VPN address. Suggested: skip
  those interface names too.

**Demos**
- No CSP is sent by `crew-server.mjs`, which serves Net Lab and Crew. Only the Docker
  examples' nginx config sets one (`script-src 'self' 'wasm-unsafe-eval'`,
  `connect-src 'self' wss:`, `frame-ancestors 'none'`). Net Lab escapes everything a
  peer sends before it reaches `innerHTML` (checked for NB-047 and NB-048), so a CSP
  is defence in depth. Suggested: send the same CSP from `crew-server.mjs`.
- Net Lab caps its event log at 500, the DOM log at 300 and chat at 200. The
  per-peer route history has no cap.

---

## Invite strings (`pw:`)

An invite (`crates/peerwire-core/src/invite.rs`) is `pw:` + unpadded base64url of
`version | peer(32) | addrs | relays`. It carries **no secret**: the PeerId is a
public key and the addresses are routing hints. Security properties:

- **Confidentiality: none, by design.** Anyone who sees an invite learns the
  PeerId and candidate IP addresses of the inviter. An invite posted publicly is
  a standing version of SEC-3 (IP disclosure) and SEC-2 (identity disclosure):
  treat invite links as semi-private, and prefer LAN/relay candidates over a
  home IP when sharing broadly.
- **Integrity: not self-authenticating, but safe.** An invite is unsigned, so a
  carrier can swap the addresses or relays. This does not break authentication:
  the handshake still runs Noise IK to the PeerId in the invite, so a swapped
  address only reaches the real peer or fails. A carrier that swaps the *PeerId*
  produces an invite to a different identity — the recipient connects to whoever
  the carrier chose, exactly as if the carrier had sent its own invite. Invites
  therefore inherit the trust of the channel they arrive on; a QR shown in person
  or a sealed signaling message is trustworthy, a plaintext forum post is not.
- **Parsing is safe.** `from_bytes` is a bounds-checked reader, refuses more than
  16 entries, rejects trailing bytes, and validates each PeerId is a real Ed25519 point.
  Reviewed, no issues; covered going forward by fuzzing (NB-014).
- **Canonical.** Each invite has exactly one link: base64url decoding refuses a last
  symbol with its unused low bits set, and a count over 16 is refused rather than
  clamped. Nothing keys on the link string today, but a future dedupe, cache or
  allow-list on it cannot be bypassed with a lookalike. `fuzz_invites` asserts
  `to_link(parse(s)) == s` for every accepted `s`.

No new finding; the exposure is documented so apps set expectations for where an
invite may be posted.

## Godot binding (NB-020, planned — security requirements)

The coming `bindings/godot` crate exposes `PeerwireMultiplayerPeer`
(`MultiplayerPeerExtension`) so Godot's high-level multiplayer — `@rpc`,
`MultiplayerSynchronizer`, `MultiplayerSpawner` — runs over Peerwire. Peerwire
delivers *authenticated, confidential bytes* to that layer, but everything Godot
does *above* the `MultiplayerPeer` is outside Peerwire's guarantees. This section
sets the requirements the binding must meet; they are tracked as SEC-9…SEC-11
(proposed follow-ups on NB-020, owner @peerwire-backend).

### SEC-9 — Untrusted packets crossing into GDScript (High, design) — NB-029

Godot's high-level multiplayer deserializes incoming packets into Variants and
dispatches `@rpc` methods by name. Variant decoding of `Object`/`Callable`/
`RID` from the network is a known remote-code-execution surface (object
instantiation, resource loading). Because Peerwire is pure P2P, **every connected
peer is a remote attacker** at this layer.

**Requirements.**
- The binding must configure the `SceneMultiplayer` with
  `allow_object_decoding = false` (Godot's default, but assert it) so peers
  cannot ship encoded `Object`s.
- Document loudly that `@rpc("any_peer")` methods receive attacker-controlled
  arguments and must validate them and check `multiplayer.get_remote_sender_id()`.
- Surface Peerwire's per-lane size caps to the `MultiplayerPeer` so an oversized or
  malformed Godot packet is rejected at the boundary, not deep in a decoder.
- A Rust panic in the cdylib must not unwind into Godot: catch/abort at the FFI
  edge (godot-rust convention), so a malformed packet can't turn a panic into UB.

### SEC-10 — Godot peer-ID spoofing and mapping collisions (High, design) — NB-029

Godot peer IDs are 32-bit ints; Peerwire PeerIds are 256-bit Ed25519 keys. The
binding derives a stable non-zero Godot ID from each PeerId. A 32-bit space makes
collisions reachable (birthday bound ~2^16 peers, but an attacker can *grind*
PeerIds offline to target a specific victim's Godot ID).

**Requirements.**
- The PeerId → Godot-ID map is authoritative per session and **collisions must be
  rejected**, not silently merged: a joiner whose derived ID clashes with a live
  peer is renumbered or refused, never allowed to inherit another peer's ID.
- `_get_packet_peer()` must return the Godot ID of the *authenticated Peerwire
  source* of that packet only; it must never be derivable from packet contents an
  attacker controls.
- Host is Godot ID 1: the binding must bind ID 1 to the actual host identity and
  reject any joiner that tries to present as 1.
- Grinding resistance: either widen the effective namespace (keep the full PeerId
  as the identity and treat the 32-bit ID as a non-security-bearing handle the app
  must not use for authorization), or document that `is_multiplayer_authority()`
  and peer-ID checks are **not** a security boundary — the PeerId is.

### SEC-11 — Host authority over RPCs in a serverless mesh (Medium, design) — NB-029

Godot's model assumes peer 1 is a trusted server/authority. Peerwire has no
inherent authority: a peer can send any packet to any peer, and
`MultiplayerSpawner`/`MultiplayerSynchronizer` let an "authority" spawn/despawn
nodes and set properties on others' scene trees. A malicious peer that the app
treats as an authority corrupts every client.

**Requirements.**
- `docs/GODOT.md` must state that Godot's authority model is only as trustworthy
  as the app's topology; in an open mesh, `@rpc("authority")` is not a security
  control.
- Recommend a pattern: designate the host as the only synchronizer/spawner
  authority and have it validate inputs, or use `peerwire-game`'s deterministic
  lockstep (where every peer recomputes state and desync is detected) instead of
  trusting an authority's state pushes.
- The binding should not auto-relay packets between peers on behalf of the Godot
  layer unless the app opts in (Peerwire relay is already gated and rate-limited;
  don't bypass it).

**Status of SEC-9…SEC-11 as of `ccc3b79`.** The binding shipped (NB-020) and was
hardened (NB-030). `docs/GODOT.md` (Security) now states each requirement: keep
`allow_object_decoding` off (the demo asserts it), treat `@rpc("any_peer")`
arguments as attacker-controlled, ids are labels (the host renumbers collisions,
only the dialled host is 1, authorize by `get_peer_key`), and Godot's authority
is only as trustworthy as the peer made authority (use the host, or lockstep on
`peerwire-game`). SEC-11 is a design property: it stays a documented residual, not
a bug. NB-037's room-key proof does not change it. It only limits who can join a
room to holders of the room key, and any of them can still be malicious. Godot's
`server_relay` is on by default; `docs/GODOT.md` names its cost, how to turn it
off, and that a relayed packet's sender id is the host's word (the host can forge
it).

**NB-029 audit (`72c7e57`, `bindings/godot/src/{peer,session}.rs`).** Each
requirement against the code:

- SEC-9: the demo asserts `allow_object_decoding` is off (`main.gd`).
  `get_max_packet_size()` is the current mode's cap, `put` refuses larger, and
  received packets wait in a queue capped at 4096 packets and 16 MiB. gdext 0.5.5
  runs every `#[func]` and virtual method under `catch_unwind`
  (`godot-core::private::handle_panic`), so a panic is logged as a Godot error
  and never unwinds into the engine; no panic is reachable from peer input in
  `session.rs`.
- SEC-10: `derive_id` never yields 0 or 1, and `IdMap::assign` moves a colliding
  key to the next derived id (`colliding_ids_are_moved`). A packet's peer is
  looked up from the authenticated Peerwire sender; a frame from a peer with no id
  is dropped. A joiner takes ASSIGN only from the host it dialled, only once,
  and never 1; a host ignores ASSIGN (`spoofed_frames_are_dropped`).
- SEC-11: documented, as above; nothing in the binding forwards packets itself.
- **N29-1 (Low) — fixed.** The data frame's mode byte is the sender's word and was
  not checked against the lane the message arrived on: a peer could label
  unreliable-lane packets `RELIABLE`, and `get_packet_mode()` reported a
  guarantee the packet never had. Repro: `on_message(host, Lane::Unreliable,
  encode_data(Reliable, ..))` queued a `RELIABLE` packet. Fix: a frame whose
  `mode.lane()` differs from the delivering lane is dropped
  (`spoofed_frames_are_dropped`, which fails with the check removed).
- **Residual.** An admitted joiner can fill the 4096-packet queue every frame,
  and Godot decodes everything it is handed: a CPU cost bounded by `max_peers`
  and each link's bandwidth, not a memory one. The host's remedy is
  `disconnect_peer` (and a ban by key); per-peer rate limits are app policy.

## Relation to the Protocol Zero red-team notes

Peerwire was bootstrapped from the Protocol Zero SDK, whose adversarial notes
(`p0/protocol-zero/REDTEAM.md`) cover a related dead-drop/signaling stack. The
mappings that carry over and are already handled in Peerwire:

- **A1 sender forgery → handled.** Peerwire signaling uses the authenticated sealed
  form (`Noise_X`, sender PeerId bound), the equivalent of P0's
  `sealAuthenticated`. A forged/anonymous seal fails to open under a claimed
  identity (`sealed.rs` test).
- **A8 packet injection/replay → handled.** AEAD per data packet + the 2048 nonce
  window; nonce recorded only after authentication (SEC/THREAT items above).
- **A7 stale/replayed signaling → partially: SEC-6.** Peerwire has no in-band
  signaling-replay guard; carry P0's lesson (bind a fresh session id) into the fix.
- **A11 metadata / A12 forward secrecy → OPEN here too: SEC-2/3/5 and SEC-7.** The
  same open problems (relay-visible timing/IP, static-key exposure) apply; Peerwire's
  per-session Nostr key and rotating-tag ideas mirror P0's mitigations.
- **A5 spam / A13 Sybil → relevant to the signaling layer**; the half-open cap
  (SEC-4) and relay rate limiting are the Peerwire analogues. A PoW-gated first
  contact (as in P0 A5) is a candidate hardening if public rooms are abused.

## Residual risks accepted for v0.1

- Traffic analysis by a global passive adversary (packet sizes/timing) is not
  addressed.
- Metadata at the signaling layer (who talks to whom, when) is minimized but not
  hidden from a relay that correlates tags and timing.
- The security of the underlying crypto crates is assumed.

## Review method

Manual review of every parser and the handshake/relay/signaling state machines in
the commit at `cf9cbb0`, plus targeted Node reproduction for SEC-1. Continuous
coverage of the parsers is tracked under fuzzing (NB-014).

Later reviews (each change touching crypto, the wire format, relaying, signalling,
traversal or the FFI) read the diff against the threat model above and reproduce
each finding on loopback, in the simulator or with unit-level probes in a scratch
checkout. The probes stay out of the tree unless the fix adopts them as tests. The
hard-NAT review (2026-10-23) used a `punch_case` probe for NB44-1, a `Node` probe
for NB44-2 and a Chromium fleet probe for NB48-1.
