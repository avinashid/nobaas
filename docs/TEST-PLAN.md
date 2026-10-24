# Test plan

How Peerwire is tested, what each layer covers, and the gate every change passes before merge.

## The gate

One command runs it:

```sh
scripts/gate.sh             # everything below that stays on loopback
scripts/gate.sh --quick     # skip the browser tests (no Playwright needed)
scripts/gate.sh --strict    # clippy with -D warnings, and the fuzz tests again in a debug build
scripts/gate.sh --nostr     # also the public-relay Nostr test; only when a change touches signaling
scripts/gate.sh --full      # the Rust steps even when no Rust changed since origin/main (GATE_BASE)
```

**Who runs what** (details in `AGENTS.md`): the author runs `--quick` (with `--strict` after a Rust change) plus
the browser tests next to the change. A reviewer reads the diff and re-runs a test only to reproduce a problem.
The lead runs `--strict` once on the merged `main` before a push. CI runs the Rust and Node steps but not the
browser tests.

| Step | Command | Passes when |
|---|---|---|
| Rust tests | `cargo nextest run --workspace --release --profile gate` and `cargo test --doc --workspace --release` (`cargo test --workspace --release` without cargo-nextest) | every test passes (ignored tests are known gaps, below) |
| Lints | `cargo clippy --workspace --all-targets` (`-- -D warnings` with `--strict`) | no errors; `--strict` is clean today |
| Fuzz, debug build (`--strict`) | `cargo nextest run --profile gate --lib -p peerwire-core -p peerwire -p peerwire-ffi -p peerwire-sim fuzz` (or `cargo test` with the same arguments) | the parser fuzzers (below) pass where integer overflow panics; release builds wrap and hide it |
| Format | `cargo fmt --all -- --check` | no diff (skipped if rustfmt is not installed) |
| wasm build | `packages/js/build.sh` | builds `packages/js/pkg/peerwire_ffi.wasm` |
| wasm size and imports | `WebAssembly.Module.imports` in Node | ≤ `WASM_MAX_BYTES` (350 KB) and 0 imports |
| Node glue | `node packages/js/test/node_pair.mjs` | two endpoints connect; 1000 × 64 B and one 64 KiB reliable message arrive, the large one intact |
| Node wasm source | `node packages/js/test/wasm_source.mjs` | `loadPeerwire` in Node reads an absolute path, a `file:` URL (string or URL) and bytes from disk as before; a missing path fails with ENOENT (disk, not fetch); an `http:` URL is fetched, and a 404 rejects with `HTTP 404` |
| Node privacy | `node packages/js/test/privacy.mjs` | host candidates are filtered; a relay-only endpoint links through its relay and refuses a direct dial |
| Node room | `node packages/js/test/room_node.mjs` | three `joinRoom` peers mesh; presence, lanes, targeted sends, backpressure, leave, a late joiner and `poll()` work |
| Node peer ids | `node packages/js/test/peer_id.mjs` | SEC-1: an over-long or malformed peer id (the 8192-character repro, wrong sizes, uppercase, non-strings) throws a TypeError at every wasm copy and the module keeps working; BroadcastChannel, Nostr, WebRTC, local and room signalling drop a `from` that isn't 64 lowercase hex |
| Node pump re-arm | `node packages/js/test/pump_rearm.mjs` | PERF-1: the endpoint re-arms its timer whatever `onTransmit` or `onEvent` do; a reliable message sent while the transport throws, refuses, or has a DataChannel over `MAX_BUFFERED` (1 MiB) queued arrives after the outage by retransmit, well inside the 10 s keepalive; `WebRtcTransport` counts those drops in `congested` |
| Node nostr bounds | `node packages/js/test/nostr_bounds.mjs` | PERF-2: 12,000 distinct relay events keep `NostrSignaling.seen` at `MAX_SEEN` (4096), oldest evicted first, recent duplicates still dropped; ids that are not 64 lowercase hex (including 4096 events with 64 KiB ids) never enter the set, and non-array tags are ignored without throwing; base64 round-trips 100 KB and 1 MB |
| Node endpoint closed | `node packages/js/test/endpoint_closed.mjs` | PERF1-2: after `Endpoint.close()` no public method calls into wasm (counted per export): late `receive` datagrams and `addCandidate` are ignored, `send`/`connect`/`connectVia` throw `endpoint closed`, `secret`/`seal`/`open`/`rttMs` return null, `isConnected` false, `stats` the counters at close; a second `close()` frees nothing; other endpoints on the module keep working |
| Node fuzz | `node packages/js/test/fuzz.mjs` | NB-014: the JS parsers (below) never throw or leave a rejection unhandled on random and mutated input |
| Node scratch buffers | `node packages/js/test/scratch_buffers.mjs` | PERF-3: `send`/`receive` reuse the endpoint's `sendBuf` (16 KiB) and `rxBuf`: messages arrive intact back to back across the 0 B, 1232 B and 16 KiB boundaries, 3-byte strings that just fit or just miss, and after wasm memory grows (detached views); no `pw_alloc` on the hot path; `toHex`/`fromHex` tables agree with the old code, non-hex input included |
| Node room drain | `node packages/js/test/room_drain.mjs` | PERF-4: 6 MiB sent at once between two rooms (over the 4 MiB reliable send window) arrives intact and in order; the frames that waited in the outbox were moved by the endpoint's `onDrain` hook (incoming datagrams, ACKs among them), not the 2 ms timer, which stops once the outbox is empty; with no hook the timer alone still delivers. `scratch_buffers.mjs` covers `sendRetry` (no copy on retry, freed at `close()`) |
| Node room leave | `node packages/js/test/room_leave.mjs` | a room that leaves while 6 MiB wait behind its full send window has said goodbye and closed before `leave()` returns (nothing waits on a timer or await, which a closing page never runs); both peers see `peer-leave` 'left' at once. With the Bye dropped, the core's sealed close (`Endpoint.disconnect`) alone still gives 'closed' at once. The `pagehide` handler leaves, then holds the page 20 ms (only if it had peers) |
| Node room adopt | `node packages/js/test/room_adopt.mjs` | `room.adoptChannel`, with a fake RTCPeerConnection: a link over signalling, or relayed by a member, moves onto a channel both ends adopt (100 KB with no pipe batch; the member relays nothing more); when the channel closes, the link goes back (signalling at once, the member after the 15 s direct timeout) and carries on; a channel one end adopts is refused after 5 s and left open; a direct link isn't moved |
| Node room same NAT | `node packages/js/test/room_same_nat.mjs` | members behind one public IPv4 (a fake RTCPeerConnection whose descriptions name it as srflx, or trickle it 1 s late on either side, and whose ICE never connects): `same-nat` traced on both sides, the dialler falls back 3 s after its offer, not 8, and the pair links over signalling; different public IPv4s still 8 s. The pair's DataChannel opening after the fallback moves a link over signalling (pipe batches 300 ms late) or relayed by a member to `direct`, which stays direct while late pipe batches arrive, with no pipe batch after; a pair that opens at once never falls back |
| Node room retry | `node packages/js/test/room_retry.mjs` | Fast lane 1, with a fake RTCPeerConnection whose first attempts fail: a pair over signalling, or relayed by a member, goes direct on the backoff's retry with the same session (one `connected`, no `peer-leave`), data over the channel and none over the pipe; a failed retry closes quietly (no `'failed'`, still on the pipe) and the next, twice as late, goes direct; the larger id's `retryDirect()` asks the dialling side; a `'public-only'` retry's offer and answer carry no host candidate or LAN address; a microphone grant retries only in a `'direct'` room without `retryOnGrant: false`; `online` and a new connection type retry, a same-type `change` doesn't; `leave()` stops them |
| Node retry backoff | `node packages/js/test/retry_backoff.mjs` | the retry schedule (`src/retry.js`) on a fake clock: 20, 40, 80, 160 s then every 5 min, each within ±20 %; peers in turn, one in flight, starts ≥ 2 s apart; a silent retry frees the slot after its hold; requests heeded once per 20 s; 20 failing peers for an hour: never two at once, ≤ 8 starts a minute, about one per peer per 5 min at the cap |
| Browser | `node packages/js/test/browser.mjs 2` | two Chromium tabs form a WebRTC mesh on loopback and exchange data |
| Arena | `node packages/js/test/arena.mjs 8 10` | every island's checkpoint hashes match, ticks keep confirming |
| README chat | `node packages/js/test/readme.mjs` | the chat block in `packages/js/README.md` runs in two tabs, messages cross both ways, and it stays ≤ 10 lines |
| wasm URL | `node packages/js/test/wasm_url.mjs` | two Chromium tabs join with `wasm: '/pkg/peerwire_ffi.wasm'` (root-relative, as a bundler's `?url` import gives) and a message crosses; a relative path, an absolute URL and a URL object load; a path that 404s rejects with `HTTP 404` |
| Poll | `node packages/js/test/poll.mjs 3` | three tabs driven only by `poll()`/`sendTo()` see each other; a closed tab leaves |
| WebRTC signals | `node packages/js/test/webrtc_signals.mjs` | NB-014: malformed offers leave no link and no unhandled rejection; 300 random signals neither throw nor reject; real offers are still answered |
| Retry direct | `node packages/js/test/retry_direct.mjs` | two Chromium tabs whose first RTCPeerConnection gets no candidates link over signalling, then go direct on the retry: one session, no leave, chat both ways; with the microphone granted (real local addresses) and `'public-only'`, no signal either tab sends, retries included, names a host candidate or a local address (MIC-1) |
| Tab close | `node packages/js/test/leave.mjs --runs=5 --crash=0` | a tab streaming reliable data to two others is closed; both see its `peer-leave` within 5 s, every run (the Bye and the core's close go out from `pagehide`). By hand: `--runs=10 --stress=3` on a loaded host, and the default `--crash=1` crashes a tab (`chrome://crash`, no `pagehide`), whose leave comes by the 30 s idle timeout |
| Netlab carry | `node packages/js/test/netlab_carry.mjs` | two Net Lab tabs linked over signalling (`?rtc=off`) get a real loopback channel as a birthday punch's winner: both move the room link onto it (route direct, the log names the channel's address and pair), chat goes over it, and when it closes both log the link back on signalling and stay linked; Net Lab's hook passes a closed link's handle on to the room (NB48b-5) |
| Netlab mic | `node packages/js/test/netlab_mic.mjs` | Net Lab's "Use microphone" button with fake media and mDNS hiding on: before, `mediaPermission` 'prompt' and `.local` addresses; after a stored grant and the click, 'granted', real local addresses with the same NAT reading, the pair (over signalling until then) retries WebRTC with no rejoin (the same ids, no leave), and the other tab sees `mic: true`, real local addresses and a direct link; nothing thrown. The global room (MIC-1), with the grant stored before the tabs open or given while they are in it: the other tab gets no local address (`theirHosts` empty, read after Net Lab notes them) and the pair still links; a birthday punch's offer carries no `a=candidate` line. The button pressed in the global room rejoins before its network re-test (held 5 s): a tab that joins during the re-test hears none of the local addresses in any description or candidate (MIC-4). A refused prompt (`--use-fake-ui-for-media-stream=deny`): 'denied', a warning, no rejoin. A hidden local address takes its pair's family from the remote |
| Netlab punch close | `node packages/js/test/netlab_punch_close.mjs` | MIC-2: a peer (a Net Lab tab driving its live Room) aborts a birthday punch while the other tab's setup is held 1 s: as the hard side while its certificate is made (no fleet opens) and while its fleet opens, as the easy side while its spray connection opens and while it applies the answer. Every connection made after the end is `closed`, and the card keeps "the peer stopped", not a stale error. A second offer straight after the first opens no second fleet (exactly `fleet` connections, all closed after the abort), and a second answer leaves the punch running; the spray's candidates (at TEST-NET-3) are dropped in the tab, so no check leaves the host |
| Nostr (opt-in) | `node packages/js/test/nostr.mjs` | two browser contexts on this host find each other via `nos.lol` / `relay.primal.net` / `nostr.mom` and connect over public STUN |

The script runs every step even after a failure, prints a PASS/FAIL/SKIP summary and exits non-zero if any step
failed. It builds first (wasm, clippy, the Rust tests with `--no-run`), then runs the Rust tests beside the Node
tests (four at a time) and, once those are done, beside the browser tests (one at a time). Each step logs to
`target/gate-logs/<step>.log`. A full `--strict` run takes about 3 minutes on the 4-core aarch64 dev host with a
warm build: 163–174 s over four runs on `f253797`, against 273 s for the old one-step-at-a-time script on the same
commit. The Rust tests take 27 s under cargo-nextest and the debug fuzz run about 30 s; the Node tests take 22 s,
set by `room_adopt.mjs`; the browser tests take 130–145 s and set the total. A fresh worktree compiles the
workspace first and takes several times that. With no Rust change since `origin/main`, the Rust steps are skipped.

**Release gate:** `scripts/gate.sh --strict` green on `main`, plus `--nostr` once per release, plus the CI matrix
(below) green on all three OSes.

## Test layers

| Layer | Where | What it is for |
|---|---|---|
| Unit | `#[cfg(test)]` in `crates/*/src` | parsers, codecs, crypto vectors, reliability state machine |
| Crate integration | `crates/<crate>/tests/` | public API of one crate, including seeded property tests |
| Simulation | `crates/peerwire-sim/tests/` | whole endpoints over a deterministic network: loss, jitter, NATs, roaming, relays, arena |
| JS glue | `packages/js/test/node_pair.mjs` | the wasm C ABI and `peerwire.js` in Node |
| Browser | `packages/js/test/{browser,arena}.mjs` | WebRTC transport and the demo pages in headless Chromium, loopback only |
| Public network | `packages/js/test/nostr.mjs` | Nostr signaling and public STUN; small, opt-in |

### Rules

- **Deterministic.** Simulation tests take a seed (`same_seed_same_run` checks that a run replays exactly).
  Property tests use `ChaCha8Rng` seeded from a fixed constant XOR a per-test salt, so a failure reproduces
  by rerunning the test; the failing input is in the assertion message.
- **Small.** Property tests run ~2000 cases; simulations stay at a few hundred peers and a few simulated seconds.
  Larger scales go in `docs/PERFORMANCE.md` as **theoretical**, with the formula.
- **Local.** Loopback, LAN or the simulator. Only `nostr.mjs` touches public services, the allowed ones.
- **Known gaps are tests too.** A behaviour we know is wrong gets an `#[ignore = "known gap: …"]` test that
  asserts the intended behaviour. Fixing the gap means removing the `ignore`. `cargo test -- --ignored` runs them.

## Coverage by component

### peerwire-core (unit, `src/`)

- `wire`: frame round trips, truncated frames fail once, frame parser survives garbage, packet bounds.
- `reliable`: out-of-order reassembly, ACK and retransmit, tail-loss probe beats the RTO, urgent fragments are
  sent twice, urgent goes ahead of queued reliable data within the shared window and takes at most half of it,
  each stream stays within the receiver's window and has its own buffer, urgent fragments in flight span at
  most the 64-fragment urgent window, the urgent receiver buffers at most that and refuses a message over 64 KiB,
  the window grows by the slots acknowledged, buffer limit.
- `stun`: RFC 5769 IPv4/IPv6 vectors; Peerwire packets are not mistaken for STUN.
- `crypto`, `identity`, `sealed`, `invite`, `addr`: Noise static key derivation, sign/verify, replay window,
  sealed-box open/reject, RFC 4648 vectors, invite round trip and garbage rejection, socket address round trip.

### peerwire-core over the simulator (`crates/peerwire-sim/tests/endpoint.rs`, `nat.rs`)

- Handshake in one RTT; wrong identity fails; simultaneous open converges to one link; replayed handshake does
  not displace a live link; peer restart replaces the stale link.
- Reliable lane exact and ordered under loss; unreliable lane at most once; bulk transfer keeps the queue short
  for game traffic.
- Tampered and replayed packets rejected; fuzzed datagrams never panic or break the link.
- Keepalive, idle timeout, heavy loss, roaming to a new address, a hub with 300 peers.
- NAT: reflexive address discovery, mapping expiry, relayed dial then punch, relay refusal, symmetric pairs stay
  relayed, direct-path failure falls back to the relay, LAN candidates behind one NAT, and the full NAT matrix
  (public, full-cone, restricted, port-restricted, symmetric sequential/random, two CGNAT stacks).

### peerwire-game (`crates/peerwire-game/tests/`)

| File | Covers |
|---|---|
| `fixed_point.rs` | int conversion (floors), saturating add/sub vs an i64 reference, mul vs `(a*b)>>16`, mul/div identities, div inverts mul within tolerance, div by zero panics, `q16_sqrt` is the exact floor and monotonic, physics step per input bit, dash, arena clamp, bit-identical replay, line of sight |
| `hilbert.rs` | known order-1/2 indices, bijection and grid-adjacent steps on every grid up to 64×64, range on 65536², partition invariants (each player once, full islands, ids, Hilbert order, non-overlapping ranges), `k_max` clamp, input-order independence with position ties, off-grid positions, spatial locality |
| `desync.rs` | CRC golden values (a change is a wire break), determinism, every hashed field changes the CRC (collisions bounded), order sensitivity, input bits not hashed, quorum known cases (exact 2/3, below 2/3, output order) and a property over island sizes 1–12 |
| `restored.rs` | the checks from the original `p0-hilbert-rollback` tests that still apply (Hilbert neighbours, ten-tick step, the wall-at-x∈[4,6] sight line, 4-peer quorum eviction); the rollback-simulator and witness parts return with NB-018 |
| `lockstep.rs` | 30 players × 600 ticks replayed by peers that registered players in different orders: identical island hashes on every tick; a golden digest of the whole run (a change is a protocol break); one wrong input diverges exactly one island and the quorum names the cheater |

### peerwire-sim arena (`crates/peerwire-sim/tests/arena.rs`)

Lockstep islands agree over super-peers; bundles survive 5 % loss with jitter; islands survive a super-peer
dying; an overloaded super-peer degrades without stalling. `cargo run -p peerwire-sim --release --example arena`
is the full harness for `docs/PERFORMANCE.md`.

### peerwire-ffi and packages/js

`two_endpoints_through_the_c_abi` (Rust), `node_pair.mjs` (wasm in Node), `browser.mjs` and `arena.mjs`
(Chromium, WebRTC over loopback), `nostr.mjs` (opt-in).

## Godot binding (NB-020)

The binding must be testable with plain `cargo test`, with no Godot binary. The design rule that makes this
possible: keep the MultiplayerPeer logic in a Godot-free Rust module (no `godot` types in its API), and make
`PeerwireMultiplayerPeer` a thin GDExtension shell that only converts types and forwards calls. The same logic module
runs on a test double, on `peerwire-sim` and inside Godot.

**Unit tests (logic module, `cargo test`):**

- *Peer IDs:* the host is 1; a joiner's ID comes from its Peerwire PeerId, is in `2..=i32::MAX`, and stays the same
  across reconnects. Collisions are forced with an injected hash, and every peer must resolve them to the same IDs
  whatever order the joins arrive in (a property test over shuffled join orders).
- *Packet queue:* FIFO; `_get_available_packet_count` matches the queue length; `_get_packet_peer` belongs to the
  packet `_get_packet_script` returns next. Target peer `0` means broadcast, `-id` means everyone except `id`, and
  an unknown target is an error. Packets from a peer that disconnected before they were drained are handled the
  way the spec decides.
- *Transfer modes:* reliable goes to the reliable lane and unreliable to the unreliable lane. Unreliable-ordered
  never delivers a packet older than one already delivered (property: random reordering, the output sequence is
  strictly increasing). Packets over the unreliable limit (1100 B) are rejected. Ordering holds per channel.
- *Connection status:* disconnected → connecting → connected; `host()` connects immediately and returns an invite
  that parses; a bad invite or a failed join returns to disconnected; `_close` and `_disconnect_peer` raise
  `peer_disconnected` once per peer.

**Simulator tests (`crates/peerwire-sim/tests/godot_peer.rs`):** a host and three joiners on `peerwire-sim` through
the logic module, at 0 % and 5 % loss with jitter and behind the NAT matrix. Every `@rpc`-style reliable packet
arrives exactly once and in order, unreliable-ordered packets never go backwards, a joiner dropping out raises
`peer_disconnected` on the others, and the same seed replays the run exactly.

**Determinism:** lockstep game logic uses `peerwire-game` fixed-point maths, not Godot physics. `lockstep.rs` is
the reference: the binding's demo state hash must match a Rust replay of the same input log.

**Headless Godot smoke test (opt-in, not in the default gate):** Godot 4.7.2 is installed at `~/.local/bin/godot`.
Once `examples/godot/` exists, a `scripts/godot-smoke.sh` starts one `godot --headless` instance that hosts and
writes its invite to a file, and a second that joins with it. They exchange an `@rpc` chat line and a few
synchronizer updates, then both exit 0 within a timeout. It runs on loopback only and gets added as a
`--godot` flag on `scripts/gate.sh`.

## Fuzzing (NB-014)

Every parser of untrusted input is fuzzed on the stable toolchain, without cargo-fuzz (which needs nightly):
seeded random and structure-aware inputs (real messages, then mutated: bit flips, boundary bytes, boundary
u16 lengths, inserts, deletes, duplicated chunks, truncation), with properties checked on whatever a parser
accepts, not only "no panic". The harness is `peerwire_sim::fuzz` (Rust) and the top of `fuzz.mjs` (JS).

```sh
cargo test --workspace --release fuzz                     # in the gate: ~400 k cases, a few seconds per target
cargo test --lib -p peerwire-core -p peerwire -p peerwire-ffi -p peerwire-sim fuzz   # debug: overflow panics
PW_FUZZ_SCALE=20 PW_FUZZ_SEED=7 cargo test --workspace --release fuzz -- --nocapture   # a soak
PW_FUZZ_SCALE=5 PW_FUZZ_SEED=7 node packages/js/test/fuzz.mjs
```

The default seed is fixed, so the gate is deterministic; `PW_FUZZ_SEED` explores others and `PW_FUZZ_SCALE`
multiplies the case counts. A failing Rust case prints its target, index and input as hex, to paste into a
regression test.

| Target | Test | Properties |
|---|---|---|
| Packets, frames | `peerwire-core` `fuzz::fuzz_packet_parse`, `fuzz_frames` | an accepted packet re-encodes to the same bytes and a well-formed one is never rejected; frames yielded re-encode to exactly the bytes consumed, a malformed tail yields one `Err` |
| Addresses | `fuzz_addresses`, `peerwire-sim` `fuzz_sim_addresses` | `SocketAddr`/`u32`/`u64`/`SimAddr` decoding accepts canonical encodings only |
| STUN | `fuzz_stun` | requests and responses (extra attributes too); STUN never parses as a Peerwire packet |
| Invites | `fuzz_invites` | bytes and `pw:` links: whatever parses survives its own round trip, ≤ 16 entries, and is the one encoding of what it decodes to (`to_link(parse(s)) == s`) |
| Sealed | `fuzz_sealed` | only an untouched message sealed to us opens, naming its real sender |
| Hostile peer | `fuzz_hostile_peer` | an authenticated peer sends sealed random frames, garbage, FWD to a bystander and RELAYED nested 12 deep, roaming and jumping time: no panic, a violation drops it and it may reconnect, half-open sessions stay bounded, and the bystander and the peer still exchange messages after |
| Receive memory | `hostile_peer_pins_bounded_receive_memory` | an authenticated peer fills each stream's partial message and out-of-order buffer: a link holds at most 2,308,712 B (urgent 134,836 B), one urgent fragment past 64 KiB of message drops the peer, and an honest 64 KiB urgent message still arrives |
| Hellos, offers | `peerwire` `traversal::fuzz_hello_and_offer_parsers` | round trips; candidates, relays and the prediction window stay within their limits |
| Authenticated signals | `traversal::fuzz_authenticated_signals` | a room-key or invite-key holder's proved hellos and offers, random and mutated: the endpoint never dials loopback, multicast, broadcast, link-local, unspecified or port 0 |
| Gateway answers | `portmap::fuzz_gateway_answers` | PCP, NAT-PMP, SSDP, URLs, IGD XML, HTTP (chunked, lengths at the integer limits): mappings have a port and lifetime, URLs and control paths are printable, SSDP points only at the private host that answered |
| Port mapper | `portmap::fuzz_mapper` | a hostile gateway and LAN hosts: datagrams only to the gateway or the SSDP group, HTTP only to private hosts, request heads printable with a 3-part request line |
| Nostr (Rust) | `nostr::fuzz_relay_messages` | mutated events and JSON nested 100,000 deep: what is passed on has a topic and payload that round-trip |
| C ABI | `peerwire-ffi` `fuzz_the_c_abi` | random call sequences and arguments: documented return codes only, a short event buffer keeps the event for a retry, every event has the documented encoding |
| Godot frames | `peerwire-godot` `fuzz_frames_from_peers` | frames round-trip; a joiner takes its id only from its host's first ASSIGN, queues packets only from known peers, keeps its inbox caps |
| JS | `packages/js/test/fuzz.mjs` | `decodeEvent` matches a reference encoder; hex, peer ids, base64 round-trip; `NostrSignaling.onRelay`, `RoomProof.checkHello`, `Room.onMessage` never throw and leave no rejection unhandled; only real proofs and real senders get through |
| WebRTC signals | `packages/js/test/webrtc_signals.mjs` | see the gate table |

Found and fixed, each with a regression test: a gateway's `Content-Length` near `usize::MAX` overflowed
`portmap::http_complete` (`huge_content_length_is_not_complete`); a relay message `null` threw out of
`NostrSignaling.onRelay` (`nostr_bounds.mjs` 3c); a malformed SDP in an offer or answer became an unhandled
rejection, ending a Node host, and left a dead link (`webrtc_signals.mjs`). Invite links were malleable: base64url
ignored the unused bits of the last symbol, and a count of 17–255 was clamped to 16, so several strings parsed to one
invite; decoding is now canonical (`b64::rejects_non_canonical_tails`, `invite::tests::one_link_per_invite`).

## Known gaps

| Gap | Test | Owner |
|---|---|---|
| Fuzzing is not coverage-guided (cargo-fuzz needs nightly) | — | qa, if the toolchain is approved |
| `browser.mjs` reports round trips and throughput but asserts only that the mesh forms and data arrives | — | qa |
| Planned crates (facade, sync, media, store, Godot binding) have no tests yet; each lands with its own, per the plans above | — | backend |

## CI

NB-002 (ops) owns `.github/workflows/ci.yml`. The jobs should call the same commands as the gate so local and CI
results match:

| Job | Runs on | Command |
|---|---|---|
| test | ubuntu, macos, windows | `cargo test --workspace --release` |
| clippy | ubuntu | `cargo clippy --workspace --all-targets -- -D warnings` |
| fmt | ubuntu | `cargo fmt --all -- --check` |
| wasm + node | ubuntu | `scripts/gate.sh --quick` steps `wasm build`, `wasm size+imports`, `node glue` |
| browser | ubuntu with `npx playwright install --with-deps chromium` | `PLAYWRIGHT=$(npm root)/playwright/index.mjs node packages/js/test/browser.mjs 2` and `arena.mjs 8 10` |

On Linux CI, `scripts/gate.sh --strict` alone covers every job except the macOS and Windows test runs.

## Turning acceptance criteria into tests

Each backlog task's acceptance criteria get a named test before the task moves to review: unit or crate
integration for an API, a simulator test for anything that crosses the network, a browser test for anything in
`packages/js` or `examples/`. The task card lists the test names and the gate output. When reviewing a branch, QA
reads the diff against the code and re-runs a test only to reproduce a problem or try a mutant (see `AGENTS.md`).
