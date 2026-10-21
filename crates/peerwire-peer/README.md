# peerwire-peer

An optional always-on peer for Peerwire. Peerwire needs no server; this is an ordinary [`peerwire`](../peerwire) node that
stays online, so the peers linked to it can relay through it when their NATs can't punch.

It:
- keeps one identity across restarts (`$PEERWIRE_DATA/identity.key`, 32 secret bytes, mode 0600), so its peer id stays
  the same;
- prints its `pw:` invite on start and writes it to `$PEERWIRE_DATA/invite` (0600, replaced atomically);
- relays between peers linked to it (the core's peer relay, rate-limited per peer) and drops app messages sent to it;
- turns away handshakes past `PEERWIRE_MAX_PEERS` before any key exchange and counts them in its stats line;
- binds `127.0.0.1:7700` by default, so nothing beyond this machine can reach it until you change `PEERWIRE_BIND`.

Anyone who knows its peer id and address can link to it; the invite carries both.

## Example

```sh
PEERWIRE_DATA=./peer-data cargo run --release -p peerwire-peer          # run; prints the peer id and invite
PEERWIRE_DATA=./peer-data cargo run --release -p peerwire-peer -- check # link to it from loopback; exit 0 if it answers
```

In Docker, with the examples server next to it:

```sh
docker compose up -d --build
docker compose exec peer peerwire-peer check
```

Settings come from the environment: `PEERWIRE_BIND`, `PEERWIRE_DATA`, `PEERWIRE_STUN`, `PEERWIRE_MAX_PEERS`, `PEERWIRE_RELAY`,
`PEERWIRE_RELAY_RATE`, `PEERWIRE_RELAY_TOTAL` and `PEERWIRE_STATS_SECS`. [docs/DOCKER.md](../../docs/DOCKER.md) lists their
defaults and covers exposing the peer, its logs (they hold peer addresses) and the container hardening.

## Status

Early development (v0.1). A binary only (`publish = false`). It serves native peers (Rust, C, Godot); browsers speak
WebRTC and can't reach it yet.

Not yet: joining rooms over Nostr, acting as a room's super-peer, and storing messages for offline peers.
