# Godot 4

`bindings/godot` is a GDExtension that adds one class, `NobaasMultiplayerPeer`. It is a `MultiplayerPeer`, so
Godot's own high-level multiplayer (`@rpc`, `MultiplayerSynchronizer`, `MultiplayerSpawner`) runs over Nobaas
without changes and without a server. One instance hosts; the others join with its `nb:` invite, or find it by
room name. Peers learn their public address from STUN, swap addresses over Nostr relays and punch through NATs.
Every link is encrypted with Noise IK.

| | |
|---|---|
| Godot | 4.3 or later (built against the 4.3 API; run here on 4.7.2) |
| godot-rust | `godot = "=0.5.5"` with feature `api-4-3` |
| Platforms | desktop (Linux, Windows, macOS). Android, iOS and web are follow-ups (see [Limits](#limits)) |

## Install

```sh
cargo build --release -p nobaas-godot     # -> target/release/libnobaas_godot.so (.dll, .dylib)
```

Copy the library and [`bindings/godot/nobaas.gdextension`](../bindings/godot/nobaas.gdextension) into your
project as `res://addons/nobaas/`, and put the library in `res://addons/nobaas/bin/` under the name the
`.gdextension` file lists for your platform. Godot loads it at the next start, and `NobaasMultiplayerPeer`
appears like a built-in class. The demo's own [`nobaas.gdextension`](../examples/godot/nobaas.gdextension)
points straight at `target/release/` instead.

## Use

```gdscript
var peer := NobaasMultiplayerPeer.new()

func host() -> void:
	var invite := peer.host()             # optional UDP port; 0 (the default) picks a free one
	multiplayer.multiplayer_peer = peer   # this instance is peer 1
	DisplayServer.clipboard_set(invite)   # share it: a link, a QR code, a chat message

func join(invite: String) -> void:
	peer.join(invite)                     # connected_to_server fires once the host accepts
	multiplayer.multiplayer_peer = peer

# Or meet by name, across NATs, with no invite to pass around:
func host_room() -> void:
	peer.host_room("my-game", "lobby-42") # app id (yours) + room name
	multiplayer.multiplayer_peer = peer

func join_room() -> void:
	peer.join_room("my-game", "lobby-42") # waits for a host to answer
	# or peer.join_room_host("my-game", "lobby-42", host_key) to accept that host only
	multiplayer.multiplayer_peer = peer
```

From there everything is standard Godot: `peer_connected` / `peer_disconnected`, `@rpc` methods,
`MultiplayerSpawner.spawn()`, `MultiplayerSynchronizer` replication configs, `set_multiplayer_authority()`.

| Method | |
|---|---|
| `host(port := 0) -> String` | Start hosting; returns the invite, or `""` if the port can't be bound |
| `join(invite: String) -> Error` | Dial the host; `ERR_INVALID_PARAMETER` for a bad invite |
| `host_room(app_id, room, port := 0) -> String` | Host and announce in a room. Only peers that know the room name get in: invite joins are turned away while it announces. Returns the invite |
| `join_room(app_id, room) -> Error` | Join whichever host proves it is in the room; `CONNECTING` until one answers |
| `join_room_host(app_id, room, host_key) -> Error` | `join_room`, accepting only the host whose `get_public_key()` is `host_key` (32 bytes) |
| `get_invite() -> String` | This peer's invite again (public address first, once STUN answered) |
| `get_public_address() -> String` | `ip:port` STUN reported, `""` until a server answers |
| `get_nat_type() -> String` | `open`, `cone`, `symmetric` or `unknown` (needs answers from two STUN servers). `cone` doesn't promise a punch: see [Through NATs](#through-nats) |
| `get_connect_error() -> String` | After `connection_failed`: `symmetric` (one side's NAT is symmetric and the other's router filters by port, so punching can't work), `unreachable`, or `""` |
| `get_connect_error_message() -> String` | After `connection_failed`: the reason as a sentence to show the player, naming the symmetric side; `""` if nothing failed |
| `set_stun_servers(PackedStringArray)` | `host:port` list, from the next host/join. Default Google + Cloudflare; empty: off |
| `set_signaling_relays(PackedStringArray)` | Nostr relay URLs, from the next host/join. Default `wss://relay.damus.io`, `wss://nos.lol`; empty: off |
| `set_privacy_mode(mode: int)` | Which addresses this peer gives out: `PRIVACY_DIRECT` (default), `PRIVACY_PUBLIC_ONLY`, `PRIVACY_RELAY_ONLY`. From the next host/join, and for the running session's next invites and offers; links already up are kept. See [Privacy](#privacy) |
| `get_privacy_mode() -> int` | The mode set |
| `get_peer_key(id) -> PackedByteArray` | The 32-byte Ed25519 key that authenticated peer `id`'s link (empty if not directly connected) |
| `get_public_key() -> PackedByteArray` | This peer's own key |
| `get_peer_rtt_ms(id) -> float` | Smoothed RTT to a directly connected peer, `-1` if unknown |
| `get_peer_loss(id) -> float` | Loss estimate 0–1 from reliable retransmissions, `-1` if unknown |

The rest of the `MultiplayerPeer` API works as documented: `refuse_new_connections`, `disconnect_peer()`,
`close()`, `transfer_mode`, `transfer_channel`, `get_unique_id()`.

## Through NATs

Most players sit behind a NAT, so neither side can simply dial the other. `nobaas` does the traversal (see
[`NAT-TRAVERSAL.md`](NAT-TRAVERSAL.md)), and the binding drives it from `_poll()`; no threads are exposed to
GDScript.

1. On `host*()` / `join*()` the peer asks two STUN servers, on its game socket, what address its NAT gave it.
   Invites list that public address first, then LAN and loopback addresses.
2. In a room, the host announces itself on the room's Nostr topic. A joiner announcing there gets a sealed offer
   from the host (its addresses, encrypted to the joiner's key) and answers with its own. Both then dial each
   other at once, which opens both NATs: the handshake goes through. `join(invite)` does the same over the host's
   own topic, so invites punch too.
3. Two cone NATs (most home routers) punch every time, and so does anything against an open or full-cone peer.
4. A `symmetric` NAT picks a new public port for each peer, so the port it reported isn't the one the other side
   sees. If it hands ports out in sequence, the peer predicts the next one and the other side probes the ports
   around it, which punches restricted and port-restricted cones too.
5. What still fails: two symmetric NATs, or a symmetric NAT with random ports against a *port-restricted* cone,
   one that only lets in packets from ports it has sent to. STUN reports all cones as `cone`, so
   `get_nat_type() == "cone"` doesn't promise a punch. Godot's star has no third peer both sides are linked to, so
   `join*()` fails with `get_connect_error() == "symmetric"`, and `get_connect_error_message()` says which side
   is symmetric, e.g. "the peer's NAT is symmetric (a new public port per destination) and this side's router
   only lets in packets from ports it has sent to, ...". Nobaas runs no TURN relay, by design. The Rust `Node`
   relays such pairs through any peer connected to both.

```gdscript
multiplayer.connection_failed.connect(func(): print("could not reach the host: ", peer.get_connect_error_message()))
```

The simulator covers the whole NAT matrix (`crates/nobaas-sim/tests/traversal.rs`). Real carrier-grade NATs across
two networks still need testing on real devices.

## How it maps onto Nobaas

- **Topology: a star, like Godot's ENet peer.** The host is peer 1, and every joiner holds one encrypted link,
  to the host. `is_server_relay_supported()` is true, so SceneMultiplayer relays joiner-to-joiner RPCs and
  replication through the host.
- **Peer ids.** A joiner's id comes from its Nobaas `PeerId` (an Ed25519 public key), so it stays the same for a
  persistent identity. The id is always in `2..=2147483647`. The host assigns it and sends it in a control frame
  right after the handshake; until it arrives the joiner stays `CONNECTING`. If two keys map to the same id, the
  host gives the newcomer the next derived id.
- **Transfer modes → lanes.**
  - `RELIABLE` uses the reliable lane: ordered, exactly once, up to 1 MiB.
  - `UNRELIABLE` uses the unreliable lane: at most once, up to 1097 B of payload.
  - `UNRELIABLE_ORDERED` uses the unreliable lane plus a 32-bit sequence number per sender and channel. The
    receiver drops anything older than the newest packet it has delivered. Payload up to 1093 B.
- **Channels** travel with each packet and are reported by `get_packet_channel()`. All reliable channels share
  one ordered stream, so a large reliable message on channel 2 delays channel 1 behind it.
- **Frame format** (inside the encrypted Nobaas message), from `bindings/godot/src/session.rs`:
  - data: `0x01 mode channel [seq u32 LE, unreliable-ordered only] payload`
  - control: `0x00 0x01 id(i32 LE)`, the host's id assignment. Other control frames are ignored.

## Security

Nobaas guarantees that every byte reaching the `MultiplayerPeer` came from the authenticated peer and was not
read or changed on the way. Everything Godot does with those bytes is outside that guarantee, and in a P2P game
**every connected peer is a potential attacker**. (Threat model: SEC-9 to SEC-11 in `docs/THREAT-MODEL.md`.)

- **Keep `allow_object_decoding` off** on the `SceneMultiplayer`. That is Godot's default, and the demo asserts
  it. With it on, a peer can make you instantiate objects.
- **`@rpc("any_peer")` arguments are attacker-controlled.** Check types, ranges and lengths, and check
  `multiplayer.get_remote_sender_id()`, before acting on them.
- **Godot peer ids are labels, not identities.** `get_packet_peer()` always comes from the authenticated link, and
  only the dialled host is ever 1. The host gives each joiner a distinct id and renumbers on a collision, so no
  peer inherits another's id. But ids are 31 bits, and someone can grind a key that lands on a chosen id. Never
  authorize on an id or on `is_multiplayer_authority()`: use `get_peer_key(id)` and compare it with keys you
  trust.
- **Godot's authority model is only as trustworthy as the peer you make the authority.** In this star the host is
  the natural authority: let it own spawning and validate what joiners send. A malicious host can still push any
  state. If no single peer can be trusted, run the game as deterministic lockstep on `nobaas-game`, where every
  peer recomputes the state and desyncs are detected.
- **Relaying between joiners is Godot's server relay** (`multiplayer.server_relay`, on by default): the host's
  SceneMultiplayer forwards joiner packets, and the binding only reports that it can. Set
  `multiplayer.server_relay = false` on the host to stop it. Joiners then see only the host.
- **The invite is not a password; the room name is.** Anyone who has the invite can join. It carries a key that
  lets its holder ask the host to dial back (to punch through NATs); someone who knows only the host's public key
  can't.
  - **Rooms.** Relays only see a hash of the room name. Hellos and offers carry an HMAC under a key derived from
    the name, so a peer that doesn't know the name can't join the room or pose as its host. Links from such peers
    are closed unseen.
  - **Give private games a room name with at least 128 random bits**, for example 22 or more random base64
    characters, or 32 hex digits from `Crypto.new().generate_random_bytes(16).hex_encode()`. Relays see the topic, an
    unsalted SHA-256 of the name, so anyone can test guesses against it offline at hash speed. A guessable name
    (`"lobby-42"`) is a public lobby: anyone who guesses it is a member, and a member could announce itself as a
    host.
  - **Pin the host where you can.** If joiners can learn the host's `get_public_key()` some other way (a friend
    code, a QR code, your own backend), `join_room_host()` accepts that host only.
  - **Close the door.** Set `refuse_new_connections` once everyone is in (a refusing host also stops announcing
    its room), or kick with `disconnect_peer()`.
- **Third parties by default.** With the defaults, `host*()` and `join*()` contact Google's and Cloudflare's STUN
  servers (they learn your public IP and that you run Nobaas) and the Nostr relays `relay.damus.io` and `nos.lol`.
  Relays see the room topic (a SHA-256 of `nobaas/room/<app id>/<room>`, so anyone who knows or guesses the name
  can watch who announces in it, and when), each peer's topic (a hash of its public key), your IP and the timing
  of messages. They never see addresses or anything else in an offer: offers are sealed to the recipient's key
  (Noise X), carry a timestamp and nonce, and are dropped when stale or replayed. A fresh Nostr key signs each
  session's events, so they are not linked to your Nobaas identity. `set_stun_servers([])` and
  `set_signaling_relays([])` turn both off (LAN and open-port play still work), or point them at servers you run:
  `cargo run -p nobaas --features nostr --example nostr_relay` is a minimal relay for a LAN. On a `ws://` relay,
  anyone on the network path can drop or inject events. Offers stay sealed and hellos stay proven, so that can stop
  a game from connecting but not let anyone into it.
- **Your key alone gets nothing.** A peer answers an offer, and so reveals its addresses and up to 8 keys it is
  linked to, only to room members (who know the name), holders of its invite, or peers it dialled.
- **Malformed packets** fail at the binding's `decode` and are dropped. Payloads above the lane caps are refused
  on the way out. Rust panics are caught at the GDExtension boundary by godot-rust and never unwind into Godot.

## Privacy

Players link only with the host, so with Godot's server relay they never see each other's IP addresses, in any
mode. The host sees every player's public address, and players see the host's. `set_privacy_mode` decides what a
peer gives out beyond that:

| Mode | A joiner gives out | A host gives out |
|---|---|---|
| `PRIVACY_DIRECT` (default) | public, LAN and loopback addresses, to the host (and in its own invite) | the same, in its invite and to every joiner |
| `PRIVACY_PUBLIC_ONLY` | public addresses only: no LAN or loopback address | public addresses only. Players on the host's LAN then reach it through the router (if it supports hairpinning) |
| `PRIVACY_RELAY_ONLY` | its public addresses to the host only, and nothing to anyone else (its own invite names no address) | as `PRIVACY_PUBLIC_ONLY`: the host is the relay, and every player links to it directly |

Under `PRIVACY_RELAY_ONLY` a room player's relay is the first peer that proves it hosts the room; anyone who knows
the room name can claim that and so learn the player's IP. Use `join_room_host()` when the host's key is known. A
player's own invite names the host as its relay, with the host's address.

```gdscript
peer.set_privacy_mode(NobaasMultiplayerPeer.PRIVACY_PUBLIC_ONLY)   # before host*() / join*(), or any time
```

## Demo

[`examples/godot/`](../examples/godot): two (or more) instances. One hosts, the others join by invite or by
typing the same room name (Host room / Join room). Each
player moves a square with the arrow keys, synced by `MultiplayerSynchronizer` and spawned by
`MultiplayerSpawner`. A text box sends an `@rpc` chat line.

```sh
cargo build --release -p nobaas-godot
godot --path examples/godot              # twice: Host in one window, paste the invite + Join in the other
GODOT=godot ./examples/godot/test.sh     # headless check on loopback: invite run + room run, exits 0 on success
```

`test.sh` runs a host and a joiner headless, twice: by invite, then by room through a local Nostr relay
(`examples/rust/nostr_relay.rs`). The joiner is `--privacy=public-only` in the first run and `relay-only` in the
second. It passes `--stun=none` and a local or no relay, so it touches no public
service. The instances move by themselves and chat. Each passes once it has seen the other peer's square move and
received its chat line:

```
NOBAAS_DEMO_OK peer=1 rtt_ms=6.9
NOBAAS_DEMO_OK peer=1813978524 rtt_ms=16.0
invite: PASS
NOBAAS_DEMO_OK peer=1 rtt_ms=10.9
NOBAAS_DEMO_OK peer=985991120 rtt_ms=-1.0
room: PASS
PASS
```

(`-1` means no RTT sample yet: that joiner finished before its first one.)

## Tests

The protocol logic (`Session`) has no Godot types, so `cargo test -p nobaas-godot` covers it without a Godot
binary:

- id derivation and collisions;
- frame encoding and malformed frames;
- mode → lane mapping and the size limits;
- unreliable-ordered dropping, including wrap-around;
- a host with two joiners over loopback: id assignment, broadcast, exclude-one targeting, kick, close;
- refusing new connections;
- a room host found by two clients (and not by a client of another room) through in-process signaling; clients
  link to the host only, and a refusing host stops being found.

## Limits

- **NATs.** Cone NATs punch, and sequential symmetric NATs mostly do; two symmetric NATs, or a random-port
  symmetric one against a port-restricted cone, fail with `get_connect_error() == "symmetric"` (no TURN; see
  [Through NATs](#through-nats)). Some networks block UDP or
  Nostr's websockets altogether.
- **Rooms have no size limit or host election.** One host per room: a second `host_room` with the same name is
  found by some joiners and not others.
- **The host's uplink carries everyone**, as with any star. Use `nobaas-game` islands and super-peers for big
  sessions.
- **Lockstep and rollback games** must run their game logic on `nobaas-game` fixed-point maths, not Godot
  physics or floats. Godot physics is not deterministic across machines.
- **Web export** has no UDP. It needs a transport through `nobaas.js` (WebRTC) bridged with `JavaScriptBridge`.
  That is follow-up work.
- **Android / iOS** need per-target builds (`aarch64-linux-android`, `aarch64-apple-ios`) and `.gdextension`
  entries. Not built or tested yet.
