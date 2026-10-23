# peerwire-ffi

A C ABI over the [`peerwire-core`](../peerwire-core) `Endpoint`: one surface for every host
that isn't Rust.

- Built for `wasm32-unknown-unknown`, it is a WebAssembly module of about 300 KB with
  **no imports** (no wasm-bindgen, no WASI). The browser package [`@peerwire/js`](../../packages/js)
  loads it and supplies WebRTC DataChannels, timers and randomness from JavaScript.
- Built natively, it is a shared or static library (`cdylib`, `staticlib`) for Dart FFI,
  Swift or C.

Like the core, it does no I/O. Addresses are `u32` handles that the host maps to its own
transports (a DataChannel, a socket), so the same endpoint runs over anything that carries
datagrams.

## Build

```sh
./packages/js/build.sh                  # -> packages/js/pkg/peerwire_ffi.wasm (size-optimised)
cargo build --release -p peerwire-ffi     # -> target/release/libpeerwire_ffi.{so,dylib,dll,a}
```

CI and `scripts/gate.sh` fail the wasm build if it grows past 350 KiB (`WASM_MAX_BYTES`,
358400 bytes) or gains an import.

## Conventions

- Times are microseconds on the host's monotonic clock.
- Peer ids are 32-byte buffers.
- Buffers are allocated in the module's memory with `pw_alloc` and freed with `pw_free`.
- Return codes: `>= 0` is success (a length, where one is returned), `< 0` is an error:
  `PW_ERR_NOT_CONNECTED` (-1), `PW_ERR_TOO_LARGE` (-2), `PW_ERR_WOULD_BLOCK` (-3),
  `PW_ERR_INVALID` (-4), `PW_ERR_BUFFER` (-5, the output buffer is too small and nothing
  was consumed).

## Functions

| Group | Functions |
|---|---|
| Memory | `pw_alloc`, `pw_free` |
| Endpoint | `pw_endpoint_new` (from a secret key, or a new identity from a 32-byte seed), `pw_endpoint_free`, `pw_secret` |
| Links | `pw_connect`, `pw_connect_via`, `pw_add_candidate` (a direct address for a relayed peer: punch to it, move there once it answers), `pw_set_relay_only`, `pw_disconnect`, `pw_is_connected`, `pw_rtt_us`, `pw_stats` |
| Data | `pw_send` (lane 0 reliable, 1 unreliable, 2 urgent) |
| Host loop | `pw_handle_datagram`, `pw_handle_timeout`, `pw_poll_timeout`, `pw_poll_transmit`, `pw_poll_event` |
| Sealed messages | `pw_seal`, `pw_open` |
| Nostr | `pw_nostr_keypair`, `pw_nostr_sign` (BIP-340 Schnorr, for Nostr signaling without a JS crypto library) |

The host loop is the core's, through handles:

```text
pw_handle_datagram(h, now_us, addr, data, len)      for every datagram received
pw_handle_timeout(h, now_us)                        once pw_poll_timeout(h) has passed
while (n = pw_poll_transmit(h, now_us, &addr, buf, 1232)) > 0: send buf[..n] to addr
while (n = pw_poll_event(h, buf, cap)) > 0: handle the event in buf[..n]
                                             (n < 0: buf needs -n bytes; the event stays queued)
```

Events are encoded as `kind(u8) peer(32) ...`:

```text
1 CONNECTED      initiator(u8) path
2 MESSAGE        lane(u8: 0 reliable, 1 unreliable) data...
3 DISCONNECTED   reason(u8: 0 timeout, 1 closed, 2 replaced, 3 violation)
4 CONNECT_FAILED
5 PATH_CHANGED   path
6 ADDR_OBSERVED  addr(u32)
path = 0 addr(u32) | 1 relay_peer(32)
```

The full contract of each function is in its doc comment in `src/lib.rs`.

## Tests

The module is tested from JavaScript, in the gate and in CI: `packages/js/test/node_pair.mjs`
links two endpoints in Node.js, and the other `packages/js/test/*.mjs` tests cover privacy,
rooms, peer-id checks, buffer reuse and calls after close. `cargo test -p peerwire-ffi` links
two endpoints through the C ABI, restores an identity from its secret, round-trips a sealed
message and verifies a Nostr signature.

## Status

Early development (v0.1). The ABI may still change; there is no C header yet.
