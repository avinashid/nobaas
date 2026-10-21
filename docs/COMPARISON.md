# How Peerwire compares

This is an honest, feature-level comparison based on each project's documented design. It is not a benchmark. Where
Peerwire is ahead only on the roadmap, the table says *planned*.

| | Peerwire | GunJS | Yjs + y-webrtc | PeerJS | libp2p | Trystero | Hyperswarm |
|---|---|---|---|---|---|---|---|
| Core language | Rust (wasm, C ABI, JS) | JS | JS | JS | Go, Rust, JS, … | JS | JS (Node) |
| Runs in browsers | yes, 300 KB wasm | yes | yes | yes | yes (limited transports) | yes | no |
| Runs natively / mobile | yes (Rust, C ABI) | Node | Node | no | yes | no | Node, Bare |
| Needs a server you run | **no** | relay peers in practice | signaling server | PeerServer | bootstrap nodes | **no** | no (public DHT) |
| Signaling | invite/QR, BroadcastChannel, Nostr; mDNS and DHT planned | relay peers | WebSocket server | PeerServer | DHT, rendezvous | Nostr, BitTorrent, MQTT, IPFS | DHT |
| Hard NAT (symmetric/CGNAT) | hole punch + **peer relay**, no TURN | through relay peers | TURN needed | TURN needed | circuit relay v2 + DCUtR | TURN needed | hole punch + relay |
| E2E encryption, authenticated peer IDs | yes (Noise IK, ID = public key) | opt-in (SEA) | transport only | transport only | yes (Noise/TLS) | transport only | yes (Noise) |
| Unreliable / real-time lane | **yes** (unreliable, reliable, urgent) | no | no | via DataChannel options | stream-based | no | stream-based |
| Game netcode (lockstep, islands, failover) | **yes** | no | no | no | no | no | no |
| Shared state / CRDT | planned (`peerwire-sync`) | **yes** (graph CRDT) | **yes** (best-in-class CRDT) | no | no | no | via Hypercore |
| Offline delivery | planned (`peerwire-store`) | yes (relay peers) | via providers | no | no | no | via Hypercore |
| Audio/video signaling | planned (`peerwire-media`) | no | no | **yes** | no | yes | no |
| Deterministic network simulator | **yes** | no | no | no | partial (testground) | no | no |

## Where Peerwire aims to win

- **Truly zero backend, including hard NATs.**
  - Other browser libraries fall back to a TURN server (cost) or to relay peers that someone has to run.
  - In Peerwire, any connected peer can relay for others, with rate limits, and the link upgrades to direct when a
    hole punch succeeds.
- **Real-time first.**
  - GunJS and Yjs sync state over reliable streams.
  - Peerwire has an unreliable lane, an urgent lane and tail-loss probes, so games and live cursors stay smooth
    under loss.
- **One core everywhere.**
  - The same audited Rust core runs in browsers, servers, mobile and embedded hosts.
  - Bugs get fixed once.
- **Measured, reproducible performance.** Every claim in [PERFORMANCE.md](PERFORMANCE.md) comes from a command
  you can run, or shows its formula.

## Where others are ahead today

- **Yjs:** a far more mature CRDT for rich text.
  - `peerwire-sync` should interoperate with Yjs documents rather than replace them.
- **GunJS:** an existing graph database with a large ecosystem.
- **libp2p:** many transports, a large production deployment, and a formal spec.
