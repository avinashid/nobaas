//! Tests restored from `p0-hilbert-rollback` (Protocol Zero, branch `sdk`, `src/lib.rs` `mod tests`), limited to
//! the API peerwire-game kept. The rest of those tests covered `RollbackIslandSimulator`, witness receipts and
//! fog-of-war commitments, which return with NB-018 and get their tests back then.

use peerwire_game::*;

/// From `test_q16_physics_hilbert_and_witness_rollback_rejection`: neighbouring cells stay close on the curve.
#[test]
fn hilbert_neighbours_are_close() {
    let d0 = xy_to_hilbert_d(64, 0, 0);
    let d1 = xy_to_hilbert_d(64, 0, 1);
    assert!(d1.abs_diff(d0) <= 3);
}

/// From the same test: four players stepped for ten ticks stay in the arena and in step with each other.
#[test]
fn four_players_step_ten_ticks() {
    let mut players: Vec<PlayerEntityQ16> = (0..4u16)
        .map(|id| PlayerEntityQ16 {
            peer_id: id,
            pos: Vec2Q16::from_ints(10, 10),
            vel: Vec2Q16::default(),
            hp: 100,
            last_input_bits: 0,
        })
        .collect();
    for _ in 0..10 {
        for p in &mut players {
            p.step_physics(0x01, q16_from_i32(1000));
        }
    }
    assert!(players.iter().all(|p| p.pos == Vec2Q16::from_ints(30, 10)));
}

/// From `test_fog_of_war_and_byzantine_crc`: a wall at x in [4, 6] hides (10, 5) from (1, 5); without it, visible.
#[test]
fn fog_of_war_wall_blocks_sight() {
    let obs = [AabbObstacleQ16 {
        min_x: q16_from_i32(4),
        min_y: q16_from_i32(0),
        max_x: q16_from_i32(6),
        max_y: q16_from_i32(10),
    }];
    let p_obs = Vec2Q16::from_ints(1, 5);
    let p_hidden = Vec2Q16::from_ints(10, 5);
    assert!(!has_line_of_sight_q16(p_obs, p_hidden, &obs));
    assert!(has_line_of_sight_q16(p_obs, p_hidden, &[]));
}

/// From `test_fog_of_war_and_byzantine_crc`: peer 3 reports a corrupted CRC and is evicted.
#[test]
fn byzantine_crc_peer_is_evicted() {
    let evicted = find_byzantine_desync_peers(&[(0, 0x1234), (1, 0x1234), (2, 0x1234), (3, 0x9999)]);
    assert_eq!(evicted, vec![3]);
}
