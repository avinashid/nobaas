//! Invites: everything needed to reach a peer, as one short string for a
//! link, a QR code or a signalling message.
//!
//! ```text
//! "nb:" base64url( version(1)=1|2 peer(32) [key(32), version 2] n_addrs(u8) [alen(u8) addr]*
//!                  n_relays(u8) [relay_peer(32) alen(u8) addr]* )
//! ```
//! The PeerId is a public key, and the handshake authenticates whoever answers
//! at those addresses. The optional `key` is a capability, not a session
//! secret: whoever holds the invite may ask the peer, over signaling, to dial
//! back (to punch through NATs); someone who knows only the PeerId may not.

use crate::addr::{Address, MAX_ADDR_LEN};
use crate::identity::PeerId;

const VERSION: u8 = 1;
const VERSION_KEYED: u8 = 2;
const PREFIX: &str = "nb:";
const MAX_ENTRIES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite<A> {
    pub peer: PeerId,
    /// Proves to the peer, in signaling offers, that the sender holds this invite.
    pub key: Option<[u8; 32]>,
    /// Addresses the peer may be reachable at (LAN, public, STUN-learned).
    pub addrs: Vec<A>,
    /// Peers that will relay for this one, and where to find them.
    pub relays: Vec<(PeerId, A)>,
}

impl<A: Address> Invite<A> {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = vec![if self.key.is_some() { VERSION_KEYED } else { VERSION }];
        v.extend_from_slice(self.peer.as_bytes());
        if let Some(k) = &self.key {
            v.extend_from_slice(k);
        }
        let put_addr = |v: &mut Vec<u8>, a: &A| {
            let start = v.len();
            v.push(0);
            a.encode(v);
            v[start] = (v.len() - start - 1) as u8;
        };
        v.push(self.addrs.len().min(MAX_ENTRIES) as u8);
        for a in self.addrs.iter().take(MAX_ENTRIES) {
            put_addr(&mut v, a);
        }
        v.push(self.relays.len().min(MAX_ENTRIES) as u8);
        for (p, a) in self.relays.iter().take(MAX_ENTRIES) {
            v.extend_from_slice(p.as_bytes());
            put_addr(&mut v, a);
        }
        v
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let mut r = Reader(b);
        let version = r.take(1)?[0];
        if version != VERSION && version != VERSION_KEYED {
            return None;
        }
        let peer = r.peer()?;
        let key = match version {
            VERSION_KEYED => Some(r.take(32)?.try_into().ok()?),
            _ => None,
        };
        let n = r.take(1)?[0] as usize;
        let mut addrs = Vec::new();
        for _ in 0..n.min(MAX_ENTRIES) {
            addrs.push(r.addr()?);
        }
        let n = r.take(1)?[0] as usize;
        let mut relays = Vec::new();
        for _ in 0..n.min(MAX_ENTRIES) {
            relays.push((r.peer()?, r.addr()?));
        }
        r.0.is_empty().then_some(Invite { peer, key, addrs, relays })
    }

    pub fn to_link(&self) -> String {
        format!("{PREFIX}{}", b64::encode(&self.to_bytes()))
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::from_bytes(&b64::decode(s.trim().strip_prefix(PREFIX)?)?)
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (h, t) = (self.0.get(..n)?, &self.0[n..]);
        self.0 = t;
        Some(h)
    }

    fn peer(&mut self) -> Option<PeerId> {
        let mut id = [0u8; 32];
        id.copy_from_slice(self.take(32)?);
        let p = PeerId(id);
        p.noise_static().map(|_| p)
    }

    fn addr<A: Address>(&mut self) -> Option<A> {
        let n = self.take(1)?[0] as usize;
        if n > MAX_ADDR_LEN {
            return None;
        }
        A::decode(self.take(n)?)
    }
}

/// Unpadded base64url (RFC 4648 section 5).
mod b64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    pub(super) fn encode(b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..=c.len() {
                s.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        s
    }

    pub(super) fn decode(s: &str) -> Option<Vec<u8>> {
        let val = |c: u8| ALPHABET.iter().position(|&a| a == c).map(|p| p as u32);
        let b = s.as_bytes();
        if b.len() % 4 == 1 {
            return None;
        }
        let mut out = Vec::with_capacity(b.len() * 3 / 4);
        for c in b.chunks(4) {
            let mut n = 0u32;
            for (i, &ch) in c.iter().enumerate() {
                n |= val(ch)? << (18 - 6 * i);
            }
            let bytes = n.to_be_bytes();
            out.extend_from_slice(&bytes[1..c.len()]);
        }
        Some(out)
    }

    #[test]
    fn rfc4648_vectors() {
        for (raw, enc) in [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE")] {
            assert_eq!(encode(raw.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), raw.as_bytes());
        }
        assert_eq!(decode("Zm9v+g"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use rand_core::OsRng;
    use std::net::SocketAddr;

    #[test]
    fn roundtrip_and_rejects_garbage() {
        let me = Identity::generate(&mut OsRng).peer_id();
        let relay = Identity::generate(&mut OsRng).peer_id();
        let mut inv = Invite::<SocketAddr> {
            peer: me,
            key: None,
            addrs: vec!["192.168.1.5:7000".parse().unwrap(), "[2001:db8::7]:7000".parse().unwrap()],
            relays: vec![(relay, "203.0.113.9:443".parse().unwrap())],
        };
        let s = inv.to_link();
        assert!(s.starts_with("nb:"));
        assert_eq!(Invite::parse(&s), Some(inv.clone()));
        println!("invite ({} chars): {s}", s.len());
        let b = inv.to_bytes();
        for cut in 0..b.len() {
            assert_eq!(Invite::<SocketAddr>::from_bytes(&b[..cut]), None);
        }
        let mut extra = b.clone();
        extra.push(0);
        assert_eq!(Invite::<SocketAddr>::from_bytes(&extra), None);
        assert_eq!(Invite::<SocketAddr>::parse("p1:AAAA"), None);
        // Version 2 carries a key.
        inv.key = Some([9; 32]);
        let b = inv.to_bytes();
        assert_eq!((b[0], Invite::from_bytes(&b)), (2, Some(inv.clone())));
        for cut in 0..b.len() {
            assert_eq!(Invite::<SocketAddr>::from_bytes(&b[..cut]), None);
        }
        let mut v3 = b.clone();
        v3[0] = 3;
        assert_eq!(Invite::<SocketAddr>::from_bytes(&v3), None);
    }
}
