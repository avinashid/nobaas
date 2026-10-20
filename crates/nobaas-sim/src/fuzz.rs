//! A seeded random-input harness for fuzz tests on the stable toolchain (no
//! cargo-fuzz), shared by the crates' parser fuzzers.
//!
//! Cases derive from `NB_FUZZ_SEED` (fixed by default, so CI is deterministic)
//! and `NB_FUZZ_SCALE` multiplies their number for a longer soak:
//!
//! ```text
//! NB_FUZZ_SCALE=50 NB_FUZZ_SEED=7 cargo test --workspace --release fuzz -- --nocapture
//! ```
//! A panicking case prints its target, index and input as hex, so it can be
//! replayed as a regression test.

use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::panic::{self, AssertUnwindSafe};

pub struct Rng(ChaCha20Rng);

impl Rng {
    /// A generator for target `name`, from `NB_FUZZ_SEED`.
    pub fn for_target(name: &str) -> Self {
        let mut s = seed();
        for b in name.bytes() {
            s = (s ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        Rng(ChaCha20Rng::seed_from_u64(s))
    }

    pub fn u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    pub fn u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    pub fn byte(&mut self) -> u8 {
        self.0.next_u32() as u8
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.0.next_u64() % n as u64) as usize
    }

    /// True one time in `n`.
    pub fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.0.fill_bytes(&mut v);
        v
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }

    /// Boundary values first, then anything.
    pub fn edgy_u32(&mut self) -> u32 {
        match self.below(6) {
            0 => self.below(4) as u32,
            1 => u32::MAX - self.below(4) as u32,
            2 => 1u32 << self.below(32),
            3 => (1u32 << self.below(32)) - 1,
            _ => self.u32(),
        }
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn seed() -> u64 {
    env("NB_FUZZ_SEED", 0x6e62_6675_7a7a)
}

/// `n` cases, times `NB_FUZZ_SCALE`.
pub fn cases(n: usize) -> usize {
    n * env("NB_FUZZ_SCALE", 1).max(1) as usize
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Mutates `b` in place with one to four of: bit flips, random or boundary
/// bytes, boundary u16 lengths (the formats' length fields), inserts, deletes,
/// duplicated chunks and truncation. Stays within `max_len` bytes.
pub fn mutate(rng: &mut Rng, b: &mut Vec<u8>, max_len: usize) {
    for _ in 0..1 + rng.below(4) {
        let at = rng.below(b.len() + 1);
        match rng.below(8) {
            0 if at < b.len() => b[at] ^= 1 << rng.below(8),
            1 if at < b.len() => b[at] = rng.byte(),
            2 if at < b.len() => b[at] = *rng.pick(&[0, 1, 2, 3, 0x7f, 0x80, 0xfe, 0xff, 0x11, 0x12, 0x13]),
            3 if at + 2 <= b.len() => {
                let n = b.len() as u16;
                let v = *rng.pick(&[0, 1, n.wrapping_sub(at as u16), n, n.wrapping_add(1), 0x7fff, 0xffff]);
                let v = if rng.one_in(2) { v.to_le_bytes() } else { v.to_be_bytes() };
                b[at..at + 2].copy_from_slice(&v);
            }
            4 => {
                let n = 1 + rng.below(8);
                let ins = rng.bytes(n);
                b.splice(at..at, ins);
            }
            5 if at < b.len() => {
                let end = (at + 1 + rng.below(16)).min(b.len());
                b.drain(at..end);
            }
            6 if !b.is_empty() => {
                let from = rng.below(b.len());
                let end = (from + 1 + rng.below(32)).min(b.len());
                let chunk = b[from..end].to_vec();
                b.splice(at..at, chunk);
            }
            _ => b.truncate(at),
        }
    }
    b.truncate(max_len);
}

/// Feeds `cases` inputs from `gen` to `check`, reporting the input of a case that panics.
pub fn run(name: &str, cases: usize, mut gen: impl FnMut(&mut Rng) -> Vec<u8>, mut check: impl FnMut(&[u8])) {
    let mut rng = Rng::for_target(name);
    for i in 0..cases {
        let input = gen(&mut rng);
        if let Err(e) = panic::catch_unwind(AssertUnwindSafe(|| check(&input))) {
            eprintln!("fuzz {name}: case {i} of seed {:#x} failed on input {}", seed(), hex(&input));
            panic::resume_unwind(e);
        }
    }
}
