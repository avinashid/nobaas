# nobaas-core

Sans-IO peer-to-peer session core for Nobaas.
Design: [docs/ARCHITECTURE.md](../../docs/ARCHITECTURE.md).

An `Endpoint` holds one identity and any number of encrypted links. It never
opens sockets, spawns threads or reads a clock: the host passes in datagrams
and the time, and takes out datagrams, events and the next timer deadline. The
same code runs on std UDP (the [`nobaas`](../nobaas) crate), in the browser as
wasm (through [`nobaas-ffi`](../nobaas-ffi)), in Godot, and in the deterministic
simulator ([`nobaas-sim`](../nobaas-sim)).

Most applications want the [`nobaas`](../nobaas) crate, which drives an
`Endpoint` from a UDP socket and adds STUN, rooms and NAT traversal. Use this
crate directly to run Nobaas over your own transport or event loop.

## Example

Two endpoints in one process, with the "network" being a loop that hands each
datagram to the other side:

```rust
use nobaas_core::{Config, Endpoint, Event, Identity, Instant, Lane};
use rand_core::OsRng;
use std::net::SocketAddr;

let (a_addr, b_addr): (SocketAddr, SocketAddr) = ("127.0.0.1:1".parse()?, "127.0.0.1:2".parse()?);
let mut a = Endpoint::new(&Identity::generate(&mut OsRng), Config::default(), [1; 32]);
let mut b = Endpoint::new(&Identity::generate(&mut OsRng), Config::default(), [2; 32]);

let now = Instant::from_millis(0);
a.connect(now, b_addr, b.peer_id())?;
let mut got = None;
for _ in 0..10 {
    while let Some(t) = a.poll_transmit(now) {
        b.handle_datagram(now, a_addr, &t.payload);
    }
    while let Some(t) = b.poll_transmit(now) {
        a.handle_datagram(now, b_addr, &t.payload);
    }
    while let Some(e) = a.poll_event() {
        if let Event::Connected { peer, .. } = e {
            a.send(&peer, Lane::Reliable, b"hello")?;
        }
    }
    while let Some(e) = b.poll_event() {
        if let Event::Message { data, .. } = e {
            got = Some(data);
        }
    }
}
assert_eq!(got.as_deref(), Some(&b"hello"[..]));
# Ok::<(), Box<dyn std::error::Error>>(())
```

A real host also calls `handle_timeout(now)` once `poll_timeout()` has passed, and
sleeps until that deadline or the next datagram. `examples/udp_pair.rs` is a
complete host loop on std UDP sockets.

## What it does

- **Identity:** Ed25519. A `PeerId` is the verifying key.
- **Handshake:** `Noise_IK_25519_ChaChaPoly_BLAKE2s`, 1 RTT. The Noise static key is the
  X25519 form of the Ed25519 identity, so dialling a `PeerId` authenticates it with no
  extra signatures.
  - A responder link is not announced until the initiator's first data packet arrives, so a
    replayed HS1 can't displace a live link.
  - A forged HS2 can't poison a dial: the handshake is rebuilt from its seed.
  - Half-open responder handshakes are capped (`Config::max_pending_handshakes`, 4096).
- **Simultaneous open** (both sides dialling, as in hole punching): the dial from the smaller
  PeerId wins, so exactly one link results.
- **Data packets:**
  - Explicit nonces and a 2048-packet anti-replay window, as in WireGuard.
  - Receiver session indices, so links survive address changes (roaming).
  - At most 1232 bytes (`MAX_DATAGRAM`), which fits the IPv6 minimum MTU.
- **Lanes:**
  - *Reliable*: ordered, exactly once, fragmented up to `Config::max_message` (1 MiB).
  - *Urgent*: ordered with *Reliable*, but each fragment is sent twice, 4 ms apart, so a
    single loss costs nothing. For small, latency-critical events; it arrives as `Lane::Reliable`.
  - *Unreliable*: at most once, up to 1100 B (`MAX_UNRELIABLE`), drop-oldest queue.
  - Frames from every lane and ACKs are packed into one packet.
- **Loss recovery and congestion control** on the reliable lanes:
  - Selective ACKs (delayed at most 25 ms) and an RFC 6298 RTO (300 ms initial, 20 ms to 5 s).
  - Fast retransmit when a fragment is 3 behind the highest acknowledged one and past a
    RACK-style reordering window (RFC 8985).
  - A tail-loss probe resends the newest fragment when ACKs stop, so a lost tail costs
    about one RTT instead of an RTO.
  - A NewReno-style AIMD congestion window (16 fragments to start, 4 at the floor), capped
    by `Config::reliable_window` (1024). It also backs off when queueing delay passes 25 ms,
    before any loss, which keeps router buffers short for real-time traffic.
  - The sender keeps in-flight fragments in ordered indexes by retransmission time
    (`by_due`) and urgent-copy time (`by_dup`), so each packet visits only what is due.
- **Liveness:** keepalive every 10 s, then a probe every 1 s while the peer is silent, and
  an idle timeout after 30 s. A re-handshake from a restarted peer gives
  `Disconnected { Replaced }` and then `Connected`.
- **Bounds:** the send buffer (4 MiB per link), receive window, message size, unreliable
  queue, relay queue and half-open handshakes are all capped.
- **Paths:** a link is `Path::Direct(addr)` or `Path::Via(relay)`.
  - `connect_via(now, relay, peer, candidates)` dials through a peer both sides are linked to.
    The relay forwards opaque Noise packets: one hop, with a token bucket of 256 KiB/s and a
    64 KiB burst per peer. `Config::relay` turns relaying off. The relay also tags each packet
    with the address it saw the sender at, which gives the receiver a hole-punch candidate.
  - A relayed link probes its candidates with PUNCH (every 100 ms for 5 s, optionally
    spraying ports above each with `Config::punch_spray`). It moves to direct only on an
    echoed PUNCH_ACK, so a one-way path is never used.
  - `set_guesses` adds port-prediction guesses for a peer behind a symmetric NAT. Each is
    probed three times and becomes the path only if the peer answers from it.
  - A direct link that goes silent for 15 s falls back to its relay. `Event::PathChanged`
    reports each move.
  - `connect_with(now, peer, &[paths])` sends the handshake down several paths at once.
    Two peers dialling each other's public address this way punch through cone NATs with
    no relay.
  - `add_candidates` adds addresses learned later (LAN, signaling).
  - `set_relay_only(true)` hides this endpoint's address from everyone but its relay: no
    punching, no direct candidates, no address sharing.
- **Public address:** every direct peer reports where it sees you (`Event::AddrObserved`),
  so any peer works as STUN. `stun` has a sans-IO RFC 5389 client codec for real STUN
  servers. It demultiplexes against Nobaas packets on the same socket (RFC 7983 first-byte ranges).
- **Sealed messages:** `sealed::seal` and `sealed::open` encrypt a one-shot message to a
  `PeerId` and authenticate the sender (`Noise_X_25519_ChaChaPoly_BLAKE2s`), for signaling
  over untrusted carriers. They have no replay protection of their own.
- **Invites:** `Invite { peer, key, addrs, relays }` ⇄ a `nb:` base64url link for QR codes,
  URLs or signaling notes. The optional `key` lets a peer prove, in signaling offers, that
  it holds the invite.
- **Stats:** `stats()` counts packets, drops and relaying for the whole endpoint.
  `peer_stats(peer)` reports per link: RTT, congestion window, fragments in flight,
  retransmits, delay backoffs and a loss estimate.
- **Addresses** implement `Address` (encode/decode, optional `port_offset`). `SocketAddr`
  does out of the box.

## Tests and examples

`cargo test --release -p nobaas-core -p nobaas-sim` covers:
- Unit tests: frame-parser fuzzing, the RFC 5769 STUN vectors, invite and sealed-message
  round trips, and the reliable sender (congestion window, tail-loss probe, and the
  `by_due`/`by_dup` indexes checked against full scans on a randomized lossy run).
- Endpoint scenarios in the simulator (`crates/nobaas-sim/tests/endpoint.rs`): handshake
  timing, loss/duplication/reordering, tampering, replay, wrong identity, simultaneous open,
  idle timeout, peer restart, HS1 replay, roaming, fuzzed datagrams, a 300-peer hub and
  determinism.
- NAT and traversal scenarios (`tests/nat.rs`, `tests/traversal.rs`), including an 8×8
  NAT-type matrix printed with `-- --nocapture nat_matrix`.
- Bulk-transfer and arena regression runs (`tests/bulk.rs`, `tests/arena.rs`).

Examples:
- `cargo run --release -p nobaas-core --example udp_pair -- [messages] [reliable|unreliable]`: real UDP
  loopback throughput.
- `cargo run --release -p nobaas-core --example stun_probe`: asks two public STUN servers for
  your address and classifies your NAT.
- `cargo run --release -p nobaas-sim --example bulk`: a bulk transfer with urgent messages
  on a simulated link with a 100 ms RTT and 100 Mbit/s uplinks.
- `cargo run --release -p nobaas-sim --example arena -- --players 512 --supers 8`:
  super-peer lockstep arena with flags for latency, loss, uplinks, redundancy and more.

## Status

Early development (v0.1). The wire format may still change.

Not yet:
- Rekeying.
- Cookies against handshake floods (only the half-open cap so far).
- A timer wheel: `poll_timeout` scans every session, O(sessions).
- Relaying over more than one hop.
