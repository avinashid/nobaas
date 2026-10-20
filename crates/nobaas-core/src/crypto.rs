//! Noise setup with a host-seeded RNG, and the anti-replay window for data packets.

use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use snow::params::{CipherChoice, DHChoice, HashChoice, NoiseParams};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::types::{Cipher, Dh, Hash, Random};

pub(crate) const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub(crate) const PROLOGUE: &[u8] = b"nobaas-core/1";

struct SeededRandom(ChaCha20Rng);

impl Random for SeededRandom {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), snow::Error> {
        self.0.fill_bytes(dest);
        Ok(())
    }
}

/// Snow's default primitives, but with randomness (ephemeral keys) drawn from a
/// seed the endpoint supplies, so simulator runs are reproducible.
struct SeededResolver {
    seed: [u8; 32],
}

impl CryptoResolver for SeededResolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(SeededRandom(ChaCha20Rng::from_seed(self.seed))))
    }

    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        DefaultResolver.resolve_dh(choice)
    }

    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        DefaultResolver.resolve_hash(choice)
    }

    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        DefaultResolver.resolve_cipher(choice)
    }
}

pub(crate) fn builder<'a>(
    seed: [u8; 32],
    local_private: &'a [u8],
) -> Result<snow::Builder<'a>, snow::Error> {
    builder_for(NOISE_PARAMS, PROLOGUE, seed, local_private)
}

pub(crate) fn builder_for<'a>(
    pattern: &str,
    prologue: &'a [u8],
    seed: [u8; 32],
    local_private: &'a [u8],
) -> Result<snow::Builder<'a>, snow::Error> {
    let params: NoiseParams = pattern.parse()?;
    snow::Builder::with_resolver(params, Box::new(SeededResolver { seed }))
        .prologue(prologue)?
        .local_private_key(local_private)
}

const WINDOW: u64 = 2048;

/// Sliding anti-replay window over data-packet nonces (as in WireGuard).
pub(crate) struct ReplayWindow {
    top: u64,
    any: bool,
    bits: [u64; (WINDOW / 64) as usize],
}

impl ReplayWindow {
    pub(crate) fn new() -> Self {
        ReplayWindow { top: 0, any: false, bits: [0; (WINDOW / 64) as usize] }
    }

    fn slot(n: u64) -> (usize, u64) {
        let i = n % WINDOW;
        ((i / 64) as usize, 1u64 << (i % 64))
    }

    /// True if `n` has not been seen and is not too old. Cheap, so call it before decrypting.
    pub(crate) fn check(&self, n: u64) -> bool {
        if !self.any || n > self.top {
            return true;
        }
        if self.top - n >= WINDOW {
            return false;
        }
        let (w, m) = Self::slot(n);
        self.bits[w] & m == 0
    }

    /// Records `n` as seen. Call only after the packet authenticated.
    pub(crate) fn commit(&mut self, n: u64) {
        if !self.any || n.saturating_sub(self.top) >= WINDOW {
            self.bits = [0; (WINDOW / 64) as usize];
            self.any = true;
            self.top = n;
        } else if n > self.top {
            for k in self.top + 1..=n {
                let (w, m) = Self::slot(k);
                self.bits[w] &= !m;
            }
            self.top = n;
        }
        let (w, m) = Self::slot(n);
        self.bits[w] |= m;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::new();
        assert!(w.check(5));
        w.commit(5);
        assert!(!w.check(5));
        assert!(w.check(3));
        w.commit(3);
        assert!(!w.check(3));
        w.commit(5000);
        assert!(!w.check(5000));
        assert!(!w.check(5)); // too old now
        assert!(w.check(4999));
        assert!(!w.check(5000 - WINDOW));
        assert!(w.check(5000 - WINDOW + 1));
        w.commit(4999);
        assert!(!w.check(4999));
        // bits reused after the window slides must be cleared
        w.commit(5000 + WINDOW - 1);
        assert!(w.check(5000 + WINDOW - 2));
    }
}
