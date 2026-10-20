# Performance model

This page separates two kinds of numbers:

- **Measured**: produced by code in this repository (the deterministic simulator in `crates/nobaas-sim`,
  real UDP loopback, or headless browsers), with the command that reproduces them.
- **Theoretical**: worked out from the wire format and published figures for links and crypto. These are
  planning estimates, not benchmarks. Each one shows its formula so you can plug in your own numbers.

Real networks (CGNAT, mobile radio, Wi-Fi contention) will be worse than the simulator. Treat the theoretical
column as an upper bound until a real-device test confirms it.

## 1. Cost of a packet

| Item | Bytes |
|---|---|
| IPv4 + UDP header | 28 (IPv6: 48) |
| Nobaas DATA header: kind, receiver index, nonce | 13 |
| AEAD tag (ChaCha20-Poly1305) | 16 |
| Frame header: unreliable / reliable | 3 / 8 |
| Largest datagram | 1232 (fits the IPv6 minimum MTU) |
| Extra in browsers: DTLS record + SCTP DATA chunk (WebRTC) | ~65 |

So a message of `s` bytes costs about **`s + 60` bytes natively** and **`s + 125` bytes in a browser**. Messages
sent in the same tick are packed into one packet, so the per-packet part is paid once per tick, not once per
message.

## 2. Core engine (measured)

| What | Result | How |
|---|---|---|
| Reliable messages over UDP loopback, one core (aarch64) | ~1.2 M msg/s | `cargo run --release -p nobaas-core --example udp_pair` |
| Handshake | 1 RTT (Noise IK) | `tests/endpoint.rs` |
| WebAssembly module | ~300 KB, 0 imports | `./packages/js/build.sh` |
| Two endpoints in Node: connect, then 1000 × 64 B + 64 KiB | connect 53 ms, transfer 51 ms | `node packages/js/test/node_pair.mjs` |
| Super-peer dies mid-game | detected in 267 ms; affected players' p99 316 ms; 0 stalls; every event delivered | `tests/arena.rs` |

**Theoretical ceiling.** ChaCha20-Poly1305 runs at roughly 1–2 GB/s per modern core. A full 1232 B packet
therefore costs about 0.6–1.2 µs of crypto, so the ceiling is roughly 1 M full packets/s, or 8 Gbit/s, per core.
The NIC or the uplink runs out long before the CPU does. Bulk reliable transfers are an exception today (see below):
the reliable lane's scans cost more than the crypto.

### Bulk transfer on a long, fast link (measured, simulator)

Setup: two hosts, 100 ms RTT, 100 Mbit/s uplinks. 64 MiB is sent as 1 MiB `Reliable` messages that keep the send
buffer (4 MiB) full, and a 64 B `Urgent` message goes every 20 ms. The default `Config` is used: a window of 1024
fragments of 1100 B, so the ceiling is about 11 MB/s at 100 ms. CPU is wall-clock time inside each endpoint, on one
aarch64 core.

| Link | Throughput | Peak cwnd / inflight | Delay backoffs | Retransmits | Urgent p50 / p99 | Sender CPU per fragment |
|---|---|---|---|---|---|---|
| Clean | 9.18 MB/s (7.3 s) | 1511 / 1025 | 1 | 0 | 50 / 130 ms | 5 µs |
| 1% loss | 0.20 MB/s (332 s) | 336 / 336 | 0 | 966 | 72 / 131 ms | 5 µs |
| 0–10 ms jitter, drawn per packet (reorders) | 7.10 MB/s (9.5 s) | 970 / 970 | 2 | 3 | 56 / 119 ms | 6 µs |
| 0–10 ms jitter, in order | 5.85 MB/s (11.5 s) | 865 / 865 | 2 | 0 | 60 / 128 ms | 5 µs |
| Clean, 1 Gbit/s uplink | 10.39 MB/s (6.5 s) | 60353 / 1026 | 0 | 0 | 50 / 130 ms | 5 µs |

Inflight counts window slots: a reliable fragment is one, an urgent fragment its share of one by wire size. The
jitter rows depend on the seed (the table is seed 1): over seeds 1–60 they average 7.39 MB/s per packet and
5.42 MB/s in order, against 7.47 and 5.58 MB/s before PERF-9 (`a6e4a69`), with Urgent p99 120–128 ms against
963–1178 ms. For example:
`for s in $(seq 1 60); do cargo run -q --release -p nobaas-sim --example bulk -- --jitter 10 --seed $s; done`.

What limits it:
- **One delay backoff costs 15% of the window.** Slow start puts 1024 fragments per RTT on the wire, about
  103 Mbit/s, so the uplink queues. Queueing delay over `QUEUE_TARGET` sets ssthresh to 85% of inflight (about
  870), and congestion avoidance then adds about one fragment per RTT. Before PERF-7B the cut halved the window
  (to about 512), which held the clean run to 5.94 MB/s and the jitter runs to 3.26 and 2.89 MB/s.
- **Loss caps the window, as in any Reno-style sender.** A sender that halves on every loss settles near
  `1.22 / √p` ≈ 12 fragments at 1% loss, which is 0.13 MB/s at 100 ms (the Mathis bound; the run matched it before
  PERF-7V). Since PERF-7V a loss found by fast retransmit while the latest RTT sample showed under 5 ms of queueing
  is taken as random, as in TCP Veno, and keeps 80% of the window: keeping a share `b` gives
  `√((1+b) / (2(1−b)p))`, **theoretical** 2.12 / √p ≈ 21 fragments, 0.23 MB/s. The run gets **0.20 MB/s**,
  because a timeout still halves, and many of its timeouts are spurious: the 64-fragment SACK bitmap can't report
  fragments further above a hole while the hole is repaired. A buffer that overflows usually shows in the RTT
  first, so that loss still halves; behind a buffer shorter than 5 ms congestive loss looks random too and keeps
  80%, as it does for every sender on that link.
- **Before PERF-8**, fast retransmit resent a lost fragment on every later ACK, so it reached `MAX_TRIES` in one
  round trip and the link was dropped after 0.54 s. Each copy is now resent once, and only after an ACK for a
  fragment sent after it; reordering within a quarter of the minimum RTT (RACK's window) is not taken as loss, so
  per-packet jitter costs no retransmissions.
- **Urgent messages don't wait behind queued bulk data.** Before PERF-9 they shared the reliable sequence, so their
  latency was the time to drain the send buffer (p50/p99 326/875 ms clean, 18/23 s at 1% loss). Urgent now has its
  own sequence and send buffer and takes free window slots first, so it waits only for the path: 50 ms one way,
  plus the queue that slow start builds before the first backoff (the p99). At most half the congestion window goes
  to urgent fragments while reliable data waits, and they count by wire size, so 50–200 small inputs a second take
  a few slots, and the window grows by the slots acknowledged. With 64 B urgent messages at 200 Hz: clean
  9.18 MB/s and 50/130 ms, against 9.00 MB/s and 333/886 ms before; at 1% loss 0.19 MB/s and 84/145 ms, against
  0.17 MB/s and 21/27 s.
- **Sender CPU no longer grows with fragments in flight.** Before PERF-6, `RelSend::has_sendable`, `next_due` and
  `write_frames` walked every in-flight fragment on each packet: about 68% of the sender's CPU at 100 Mbit/s and
  81% at a steady 1024 in flight (19 and 28 µs per fragment). They now read ordered indexes of retransmission and
  duplicate times, and perf (frame pointers, inline-expanded stacks) puts them at **3–4%** of the process, about
  **7–9%** of the sender. At about 6 µs per fragment one core sends roughly 190 MB/s. The simulator output is
  unchanged.

Reproduce with `cargo run --release -p nobaas-sim --example bulk` (all variants), or one variant with flags such
as `-- --loss 0.01 --cpu 1 --trace 1`. The CPU split is `scripts/perf-relsend.sh [flags]`, for example `--up 1000`
(set `PERF="sudo perf"` where unprivileged profiling is off). It builds with frame pointers and debug info, records
with `perf record --call-graph fp`, and counts the samples whose inline-expanded stack contains each function.

## 3. Multiplayer games (measured, simulator)

Setup: 12-player islands, 60 Hz lockstep, 20 ms + 0–10 ms jitter, 1% loss, 20% of peers behind symmetric NAT,
250 ms router buffers.

| Super-peer uplink | Players at p99 lag < 120 ms | Players still in sync (no stalls) |
|---|---|---|
| 10 Mbit/s | **96** (p50/p99 70/109 ms) | 192 (113/335 ms) |
| 20 Mbit/s | **192** (72/116 ms) | 256 (78/216 ms) |

- **512 players, 8 super-peers:** lag p50/p99 59/79 ms, projectile p99 96 ms, 0.06% late events. Each super-peer
  sends 5.4 Mbit/s up; each player sends ~40 kbit/s up (~440 kbit/s with the direct island mesh on).
- **No symmetric NATs:** lag 34/51 ms and super-peer upload 2.7 Mbit/s.
- **Scale:** 4096 players simulated; per-node load stays flat because islands are independent.
- **Overload:** stalls begin at about 2.5× oversubscription. Below that, overload control lowers the update rate
  instead of stalling.

Reproduce with `cargo run --release -p nobaas-sim --example arena -- --players 512 --supers 8` (add `--help`
for the flags).

**Rule of thumb (theoretical).**
- Players per super-peer ≈ `0.8 × uplink / (60 Hz × bundle bytes × 8)`.
- A bundle is one packet to each player, carrying the other island members' run-length-encoded inputs: about
  60 B of overhead plus ~10 B per member, so ~170 B.
- The 0.8 leaves headroom so queues stay short and p99 latency stays low.
- With 10 Mbit/s that gives **~98 players**, close to the measured 96.
- For 64 players it predicts 5.2 Mbit/s, close to the measured 5.4 Mbit/s.

## 4. Whiteboard and collaborative documents (theoretical)

Assume a stroke update of 64 B sent 20 times a second while someone draws.

- **Per-pair cost:** (64 + 125) × 20 × 8 ≈ **30 kbit/s** in a browser.
- **Full mesh**, where everyone sends to everyone: per-peer uplink ≈ `(N − 1) × 30 kbit/s`.
  - On 5 Mbit/s: **~160 active drawers**, by bandwidth.
  - In practice browsers cap at roughly 50–100 RTCPeerConnections, so the mesh limit is **~50 peers**.
  - Most peers are idle at any moment, which helps further.
- **Beyond ~50 peers:** a super-peer fans updates out, so each client keeps one link.
  - Each tick it sends every viewer one packet holding all active drawers' updates, so per viewer it costs
    `20 Hz × (125 + 64 × D) × 8` bits/s, where `D` is the number of people drawing at that moment.
  - Viewers per super-peer ≈ `0.8 × uplink / that`. With 20 Mbit/s up: **~530** viewers with 1 drawer,
    **~225** with 5, **~70** with 20.
  - The super-peer should be a desktop or native peer, not a browser tab, because of the connection cap above.
    For more viewers, use the tree fan-out in §6.
- **Sync latency:** one network hop (RTT/2), plus one tick if updates are batched (≤ 50 ms at 20 Hz).

## 5. Chat (theoretical)

- **Messages are tiny.** A 200 B message costs ~325 B on the wire per recipient.
- **Rooms:**
  - A 50-person room in a full mesh costs the sender ~16 KB per message.
  - Larger rooms use a super-peer or gossip tree, where each peer forwards to a fixed fan-out `d`.
- **Gossip delivery:** `log_d(N)` hops.
  - With `d = 6`, a 10,000-member room is reached in **~5–6 hops**, about 150–300 ms on typical internet RTTs.
- **Offline delivery:** messages for an offline peer are stored by peers who share the room, up to a quota per
  peer. They are erasure-coded so any k of n holders can rebuild them. This is planned (`nobaas-store`); see the
  roadmap.

## 6. Meetings: audio and video (theoretical)

Media runs over WebRTC media tracks in browsers. Nobaas does the signaling, identity and topology.

| Stream | Typical bitrate |
|---|---|
| Opus audio | 32 kbit/s |
| Video 360p / 720p / 1080p | 0.5 / 1.5 / 3 Mbit/s |

- **Mesh**, where each peer sends its stream to every other: per-peer uplink = `(N − 1) × bitrate`.
  - 720p on 10 Mbit/s: **~6 participants**.
  - 360p: ~20 by bandwidth, but CPU (encrypting N−1 copies) limits it to about **8–12**.
- **Peer SFU**, where the participant with the best uplink forwards the streams:
  - It receives N streams and sends N × (N − 1).
  - With simulcast at 360p and 50 Mbit/s up, that is about **10 active video participants**, with audio for many more.
- **Webinar or broadcast:** one speaker, and viewers forward to `d` others.
  - At 1.5 Mbit/s and `d = 4`, each relaying viewer needs 6 Mbit/s up.
  - 1000 viewers sit at depth `log_4(1000)` ≈ 5, adding **~100–200 ms** of latency.

## 7. Connection setup (theoretical, plus measured where noted)

| Path | Time to first message |
|---|---|
| LAN or same browser (BroadcastChannel) | 1 RTT handshake + signaling ≈ **< 50 ms** (measured in browser tests) |
| Internet, both peers on cone NAT | signaling (Nostr relay ≈ 100–300 ms) + ICE/hole punch (1–2 RTT) + 1 RTT ≈ **0.3–1 s** |
| One side behind symmetric NAT | as above, through a peer relay; upgrades to direct if a punch succeeds |
| Both sides behind symmetric NAT | stays relayed through a mutual peer: adds one hop of latency |

## What would change these numbers

- **Better:** IPv6 (no NAT, so direct paths), QUIC/GSO batching, or a super-peer on wired fibre.
- **Worse:** mobile radio (RTT 50–150 ms, bursty loss), Wi-Fi contention, or carrier-grade NAT with short timeouts
  (keepalives cost more).
