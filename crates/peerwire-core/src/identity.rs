use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{CryptoRng, RngCore};
use std::fmt;

/// A peer's public identity: its Ed25519 verifying key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The X25519 (Montgomery) form of this identity, used as the Noise static key.
    /// `None` if the bytes are not a valid Ed25519 point.
    pub(crate) fn noise_static(&self) -> Option<[u8; 32]> {
        VerifyingKey::from_bytes(&self.0).ok().map(|vk| vk.to_montgomery().to_bytes())
    }

    /// Verifies an Ed25519 signature made by this peer.
    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> bool {
        match VerifyingKey::from_bytes(&self.0) {
            Ok(vk) => vk.verify(msg, &Signature::from_bytes(sig)).is_ok(),
            Err(_) => false,
        }
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({self})")
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A local identity: an Ed25519 signing key.
#[derive(Clone)]
pub struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn generate<R: RngCore + CryptoRng>(rng: &mut R) -> Self {
        Identity { key: SigningKey::generate(rng) }
    }

    pub fn from_secret_bytes(secret: &[u8; 32]) -> Self {
        Identity { key: SigningKey::from_bytes(secret) }
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    pub fn peer_id(&self) -> PeerId {
        PeerId(self.key.verifying_key().to_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }

    /// The X25519 scalar matching [`PeerId::noise_static`] (clamped by X25519 itself).
    pub(crate) fn noise_private(&self) -> [u8; 32] {
        self.key.to_scalar_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    #[test]
    fn noise_static_matches_private_scalar() {
        let mut rng = ChaCha20Rng::seed_from_u64(1);
        for _ in 0..16 {
            let id = Identity::generate(&mut rng);
            let resolver = snow::resolvers::DefaultResolver;
            use snow::resolvers::CryptoResolver;
            let mut dh = resolver.resolve_dh(&snow::params::DHChoice::Curve25519).unwrap();
            dh.set(&id.noise_private());
            assert_eq!(dh.pubkey(), &id.peer_id().noise_static().unwrap()[..]);
        }
    }

    #[test]
    fn sign_verify() {
        let id = Identity::generate(&mut ChaCha20Rng::seed_from_u64(2));
        let sig = id.sign(b"hello");
        assert!(id.peer_id().verify(b"hello", &sig));
        assert!(!id.peer_id().verify(b"hellp", &sig));
    }
}
