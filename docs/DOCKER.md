# Docker: the always-on peer and the examples server

Nobaas needs no server. An always-on peer is optional: an ordinary peer that stays online, so the peers linked to it
can relay through it when their NATs can't punch (see [NAT-TRAVERSAL.md](NAT-TRAVERSAL.md)). It keeps one identity
across restarts, so its invite stays valid for the same peer id. The compose file runs it next to a static server for
the browser demos.

```sh
docker compose up -d --build
docker compose logs peer              # the peer id and its nb: invite
docker compose exec peer nobaas-peer check
open http://127.0.0.1:7780            # the demos (any browser on this machine)
docker compose down                   # add -v to delete the peer's identity too
```

## What runs

| Service | Image | Listens on (default) | User | Root filesystem |
|---|---|---|---|---|
| `peer` | `nobaas-peer` (`Dockerfile` target `peer`): the `nobaas-peer` binary on `gcr.io/distroless/cc-debian12:nonroot`, no shell | UDP `127.0.0.1:7700`, host network | 65532 | read-only; identity in the `peer-data` volume at `/data` |
| `examples` | `nobaas-examples` (target `examples`): `packages/js` `demo/`, `src/` and the built `pkg/nobaas_ffi.wasm` on `nginx:stable-alpine` | TCP `127.0.0.1:7780` → 8080 | `nginx` (101) | read-only; tmpfs `/tmp` |

Both drop every Linux capability, set `no-new-privileges`, and have memory and pid limits. Nothing is reachable from
another host until you change a setting below.

**Why host networking for the peer.** A node gives out the addresses its socket is bound to, and sees the real source
address of each packet. Behind Docker's port mapping it would give out the container's bridge address and see Docker's
gateway as every peer's address, which breaks invites, relaying and hole punching. With `network_mode: host` and a
bind of `127.0.0.1:7700`, the peer is on loopback only, like any local process. (Docker Desktop on macOS and Windows
runs containers in a VM, where host networking reaches only that VM; there, run the binary natively with
`cargo run --release -p nobaas-peer`.)

## The peer

`nobaas-peer` is a `nobaas::Node` with the default `Config` (relay on, rate-limited per peer) that never sends app
data of its own:

- First start: a new identity in `$NOBAAS_DATA/identity.key` (32 secret bytes, mode 0600). Later starts reuse it.
- Every start: it prints its peer id and invite, and writes the invite to `$NOBAAS_DATA/invite` (0600). The invite
  carries a fresh invite key each start, so hand out the new one after a restart.
- It accepts links from anyone who knows its peer id and address (both are in the invite), up to `NOBAAS_MAX_PEERS`.
  Past that, the core drops a new handshake before any key exchange, so a newcomer gets no answer and nothing relayed.
  The invite key gates only signaling offers, and the peer uses no signaling yet. Messages sent to it are dropped. It
  forwards packets between two peers linked to it when one asks (the core's peer relay).
- `nobaas-peer check [FILE]` links to the invite in `FILE` (default `$NOBAAS_DATA/invite`) from `127.0.0.1` and exits
  0 when the handshake completes within 5 s. It is the image's `HEALTHCHECK`.
- It logs a line per link and unlink, and one stats line per `NOBAAS_STATS_SECS`: links, refused links, dropped
  handshakes (including those past the cap), packets, relayed and dropped, failed authentications. Refusals are only
  counted, so a flood of handshakes can't fill the log.
- **The logs hold every linked peer's IP address and port**, which is personal data. Compose caps them at three
  10 MB files per service (`logging:`); keep or ship them accordingly.

| Variable | Default | |
|---|---|---|
| `NOBAAS_BIND` | `127.0.0.1:7700` | UDP address. `0.0.0.0:7700` makes it reachable from other hosts (open that UDP port in your firewall yourself) |
| `NOBAAS_DATA` | `/data` in the image; unset natively | where the identity and invite live; unset: a new identity each run, no files |
| `NOBAAS_STUN` | the default servers (Google, Cloudflare) | comma-separated `host:port`; `none` turns STUN off. A loopback bind never uses STUN |
| `NOBAAS_MAX_PEERS` | `256` | most links at once (handshakes under way don't count; the core caps those at 4096) |
| `NOBAAS_RELAY` | `1` | `0`: link but never relay |
| `NOBAAS_RELAY_RATE` | `262144` | bytes per second each peer may have relayed (burst 64 KiB) |
| `NOBAAS_RELAY_TOTAL` | `8388608` | bytes per second relayed for all peers together (burst a quarter of that); `0`: no limit |
| `NOBAAS_STATS_SECS` | `60` | `0`: no stats lines |

Run it without Docker the same way: `NOBAAS_DATA=./peer-data cargo run --release -p nobaas-peer`.

**Not yet:** joining rooms over Nostr (so room members find it without an invite), acting as a room's super-peer, and
storing messages for offline peers. Browsers can't reach it either: they speak WebRTC, and it speaks UDP, so today it
serves native peers (Rust, C, Godot).

## Exposing it

To run a public always-on peer on a server with its own address:

```sh
NOBAAS_BIND=0.0.0.0:7700 docker compose up -d peer
```

Then it gives out its LAN/public address and loopback, learns its public address over STUN if it is behind a NAT, and
anyone who knows its peer id and address can link and relay through it, within the per-peer rate, the total relay rate
and the peer cap. Identities are free to make, so one host can fill every slot: the most it can have relayed out of
your server is `NOBAAS_RELAY_TOTAL` (8 MiB/s by default), and it can aim all of it at one linked peer, from your
address. Lower `NOBAAS_RELAY_TOTAL` and `NOBAAS_MAX_PEERS` to what you want to give away. Each linked peer
can also make the server hold about 2.3 MB of received data (about 2.6 MB allocated), so 256 peers can pin about
0.6 GB of memory; size `NOBAAS_MAX_PEERS` to the memory you have. Relayed traffic is
end-to-end encrypted between the two peers; the relay sees their addresses, packet
sizes and timing. See [THREAT-MODEL.md](THREAT-MODEL.md).

The examples server is static files only (GET and HEAD; anything else is 403; dotfiles are 403). It sends a
Content-Security-Policy that allows only same-origin scripts, styles and wasm, `wss:` and same-origin connections, and
no framing. To show it on your LAN, set
`NOBAAS_EXAMPLES_PORT` and change the `127.0.0.1:` prefix of its `ports` entry; it has no TLS, so put it behind your
own HTTPS proxy before exposing it further.

## Checking it

Tested on loopback only (Docker 29.7, Compose v5.5, aarch64):

```sh
docker compose up -d --build
docker compose ps                                  # both (healthy)
docker compose exec peer nobaas-peer check         # ok: linked <peer id>
ss -lun | grep 7700; ss -lnt | grep 7780           # 127.0.0.1 only
curl -sI http://127.0.0.1:7780/pkg/nobaas_ffi.wasm # 200, application/wasm
docker compose restart peer && docker compose logs peer | grep -o 'peer [0-9a-f]*'    # the same peer id twice
```
