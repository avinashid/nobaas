# Godot 4

`bindings/godot` is a GDExtension that adds one class, `NobaasMultiplayerPeer`. It is a `MultiplayerPeer`, so
Godot's own high-level multiplayer (`@rpc`, `MultiplayerSynchronizer`, `MultiplayerSpawner`) runs over Nobaas
without changes and without a server. One instance hosts and hands out an `nb:` invite. The others join with it.
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
```

From there everything is standard Godot: `peer_connected` / `peer_disconnected`, `@rpc` methods,
`MultiplayerSpawner.spawn()`, `MultiplayerSynchronizer` replication configs, `set_multiplayer_authority()`.

| Method | |
|---|---|
| `host(port := 0) -> String` | Start hosting; returns the invite, or `""` if the port can't be bound |
| `join(invite: String) -> Error` | Dial the host; `ERR_INVALID_PARAMETER` for a bad invite |
| `get_invite() -> String` | This peer's invite again |
| `get_peer_rtt_ms(id) -> float` | Smoothed RTT to a directly connected peer, `-1` if unknown |
| `get_peer_loss(id) -> float` | Loss estimate 0–1 from reliable retransmissions, `-1` if unknown |

The rest of the `MultiplayerPeer` API works as documented: `refuse_new_connections`, `disconnect_peer()`,
`close()`, `transfer_mode`, `transfer_channel`, `get_unique_id()`.

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

## Demo

[`examples/godot/`](../examples/godot): two (or more) instances. One hosts, the others join by invite. Each
player moves a square with the arrow keys, synced by `MultiplayerSynchronizer` and spawned by
`MultiplayerSpawner`. A text box sends an `@rpc` chat line.

```sh
cargo build --release -p nobaas-godot
godot --path examples/godot              # twice: Host in one window, paste the invite + Join in the other
GODOT=godot ./examples/godot/test.sh     # headless check on loopback: host + joiner, exits 0 on success
```

`test.sh` runs both instances headless. They move by themselves and chat. Each passes once it has seen the other
peer's square move and received its chat line:

```
NOBAAS_DEMO_OK peer=1 rtt_ms=6.9
NOBAAS_DEMO_OK peer=712076104 rtt_ms=8.8
PASS
```

## Tests

The protocol logic (`Session`) has no Godot types, so `cargo test -p nobaas-godot` covers it without a Godot
binary:

- id derivation and collisions;
- frame encoding and malformed frames;
- mode → lane mapping and the size limits;
- unreliable-ordered dropping, including wrap-around;
- a host with two joiners over loopback: id assignment, broadcast, exclude-one targeting, kick, close;
- refusing new connections.

## Limits

- **Reachability.** An invite lists the host's LAN and loopback addresses, plus any public address peers have
  reported. Today that covers the same machine, the same LAN, or a host with a reachable UDP port. NAT traversal
  across the internet (STUN, signaling adapters, peer relays) arrives in the `nobaas` facade with NB-004; the
  binding will pick it up without API changes.
- **The host's uplink carries everyone**, as with any star. Use `nobaas-game` islands and super-peers for big
  sessions.
- **The invite is not a password.** It holds no secret, and anyone who has it can join. Use
  `refuse_new_connections` once everyone is in, or kick with `disconnect_peer()`. Godot peer ids are labels the
  host assigns, not identities; the Nobaas `PeerId` behind each link is authenticated.
- **Lockstep and rollback games** must run their game logic on `nobaas-game` fixed-point maths, not Godot
  physics or floats. Godot physics is not deterministic across machines.
- **Web export** has no UDP. It needs a transport through `nobaas.js` (WebRTC) bridged with `JavaScriptBridge`.
  That is follow-up work.
- **Android / iOS** need per-target builds (`aarch64-linux-android`, `aarch64-apple-ios`) and `.gdextension`
  entries. Not built or tested yet.
