# Threat model

Scope of this document: `nobaas-core` (handshake, wire format, lanes, relay, NAT
traversal, STUN, invites, sealed messages), the C ABI (`nobaas-ffi`), the
JavaScript glue and transport (`packages/js`), and the Nostr / BroadcastChannel
signaling adapters. It records what Nobaas defends against, what it does not, and
the findings of the first security review (M5, NB-013).

Status: pre-1.0, no external audit. See [SECURITY.md](../SECURITY.md).

## What Nobaas protects

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

**Status as of `9273ea3`, with `6f947f3` (NPM-3).**

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

### SEC-1 — Unvalidated peer-id length corrupts the wasm heap (High) — NB-021

**Status: fixed** in NB-021 (merged `ccc3b79`). Every fixed-size copy into wasm
memory checks its length first (`peerBytes`, `exact`), every variable-size one
takes bytes only (`bytesOf`), and the signaling adapters (`nostr.js`, `webrtc.js`,
`local.js`, `room.js`) drop a `from` that is not 64 lowercase hex characters. The
reproduction below now throws a `TypeError` and the module stays healthy
(`packages/js/test/peer_id.mjs`, in the gate). The C ABI peer-length argument is
a separate defence-in-depth card.

`packages/js/nobaas.js` `Endpoint.setPeer(id)` copies `id` into the fixed 32-byte
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
  in `nb_alloc` (`"100000000"` grew wasm memory to ~101 MB, never returned). It
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
(the room tag is `sha256("nobaas/room/<room>")`, computable by anyone with the
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

The per-peer tag is `sha256("nobaas/peer/<PeerId>")`, a fixed function of the
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

Several `nb_*` functions dereference the endpoint handle and in/out buffers
without null or length checks (e.g. `nb_secret`, `nb_send`, `nb_stats`), and
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

---

---

## Invite strings (`nb:`)

An invite (`crates/nobaas-core/src/invite.rs`) is `nb:` + unpadded base64url of
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
- **Parsing is safe.** `from_bytes` is a bounds-checked reader, caps entries at
  16, rejects trailing bytes, and validates each PeerId is a real Ed25519 point.
  Reviewed, no issues; covered going forward by fuzzing (NB-014).

No new finding; the exposure is documented so apps set expectations for where an
invite may be posted.

## Godot binding (NB-020, planned — security requirements)

The coming `bindings/godot` crate exposes `NobaasMultiplayerPeer`
(`MultiplayerPeerExtension`) so Godot's high-level multiplayer — `@rpc`,
`MultiplayerSynchronizer`, `MultiplayerSpawner` — runs over Nobaas. Nobaas
delivers *authenticated, confidential bytes* to that layer, but everything Godot
does *above* the `MultiplayerPeer` is outside Nobaas's guarantees. This section
sets the requirements the binding must meet; they are tracked as SEC-9…SEC-11
(proposed follow-ups on NB-020, owner @nobaas-backend).

### SEC-9 — Untrusted packets crossing into GDScript (High, design) — NB-029

Godot's high-level multiplayer deserializes incoming packets into Variants and
dispatches `@rpc` methods by name. Variant decoding of `Object`/`Callable`/
`RID` from the network is a known remote-code-execution surface (object
instantiation, resource loading). Because Nobaas is pure P2P, **every connected
peer is a remote attacker** at this layer.

**Requirements.**
- The binding must configure the `SceneMultiplayer` with
  `allow_object_decoding = false` (Godot's default, but assert it) so peers
  cannot ship encoded `Object`s.
- Document loudly that `@rpc("any_peer")` methods receive attacker-controlled
  arguments and must validate them and check `multiplayer.get_remote_sender_id()`.
- Surface Nobaas's per-lane size caps to the `MultiplayerPeer` so an oversized or
  malformed Godot packet is rejected at the boundary, not deep in a decoder.
- A Rust panic in the cdylib must not unwind into Godot: catch/abort at the FFI
  edge (godot-rust convention), so a malformed packet can't turn a panic into UB.

### SEC-10 — Godot peer-ID spoofing and mapping collisions (High, design) — NB-029

Godot peer IDs are 32-bit ints; Nobaas PeerIds are 256-bit Ed25519 keys. The
binding derives a stable non-zero Godot ID from each PeerId. A 32-bit space makes
collisions reachable (birthday bound ~2^16 peers, but an attacker can *grind*
PeerIds offline to target a specific victim's Godot ID).

**Requirements.**
- The PeerId → Godot-ID map is authoritative per session and **collisions must be
  rejected**, not silently merged: a joiner whose derived ID clashes with a live
  peer is renumbered or refused, never allowed to inherit another peer's ID.
- `_get_packet_peer()` must return the Godot ID of the *authenticated Nobaas
  source* of that packet only; it must never be derivable from packet contents an
  attacker controls.
- Host is Godot ID 1: the binding must bind ID 1 to the actual host identity and
  reject any joiner that tries to present as 1.
- Grinding resistance: either widen the effective namespace (keep the full PeerId
  as the identity and treat the 32-bit ID as a non-security-bearing handle the app
  must not use for authorization), or document that `is_multiplayer_authority()`
  and peer-ID checks are **not** a security boundary — the PeerId is.

### SEC-11 — Host authority over RPCs in a serverless mesh (Medium, design) — NB-029

Godot's model assumes peer 1 is a trusted server/authority. Nobaas has no
inherent authority: a peer can send any packet to any peer, and
`MultiplayerSpawner`/`MultiplayerSynchronizer` let an "authority" spawn/despawn
nodes and set properties on others' scene trees. A malicious peer that the app
treats as an authority corrupts every client.

**Requirements.**
- `docs/GODOT.md` must state that Godot's authority model is only as trustworthy
  as the app's topology; in an open mesh, `@rpc("authority")` is not a security
  control.
- Recommend a pattern: designate the host as the only synchronizer/spawner
  authority and have it validate inputs, or use `nobaas-game`'s deterministic
  lockstep (where every peer recomputes state and desync is detected) instead of
  trusting an authority's state pushes.
- The binding should not auto-relay packets between peers on behalf of the Godot
  layer unless the app opts in (Nobaas relay is already gated and rate-limited;
  don't bypass it).

**Status of SEC-9…SEC-11 as of `ccc3b79`.** The binding shipped (NB-020) and was
hardened (NB-030). `docs/GODOT.md` (Security) now states each requirement: keep
`allow_object_decoding` off (the demo asserts it), treat `@rpc("any_peer")`
arguments as attacker-controlled, ids are labels (the host renumbers collisions,
only the dialled host is 1, authorize by `get_peer_key`), and Godot's authority
is only as trustworthy as the peer made authority (use the host, or lockstep on
`nobaas-game`). SEC-11 is a design property: it stays a documented residual, not
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
  looked up from the authenticated Nobaas sender; a frame from a peer with no id
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

Nobaas was bootstrapped from the Protocol Zero SDK, whose adversarial notes
(`p0/protocol-zero/REDTEAM.md`) cover a related dead-drop/signaling stack. The
mappings that carry over and are already handled in Nobaas:

- **A1 sender forgery → handled.** Nobaas signaling uses the authenticated sealed
  form (`Noise_X`, sender PeerId bound), the equivalent of P0's
  `sealAuthenticated`. A forged/anonymous seal fails to open under a claimed
  identity (`sealed.rs` test).
- **A8 packet injection/replay → handled.** AEAD per data packet + the 2048 nonce
  window; nonce recorded only after authentication (SEC/THREAT items above).
- **A7 stale/replayed signaling → partially: SEC-6.** Nobaas has no in-band
  signaling-replay guard; carry P0's lesson (bind a fresh session id) into the fix.
- **A11 metadata / A12 forward secrecy → OPEN here too: SEC-2/3/5 and SEC-7.** The
  same open problems (relay-visible timing/IP, static-key exposure) apply; Nobaas's
  per-session Nostr key and rotating-tag ideas mirror P0's mitigations.
- **A5 spam / A13 Sybil → relevant to the signaling layer**; the half-open cap
  (SEC-4) and relay rate limiting are the Nobaas analogues. A PoW-gated first
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
