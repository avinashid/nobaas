# peerwire-godot

A Godot 4 GDExtension that runs Godot's own high-level multiplayer (`@rpc`,
`MultiplayerSynchronizer`, `MultiplayerSpawner`) over Peerwire, with no server. It adds one
class, `PeerwireMultiplayerPeer`, a `MultiplayerPeer`: one instance hosts, and the others join
with its `pw:` invite or find it by room name. Peers learn their public address from STUN,
swap addresses over Nostr relays and punch through NATs. Every link is encrypted with Noise IK.

The full guide is [docs/GODOT.md](../../docs/GODOT.md): install, API, NAT traversal, router
port mapping, security, privacy and limits.

| | |
|---|---|
| Godot | 4.3 or later (built against the 4.3 API) |
| godot-rust | `godot = "=0.5.5"` with feature `api-4-3` |
| Platforms | desktop (Linux, Windows, macOS); Android, iOS and web are not built yet |

## Build and install

```sh
cargo build --release -p peerwire-godot     # -> target/release/libpeerwire_godot.so (.dll, .dylib)
```

Copy the library and [`peerwire.gdextension`](peerwire.gdextension) into your project as
`res://addons/peerwire/`, with the library in `res://addons/peerwire/bin/` under the name the
`.gdextension` file lists for your platform.

## Example

```gdscript
var peer := PeerwireMultiplayerPeer.new()

func host() -> void:
	var invite := peer.host()             # optional UDP port; 0 picks a free one
	multiplayer.multiplayer_peer = peer   # this instance is peer 1
	DisplayServer.clipboard_set(invite)   # share it: a link, a QR code, a chat message

func join(invite: String) -> void:
	peer.join(invite)
	multiplayer.multiplayer_peer = peer

# Or meet by name, across NATs, with no invite to pass around:
func host_room() -> void:
	peer.host_room("my-game", "lobby-42")
	multiplayer.multiplayer_peer = peer

func join_room() -> void:
	peer.join_room("my-game", "lobby-42")
	multiplayer.multiplayer_peer = peer
```

[`examples/godot`](../../examples/godot) is a playable demo of this.

## Layout and tests

- `src/peer.rs`: the `PeerwireMultiplayerPeer` class.
- `src/session.rs`: the protocol (peer ids, frames, lanes, targeting, rooms), with no Godot
  types, so `cargo test -p peerwire-godot` covers it without a Godot binary, over loopback.
- `examples/godot/test.sh` runs the demo headless on loopback, twice: one instance hosts
  and a second joins, first with its invite, then by room name through a local Nostr relay.
  It uses no public STUN or relays, and needs Godot 4.3+ (`GODOT=/path/to/godot`, default
  `godot` on `PATH`).

## Status

Early development (v0.1). Not published to crates.io (`publish = false`); build it from this
repository. Rooms have no host election, and the host's uplink carries every player, as in
any star topology.
