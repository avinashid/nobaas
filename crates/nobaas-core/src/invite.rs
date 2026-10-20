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
        // A count over MAX_ENTRIES is refused, not clamped: each invite has one encoding.
        let n = r.count()?;
        let mut addrs = Vec::new();
        for _ in 0..n {
            addrs.push(r.addr()?);
        }
        let n = r.count()?;
        let mut relays = Vec::new();
        for _ in 0..n {
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

    fn count(&mut self) -> Option<usize> {
        let n = self.take(1)?[0] as usize;
        (n <= MAX_ENTRIES).then_some(n)
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

/// Unpadded base64url (RFC 4648 section 5), canonical: decoding refuses a last
/// symbol whose unused low bits are set, so every byte string has one link.
mod b64 {
    pub(super) const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

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
            // Bits below the decoded bytes: 4 of them after 2 symbols, 2 after 3, none after 4.
            if n & (0xFF_FFFF >> (8 * (c.len() - 1))) != 0 {
                return None;
            }
            let bytes = n.to_be_bytes();
            out.extend_from_slice(&bytes[1..c.len()]);
        }
        Some(out)
    }

    #[test]
    fn rfc4648_vectors() {
        for (raw, enc) in
            [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE")]
        {
            assert_eq!(encode(raw.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), raw.as_bytes());
        }
        assert_eq!(decode("Zm9v+g"), None);
    }

    #[test]
    fn rejects_non_canonical_tails() {
        // "Zg" is "f"; "Zh".."Zv" set some of the 4 unused bits and used to decode to "f" too.
        for last in ALPHABET.iter().map(|&c| c as char) {
            let ok = ALPHABET.iter().position(|&a| a as char == last).unwrap() & 15 == 0;
            assert_eq!(decode(&format!("Z{last}")).is_some(), ok, "Z{last}");
        }
        // "Zm8" is "fo"; the last symbol has 2 unused bits.
        for last in ALPHABET.iter().map(|&c| c as char) {
            let ok = ALPHABET.iter().position(|&a| a as char == last).unwrap() & 3 == 0;
            assert_eq!(decode(&format!("Zm{last}")).is_some(), ok, "Zm{last}");
        }
        assert_eq!(decode("Zh"), None);
        assert_eq!(decode("Zm9"), None);
        // Every byte string survives a round trip, and its encoding is the only one accepted.
        for len in 0..7 {
            for seed in 0..64u8 {
                let raw: Vec<u8> =
                    (0..len).map(|i| seed.wrapping_mul(37).wrapping_add((i as u8).wrapping_mul(101))).collect();
                assert_eq!(decode(&encode(&raw)).unwrap(), raw);
            }
        }
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

    #[test]
    fn one_link_per_invite() {
        let me = Identity::generate(&mut OsRng).peer_id();
        let relay = Identity::generate(&mut OsRng).peer_id();
        let full = |n: usize| Invite::<u32> {
            peer: me,
            key: n.is_multiple_of(2).then_some([7; 32]),
            addrs: (0..n as u32).collect(),
            relays: (0..n as u32).map(|a| (relay, a)).collect(),
        };
        // 67 + 42n bytes with a key and 35 + 42n without: base64 tails of 2 and of 3 symbols.
        for n in 0..=MAX_ENTRIES {
            let inv = full(n);
            let s = inv.to_link();
            assert_eq!(Invite::parse(&s).as_ref(), Some(&inv));
            assert_eq!(Invite::<u32>::parse(&s).unwrap().to_link(), s);
            // The same link with the unused low bits of its last symbol set is refused.
            let tail = (s.len() - PREFIX.len()) % 4;
            if tail != 0 {
                let last = s.as_bytes()[s.len() - 1];
                let i = b64::ALPHABET.iter().position(|&a| a == last).unwrap();
                for bits in 1..(if tail == 2 { 16 } else { 4 }) {
                    let lookalike = format!("{}{}", &s[..s.len() - 1], b64::ALPHABET[i | bits] as char);
                    assert_eq!(Invite::<u32>::parse(&lookalike), None, "{lookalike}");
                }
            }
        }
        // A count over 16 is refused, even when exactly 16 entries follow (it used to be clamped).
        let b = full(MAX_ENTRIES).to_bytes();
        let n_addrs = 1 + 32 + 32;
        let n_relays = n_addrs + 1 + MAX_ENTRIES * 5;
        assert_eq!((b[n_addrs], b[n_relays]), (16, 16));
        for (at, count) in [(n_addrs, 17), (n_addrs, 255), (n_relays, 17), (n_relays, 255)] {
            let mut bad = b.clone();
            bad[at] = count;
            assert_eq!(Invite::<u32>::from_bytes(&bad), None, "count {count} at {at}");
            assert_eq!(Invite::<u32>::parse(&format!("{PREFIX}{}", b64::encode(&bad))), None);
        }
        // to_bytes never writes more than 16 of each.
        let mut big = full(MAX_ENTRIES);
        big.addrs.push(99);
        big.relays.push((relay, 99));
        assert_eq!(Invite::<u32>::from_bytes(&big.to_bytes()), Some(full(MAX_ENTRIES)));
    }
}
