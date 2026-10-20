//! Seeded generators for the property tests: every case is reproducible from `SEED` and its index.

#![allow(dead_code)]

use nobaas_game::{PlayerEntityQ16, Vec2Q16, Q16_16};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};

pub const SEED: u64 = 0x4e42_5f51_415f_3031;

/// Cases per property; small enough to keep `cargo test` fast on a 4-core host.
pub const CASES: usize = 2000;

pub fn rng(salt: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(SEED ^ salt)
}

/// Uniform in `lo..=hi`.
pub fn range(r: &mut ChaCha8Rng, lo: i64, hi: i64) -> i64 {
    lo + (r.next_u64() % ((hi - lo) as u64 + 1)) as i64
}

/// A `Q16_16` whose magnitude is spread across bit widths, so small, fractional and huge values all show up.
pub fn q16(r: &mut ChaCha8Rng) -> Q16_16 {
    let bits = r.next_u32() % 32;
    let v = (r.next_u32() >> (31 - bits)) as i32;
    if r.next_u32() & 1 == 0 {
        v
    } else {
        -v
    }
}

pub fn player(r: &mut ChaCha8Rng, peer_id: u16, arena: i32) -> PlayerEntityQ16 {
    let mut coord = || range(r, 0, ((arena as i64) << 16) - 1) as Q16_16;
    PlayerEntityQ16 {
        peer_id,
        pos: Vec2Q16::new(coord(), coord()),
        vel: Vec2Q16::new(coord() % (4 << 16), coord() % (4 << 16)),
        hp: (r.next_u32() % 101) as i16,
        last_input_bits: (r.next_u32() & 0x1f) as u16,
    }
}

pub fn shuffle<T>(r: &mut ChaCha8Rng, v: &mut [T]) {
    for i in (1..v.len()).rev() {
        v.swap(i, (r.next_u64() % (i as u64 + 1)) as usize);
    }
}
