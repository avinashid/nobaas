# Test plan

How Nobaas is tested, what each layer covers, and the gate every change passes before merge.

## The gate

One command runs it:

```sh
scripts/gate.sh             # everything below that stays on loopback
scripts/gate.sh --quick     # skip the two browser tests (no Playwright needed)
scripts/gate.sh --strict    # clippy with -D warnings
scripts/gate.sh --nostr     # also the public-relay Nostr test; only when a change touches signaling
```

| Step | Command | Passes when |
|---|---|---|
| Rust tests | `cargo test --workspace --release` | every test passes (ignored tests are known gaps, below) |
| Lints | `cargo clippy --workspace --all-targets` (`-- -D warnings` with `--strict`) | no errors; `--strict` is clean today |
| Format | `cargo fmt --all -- --check` | skipped while rustfmt is not installed on the dev host; CI runs it |
| wasm build | `packages/js/build.sh` | builds `packages/js/pkg/nobaas_ffi.wasm` |
| wasm size and imports | `WebAssembly.Module.imports` in Node | ≤ `WASM_MAX_BYTES` (350 KB) and 0 imports |
| Node glue | `node packages/js/test/node_pair.mjs` | two endpoints connect; 1000 × 64 B and one 64 KiB reliable message arrive, the large one intact |
| Browser | `node packages/js/test/browser.mjs 2` | two Chromium tabs form a WebRTC mesh on loopback and exchange data |
| Arena | `node packages/js/test/arena.mjs 8 10` | every island's checkpoint hashes match, ticks keep confirming |
| Nostr (opt-in) | `node packages/js/test/nostr.mjs` | two browser contexts on this host find each other via `relay.damus.io` / `nos.lol` and connect over public STUN |

The script runs every step even after a failure, prints a PASS/FAIL/SKIP summary and exits non-zero if any step
failed. A full run takes about 2.5 minutes on the 4-core aarch64 dev host (Rust tests ~65 s, wasm build ~20 s,
browser + arena ~30 s).

**Release gate:** `scripts/gate.sh --strict` green on `main`, plus `--nostr` once per release, plus the CI matrix
(below) green on all three OSes.

## Test layers

| Layer | Where | What it is for |
|---|---|---|
| Unit | `#[cfg(test)]` in `crates/*/src` | parsers, codecs, crypto vectors, reliability state machine |
| Crate integration | `crates/<crate>/tests/` | public API of one crate, including seeded property tests |
| Simulation | `crates/nobaas-sim/tests/` | whole endpoints over a deterministic network: loss, jitter, NATs, roaming, relays, arena |
| JS glue | `packages/js/test/node_pair.mjs` | the wasm C ABI and `nobaas.js` in Node |
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

### nobaas-core (unit, `src/`)

- `wire`: frame round trips, truncated frames fail once, frame parser survives garbage, packet bounds.
- `reliable`: out-of-order reassembly, ACK and retransmit, tail-loss probe beats the RTO, urgent fragments are
  sent twice, buffer limit.
- `stun`: RFC 5769 IPv4/IPv6 vectors; Nobaas packets are not mistaken for STUN.
- `crypto`, `identity`, `sealed`, `invite`, `addr`: Noise static key derivation, sign/verify, replay window,
  sealed-box open/reject, RFC 4648 vectors, invite round trip and garbage rejection, socket address round trip.

### nobaas-core over the simulator (`crates/nobaas-sim/tests/endpoint.rs`, `nat.rs`)

- Handshake in one RTT; wrong identity fails; simultaneous open converges to one link; replayed handshake does
  not displace a live link; peer restart replaces the stale link.
- Reliable lane exact and ordered under loss; unreliable lane at most once; bulk transfer keeps the queue short
  for game traffic.
- Tampered and replayed packets rejected; fuzzed datagrams never panic or break the link.
- Keepalive, idle timeout, heavy loss, roaming to a new address, a hub with 300 peers.
- NAT: reflexive address discovery, mapping expiry, relayed dial then punch, relay refusal, symmetric pairs stay
  relayed, direct-path failure falls back to the relay, LAN candidates behind one NAT, and the full NAT matrix
  (public, full-cone, restricted, port-restricted, symmetric sequential/random, two CGNAT stacks).

### nobaas-game (`crates/nobaas-game/tests/`)

| File | Covers |
|---|---|
| `fixed_point.rs` | int conversion (floors), saturating add/sub vs an i64 reference, mul vs `(a*b)>>16`, mul/div identities, div inverts mul within tolerance, div by zero panics, `q16_sqrt` is the exact floor and monotonic, physics step per input bit, dash, arena clamp, bit-identical replay, line of sight |
| `hilbert.rs` | known order-1/2 indices, bijection and grid-adjacent steps on every grid up to 64×64, range on 65536², partition invariants (each player once, full islands, ids, Hilbert order, non-overlapping ranges), `k_max` clamp, input-order independence with position ties, off-grid positions, spatial locality |
| `desync.rs` | CRC golden values (a change is a wire break), determinism, every hashed field changes the CRC (collisions bounded), order sensitivity, input bits not hashed, quorum known cases (exact 2/3, below 2/3, output order) and a property over island sizes 1–12 |

### nobaas-sim arena (`crates/nobaas-sim/tests/arena.rs`)

Lockstep islands agree over super-peers; bundles survive 5 % loss with jitter; islands survive a super-peer
dying; an overloaded super-peer degrades without stalling. `cargo run -p nobaas-sim --release --example arena`
is the full harness for `docs/PERFORMANCE.md`.

### nobaas-ffi and packages/js

`two_endpoints_through_the_c_abi` (Rust), `node_pair.mjs` (wasm in Node), `browser.mjs` and `arena.mjs`
(Chromium, WebRTC over loopback), `nostr.mjs` (opt-in).

## Known gaps

| Gap | Test | Owner |
|---|---|---|
| `q16_mul` and `q16_div` wrap on overflow while `q16_add`/`q16_sub` saturate: `q16_mul(i32::MAX, 2.0) == -2` | `fixed_point::mul_saturates_on_overflow` (ignored) | backend |
| `AabbObstacleQ16::blocks_segment` samples 17 points, so a wall thinner than `length / 16` is missed on long sight lines (a fog-of-war leak) | `fixed_point::thin_wall_blocks_long_segment` (ignored) | backend |
| No property tests yet for `nobaas-core` codecs (wire, invite, STUN) beyond the garbage-input tests | — | qa (with NB-014 fuzzing) |
| `browser.mjs` reports round trips and throughput but asserts only that the mesh forms and data arrives | — | qa |
| No rustfmt on the dev host, so formatting is only checked in CI | — | ops (NB-002) |
| Planned crates (facade, sync, media, store) have no tests yet; each lands with its own | — | backend |

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
runs `scripts/gate.sh --strict` on it and posts the summary block plus any failing output on the card.
