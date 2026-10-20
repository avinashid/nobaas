//! Hilbert-curve index and partitioning of players into causal islands.

mod common;

use common::{player, range, rng, shuffle, CASES};
use nobaas_game::*;
use std::collections::BTreeSet;

#[test]
fn hilbert_known_values() {
    assert_eq!(xy_to_hilbert_d(1, 0, 0), 0);
    // Order-1 curve: (0,0) -> (0,1) -> (1,1) -> (1,0).
    let order1: Vec<u64> = [(0, 0), (0, 1), (1, 1), (1, 0)].iter().map(|&(x, y)| xy_to_hilbert_d(2, x, y)).collect();
    assert_eq!(order1, [0, 1, 2, 3]);
    // Order-2 curve starts at the origin and ends at the bottom-right corner.
    assert_eq!(xy_to_hilbert_d(4, 0, 0), 0);
    assert_eq!(xy_to_hilbert_d(4, 1, 0), 1);
    assert_eq!(xy_to_hilbert_d(4, 3, 0), 15);
}

/// For every power-of-two grid up to 64x64: the index is a bijection onto `0..n*n`, and consecutive indices
/// are grid neighbours (Manhattan distance 1), which is what keeps islands spatially compact.
#[test]
fn hilbert_is_a_continuous_bijection() {
    for order in 0..=6 {
        let n = 1u32 << order;
        let mut at = vec![None; (n * n) as usize];
        for x in 0..n {
            for y in 0..n {
                let d = xy_to_hilbert_d(n, x, y) as usize;
                assert!(d < at.len(), "n={n} ({x},{y}) -> {d}");
                assert!(at[d].is_none(), "n={n} index {d} hit twice");
                at[d] = Some((x, y));
            }
        }
        let path: Vec<(u32, u32)> = at.into_iter().map(Option::unwrap).collect();
        for w in path.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            assert_eq!(x0.abs_diff(x1) + y0.abs_diff(y1), 1, "n={n} jump {:?} -> {:?}", w[0], w[1]);
        }
    }
}

#[test]
fn hilbert_large_grid_stays_in_range() {
    let mut r = rng(10);
    let n = 1u32 << 16;
    for _ in 0..CASES {
        let (x, y) = (range(&mut r, 0, n as i64 - 1) as u32, range(&mut r, 0, n as i64 - 1) as u32);
        assert!(xy_to_hilbert_d(n, x, y) < (n as u64) * (n as u64));
    }
    assert_eq!(xy_to_hilbert_d(n, 0, 0), 0);
    assert_eq!(xy_to_hilbert_d(n, n - 1, 0), (n as u64) * (n as u64) - 1);
}

// ---------------------------------------------------------------- partition

fn players(r: &mut rand_chacha::ChaCha8Rng, count: usize, arena: i32) -> Vec<PlayerEntityQ16> {
    (0..count).map(|i| player(r, i as u16, arena)).collect()
}

#[test]
fn partition_empty_and_small() {
    assert!(partition_into_hilbert_islands(64, &[], 12).is_empty());
    let mut r = rng(11);
    let ps = players(&mut r, 5, 64);
    let islands = partition_into_hilbert_islands(64, &ps, 12);
    assert_eq!(islands.len(), 1);
    assert_eq!(islands[0].players.len(), 5);
    assert_eq!(islands[0].island_id, 0);
}

#[test]
fn partition_clamps_k_max() {
    let mut r = rng(12);
    let ps = players(&mut r, 40, 64);
    assert!(partition_into_hilbert_islands(64, &ps, 0).iter().all(|i| i.players.len() == 1), "k_max 0 -> 1");
    let big = partition_into_hilbert_islands(64, &ps, 100);
    assert_eq!(big.iter().map(|i| i.players.len()).max(), Some(MAX_ISLAND_PLAYERS), "k_max capped at 12");
}

/// Invariants for random arenas: each player in exactly one island, islands are full except the last, ids
/// count up from 0, members are in Hilbert order and island ranges don't overlap.
#[test]
fn partition_invariants() {
    let mut r = rng(13);
    for case in 0..CASES / 10 {
        let count = range(&mut r, 1, 300) as usize;
        let k = range(&mut r, 1, 12) as usize;
        let grid = 1u32 << range(&mut r, 1, 10);
        let ps = players(&mut r, count, grid as i32);
        let islands = partition_into_hilbert_islands(grid, &ps, k);

        assert_eq!(islands.len(), count.div_ceil(k), "case {case}");
        let mut seen = BTreeSet::new();
        let mut prev_max = 0;
        for (i, isl) in islands.iter().enumerate() {
            assert_eq!(isl.island_id as usize, i);
            let full = i + 1 < islands.len();
            assert!(isl.players.len() <= k && (!full || isl.players.len() == k), "case {case} island {i}");
            let ds: Vec<u64> = isl
                .players
                .iter()
                .map(|p| xy_to_hilbert_d(grid, q16_to_i32(p.pos.x) as u32, q16_to_i32(p.pos.y) as u32))
                .collect();
            assert!(ds.windows(2).all(|w| w[0] <= w[1]), "case {case}: members out of Hilbert order");
            assert_eq!((isl.hilbert_min_d, isl.hilbert_max_d), (ds[0], *ds.last().unwrap()));
            assert!(isl.hilbert_min_d >= prev_max, "case {case}: island ranges overlap");
            prev_max = isl.hilbert_max_d;
            for p in &isl.players {
                assert!(seen.insert(p.peer_id), "case {case}: peer {} twice", p.peer_id);
            }
        }
        assert_eq!(seen.len(), count);
    }
}

/// Every peer must compute the same islands whatever order it learned about the players in.
#[test]
fn partition_ignores_input_order() {
    let mut r = rng(14);
    for _ in 0..CASES / 20 {
        let count = range(&mut r, 1, 200) as usize;
        let mut ps = players(&mut r, count, 32);
        // Force ties on position so the peer-id tie-break is exercised.
        for i in (1..ps.len()).step_by(3) {
            ps[i].pos = ps[i - 1].pos;
        }
        let want = partition_into_hilbert_islands(32, &ps, 12);
        shuffle(&mut r, &mut ps);
        assert_eq!(partition_into_hilbert_islands(32, &ps, 12), want);
    }
}

#[test]
fn partition_clamps_positions_outside_the_grid() {
    let mk = |id, x, y| PlayerEntityQ16 { peer_id: id, pos: Vec2Q16::new(x, y), ..Default::default() };
    let ps = [mk(1, -Q16_ONE * 5, 0), mk(2, i32::MAX, i32::MAX), mk(3, 0, 0), mk(4, Q16_ONE * 7, 0)];
    let islands = partition_into_hilbert_islands(8, &ps, 12);
    let ids: Vec<u16> = islands[0].players.iter().map(|p| p.peer_id).collect();
    // (-5,0) clamps to (0,0) = d 0, tie broken by peer id; (7,0) and (MAX,MAX)->(7,7) sit on the far side.
    assert_eq!(ids[..2], [1, 3]);
    assert_eq!(islands[0].hilbert_max_d, xy_to_hilbert_d(8, 7, 0));
    assert_eq!(islands[0].players.len(), 4);
}

#[test]
fn nearby_players_share_islands() {
    // Twelve players huddled in each corner of a 256 grid: four islands, one per corner.
    let mut ps = Vec::new();
    for (c, (cx, cy)) in [(0, 0), (240, 0), (0, 240), (240, 240)].into_iter().enumerate() {
        for i in 0..12 {
            let id = (c * 12 + i) as u16;
            ps.push(PlayerEntityQ16 {
                peer_id: id,
                pos: Vec2Q16::from_ints(cx + (i as i32 % 4), cy + (i as i32 / 4)),
                ..Default::default()
            });
        }
    }
    let islands = partition_into_hilbert_islands(256, &ps, 12);
    assert_eq!(islands.len(), 4);
    for isl in &islands {
        let corner = isl.players[0].peer_id / 12;
        assert!(isl.players.iter().all(|p| p.peer_id / 12 == corner), "island {} mixes corners", isl.island_id);
    }
}
