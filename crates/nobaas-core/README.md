# nobaas-core

Sans-IO peer-to-peer session core for Nobaas.
Design: [docs/ARCHITECTURE.md](../../docs/ARCHITECTURE.md).

An `Endpoint` holds one identity and any number of encrypted links. It never
opens sockets, spawns threads or reads a clock: the host passes in datagrams
and the time, and takes out datagrams, events and the next timer deadline.
`examples/udp_pair.rs` shows a complete host loop on std UDP sockets in about 60 lines.

## What it does

- **Identity:** Ed25519. A `PeerId` is the verifying key.
- **Handshake:** `Noise_IK_25519_ChaChaPoly_BLAKE2s`, 1 RTT. The Noise static key is the
  X25519 form of the Ed25519 identity, so dialling a `PeerId` authenticates it with no
  extra signatures.
  - A responder link is not announced until the initiator's first data packet arrives, so a
    replayed HS1 can't displace a live link.
  - A forged HS2 can't poison a dial: the handshake is rebuilt from its seed.
- **Simultaneous open** (both sides dialling, as in hole punching): the dial from the smaller
  PeerId wins on the first packet, so exactly one link results.
- **Data packets:**
  - Explicit nonces and a 2048-packet anti-replay window, as in WireGuard.
  - Receiver session indices, so links survive address changes (roaming).
  - At most 1232 bytes, which fits the IPv6 minimum MTU.
- **Lanes:**
  - *Reliable*: ordered, exactly once, fragmented up to 1 MiB, selective ACK, RFC 6298 RTO.
  - *Unreliable*: at most once, up to 1100 B, drop-oldest queue.
  - Frames from both lanes and ACKs are packed into one packet.
- **Liveness:** keepalive every 10 s, idle timeout after 30 s. A re-handshake from a restarted
  peer gives `Disconnected { Replaced }` and then `Connected`.
- **Bounds:** send buffers, receive windows, message size and the number of half-open
  handshakes are all capped.
- **Paths:** a link is `Path::Direct(addr)` or `Path::Via(relay)`.
  - `connect_via(now, relay, peer, candidates)` dials through a peer both sides are linked to.
    The relay forwards opaque Noise packets (one hop, token-bucket limited, `Config::relay`
    turns it off). It also tags each packet with the address it saw the sender at, which
    gives the receiver a hole-punch candidate.
  - A relayed link probes its candidates with PUNCH (every 100 ms for 5 s, optionally
    spraying ports above each for sequential NATs). It moves to direct only on an echoed
    PUNCH_ACK, so a one-way path is never used.
  - A direct link that goes silent for 15 s falls back to its relay. `Event::PathChanged`
    reports each move.
  - `connect_with(now, peer, &[paths])` sends the handshake down several paths at once.
    Two peers dialling each other's public address this way punch through cone NATs with
    no relay.
  - `add_candidates` adds addresses learned later (LAN, signaling).
- **Public address:** every direct peer reports where it sees you (`Event::AddrObserved`),
  so any peer works as STUN. `stun` has a sans-IO RFC 5389 client codec for real STUN
  servers. It demultiplexes against Nobaas packets on the same socket (RFC 7983 first-byte ranges).
- **Invites:** `Invite { peer, key, addrs, relays }` ⇄ a `nb:` base64url link for QR codes,
  URLs or signaling notes.
- **Addresses** implement `Address` (encode/decode, optional `port_offset`). `SocketAddr`
  does out of the box.

## Tests and examples

`cargo test --release -p nobaas-core -p nobaas-sim` covers:
- 16 unit tests, including frame-parser fuzzing and the RFC 5769 STUN vectors.
- 14 endpoint scenarios: handshake timing, loss/duplication/reordering, tampering, replay,
  wrong identity, simultaneous open, idle timeout, peer restart, HS1 replay, roaming,
  200k fuzzed datagrams, a 300-peer hub, and determinism.
- 10 NAT scenarios (`tests/nat.rs`), including an 8×8 NAT-type matrix printed with
  `-- --nocapture nat_matrix`.
- 2 arena regression runs (`tests/arena.rs`).

Examples:
- `cargo run --release -p nobaas-core --example udp_pair [count] [unreliable]`: real UDP
  loopback throughput.
- `cargo run --release -p nobaas-core --example stun_probe`: asks two public STUN servers for
  your address and classifies your NAT.
- `cargo run --release -p nobaas-sim --example arena -- --players 512 --supers 8`:
  super-peer lockstep arena with flags for latency, loss, uplinks, redundancy and more.

## Not yet

- Congestion control (fixed 256-fragment window).
- Cookies against handshake floods (only a cap so far).
- Rekeying.
- Timer wheel (timers are an O(sessions) scan).
- Browser transport (WebRTC DataChannel), Nostr and mDNS signaling, overlay and bindings.
