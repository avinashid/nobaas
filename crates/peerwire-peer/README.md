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

## As a relay peer

Give its invite to the peers of an app, and they keep a link to it (`Node::set_relay_peers` in Rust,
`set_relay_peers` in Godot). Two of them whose NATs can't punch each other, such as two symmetric NATs on phone
hotspots, then link through it. Bind it to a public address (`PEERWIRE_BIND=0.0.0.0:7700`, the UDP port open) and
keep STUN on, or give the host a public IP, so the invite lists an address the peers can reach. With
`PEERWIRE_DATA` set, the invite stays the same across restarts.

Each peer may have it relay `PEERWIRE_RELAY_RATE` bytes per second (256 KiB by default), summed over everything it
sends through it; `PEERWIRE_RELAY_TOTAL` caps all peers together. The relay can't read what it forwards, but it sees
the peers' addresses, which of them talk, and how much. See
[docs/NAT-TRAVERSAL.md](../../docs/NAT-TRAVERSAL.md#relay-peers).

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
