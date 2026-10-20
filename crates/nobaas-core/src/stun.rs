//! Minimal STUN (RFC 5389) Binding client codec, for learning our public
//! address from any STUN server before we have peers to ask.
//!
//! STUN shares the socket with Nobaas packets. Following RFC 7983, STUN messages
//! start with a byte in 0..=3 and Nobaas packets with 16..=31 (version nibble 1),
//! so [`is_stun`] demultiplexes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED: u16 = 0x0001;
const ATTR_XOR_MAPPED: u16 = 0x0020;

pub type TxId = [u8; 12];

/// Whether a datagram looks like a STUN message rather than a Nobaas packet.
pub fn is_stun(b: &[u8]) -> bool {
    b.len() >= 20 && b[0] <= 3 && u32::from_be_bytes([b[4], b[5], b[6], b[7]]) == MAGIC
}

pub fn binding_request(tx: &TxId) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    v.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&MAGIC.to_be_bytes());
    v.extend_from_slice(tx);
    v
}

/// The reflexive address in a Binding success response to transaction `tx`.
pub fn parse_binding_response(b: &[u8], tx: &TxId) -> Option<SocketAddr> {
    if !is_stun(b) || u16::from_be_bytes([b[0], b[1]]) != BINDING_SUCCESS || &b[8..20] != tx {
        return None;
    }
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    let mut attrs = b.get(20..20 + len)?;
    let mut mapped = None;
    while attrs.len() >= 4 {
        let t = u16::from_be_bytes([attrs[0], attrs[1]]);
        let l = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let v = attrs.get(4..4 + l)?;
        match t {
            ATTR_XOR_MAPPED => return decode_addr(v, Some(tx)),
            ATTR_MAPPED => mapped = decode_addr(v, None),
            _ => {}
        }
        attrs = attrs.get((4 + l).next_multiple_of(4)..).unwrap_or(&[]);
    }
    mapped
}

fn decode_addr(v: &[u8], xor: Option<&TxId>) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let mut key = [0u8; 16];
    if let Some(tx) = xor {
        key[..4].copy_from_slice(&MAGIC.to_be_bytes());
        key[4..].copy_from_slice(tx);
    }
    let port = u16::from_be_bytes([v[2] ^ key[0], v[3] ^ key[1]]);
    let ip = match (v[1], v.len()) {
        (1, 8) => IpAddr::V4(Ipv4Addr::new(v[4] ^ key[0], v[5] ^ key[1], v[6] ^ key[2], v[7] ^ key[3])),
        (2, 20) => {
            let mut o = [0u8; 16];
            for i in 0..16 {
                o[i] = v[4 + i] ^ key[i];
            }
            IpAddr::V6(Ipv6Addr::from(o))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc5769_ipv4_response() {
        // RFC 5769 section 2.2: response carrying XOR-MAPPED-ADDRESS 192.0.2.1:32853
        let tx: TxId = [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae];
        let msg: [u8; 80] = [
            0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa,
            0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f,
            0x72, 0x20, 0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43, 0x00, 0x08, 0x00,
            0x14, 0x2b, 0x91, 0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53,
            0xf0, 0x6b, 0xe7, 0xd7, 0x80, 0x28, 0x00, 0x04, 0xc0, 0x7d, 0x4c, 0x96,
        ];
        assert!(is_stun(&msg));
        assert_eq!(parse_binding_response(&msg, &tx), Some("192.0.2.1:32853".parse().unwrap()));
        let mut other = tx;
        other[0] ^= 1;
        assert_eq!(parse_binding_response(&msg, &other), None);
        for cut in 0..msg.len() {
            let _ = parse_binding_response(&msg[..cut], &tx);
        }
    }

    #[test]
    fn rfc5769_ipv6_response() {
        // RFC 5769 section 2.3: XOR-MAPPED-ADDRESS [2001:db8:1234:5678:11:2233:4455:6677]:32853
        let tx: TxId = [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae];
        let mut msg = vec![0x01, 0x01, 0x00, 0x18, 0x21, 0x12, 0xa4, 0x42];
        msg.extend_from_slice(&tx);
        msg.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc,
            0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
        ]);
        assert_eq!(
            parse_binding_response(&msg, &tx),
            Some("[2001:db8:1234:5678:11:2233:4455:6677]:32853".parse().unwrap())
        );
    }

    #[test]
    fn nobaas_packets_are_not_stun() {
        let req = binding_request(&[9; 12]);
        assert!(is_stun(&req));
        for kind in 0x11..=0x13 {
            let mut p0 = req.clone();
            p0[0] = kind; // HS1, HS2, DATA
            assert!(!is_stun(&p0));
        }
    }
}
