# Live examples

| Example | Status | Shows |
|---|---|---|
| chat | planned (NB-007) | rooms, presence, history for late joiners |
| whiteboard | planned (NB-008) | CRDT strokes, live cursors |
| game | planned (NB-009); today: `packages/js/demo/arena.html` | super-peer lockstep islands |
| meet | planned (NB-010) | WebRTC audio/video over Peerwire signaling |
| [godot](godot/) | working (NB-020, NB-004) | Godot 4 `@rpc`, `MultiplayerSynchronizer`, `MultiplayerSpawner` over Peerwire, by invite or room, through NATs; see [docs/GODOT.md](../docs/GODOT.md) |
| [rust/echo.rs](rust/echo.rs) | working (NB-003) | `peerwire::Node`: host prints an invite, joiner echoes |
| [rust/echo_async.rs](rust/echo_async.rs) | working (NB-003) | the same with `peerwire::AsyncNode` on tokio |
| [rust/natcheck.rs](rust/natcheck.rs) | working | field test on real networks: STUN port-pattern probe (cone / symmetric sequential / random), then meets a peer in a Nostr room and reports direct, relayed or the failure reason |
| [../docker-compose.yml](../docker-compose.yml) | working (NB-015) | serves these demos on `http://127.0.0.1:7780` next to an always-on peer; see [docs/DOCKER.md](../docs/DOCKER.md) |
| [rust/nostr_relay.rs](rust/nostr_relay.rs) | working (NB-004) | a minimal in-memory Nostr relay for tests and LAN play |

Until then, the working demos are in [`packages/js/demo`](../packages/js/demo).
