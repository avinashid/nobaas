//! Packet and frame formats. All parsing is bounds-checked and allocation-free.
//!
//! ```text
//! packet  = kind(1: version<<4 | type) ...
//!   HS1   = kind sender_idx(u32) noise_msg1
//!   HS2   = kind sender_idx(u32) receiver_idx(u32) noise_msg2
//!   DATA  = kind receiver_idx(u32) nonce(u64) aead(frames)
//! frames  = PING | ACK cum(u32) bits(u64) | REL seq(u32) flags(u8) len(u16) data
//!         | UNREL len(u16) data | CLOSE
//!         | OBSERVED alen(u8) addr          the address the sender sees us at
//!         | FWD to(32) len(u16) packet       relay this packet to peer `to`
//!         | FWD_QUIET to(32) len(u16) packet  the same, without tagging it with
//!                                          the sender's address (relay-only peers)
//!         | RELAYED alen(u8) addr len(u16) packet
//!                                          a relayed packet, with the address the
//!                                          relay saw its origin at (alen 0: none)
//!         | PUNCH token(u64) | PUNCH_ACK token(u64)
//! ```
//! Integers are little-endian.

use crate::addr::MAX_ADDR_LEN;
use crate::identity::PeerId;

pub const MAX_DATAGRAM: usize = 1232; // IPv6 minimum MTU 1280 - IPv6 40 - UDP 8
const VERSION: u8 = 1;

pub(crate) const HS1: u8 = 1;
pub(crate) const HS2: u8 = 2;
pub(crate) const DATA: u8 = 3;

pub(crate) const DATA_HEADER: usize = 1 + 4 + 8;
pub(crate) const TAG: usize = 16;
pub(crate) const MAX_PLAINTEXT: usize = MAX_DATAGRAM - DATA_HEADER - TAG;
/// Largest message accepted on the unreliable lane. Small enough that a whole
/// packet still fits inside a relay frame.
pub const MAX_UNRELIABLE: usize = 1100;
/// Reliable messages are split into fragments of at most this size.
pub(crate) const FRAGMENT: usize = 1100;
pub(crate) const FWD_OVERHEAD: usize = 1 + 32 + 2;
pub(crate) const RELAYED_OVERHEAD: usize = 1 + 1 + MAX_ADDR_LEN + 2;
/// Largest packet that can be relayed.
pub(crate) const MAX_RELAYED_PACKET: usize = MAX_PLAINTEXT - FWD_OVERHEAD;
/// Plaintext budget per packet on a relayed link.
pub(crate) const RELAYED_PLAINTEXT: usize = MAX_RELAYED_PACKET - DATA_HEADER - TAG;

// A full relayed-link packet fits in a FWD frame, and RELAYED is no larger than FWD.
const _: () = assert!(RELAYED_OVERHEAD <= FWD_OVERHEAD);
const _: () = assert!(REL_OVERHEAD + FRAGMENT <= RELAYED_PLAINTEXT);
const _: () = assert!(UNREL_OVERHEAD + MAX_UNRELIABLE <= RELAYED_PLAINTEXT);

const F_PING: u8 = 1;
const F_ACK: u8 = 2;
const F_REL: u8 = 3;
const F_UNREL: u8 = 4;
const F_CLOSE: u8 = 5;
const F_OBSERVED: u8 = 6;
const F_FWD: u8 = 7;
const F_RELAYED: u8 = 8;
const F_PUNCH: u8 = 9;
const F_PUNCH_ACK: u8 = 10;
const F_FWD_QUIET: u8 = 11;

pub(crate) const ACK_LEN: usize = 13;
pub(crate) const REL_OVERHEAD: usize = 8;
pub(crate) const UNREL_OVERHEAD: usize = 3;

pub(crate) enum Packet<'a> {
    Hs1 { sender: u32, noise: &'a [u8] },
    Hs2 { sender: u32, receiver: u32, noise: &'a [u8] },
    Data { receiver: u32, nonce: u64, ciphertext: &'a [u8] },
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn u64le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

pub(crate) fn parse(b: &[u8]) -> Option<Packet<'_>> {
    if b.is_empty() || b.len() > MAX_DATAGRAM || b[0] >> 4 != VERSION {
        return None;
    }
    match b[0] & 0x0f {
        HS1 if b.len() > 5 => Some(Packet::Hs1 { sender: u32le(&b[1..]), noise: &b[5..] }),
        HS2 if b.len() > 9 => Some(Packet::Hs2 {
            sender: u32le(&b[1..]),
            receiver: u32le(&b[5..]),
            noise: &b[9..],
        }),
        DATA if b.len() >= DATA_HEADER + TAG => Some(Packet::Data {
            receiver: u32le(&b[1..]),
            nonce: u64le(&b[5..]),
            ciphertext: &b[DATA_HEADER..],
        }),
        _ => None,
    }
}

fn kind(t: u8) -> u8 {
    VERSION << 4 | t
}

pub(crate) fn encode_hs1(sender: u32, noise: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + noise.len());
    v.push(kind(HS1));
    v.extend_from_slice(&sender.to_le_bytes());
    v.extend_from_slice(noise);
    v
}

pub(crate) fn encode_hs2(sender: u32, receiver: u32, noise: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + noise.len());
    v.push(kind(HS2));
    v.extend_from_slice(&sender.to_le_bytes());
    v.extend_from_slice(&receiver.to_le_bytes());
    v.extend_from_slice(noise);
    v
}

pub(crate) fn encode_data_header(receiver: u32, nonce: u64, out: &mut Vec<u8>) {
    out.push(kind(DATA));
    out.extend_from_slice(&receiver.to_le_bytes());
    out.extend_from_slice(&nonce.to_le_bytes());
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame<'a> {
    Ping,
    Ack { cum: u32, bits: u64 },
    Reliable { seq: u32, fin: bool, data: &'a [u8] },
    Unreliable(&'a [u8]),
    Close,
    Observed(&'a [u8]),
    /// `tag`: the relay may tell `to` the address it sees the sender at.
    Fwd { to: PeerId, packet: &'a [u8], tag: bool },
    Relayed { addr: &'a [u8], packet: &'a [u8] },
    Punch(u64),
    PunchAck(u64),
}

/// Iterates over the frames in a decrypted payload. Yields `Err(())` once and
/// stops if the payload is malformed.
pub(crate) struct Frames<'a> {
    b: &'a [u8],
    failed: bool,
}

pub(crate) fn frames(b: &[u8]) -> Frames<'_> {
    Frames { b, failed: false }
}

impl<'a> Iterator for Frames<'a> {
    type Item = Result<Frame<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.b.is_empty() {
            return None;
        }
        let b = self.b;
        let (frame, used) = match b[0] {
            F_PING => (Frame::Ping, 1),
            F_CLOSE => (Frame::Close, 1),
            F_ACK if b.len() >= ACK_LEN => {
                (Frame::Ack { cum: u32le(&b[1..]), bits: u64le(&b[5..]) }, ACK_LEN)
            }
            F_REL if b.len() >= REL_OVERHEAD => {
                let len = u16::from_le_bytes([b[6], b[7]]) as usize;
                if b.len() < REL_OVERHEAD + len || b[5] & !1 != 0 {
                    self.failed = true;
                    return Some(Err(()));
                }
                let data = &b[REL_OVERHEAD..REL_OVERHEAD + len];
                (Frame::Reliable { seq: u32le(&b[1..]), fin: b[5] & 1 == 1, data }, REL_OVERHEAD + len)
            }
            F_UNREL if b.len() >= UNREL_OVERHEAD => {
                let len = u16::from_le_bytes([b[1], b[2]]) as usize;
                if b.len() < UNREL_OVERHEAD + len {
                    self.failed = true;
                    return Some(Err(()));
                }
                (Frame::Unreliable(&b[UNREL_OVERHEAD..UNREL_OVERHEAD + len]), UNREL_OVERHEAD + len)
            }
            F_OBSERVED if b.len() >= 2 && b.len() >= 2 + b[1] as usize && b[1] as usize <= MAX_ADDR_LEN => {
                let n = b[1] as usize;
                (Frame::Observed(&b[2..2 + n]), 2 + n)
            }
            F_FWD | F_FWD_QUIET if b.len() >= FWD_OVERHEAD => {
                let len = u16::from_le_bytes([b[33], b[34]]) as usize;
                if b.len() < FWD_OVERHEAD + len {
                    self.failed = true;
                    return Some(Err(()));
                }
                let mut to = [0u8; 32];
                to.copy_from_slice(&b[1..33]);
                let packet = &b[FWD_OVERHEAD..FWD_OVERHEAD + len];
                (Frame::Fwd { to: PeerId(to), packet, tag: b[0] == F_FWD }, FWD_OVERHEAD + len)
            }
            F_RELAYED if b.len() >= 4 && (b[1] as usize) <= MAX_ADDR_LEN && b.len() >= 4 + b[1] as usize => {
                let n = b[1] as usize;
                let len = u16::from_le_bytes([b[2 + n], b[3 + n]]) as usize;
                if b.len() < 4 + n + len {
                    self.failed = true;
                    return Some(Err(()));
                }
                (Frame::Relayed { addr: &b[2..2 + n], packet: &b[4 + n..4 + n + len] }, 4 + n + len)
            }
            F_PUNCH if b.len() >= 9 => (Frame::Punch(u64le(&b[1..])), 9),
            F_PUNCH_ACK if b.len() >= 9 => (Frame::PunchAck(u64le(&b[1..])), 9),
            _ => {
                self.failed = true;
                return Some(Err(()));
            }
        };
        self.b = &b[used..];
        Some(Ok(frame))
    }
}

pub(crate) fn put_ping(out: &mut Vec<u8>) {
    out.push(F_PING);
}

pub(crate) fn put_close(out: &mut Vec<u8>) {
    out.push(F_CLOSE);
}

pub(crate) fn put_ack(out: &mut Vec<u8>, cum: u32, bits: u64) {
    out.push(F_ACK);
    out.extend_from_slice(&cum.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
}

pub(crate) fn put_reliable(out: &mut Vec<u8>, seq: u32, fin: bool, data: &[u8]) {
    out.push(F_REL);
    out.extend_from_slice(&seq.to_le_bytes());
    out.push(fin as u8);
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(data);
}

pub(crate) fn put_unreliable(out: &mut Vec<u8>, data: &[u8]) {
    out.push(F_UNREL);
    out.extend_from_slice(&(data.len() as u16).to_le_bytes());
    out.extend_from_slice(data);
}

pub(crate) fn put_observed(out: &mut Vec<u8>, addr: &[u8]) {
    out.push(F_OBSERVED);
    out.push(addr.len() as u8);
    out.extend_from_slice(addr);
}

pub(crate) fn put_fwd(out: &mut Vec<u8>, to: &PeerId, packet: &[u8], tag: bool) {
    out.push(if tag { F_FWD } else { F_FWD_QUIET });
    out.extend_from_slice(to.as_bytes());
    out.extend_from_slice(&(packet.len() as u16).to_le_bytes());
    out.extend_from_slice(packet);
}

pub(crate) fn put_relayed(out: &mut Vec<u8>, addr: &[u8], packet: &[u8]) {
    out.push(F_RELAYED);
    out.push(addr.len() as u8);
    out.extend_from_slice(addr);
    out.extend_from_slice(&(packet.len() as u16).to_le_bytes());
    out.extend_from_slice(packet);
}

pub(crate) fn put_punch(out: &mut Vec<u8>, ack: bool, token: u64) {
    out.push(if ack { F_PUNCH_ACK } else { F_PUNCH });
    out.extend_from_slice(&token.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let mut b = Vec::new();
        put_ack(&mut b, 7, 0b101);
        put_ping(&mut b);
        put_reliable(&mut b, 9, true, b"abc");
        put_unreliable(&mut b, b"xy");
        put_close(&mut b);
        put_observed(&mut b, b"addr");
        put_fwd(&mut b, &PeerId([7; 32]), b"pkt", true);
        put_fwd(&mut b, &PeerId([8; 32]), b"q", false);
        put_relayed(&mut b, b"", b"p2");
        put_relayed(&mut b, b"a", b"");
        put_punch(&mut b, false, 11);
        put_punch(&mut b, true, 12);
        let got: Vec<_> = frames(&b).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            got,
            vec![
                Frame::Ack { cum: 7, bits: 5 },
                Frame::Ping,
                Frame::Reliable { seq: 9, fin: true, data: b"abc" },
                Frame::Unreliable(b"xy"),
                Frame::Close,
                Frame::Observed(b"addr"),
                Frame::Fwd { to: PeerId([7; 32]), packet: b"pkt", tag: true },
                Frame::Fwd { to: PeerId([8; 32]), packet: b"q", tag: false },
                Frame::Relayed { addr: b"", packet: b"p2" },
                Frame::Relayed { addr: b"a", packet: b"" },
                Frame::Punch(11),
                Frame::PunchAck(12),
            ]
        );
    }

    #[test]
    fn truncated_frames_fail_once() {
        let mut cases: Vec<Vec<u8>> = vec![Vec::new(); 6];
        put_reliable(&mut cases[0], 1, false, b"hello");
        put_observed(&mut cases[1], b"addr");
        put_fwd(&mut cases[2], &PeerId([1; 32]), b"pkt", true);
        put_relayed(&mut cases[3], b"ab", b"pkt");
        put_punch(&mut cases[4], true, 3);
        put_fwd(&mut cases[5], &PeerId([1; 32]), b"pkt", false);
        for b in cases {
            for cut in 1..b.len() {
                let r: Vec<_> = frames(&b[..cut]).collect();
                assert_eq!(r.len(), 1);
                assert!(r[0].is_err());
            }
        }
    }

    #[test]
    fn frame_parser_survives_garbage() {
        // xorshift: no rand dependency needed here
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..200_000 {
            let len = (next() % 64) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if let Some(f) = b.first_mut() {
                *f = 1 + *f % 11; // mostly valid frame types
            }
            for f in frames(&b) {
                if f.is_err() {
                    break;
                }
            }
        }
    }


    #[test]
    fn packet_parse_bounds() {
        assert!(parse(&[]).is_none());
        assert!(parse(&[kind(HS1), 0, 0, 0, 0]).is_none());
        assert!(parse(&[kind(DATA); DATA_HEADER + TAG - 1]).is_none());
        assert!(parse(&[0x20 | HS1, 0, 0, 0, 0, 1]).is_none()); // wrong version
        assert!(parse(&vec![kind(DATA); MAX_DATAGRAM + 1]).is_none());
    }
}
