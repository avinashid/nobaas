//! Q16.16 fixed-point maths, 2D physics step and line of sight.

mod common;

use common::{q16, range, rng, CASES};
use nobaas_game::*;

#[test]
fn int_conversion_round_trips_and_floors() {
    for v in [-32768, -1, 0, 1, 7, 32767] {
        assert_eq!(q16_to_i32(q16_from_i32(v)), v);
    }
    // Arithmetic shift: conversion back to an integer floors toward minus infinity.
    assert_eq!(q16_to_i32(Q16_HALF), 0);
    assert_eq!(q16_to_i32(-Q16_HALF), -1);
    assert_eq!(q16_to_i32(Q16_ONE + Q16_HALF), 1);
    assert_eq!(Q16_ONE, 65536);
}

#[test]
fn add_and_sub_saturate() {
    assert_eq!(q16_add(i32::MAX, 1), i32::MAX);
    assert_eq!(q16_add(i32::MIN, -1), i32::MIN);
    assert_eq!(q16_sub(i32::MIN, 1), i32::MIN);
    assert_eq!(q16_sub(i32::MAX, -1), i32::MAX);
    let mut r = rng(1);
    for _ in 0..CASES {
        let (a, b) = (q16(&mut r), q16(&mut r));
        let want = (a as i64 + b as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        assert_eq!(q16_add(a, b), want, "{a} + {b}");
        assert_eq!(q16_add(a, b), q16_add(b, a));
        let want = (a as i64 - b as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        assert_eq!(q16_sub(a, b), want, "{a} - {b}");
    }
}

#[test]
fn mul_known_values() {
    let two = q16_from_i32(2);
    assert_eq!(q16_mul(two, q16_from_i32(3)), q16_from_i32(6));
    assert_eq!(q16_mul(Q16_HALF, Q16_HALF), Q16_ONE / 4);
    assert_eq!(q16_mul(-Q16_HALF, two), -Q16_ONE);
    assert_eq!(q16_mul(1, 1), 0, "sub-ulp products truncate");
    assert_eq!(q16_mul(-1, 1), -1, "truncation is a floor (arithmetic shift)");
}

#[test]
fn mul_matches_wide_reference_in_range() {
    let mut r = rng(2);
    for _ in 0..CASES {
        let (a, b) = (q16(&mut r), q16(&mut r));
        let wide = (a as i64 * b as i64) >> 16;
        assert_eq!(q16_mul(a, b), q16_mul(b, a), "commutative");
        assert_eq!(q16_mul(a, Q16_ONE), a, "identity");
        assert_eq!(q16_mul(a, 0), 0);
        if (i32::MIN as i64..=i32::MAX as i64).contains(&wide) {
            assert_eq!(q16_mul(a, b) as i64, wide, "{a} * {b}");
        }
    }
}

/// `q16_mul`/`q16_div` saturate like `q16_add`/`q16_sub` instead of wrapping and flipping sign.
#[test]
fn mul_saturates_on_overflow() {
    let two = q16_from_i32(2);
    assert_eq!(q16_mul(i32::MAX, two), i32::MAX);
    assert_eq!(q16_mul(i32::MIN, two), i32::MIN);
    assert_eq!(q16_mul(i32::MAX, -two), i32::MIN);
    assert_eq!(q16_mul(i32::MIN, -two), i32::MAX);
    assert_eq!(q16_mul(i32::MIN, i32::MIN), i32::MAX);
    assert_eq!(q16_mul(i32::MIN, -Q16_ONE), i32::MAX, "-MIN does not fit");
    assert_eq!(q16_mul(i32::MAX, Q16_ONE), i32::MAX);
    assert_eq!(q16_mul(i32::MIN, Q16_ONE), i32::MIN);
}

#[test]
fn div_saturates_on_overflow() {
    assert_eq!(q16_div(i32::MAX, Q16_HALF), i32::MAX);
    assert_eq!(q16_div(i32::MIN, Q16_HALF), i32::MIN);
    assert_eq!(q16_div(i32::MAX, -Q16_HALF), i32::MIN);
    assert_eq!(q16_div(i32::MIN, -Q16_HALF), i32::MAX);
    assert_eq!(q16_div(i32::MAX, 1), i32::MAX, "smallest positive divisor");
    assert_eq!(q16_div(i32::MIN, -1), i32::MAX);
    assert_eq!(q16_div(i32::MIN, -Q16_ONE), i32::MAX, "-MIN does not fit");
    assert_eq!(q16_div(i32::MIN, Q16_ONE), i32::MIN);
}

#[test]
fn mul_and_div_match_clamped_wide_reference() {
    let clamp = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let mut r = rng(11);
    for _ in 0..CASES {
        let (a, b) = (q16(&mut r), q16(&mut r));
        assert_eq!(q16_mul(a, b), clamp((a as i64 * b as i64) >> 16), "{a} * {b}");
        if b != 0 {
            assert_eq!(q16_div(a, b), clamp(((a as i64) << 16) / b as i64), "{a} / {b}");
        }
    }
}

#[test]
fn mul_and_div_stay_const() {
    const P: Q16_16 = q16_mul(i32::MAX, 2 << 16);
    const Q: Q16_16 = q16_div(i32::MIN, Q16_HALF);
    assert_eq!((P, Q), (i32::MAX, i32::MIN));
}

#[test]
fn div_known_values() {
    assert_eq!(q16_div(q16_from_i32(6), q16_from_i32(3)), q16_from_i32(2));
    assert_eq!(q16_div(Q16_ONE, q16_from_i32(4)), Q16_ONE / 4);
    assert_eq!(q16_div(q16_from_i32(-1), q16_from_i32(2)), -Q16_HALF);
    // Integer division truncates toward zero (unlike q16_mul, which floors).
    assert_eq!(q16_div(-1, q16_from_i32(2)), 0);
}

#[test]
#[should_panic(expected = "division by zero")]
fn div_by_zero_panics() {
    q16_div(Q16_ONE, 0);
}

#[test]
fn div_inverts_mul_within_one_ulp() {
    let mut r = rng(3);
    for _ in 0..CASES {
        // Keep a*b in range: |a| < 2^7 units, |b| in [1/16, 2^7) units.
        let a = range(&mut r, -(1 << 23), 1 << 23) as i32;
        let b = range(&mut r, 1 << 12, 1 << 23) as i32 * if r_bit(&mut r) { 1 } else { -1 };
        assert_eq!(q16_div(a, Q16_ONE), a, "identity");
        let back = q16_div(q16_mul(a, b), b);
        // Floor in mul, truncation in div, then a further 1/b of rounding: allow 1 ulp per unit of 1/|b|.
        let tol = 1 + (Q16_ONE / b.abs()) as i64;
        assert!((back as i64 - a as i64).abs() <= tol, "a={a} b={b} back={back}");
    }
}

fn r_bit(r: &mut rand_chacha::ChaCha8Rng) -> bool {
    range(r, 0, 1) == 1
}

#[test]
fn sqrt_known_values() {
    assert_eq!(q16_sqrt(0), 0);
    assert_eq!(q16_sqrt(-Q16_ONE), 0, "negative input clamps to 0");
    assert_eq!(q16_sqrt(Q16_ONE), Q16_ONE);
    assert_eq!(q16_sqrt(q16_from_i32(4)), q16_from_i32(2));
    assert_eq!(q16_sqrt(q16_from_i32(10000)), q16_from_i32(100));
    assert_eq!(q16_sqrt(Q16_ONE / 4), Q16_HALF);
    // sqrt(2) = 1.41421356... -> floor(1.41421356 * 65536) = 92681
    assert_eq!(q16_sqrt(q16_from_i32(2)), 92681);
}

#[test]
fn sqrt_is_exact_floor() {
    let mut r = rng(4);
    let check = |x: i32| {
        let s = q16_sqrt(x) as u64;
        let target = (x as u64) << 16;
        assert!(s * s <= target && (s + 1) * (s + 1) > target, "sqrt({x}) = {s}");
    };
    for x in [1, 2, 3, i32::MAX, i32::MAX - 1] {
        check(x);
    }
    for _ in 0..CASES {
        check(q16(&mut r).max(1));
    }
}

#[test]
fn sqrt_is_monotonic() {
    let mut r = rng(5);
    for _ in 0..CASES {
        let (a, b) = (q16(&mut r).abs(), q16(&mut r).abs());
        let (lo, hi) = (a.min(b), a.max(b));
        assert!(q16_sqrt(lo) <= q16_sqrt(hi), "{lo} {hi}");
    }
}

#[test]
fn vec2_constructors() {
    assert_eq!(Vec2Q16::from_ints(3, -4), Vec2Q16::new(3 << 16, -4 << 16));
    assert_eq!(Vec2Q16::default(), Vec2Q16::new(0, 0));
}

// ---------------------------------------------------------------- physics step

fn player_at(x: i32, y: i32) -> PlayerEntityQ16 {
    PlayerEntityQ16 { peer_id: 1, pos: Vec2Q16::from_ints(x, y), hp: 100, ..Default::default() }
}

#[test]
fn step_moves_by_input_bits() {
    let arena = q16_from_i32(100);
    let cases: [(u16, (i32, i32)); 8] = [
        (0x00, (50, 50)),
        (0x01, (52, 50)),
        (0x02, (48, 50)),
        (0x04, (50, 52)),
        (0x08, (50, 48)),
        (0x03, (50, 50)), // +X and -X cancel
        (0x05, (52, 52)),
        (0x11, (54, 50)), // dash doubles speed
    ];
    for (bits, (x, y)) in cases {
        let mut p = player_at(50, 50);
        p.step_physics(bits, arena);
        assert_eq!(p.pos, Vec2Q16::from_ints(x, y), "bits {bits:#x}");
        assert_eq!(p.last_input_bits, bits);
    }
    let mut p = player_at(50, 50);
    p.step_physics(0x19, arena);
    assert_eq!(p.vel, Vec2Q16::from_ints(4, -4));
}

#[test]
fn step_clamps_to_arena() {
    let arena = q16_from_i32(100);
    let mut p = player_at(99, 1);
    p.step_physics(0x01 | 0x08 | 0x10, arena);
    assert_eq!(p.pos, Vec2Q16::new(arena, 0));
    let mut r = rng(6);
    let mut p = player_at(50, 50);
    for _ in 0..CASES {
        p.step_physics((range(&mut r, 0, 0x1f)) as u16, arena);
        assert!((0..=arena).contains(&p.pos.x) && (0..=arena).contains(&p.pos.y), "{p:?}");
    }
}

#[test]
fn step_is_deterministic_replay() {
    // The same input sequence from the same state gives a bit-identical state: the basis of rollback.
    let arena = q16_from_i32(256);
    let inputs: Vec<u16> = {
        let mut r = rng(7);
        (0..600).map(|_| range(&mut r, 0, 0x1f) as u16).collect()
    };
    let run = || {
        let mut p = player_at(128, 128);
        for &i in &inputs {
            p.step_physics(i, arena);
        }
        p
    };
    assert_eq!(run(), run());
}

// ---------------------------------------------------------------- line of sight

fn wall(x0: i32, y0: i32, x1: i32, y1: i32) -> AabbObstacleQ16 {
    AabbObstacleQ16 { min_x: x0 << 16, min_y: y0 << 16, max_x: x1 << 16, max_y: y1 << 16 }
}

#[test]
fn line_of_sight_basic() {
    let a = Vec2Q16::from_ints(0, 0);
    let b = Vec2Q16::from_ints(32, 0);
    assert!(has_line_of_sight_q16(a, b, &[]));
    assert!(!has_line_of_sight_q16(a, b, &[wall(10, -2, 14, 2)]), "wall across the segment");
    assert!(has_line_of_sight_q16(a, b, &[wall(10, 5, 14, 9)]), "wall beside the segment");
    assert!(!has_line_of_sight_q16(a, b, &[wall(10, 5, 14, 9), wall(30, -1, 40, 1)]), "any wall blocks");
    assert!(!has_line_of_sight_q16(a, a, &[wall(-1, -1, 1, 1)]), "standing inside a wall");
    // Boundaries are inclusive.
    assert!(!has_line_of_sight_q16(a, b, &[wall(32, 0, 33, 1)]));
}

#[test]
fn line_of_sight_is_symmetric_for_walls_wider_than_a_sample() {
    // Walls at least one sample step thick in both axes are seen from both ends.
    let mut r = rng(8);
    for _ in 0..CASES {
        let mut pt = || Vec2Q16::from_ints(range(&mut r, 0, 64) as i32, range(&mut r, 0, 64) as i32);
        let (a, b) = (pt(), pt());
        let step = ((a.x - b.x).abs().max((a.y - b.y).abs()) >> 16) / 16 + 1;
        let (x, y) = (range(&mut r, 0, 64) as i32, range(&mut r, 0, 64) as i32);
        let w = [wall(x, y, x + step, y + step)];
        assert_eq!(has_line_of_sight_q16(a, b, &w), has_line_of_sight_q16(b, a, &w), "{a:?} {b:?} {w:?}");
    }
}

/// A wall thinner than `|segment| / 16` used to fall between the 17 samples of the old raycast and leak fog of
/// war on long sight lines.
#[test]
fn thin_wall_blocks_long_segment() {
    let a = Vec2Q16::from_ints(0, 0);
    let b = Vec2Q16::from_ints(160, 0);
    assert!(!has_line_of_sight_q16(a, b, &[wall(5, -1, 6, 1)]));
    // Zero-thickness and one-ulp walls, including across the whole i32 range.
    assert!(!has_line_of_sight_q16(a, b, &[wall(7, -1, 7, 1)]), "zero thickness");
    let ulp = AabbObstacleQ16 { min_x: 0, min_y: -1, max_x: 0, max_y: 1 };
    assert!(!has_line_of_sight_q16(Vec2Q16::new(i32::MIN, -1), Vec2Q16::new(i32::MAX, 1), &[ulp]));
    // A diagonal through a one-ulp-thick wall.
    let (c, d) = (Vec2Q16::from_ints(0, 0), Vec2Q16::from_ints(1000, 777));
    let wall_x = AabbObstacleQ16 { min_x: 500 << 16, min_y: 0, max_x: (500 << 16) + 1, max_y: 1000 << 16 };
    assert!(!has_line_of_sight_q16(c, d, &[wall_x]));
}

#[test]
fn line_of_sight_touch_graze_and_end_inside() {
    let a = Vec2Q16::from_ints(0, 0);
    let b = Vec2Q16::from_ints(100, 0);
    // Grazing an edge or a corner blocks (edges are inclusive); one ulp off does not.
    assert!(!has_line_of_sight_q16(a, b, &[wall(40, 0, 60, 10)]), "runs along the bottom edge");
    assert!(!has_line_of_sight_q16(a, b, &[wall(40, -10, 60, 0)]), "runs along the top edge");
    let off = AabbObstacleQ16 { min_x: 40 << 16, min_y: 1, max_x: 60 << 16, max_y: 10 << 16 };
    assert!(has_line_of_sight_q16(a, b, &[off]), "one ulp above the line");
    let (d0, d1) = (Vec2Q16::from_ints(0, 0), Vec2Q16::from_ints(10, 10));
    assert!(!has_line_of_sight_q16(d0, d1, &[wall(5, -5, 10, 5)]), "touches the corner at (5, 5)");
    let corner = AabbObstacleQ16 { min_x: (5 << 16) + 1, min_y: -(5 << 16), max_x: 10 << 16, max_y: 5 << 16 };
    assert!(has_line_of_sight_q16(d0, d1, &[corner]), "misses the corner by one ulp");
    // Segments that start, end, or lie entirely inside a wall.
    assert!(!has_line_of_sight_q16(a, b, &[wall(95, -1, 120, 1)]), "ends inside");
    assert!(!has_line_of_sight_q16(a, b, &[wall(-5, -1, 5, 1)]), "starts inside");
    assert!(!has_line_of_sight_q16(a, b, &[wall(-5, -5, 200, 5)]), "entirely inside");
    // Touching only at the end point blocks; stopping one ulp short does not.
    assert!(!has_line_of_sight_q16(a, b, &[wall(100, -1, 110, 1)]), "end point on the edge");
    let short = Vec2Q16::new((100 << 16) - 1, 0);
    assert!(has_line_of_sight_q16(a, short, &[wall(100, -1, 110, 1)]), "stops one ulp short");
    // Walls in line with the segment but past either end, and a point segment beside a wall.
    assert!(has_line_of_sight_q16(a, b, &[wall(101, -1, 110, 1), wall(-10, -1, -1, 1)]));
    assert!(has_line_of_sight_q16(a, a, &[wall(1, 1, 2, 2)]));
}

/// Exact reference on a small integer grid. The segment enters and leaves the box at `t = k / |dx|` or
/// `t = k / |dy|`, so if it touches the box at all it does so at some `t = k / (|dx| * |dy|)`: checking those
/// points (exactly, in scaled integers) decides the test.
#[test]
fn line_of_sight_matches_exact_reference_and_is_symmetric() {
    let mut r = rng(12);
    for _ in 0..CASES {
        let mut c = || range(&mut r, 0, 31);
        let (ax, ay, bx, by) = (c(), c(), c(), c());
        let (x0, y0) = (c(), c());
        let (x1, y1) = (x0 + c() / 4, y0 + c() / 4);
        let w = [wall(x0 as i32, y0 as i32, x1 as i32, y1 as i32)];
        let (a, b) = (Vec2Q16::from_ints(ax as i32, ay as i32), Vec2Q16::from_ints(bx as i32, by as i32));
        let n = (bx - ax).abs().max(1) * (by - ay).abs().max(1);
        let hit = (0..=n).any(|k| {
            let (px, py) = (ax * n + (bx - ax) * k, ay * n + (by - ay) * k);
            px >= x0 * n && px <= x1 * n && py >= y0 * n && py <= y1 * n
        });
        assert_eq!(has_line_of_sight_q16(a, b, &w), !hit, "{a:?} {b:?} {w:?}");
        assert_eq!(has_line_of_sight_q16(a, b, &w), has_line_of_sight_q16(b, a, &w), "{a:?} {b:?} {w:?}");
    }
}
