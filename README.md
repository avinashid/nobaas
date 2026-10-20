# Nobaas

[![CI](https://github.com/avinashid/nobaas/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/avinashid/nobaas/actions/workflows/ci.yml)

**No backend as a service.** Nobaas is a peer-to-peer networking toolkit written in Rust. Use it to build
multiplayer games, collaborative whiteboards, video meetings and chat without running servers or paying for relays.

> **Status: early development (v0.1).** The transport core is working and tested; the higher-level
> building blocks (sync, rooms, media) are in progress.

```
            your app (game · whiteboard · meet · chat)
    ┌───────────────────────────────────────────────────────┐
    │ nobaas-game   nobaas-sync   nobaas-media   nobaas-store│  building blocks
    ├───────────────────────────────────────────────────────┤
    │ nobaas-core: identity · Noise handshake · lanes ·     │  sans-IO session core
    │              NAT traversal · peer relay · invites     │
    ├───────────────────────────────────────────────────────┤
    │ UDP · WebRTC DataChannel · (QUIC, WebTransport later) │  transports
    ├───────────────────────────────────────────────────────┤
    │ signaling adapters: invite/QR · BroadcastChannel ·    │  how peers find each other
    │ Nostr relays · mDNS · DHT                             │
    └───────────────────────────────────────────────────────┘
     Rust · WebAssembly · JavaScript/TypeScript · C ABI · Docker
```

## Why Nobaas

- **Zero backend.** Peers find each other through pluggable signaling (a link or QR code, public Nostr relays,
  the LAN), and traverse NATs by hole punching. Any connected peer can relay for others, so no TURN server is
  needed.
- **Encrypted by default.** Every link uses a Noise IK handshake. A peer's ID *is* its public key, so
  dialling an ID authenticates it. Signaling channels are untrusted by design.
- **Built for real-time.** Each link has three lanes: *unreliable* for state, *reliable* (ordered, up to
  1 MiB) for events, and *urgent*, which is reliable but sends every packet twice for hits and inputs. Tail-loss
  probes and selective ACKs keep p99 latency low under packet loss.
- **One core everywhere.** The core is sans-IO Rust (no sockets, threads or clock inside), so the same code runs
  natively, in browsers as a 300 KB WebAssembly module with zero imports, in Node, behind a C ABI for
  Swift/Kotlin/Dart, and in Docker.
- **Scales past the mesh.** Super-peers relay for groups ("islands") of up to 12 players, with failover and overload
  control, so a session can grow well past what a full mesh can carry. See
  [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Quick start

### Rust

```sh
cargo test --workspace --release
cargo run --release -p nobaas-core --example udp_pair          # two peers over real UDP loopback
cargo run --release -p nobaas-sim --example arena -- --players 512 --supers 8
```

### Browser / JavaScript

```sh
./packages/js/build.sh                       # -> packages/js/pkg/nobaas_ffi.wasm
python3 -m http.server -d packages/js 8000   # open http://localhost:8000/demo/ in two tabs
```

```js
import { loadNobaas, Lane } from './nobaas.js';

const nb = await loadNobaas(new URL('./pkg/nobaas_ffi.wasm', import.meta.url));
const ep = nb.endpoint();           // a fresh identity; ep.id is your PeerId
// hand datagrams between ep and a transport (WebRTC, see webrtc.js), then:
ep.send(peerId, Lane.Reliable, new TextEncoder().encode('hello'));
```

## Repository layout

| Path | What it is |
|---|---|
| `crates/nobaas-core` | Sans-IO session core: identity, Noise handshake, reliable/unreliable/urgent lanes, NAT traversal, peer relay, STUN codec, invites |
| `crates/nobaas-sim` | Deterministic network simulator (latency, jitter, loss, NAT types, uplink limits) and the multiplayer arena benchmark |
| `crates/nobaas-ffi` | C ABI: one surface for WebAssembly (browsers, Node) and native FFI |
| `crates/nobaas-game` | Deterministic fixed-point maths, Hilbert-curve islands and desync detection |
| `packages/js` | JavaScript package: wasm glue, WebRTC transport, Nostr signaling, demos |
| `examples/` | Live examples: chat, whiteboard, game, meet (in progress) |
| `docs/` | Architecture, performance model, comparison with other libraries |

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Performance model](docs/PERFORMANCE.md): measured and theoretical numbers
- [Comparison with GunJS, Yjs, PeerJS, libp2p, Trystero, Hyperswarm](docs/COMPARISON.md)
- [Security policy](SECURITY.md) · [Contributing](CONTRIBUTING.md)

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
