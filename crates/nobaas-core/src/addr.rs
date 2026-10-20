//! Transport addresses and the routes a link can take.

use crate::identity::PeerId;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Whatever the transport uses to address a remote: a `SocketAddr`, a
/// simulator address, a WebRTC DataChannel handle.
///
/// Addresses travel inside encrypted frames (a peer tells you the address it
/// sees you at, a relay tells you where a dialler came from), so they must
/// encode to at most [`MAX_ADDR_LEN`] bytes.
pub trait Address: Clone + Eq + Hash + Debug {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(b: &[u8]) -> Option<Self>;
    /// The same host with the port moved by `delta`, for guessing the next
    /// mapping of a NAT that allocates ports in sequence. `None` if the
    /// transport has no ports.
    fn port_offset(&self, _delta: i32) -> Option<Self> {
        None
    }
    /// The UDP address this stands for, if any: lets a host ask STUN servers
    /// over any transport whose addresses are IP addresses.
    fn to_socket_addr(&self) -> Option<SocketAddr> {
        None
    }
    /// The reverse of [`to_socket_addr`](Address::to_socket_addr).
    fn from_socket_addr(_addr: SocketAddr) -> Option<Self> {
        None
    }
}

pub const MAX_ADDR_LEN: usize = 19;

impl Address for SocketAddr {
    fn encode(&self, out: &mut Vec<u8>) {
        match self.ip() {
            IpAddr::V4(ip) => {
                out.push(4);
                out.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                out.push(6);
                out.extend_from_slice(&ip.octets());
            }
        }
        out.extend_from_slice(&self.port().to_le_bytes());
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let (ip, rest) = match b.first()? {
            4 if b.len() == 7 => (IpAddr::V4(Ipv4Addr::new(b[1], b[2], b[3], b[4])), &b[5..]),
            6 if b.len() == 19 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(&b[1..17]);
                (IpAddr::V6(Ipv6Addr::from(o)), &b[17..])
            }
            _ => return None,
        };
        Some(SocketAddr::new(ip, u16::from_le_bytes([rest[0], rest[1]])))
    }

    fn port_offset(&self, delta: i32) -> Option<Self> {
        let p = i32::from(self.port()) + delta;
        (1..=65535).contains(&p).then(|| SocketAddr::new(self.ip(), p as u16))
    }

    fn to_socket_addr(&self) -> Option<SocketAddr> {
        Some(*self)
    }

    fn from_socket_addr(addr: SocketAddr) -> Option<Self> {
        Some(addr)
    }
}

/// Opaque transport handles (a WebRTC DataChannel id, a simulator node). They
/// mean nothing to other peers, so hosts using them should turn off
/// `Config::share_addresses`.
impl Address for u32 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }
    fn decode(b: &[u8]) -> Option<Self> {
        Some(u32::from_le_bytes(b.try_into().ok()?))
    }
}

impl Address for u64 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }
    fn decode(b: &[u8]) -> Option<Self> {
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }
}

/// How packets for a link reach the peer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Path<A> {
    Direct(A),
    /// Through a peer we hold a link to. The relay sees only ciphertext.
    Via(PeerId),
}

impl<A> Path<A> {
    pub fn direct(&self) -> Option<&A> {
        match self {
            Path::Direct(a) => Some(a),
            Path::Via(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_addr_roundtrip() {
        for s in ["1.2.3.4:5", "[2001:db8::1]:65535", "0.0.0.0:0"] {
            let a: SocketAddr = s.parse().unwrap();
            let mut b = Vec::new();
            a.encode(&mut b);
            assert!(b.len() <= MAX_ADDR_LEN);
            assert_eq!(SocketAddr::decode(&b), Some(a));
            assert_eq!(SocketAddr::decode(&b[..b.len() - 1]), None);
        }
        let a: SocketAddr = "1.2.3.4:65535".parse().unwrap();
        assert_eq!(a.port_offset(1), None);
        assert_eq!(a.port_offset(-5).unwrap().port(), 65530);
    }
}
