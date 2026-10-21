//! `peerwire-game`: deterministic building blocks for multiplayer games.
//!
//! - `Q16_16` fixed-point maths (no floats, so every peer computes bit-identical state),
//! - Hilbert-curve partitioning of players into "islands" of at most 12 (`MAX_ISLAND_PLAYERS`),
//! - a 16-bit state checksum per island and tick, and quorum-based desync detection.

use siphasher::sip::SipHasher13;
use std::collections::BTreeMap;
use std::hash::Hasher;

/// Fixed-point 16.16 format (`1 unit = 65536`). Zero floating-point instructions across the entire crate.
pub type Q16_16 = i32;

pub const Q16_ONE: Q16_16 = 1 << 16;
pub const Q16_HALF: Q16_16 = 1 << 15;

/// Maximum players per Hilbert-Curve Causal Island (`Subsystem E.2`: `K_max = 12`).
pub const MAX_ISLAND_PLAYERS: usize = 12;

/// Maximum un-witnessed input arrival delay in 60Hz ticks (`Subsystem E.3`: `t + 2`).
pub const MAX_UNWITNESSED_DELAY_TICKS: u32 = 2;

/// Minimum flight delay in 60Hz ticks for Future-Stamped Boundary Handoffs (`delta_flight >= 4`).
pub const MIN_BOUNDARY_FLIGHT_TICKS: u32 = 4;

/// State CRC lookback offset in ticks (`tick - 8`).
pub const STATE_CRC_LOOKBACK_TICKS: u32 = 8;

/// Rollback ring buffer depth in ticks (64 ticks > 1 second at 60Hz).
pub const ROLLBACK_RING_SIZE: usize = 64;

// ============================================================================
// Subsystem E.1: Q16.16 Fixed-Point Deterministic Math & 2D Physics Core
// ============================================================================

#[inline(always)]
pub const fn q16_from_i32(v: i32) -> Q16_16 {
    v << 16
}

#[inline(always)]
pub const fn q16_to_i32(v: Q16_16) -> i32 {
    v >> 16
}

#[inline(always)]
pub const fn q16_add(a: Q16_16, b: Q16_16) -> Q16_16 {
    a.saturating_add(b)
}

#[inline(always)]
pub const fn q16_sub(a: Q16_16, b: Q16_16) -> Q16_16 {
    a.saturating_sub(b)
}

/// Clamps a widened result into `Q16_16`, so mul/div saturate like `q16_add`/`q16_sub`.
#[inline(always)]
const fn q16_saturate(v: i64) -> Q16_16 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

/// Saturating Q16.16 multiply (rounds toward negative infinity).
#[inline(always)]
pub const fn q16_mul(a: Q16_16, b: Q16_16) -> Q16_16 {
    q16_saturate(((a as i64) * (b as i64)) >> 16)
}

/// Saturating Q16.16 divide (truncates toward zero). Panics if `b == 0`.
#[inline(always)]
pub const fn q16_div(a: Q16_16, b: Q16_16) -> Q16_16 {
    assert!(b != 0, "Q16.16 division by zero");
    q16_saturate(((a as i64) << 16) / (b as i64))
}

/// Deterministic integer square root for `Q16_16` (`sqrt(x)` in Q16.16). Zero floats.
pub const fn q16_sqrt(x: Q16_16) -> Q16_16 {
    if x <= 0 {
        return 0;
    }
    let val = (x as u64) << 16;
    let mut res = 0u64;
    let mut bit = 1u64 << 62;
    let mut rem = val;
    while bit > rem {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= res + bit {
            rem -= res + bit;
            res = (res >> 1) + bit;
        } else {
            res >>= 1;
        }
        bit >>= 2;
    }
    res as Q16_16
}

/// 2D fixed-point vector in `Q16_16`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vec2Q16 {
    pub x: Q16_16,
    pub y: Q16_16,
}

impl Vec2Q16 {
    pub const fn new(x: Q16_16, y: Q16_16) -> Self {
        Self { x, y }
    }

    pub const fn from_ints(x: i32, y: i32) -> Self {
        Self { x: q16_from_i32(x), y: q16_from_i32(y) }
    }
}

/// Axis-Aligned Bounding Box obstacle in `Q16_16` for deterministic raycasting and Fog-of-War.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AabbObstacleQ16 {
    pub min_x: Q16_16,
    pub min_y: Q16_16,
    pub max_x: Q16_16,
    pub max_y: Q16_16,
}

impl AabbObstacleQ16 {
    /// Exact test of whether the segment `p0..=p1` touches this box (edges inclusive). Liang–Barsky slab
    /// clipping with each crossing `t` kept as an integer fraction and compared in `i128`: zero floats, no
    /// sampling, so walls of any thickness block and the result is symmetric in `p0`/`p1`.
    pub fn blocks_segment(&self, p0: Vec2Q16, p1: Vec2Q16) -> bool {
        // Entry and exit of the segment inside the box so far, as `num / den` with `den > 0`, within [0, 1].
        let (mut enter_n, mut enter_d) = (0i128, 1i128);
        let (mut exit_n, mut exit_d) = (1i128, 1i128);
        let axes = [(p0.x, p1.x, self.min_x, self.max_x), (p0.y, p1.y, self.min_y, self.max_y)];
        for (a, b, lo, hi) in axes {
            let (a, b, lo, hi) = (a as i128, b as i128, lo as i128, hi as i128);
            let d = b - a;
            if d == 0 {
                if a < lo || a > hi {
                    return false;
                }
                continue;
            }
            // Crossing of the near and far slab planes along this axis.
            let (near_n, far_n, den) = if d > 0 { (lo - a, hi - a, d) } else { (a - hi, a - lo, -d) };
            if near_n * enter_d > enter_n * den {
                (enter_n, enter_d) = (near_n, den);
            }
            if far_n * exit_d < exit_n * den {
                (exit_n, exit_d) = (far_n, den);
            }
        }
        enter_n * exit_d <= exit_n * enter_d
    }
}

/// Deterministic line-of-sight check between two `Vec2Q16` coordinates across arena obstacles.
pub fn has_line_of_sight_q16(a: Vec2Q16, b: Vec2Q16, obstacles: &[AabbObstacleQ16]) -> bool {
    !obstacles.iter().any(|obs| obs.blocks_segment(a, b))
}

// ============================================================================
// Subsystem E.2: Hilbert-Curve Causal Islands (`K_max = 12`) & 60Hz Rollback
// ============================================================================

/// Maps 2D grid coordinates `(x, y)` on an `n x n` power-of-two grid to a 1D Hilbert Curve index `d`.
#[inline(always)]
pub fn xy_to_hilbert_d(n: u32, mut x: u32, mut y: u32) -> u64 {
    let mut d: u64 = 0;
    let mut s = n >> 1;
    while s > 0 {
        let rx = u32::from((x & s) > 0);
        let ry = u32::from((y & s) > 0);
        d += (s as u64) * (s as u64) * ((3 * rx) ^ ry) as u64;
        x &= s - 1;
        y &= s - 1;
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    d
}

/// Deterministic player state inside a Causal Island (`Q16_16` coordinates & velocities).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayerEntityQ16 {
    pub peer_id: u16,
    pub pos: Vec2Q16,
    pub vel: Vec2Q16,
    pub hp: i16,
    pub last_input_bits: u16,
}

impl PlayerEntityQ16 {
    /// Input bitmask flags (`bit 0 = +X, bit 1 = -X, bit 2 = +Y, bit 3 = -Y, bit 4 = dash`).
    pub fn step_physics(&mut self, input_bits: u16, arena_max_q16: Q16_16) {
        self.last_input_bits = input_bits;
        let speed = if (input_bits & 0x10) != 0 { q16_from_i32(4) } else { q16_from_i32(2) };
        let mut vx = 0;
        let mut vy = 0;
        if (input_bits & 0x01) != 0 {
            vx = q16_add(vx, speed);
        }
        if (input_bits & 0x02) != 0 {
            vx = q16_sub(vx, speed);
        }
        if (input_bits & 0x04) != 0 {
            vy = q16_add(vy, speed);
        }
        if (input_bits & 0x08) != 0 {
            vy = q16_sub(vy, speed);
        }
        self.vel = Vec2Q16::new(vx, vy);
        self.pos.x = q16_add(self.pos.x, self.vel.x).clamp(0, arena_max_q16);
        self.pos.y = q16_add(self.pos.y, self.vel.y).clamp(0, arena_max_q16);
    }
}

/// Clusters any number of arena players into Hilbert-Curve Causal Islands of at most
/// `MAX_ISLAND_PLAYERS` (`K_max = 12`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CausalIslandCluster {
    pub island_id: u16,
    pub hilbert_min_d: u64,
    pub hilbert_max_d: u64,
    pub players: Vec<PlayerEntityQ16>,
}

pub fn partition_into_hilbert_islands(
    grid_size: u32,
    players: &[PlayerEntityQ16],
    k_max: usize,
) -> Vec<CausalIslandCluster> {
    let cap = k_max.clamp(1, MAX_ISLAND_PLAYERS);
    let mut indexed: Vec<(u64, PlayerEntityQ16)> = players
        .iter()
        .map(|p| {
            let gx = (q16_to_i32(p.pos.x).max(0) as u32).min(grid_size.saturating_sub(1));
            let gy = (q16_to_i32(p.pos.y).max(0) as u32).min(grid_size.saturating_sub(1));
            (xy_to_hilbert_d(grid_size, gx, gy), *p)
        })
        .collect();
    indexed.sort_by_key(|(d, p)| (*d, p.peer_id));

    indexed
        .chunks(cap)
        .enumerate()
        .map(|(idx, chunk)| CausalIslandCluster {
            island_id: idx as u16,
            hilbert_min_d: chunk.first().map(|c| c.0).unwrap_or(0),
            hilbert_max_d: chunk.last().map(|c| c.0).unwrap_or(0),
            players: chunk.iter().map(|c| c.1).collect(),
        })
        .collect()
}

// ============================================================================

/// Computes a deterministic 16-bit SipHash-1-3 CRC over an island's `PlayerEntityQ16` state at `tick`
/// (`Subsystem E.3`: included in every 60Hz input frame for `tick - 8`).
pub fn compute_island_state_crc16(tick: u32, players: &[PlayerEntityQ16]) -> u16 {
    let mut hasher = SipHasher13::new_with_keys(0x5030_4352_4331_3656, 0x335F_4849_4C42_5254);
    hasher.write_u32(tick);
    for p in players {
        hasher.write_u16(p.peer_id);
        hasher.write_i32(p.pos.x);
        hasher.write_i32(p.pos.y);
        hasher.write_i32(p.vel.x);
        hasher.write_i32(p.vel.y);
        hasher.write_i16(p.hp);
    }
    (hasher.finish() & 0xFFFF) as u16
}

/// Evaluates `tick - 8` state CRCs across island peers (`[(peer_id, crc16)]`) and returns the list
/// of Byzantine / memory-hacking `peer_id`s that diverge from the `>= 2/3` quorum (`Subsystem E.3`).
pub fn find_byzantine_desync_peers(peer_crcs: &[(u16, u16)]) -> Vec<u16> {
    if peer_crcs.is_empty() {
        return Vec::new();
    }
    let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
    for &(_, crc) in peer_crcs {
        *counts.entry(crc).or_insert(0) += 1;
    }
    let (&majority_crc, &maj_count) = counts.iter().max_by_key(|(_, c)| *c).unwrap();
    // Quorum requires >= 2/3 agreement (`maj_count * 3 >= peer_crcs.len() * 2`).
    if maj_count * 3 < peer_crcs.len() * 2 {
        return Vec::new();
    }
    peer_crcs.iter().filter(|(_, crc)| *crc != majority_crc).map(|(pid, _)| *pid).collect()
}

// The README examples run as doc tests (`cargo test --doc`).
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
