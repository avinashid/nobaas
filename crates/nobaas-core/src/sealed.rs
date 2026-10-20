//! Sealed messages: one-shot, encrypted to a PeerId and authenticated as
//! coming from ours, with no session. For signalling over untrusted carriers
//! (Nostr relays, a QR code, a shared channel) where the payload, such as a
//! WebRTC offer, carries IP addresses.
//!
//! `Noise_X_25519_ChaChaPoly_BLAKE2s`: the recipient's static key is the X25519
//! form of its PeerId; the sender's static key travels encrypted, and the
//! payload starts with the sender's Ed25519 PeerId, which must match it.
//! No replay protection: callers that care put a timestamp or nonce inside.

use crate::crypto;
use crate::identity::{Identity, PeerId};

const PATTERN: &str = "Noise_X_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"nobaas-core/sealed/1";
/// Bytes a sealed message adds to its plaintext.
pub const SEAL_OVERHEAD: usize = 32 + 48 + 32 + 16;
/// Largest plaintext (a Noise message is at most 65535 bytes).
pub const MAX_SEALED: usize = 65535 - SEAL_OVERHEAD;

/// Encrypts `plaintext` to `to`. `seed` must be fresh random bytes (it makes
/// the ephemeral key).
pub fn seal(me: &Identity, to: &PeerId, seed: [u8; 32], plaintext: &[u8]) -> Option<Vec<u8>> {
    if plaintext.len() > MAX_SEALED {
        return None;
    }
    let remote = to.noise_static()?;
    let private = me.noise_private();
    let mut hs = crypto::builder_for(PATTERN, PROLOGUE, seed, &private)
        .ok()?
        .remote_public_key(&remote)
        .ok()?
        .build_initiator()
        .ok()?;
    let mut payload = Vec::with_capacity(32 + plaintext.len());
    payload.extend_from_slice(me.peer_id().as_bytes());
    payload.extend_from_slice(plaintext);
    let mut out = vec![0u8; payload.len() + SEAL_OVERHEAD];
    let n = hs.write_message(&payload, &mut out).ok()?;
    out.truncate(n);
    Some(out)
}

/// Decrypts a message sealed to `me`; returns the authenticated sender.
pub fn open(me: &Identity, sealed: &[u8]) -> Option<(PeerId, Vec<u8>)> {
    let private = me.noise_private();
    let mut hs = crypto::builder_for(PATTERN, PROLOGUE, [0; 32], &private).ok()?.build_responder().ok()?;
    let mut out = vec![0u8; sealed.len()];
    let n = hs.read_message(sealed, &mut out).ok()?;
    if n < 32 {
        return None;
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&out[..32]);
    let from = PeerId(id);
    // The claimed identity must be the one whose X25519 form sealed the message.
    if from.noise_static().as_ref().map(|k| &k[..]) != hs.get_remote_static() {
        return None;
    }
    Some((from, out[32..n].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn seal_open_and_reject() {
        let (a, b, c) =
            (Identity::generate(&mut OsRng), Identity::generate(&mut OsRng), Identity::generate(&mut OsRng));
        let s = seal(&a, &b.peer_id(), [7; 32], b"offer sdp").unwrap();
        assert_eq!(s.len(), 9 + SEAL_OVERHEAD);
        assert_eq!(open(&b, &s), Some((a.peer_id(), b"offer sdp".to_vec())));
        assert_eq!(open(&c, &s), None, "only the recipient can open it");
        for i in 0..s.len() {
            let mut t = s.clone();
            t[i] ^= 1;
            assert_eq!(open(&b, &t), None, "byte {i} tampered");
        }
        // A sender can't claim someone else's PeerId.
        let mut forged = seal(&c, &b.peer_id(), [8; 32], b"x").unwrap();
        assert!(open(&b, &forged).is_some_and(|(p, _)| p == c.peer_id()));
        forged.truncate(10);
        assert_eq!(open(&b, &forged), None);
    }
}
