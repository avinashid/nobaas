//! Island state checksums (CRC16) and quorum desync detection.

mod common;

use common::{player, range, rng, CASES};
use peerwire_game::*;
use rand_core::RngCore;

fn island() -> Vec<PlayerEntityQ16> {
    (0..4u16)
        .map(|i| PlayerEntityQ16 {
            peer_id: i + 1,
            pos: Vec2Q16::from_ints(10 * i as i32, 5),
            vel: Vec2Q16::from_ints(2, 0),
            hp: 100,
            last_input_bits: 0x01,
        })
        .collect()
}

/// Peers on different platforms and versions must agree on the checksum byte for byte. If this changes, it is
/// a wire-compatibility break: bump the protocol version rather than the constant.
#[test]
fn crc_golden_values() {
    assert_eq!(compute_island_state_crc16(0, &[]), GOLDEN_EMPTY);
    assert_eq!(compute_island_state_crc16(1234, &island()), GOLDEN_ISLAND);
}

const GOLDEN_EMPTY: u16 = 61147;
const GOLDEN_ISLAND: u16 = 43494;

#[test]
fn crc_is_deterministic() {
    let mut r = rng(20);
    for _ in 0..CASES / 4 {
        let ps: Vec<_> = (0..12).map(|i| player(&mut r, i, 1024)).collect();
        let tick = r.next_u32();
        assert_eq!(compute_island_state_crc16(tick, &ps), compute_island_state_crc16(tick, &ps.clone()));
    }
}

type Mutant<'a> = Box<dyn Fn(&mut PlayerEntityQ16) + 'a>;

/// Any single-field change to the hashed state should change the CRC except for the 1-in-65536 collisions of a
/// 16-bit checksum. Over CASES mutations, more than a handful of collisions would mean a field is not hashed.
#[test]
fn crc_detects_single_field_changes() {
    let mut r = rng(21);
    let mut collisions = 0;
    let mut total = 0;
    for _ in 0..CASES {
        let n = range(&mut r, 1, 12) as usize;
        let ps: Vec<_> = (0..n as u16).map(|i| player(&mut r, i, 1024)).collect();
        let tick = r.next_u32();
        let base = compute_island_state_crc16(tick, &ps);
        let i = range(&mut r, 0, n as i64 - 1) as usize;
        let delta = (r.next_u32() | 1) as i32;
        let mutants: [Mutant; 6] = [
            Box::new(|p| p.peer_id ^= 1 + delta as u16 % 0xfff),
            Box::new(|p| p.pos.x = p.pos.x.wrapping_add(delta)),
            Box::new(|p| p.pos.y = p.pos.y.wrapping_add(delta)),
            Box::new(|p| p.vel.x = p.vel.x.wrapping_add(delta)),
            Box::new(|p| p.vel.y = p.vel.y.wrapping_add(delta)),
            Box::new(|p| p.hp = p.hp.wrapping_add(delta as i16 | 1)),
        ];
        for m in &mutants {
            let mut q = ps.clone();
            m(&mut q[i]);
            total += 1;
            collisions += usize::from(compute_island_state_crc16(tick, &q) == base);
        }
        total += 1;
        collisions += usize::from(compute_island_state_crc16(tick.wrapping_add(1), &ps) == base);
    }
    // Expected total / 65536 (about 0.2 here); 5 is far beyond chance.
    assert!(collisions < 5, "{collisions} collisions in {total} mutations");
}

#[test]
fn crc_depends_on_player_order() {
    // Order matters, which is why partitioning sorts members deterministically before hashing.
    let ps = island();
    let mut rev = ps.clone();
    rev.reverse();
    assert_ne!(compute_island_state_crc16(7, &ps), compute_island_state_crc16(7, &rev));
}

#[test]
fn crc_ignores_last_input_bits() {
    // Input bits travel in the input frame itself; the checksum covers simulated state only.
    let ps = island();
    let mut q = ps.clone();
    q[0].last_input_bits ^= 0x1f;
    assert_eq!(compute_island_state_crc16(7, &ps), compute_island_state_crc16(7, &q));
}

// ---------------------------------------------------------------- quorum

#[test]
fn quorum_known_cases() {
    let f = find_byzantine_desync_peers;
    assert!(f(&[]).is_empty());
    assert!(f(&[(1, 0xaaaa)]).is_empty());
    assert!(f(&[(1, 1), (2, 1), (3, 1)]).is_empty(), "all agree");
    assert_eq!(f(&[(1, 1), (2, 1), (3, 9)]), [3], "2 of 3 is exactly 2/3: quorum");
    assert!(f(&[(1, 1), (2, 9)]).is_empty(), "1 of 2: no quorum, nobody blamed");
    assert!(f(&[(1, 1), (2, 1), (3, 1), (4, 9), (5, 8)]).is_empty(), "3 of 5 < 2/3");
    assert_eq!(f(&[(1, 1), (2, 1), (3, 1), (4, 1), (5, 9), (6, 8)]), [5, 6], "4 of 6 = 2/3");
    // The honest crc is whatever the quorum says, even when the "bad" value is numerically smaller.
    assert_eq!(f(&[(10, 0xffff), (11, 0), (12, 0xffff), (13, 0xffff)]), [11]);
    // Output keeps input order.
    assert_eq!(f(&[(9, 2), (1, 7), (5, 7), (3, 7), (4, 7), (8, 3), (2, 7), (6, 7), (7, 7)]), [9, 8]);
}

/// For island sizes 1..=12 with `m` divergent peers (each with its own random wrong crc): the divergent set is
/// reported exactly when the honest peers are at least 2/3, and nothing is reported otherwise.
#[test]
fn quorum_property() {
    let mut r = rng(22);
    for _ in 0..CASES {
        let n = range(&mut r, 1, MAX_ISLAND_PLAYERS as i64) as usize;
        let m = range(&mut r, 0, n as i64 - 1) as usize;
        let honest = r.next_u32() as u16;
        let mut crcs: Vec<(u16, u16)> = (0..n as u16).map(|id| (id, honest)).collect();
        let mut bad: Vec<u16> = Vec::new();
        while bad.len() < m {
            let id = range(&mut r, 0, n as i64 - 1) as u16;
            if !bad.contains(&id) {
                bad.push(id);
                crcs[id as usize].1 = honest ^ (1 + (r.next_u32() as u16 % 0xfffe));
            }
        }
        bad.sort();
        let got = find_byzantine_desync_peers(&crcs);
        if (n - m) * 3 >= n * 2 {
            assert_eq!(got, bad, "n={n} m={m}");
        } else {
            // Divergent peers may agree with each other; still nobody reaches quorum unless they form one.
            let mut counts = std::collections::BTreeMap::new();
            for &(_, c) in &crcs {
                *counts.entry(c).or_insert(0usize) += 1;
            }
            let top = *counts.values().max().unwrap();
            if top * 3 < n * 2 {
                assert!(got.is_empty(), "n={n} m={m} got={got:?}");
            }
        }
    }
}
