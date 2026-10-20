# nobaas-peer

An optional always-on peer for Nobaas. Nobaas needs no server; this is an ordinary [`nobaas`](../nobaas) node that
stays online, so the peers linked to it can relay through it when their NATs can't punch.

It:
- keeps one identity across restarts (`$NOBAAS_DATA/identity.key`, 32 secret bytes, mode 0600), so its peer id stays
  the same;
- prints its `nb:` invite on start and writes it to `$NOBAAS_DATA/invite` (0600, replaced atomically);
- relays between peers linked to it (the core's peer relay, rate-limited per peer) and drops app messages sent to it;
- closes links over `NOBAAS_MAX_PEERS` and counts them in its stats line;
- binds `127.0.0.1:7700` by default, so nothing beyond this machine can reach it until you change `NOBAAS_BIND`.

Anyone who knows its peer id and address can link to it; the invite carries both.

## Example

```sh
NOBAAS_DATA=./peer-data cargo run --release -p nobaas-peer          # run; prints the peer id and invite
NOBAAS_DATA=./peer-data cargo run --release -p nobaas-peer -- check # link to it from loopback; exit 0 if it answers
```

In Docker, with the examples server next to it:

```sh
docker compose up -d --build
docker compose exec peer nobaas-peer check
```

Settings come from the environment: `NOBAAS_BIND`, `NOBAAS_DATA`, `NOBAAS_STUN`, `NOBAAS_MAX_PEERS`, `NOBAAS_RELAY`,
`NOBAAS_RELAY_RATE` and `NOBAAS_STATS_SECS`. [docs/DOCKER.md](../../docs/DOCKER.md) lists their defaults and covers
exposing the peer, its logs (they hold peer addresses) and the container hardening.

## Status

Early development (v0.1). A binary only (`publish = false`). It serves native peers (Rust, C, Godot); browsers speak
WebRTC and can't reach it yet.

Not yet: joining rooms over Nostr, acting as a room's super-peer, and storing messages for offline peers.
