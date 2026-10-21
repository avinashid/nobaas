# peerwire-game

Deterministic building blocks for peer-to-peer multiplayer games: with no server to hold
the truth, every peer has to compute the same state from the same inputs, bit for bit.

- **`Q16_16` fixed-point maths:** add, sub, mul, div and sqrt on `i32` with 16 fractional
  bits, `Vec2Q16`, axis-aligned obstacles and a line-of-sight test. The crate uses no
  floating point, so results are identical on every CPU, in wasm and natively.
- **Player physics:** `PlayerEntityQ16::step_physics` moves a player one tick from an input
  bitmask (+X, -X, +Y, -Y, dash), clamped to the arena.
- **Hilbert-curve islands:** `partition_into_hilbert_islands` groups players that are near
  each other into islands of at most 12 (`MAX_ISLAND_PLAYERS`), so each island can run its
  own lockstep and large arenas don't need a full mesh.
- **Desync detection:** `compute_island_state_crc16` hashes an island's state at a tick
  (SipHash-1-3, truncated to 16 bits), and `find_byzantine_desync_peers` returns the peers
  whose checksum differs from a two-thirds quorum.
- **Timing constants** for lockstep over islands at 60 Hz: input delay, boundary hand-off
  delay, checksum lookback and rollback depth.

The crate has no networking; send inputs and checksums over [`peerwire`](../peerwire) (or the
browser package) however your game likes. The [arena benchmark](../peerwire-sim/examples/arena.rs)
in `peerwire-sim` puts the pieces together over simulated networks.

## Example

```rust
use peerwire_game::*;

let mut players: Vec<PlayerEntityQ16> = (0..4)
    .map(|i| PlayerEntityQ16 { peer_id: i, pos: Vec2Q16::from_ints(10 * i as i32, 10), hp: 100, ..Default::default() })
    .collect();

// Every peer applies the same inputs for this tick...
for p in &mut players {
    p.step_physics(0b0001, q16_from_i32(1000)); // move +X
}

// ...so every peer computes the same islands and checksums.
let islands = partition_into_hilbert_islands(1024, &players, MAX_ISLAND_PLAYERS);
let crc = compute_island_state_crc16(1, &islands[0].players);

// Peers exchange checksums; whoever disagrees with the quorum has desynced.
let reports = [(0, crc), (1, crc), (2, crc), (3, crc ^ 1)];
assert_eq!(find_byzantine_desync_peers(&reports), vec![3]);
```

## Tests

`cargo test --release -p peerwire-game` covers the fixed-point maths against reference values,
Hilbert partitioning (locality, island size limits), desync detection, and a lockstep replay
in which peers that learn the same inputs in different orders still compute identical
checksums on every tick for 600 ticks.

## Status

Early development (v0.1). The API may still change.
