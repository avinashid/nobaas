//! Fuzz tests for every parser in the core, on the stable toolchain (no
//! cargo-fuzz): seeded random and structure-aware inputs, mutated, with
//! round-trip and bounds properties checked on whatever a parser accepts.
//! The harness (seeds, scale, mutations) is `peerwire_sim::fuzz`:
//!
//! ```text
//! PW_FUZZ_SCALE=50 PW_FUZZ_SEED=7 cargo test -p peerwire-core --release fuzz -- --nocapture
//! ```

use crate::addr::{Address, MAX_ADDR_LEN};
use crate::endpoint::{Config, DisconnectReason, Endpoint, Event, Lane, SendError};
use crate::identity::{Identity, PeerId};
use crate::invite::Invite;
use crate::reliable::{MAX_URGENT, RECV_WINDOW, URGENT_RECV_WINDOW};
use crate::time::Instant;
use crate::wire::{self, Frame, Packet, FRAGMENT, MAX_DATAGRAM, MAX_PLAINTEXT, MAX_RELAYED_PACKET};
use crate::{sealed, stun};
use peerwire_sim::fuzz::{cases, hex, mutate, run, seed, Rng};
use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

fn identities(n: u8) -> Vec<Identity> {
    (0..n).map(|i| Identity::from_secret_bytes(&[i.wrapping_add(0x41); 32])).collect()
}

fn random_socket_addr(rng: &mut Rng) -> SocketAddr {
    let ip = if rng.one_in(2) {
        IpAddr::V4(Ipv4Addr::from(rng.u32()))
    } else {
        IpAddr::V6(Ipv6Addr::from(u128::from(rng.u64()) << 64 | u128::from(rng.u64())))
    };
    SocketAddr::new(ip, rng.u32() as u16)
}

fn put_frame(out: &mut Vec<u8>, f: &Frame<'_>) {
    match *f {
        Frame::Ping => wire::put_ping(out),
        Frame::Close => wire::put_close(out),
        Frame::Ack { cum, bits, urgent } => wire::put_ack(out, cum, bits, urgent),
        Frame::Reliable { seq, fin, data, urgent } => wire::put_reliable(out, seq, fin, data, urgent),
        Frame::Unreliable(d) => wire::put_unreliable(out, d),
        Frame::Observed(a) => wire::put_observed(out, a),
        Frame::Fwd { to, packet, tag } => wire::put_fwd(out, &to, packet, tag),
        Frame::Relayed { addr, packet } => wire::put_relayed(out, addr, packet),
        Frame::Punch(t) => wire::put_punch(out, false, t),
        Frame::PunchAck(t) => wire::put_punch(out, true, t),
    }
}

/// One to six well-formed frames with random contents (lengths up to `max_data`).
fn random_frames(rng: &mut Rng, max_data: usize, peers: &[PeerId], packets: &[Vec<u8>]) -> Vec<u8> {
    let mut b = Vec::new();
    for _ in 0..1 + rng.below(6) {
        let data = |rng: &mut Rng| {
            let n = rng.below(max_data + 1);
            rng.bytes(n)
        };
        let packet = |rng: &mut Rng| match packets.is_empty() || rng.one_in(3) {
            true => data(rng),
            false => {
                let mut p = rng.pick(packets).clone();
                if rng.one_in(2) {
                    mutate(rng, &mut p, MAX_DATAGRAM);
                }
                p
            }
        };
        let addr = |rng: &mut Rng| match rng.below(3) {
            0 => {
                let mut a = Vec::new();
                random_socket_addr(rng).encode(&mut a);
                a
            }
            1 => Vec::new(),
            _ => {
                let n = rng.below(MAX_ADDR_LEN + 1);
                rng.bytes(n)
            }
        };
        match rng.below(10) {
            0 => wire::put_ping(&mut b),
            1 => wire::put_ack(&mut b, rng.edgy_u32(), rng.u64() >> rng.below(64), rng.one_in(2)),
            2 => {
                let seq = match rng.below(3) {
                    0 => rng.below(70) as u32,
                    1 => 1000 + rng.below(60) as u32,
                    _ => rng.edgy_u32(),
                };
                let d = data(rng);
                wire::put_reliable(&mut b, seq, rng.one_in(2), &d, rng.one_in(2))
            }
            3 => {
                let d = data(rng);
                wire::put_unreliable(&mut b, &d)
            }
            4 => wire::put_close(&mut b),
            5 => {
                let a = addr(rng);
                wire::put_observed(&mut b, &a)
            }
            6 => {
                let to = match peers.is_empty() || rng.one_in(4) {
                    true => PeerId(rng.bytes(32).try_into().unwrap()),
                    false => *rng.pick(peers),
                };
                let p = packet(rng);
                wire::put_fwd(&mut b, &to, &p, rng.one_in(2))
            }
            7 => {
                let (a, p) = (addr(rng), packet(rng));
                wire::put_relayed(&mut b, &a, &p)
            }
            _ => wire::put_punch(&mut b, rng.one_in(2), rng.u64()),
        }
    }
    b
}

#[test]
fn fuzz_packet_parse() {
    run(
        "packet",
        cases(50_000),
        |rng| {
            let mut b = match rng.below(4) {
                0 => {
                    let n = rng.below(MAX_DATAGRAM + 40);
                    rng.bytes(n)
                }
                1 => {
                    let n = rng.below(120);
                    wire::encode_hs1(rng.u32(), &rng.bytes(n))
                }
                2 => {
                    let n = rng.below(120);
                    wire::encode_hs2(rng.u32(), rng.u32(), &rng.bytes(n))
                }
                _ => {
                    let mut b = Vec::new();
                    wire::encode_data_header(rng.u32(), rng.u64(), &mut b);
                    let n = rng.below(MAX_DATAGRAM);
                    b.extend(rng.bytes(n));
                    b
                }
            };
            if rng.below(3) != 0 {
                mutate(rng, &mut b, MAX_DATAGRAM + 40);
            }
            b
        },
        |b| {
            // Whatever is accepted is a well-formed packet that encodes back to the same bytes.
            let again = match wire::parse(b) {
                None => {
                    // Rejected only if empty, oversized, of another version or type, or short.
                    let ok = !b.is_empty()
                        && b.len() <= MAX_DATAGRAM
                        && b[0] >> 4 == 1
                        && match b[0] & 0x0f {
                            1 => b.len() > 5,
                            2 => b.len() > 9,
                            3 => b.len() >= wire::DATA_HEADER + wire::TAG,
                            _ => false,
                        };
                    return assert!(!ok, "a well-formed packet was rejected");
                }
                Some(Packet::Hs1 { sender, noise }) => wire::encode_hs1(sender, noise),
                Some(Packet::Hs2 { sender, receiver, noise }) => wire::encode_hs2(sender, receiver, noise),
                Some(Packet::Data { receiver, nonce, ciphertext }) => {
                    assert!(ciphertext.len() >= wire::TAG);
                    let mut v = Vec::new();
                    wire::encode_data_header(receiver, nonce, &mut v);
                    v.extend_from_slice(ciphertext);
                    v
                }
            };
            assert_eq!(again, b);
        },
    );
}

#[test]
fn fuzz_frames() {
    let peers: Vec<PeerId> = identities(3).iter().map(Identity::peer_id).collect();
    run(
        "frames",
        cases(50_000),
        |rng| {
            let mut b = random_frames(rng, 80, &peers, &[]);
            if rng.below(4) != 0 {
                mutate(rng, &mut b, MAX_PLAINTEXT);
            }
            b
        },
        |b| {
            // The frames yielded, re-encoded, are exactly the bytes consumed; a
            // malformed tail yields one Err and ends the iteration.
            let mut again = Vec::new();
            let mut failed = false;
            for f in wire::frames(b) {
                assert!(!failed, "a frame after Err");
                match f {
                    Ok(f) => {
                        match &f {
                            Frame::Observed(a) => assert!(a.len() <= MAX_ADDR_LEN),
                            Frame::Relayed { addr, packet } => {
                                assert!(addr.len() <= MAX_ADDR_LEN);
                                let _ = SocketAddr::decode(addr);
                                let _ = wire::parse(packet);
                            }
                            Frame::Fwd { packet, .. } => {
                                let _ = wire::parse(packet);
                            }
                            _ => {}
                        }
                        put_frame(&mut again, &f);
                    }
                    Err(()) => failed = true,
                }
            }
            assert_eq!(again, b[..again.len()]);
            assert!(failed || again.len() == b.len(), "frames stopped early without an error");
        },
    );
}

#[test]
fn fuzz_addresses() {
    let mut seen = [0usize; 3];
    run(
        "addr",
        cases(30_000),
        |rng| match rng.below(3) {
            0 => {
                let n = rng.below(MAX_ADDR_LEN + 4);
                let mut b = rng.bytes(n);
                if let Some(t) = b.first_mut() {
                    *t = *rng.pick(&[4, 6, *t]);
                }
                b
            }
            _ => {
                let mut b = Vec::new();
                random_socket_addr(rng).encode(&mut b);
                if rng.one_in(2) {
                    mutate(rng, &mut b, MAX_ADDR_LEN + 4);
                }
                b
            }
        },
        |b| {
            // Every accepted encoding is canonical: it encodes back to itself.
            fn canonical<A: Address>(b: &[u8]) -> bool {
                let Some(a) = A::decode(b) else { return false };
                let mut v = Vec::new();
                a.encode(&mut v);
                assert_eq!(v, b);
                assert!(v.len() <= MAX_ADDR_LEN);
                true
            }
            seen[0] += canonical::<SocketAddr>(b) as usize;
            seen[1] += canonical::<u32>(b) as usize;
            seen[2] += canonical::<u64>(b) as usize;
        },
    );
    println!("fuzz addr: accepted SocketAddr {} u32 {} u64 {}", seen[0], seen[1], seen[2]);
    assert!(seen.iter().all(|&n| n > 0), "the generator must reach every decoder");
}

#[test]
fn fuzz_stun() {
    let tx: stun::TxId = *b"fuzz-tx-0001";
    let mut found = 0;
    run(
        "stun",
        cases(30_000),
        |rng| {
            let mut b = match rng.below(4) {
                0 => stun::binding_request(&tx),
                1 => {
                    let n = rng.below(80);
                    let mut b = stun::binding_request(&tx);
                    b.extend(rng.bytes(n));
                    b
                }
                _ => stun::binding_response(&tx, random_socket_addr(rng)),
            };
            // Extra attributes before the address, as real servers send.
            if rng.one_in(3) && b.len() >= 20 {
                let n = rng.below(12);
                let mut attr = vec![0x80, 0x22, 0, n as u8];
                attr.extend(rng.bytes(n.next_multiple_of(4)));
                b.splice(20..20, attr.iter().copied());
                let len = (b.len() - 20) as u16;
                b[2..4].copy_from_slice(&len.to_be_bytes());
            }
            if rng.below(4) != 0 {
                mutate(rng, &mut b, 600);
            }
            b
        },
        |b| {
            if stun::is_stun(b) {
                assert!(wire::parse(b).is_none(), "STUN and Peerwire packets must not overlap");
            }
            if let Some(t) = stun::parse_binding_request(b) {
                assert!(stun::is_stun(b) && t[..] == b[8..20]);
            }
            found += stun::parse_binding_response(b, &tx).is_some() as usize;
            let _ = stun::parse_binding_response(b, b"other-tx-002");
        },
    );
    println!("fuzz stun: {found} responses parsed");
    assert!(found > 0);
}

#[test]
fn fuzz_invites() {
    let ids = identities(6);
    let mut accepted = 0usize;
    run(
        "invite",
        cases(8_000),
        |rng| {
            let inv = Invite {
                peer: rng.pick(&ids).peer_id(),
                key: rng.one_in(2).then(|| rng.bytes(32).try_into().unwrap()),
                addrs: (0..rng.below(5)).map(|_| random_socket_addr(rng)).collect(),
                relays: (0..rng.below(4)).map(|_| (rng.pick(&ids).peer_id(), random_socket_addr(rng))).collect(),
            };
            let mut b = if rng.one_in(2) { inv.to_bytes() } else { inv.to_link().into_bytes() };
            if rng.below(5) != 0 {
                mutate(rng, &mut b, 2000);
            }
            b
        },
        |b| {
            // Bytes and links; whatever parses must survive its own round trip, and be the one
            // encoding of what it decodes to (a link up to the whitespace parse trims).
            fn round_trip<A: Address>(inv: Invite<A>) {
                assert!(inv.addrs.len() <= 16 && inv.relays.len() <= 16);
                assert_eq!(Invite::<A>::from_bytes(&inv.to_bytes()).as_ref(), Some(&inv));
                assert_eq!(Invite::<A>::parse(&inv.to_link()).as_ref(), Some(&inv));
            }
            if let Some(inv) = Invite::<SocketAddr>::from_bytes(b) {
                accepted += 1;
                assert_eq!(inv.to_bytes(), b);
                round_trip(inv);
            }
            if let Some(inv) = Invite::<u32>::from_bytes(b) {
                assert_eq!(inv.to_bytes(), b);
                round_trip(inv);
            }
            let s = String::from_utf8_lossy(b);
            if let Some(inv) = Invite::<SocketAddr>::parse(&s) {
                accepted += 1;
                assert_eq!(inv.to_link(), s.trim());
                round_trip(inv);
            }
            if let Some(inv) = Invite::<u64>::parse(&s) {
                assert_eq!(inv.to_link(), s.trim());
                round_trip(inv);
            }
        },
    );
    println!("fuzz invite: {accepted} accepted, all canonical");
    assert!(accepted > 0);
}

#[test]
fn fuzz_sealed() {
    let ids = identities(3);
    let (sender, me) = (&ids[0], &ids[1]);
    let original = RefCell::new(Vec::new());
    let mut opened = 0;
    run(
        "sealed",
        cases(3_000),
        |rng| {
            let n = rng.below(300);
            let mut b = match rng.below(4) {
                0 => rng.bytes(n),
                _ => {
                    let to = if rng.one_in(8) { ids[2].peer_id() } else { me.peer_id() };
                    sealed::seal(sender, &to, rng.bytes(32).try_into().unwrap(), &rng.bytes(n)).unwrap()
                }
            };
            *original.borrow_mut() = b.clone();
            if rng.below(3) != 0 {
                mutate(rng, &mut b, 600);
            }
            b
        },
        |b| {
            // Only an untouched message sealed to us opens, and it names its real sender.
            if let Some((from, pt)) = sealed::open(me, b) {
                assert_eq!(b, &original.borrow()[..], "a tampered message opened");
                assert_eq!(from, sender.peer_id());
                assert_eq!(pt.len() + sealed::SEAL_OVERHEAD, b.len());
                opened += 1;
            }
        },
    );
    println!("fuzz sealed: {opened} opened");
    assert!(opened > 0);
}

/// Endpoints on loopback addresses that deliver each other's datagrams at once.
struct Net {
    nodes: Vec<(SocketAddr, Endpoint<SocketAddr>)>,
    now: Instant,
    /// Real packets seen on the wire, to put inside FWD and RELAYED frames.
    captured: Vec<Vec<u8>>,
    events: Vec<(usize, Event<SocketAddr>)>,
}

impl Net {
    fn new(ids: &[Identity]) -> Self {
        let nodes = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (Net::addr(i), Endpoint::new(id, Config::default(), [i as u8 + 1; 32])))
            .collect();
        Net { nodes, now: Instant::ZERO, captured: Vec::new(), events: Vec::new() }
    }

    fn addr(i: usize) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 4000 + i as u16))
    }

    fn deliver(&mut self) {
        for _ in 0..64 {
            let mut sent = Vec::new();
            for (i, (_, ep)) in self.nodes.iter_mut().enumerate() {
                while let Some(t) = ep.poll_transmit(self.now) {
                    sent.push((i, t));
                }
            }
            if sent.is_empty() {
                break;
            }
            for (i, t) in sent {
                let n = self.captured.len();
                match n < 512 {
                    true => self.captured.push(t.payload.clone()),
                    false => self.captured[t.payload.len() * 31 % n] = t.payload.clone(),
                }
                let from = self.nodes[i].0;
                if let Some((_, ep)) = self.nodes.iter_mut().find(|(a, _)| *a == t.dst) {
                    ep.handle_datagram(self.now, from, &t.payload);
                }
            }
        }
        for (i, (_, ep)) in self.nodes.iter_mut().enumerate() {
            while let Some(e) = ep.poll_event() {
                self.events.push((i, e));
            }
        }
    }

    fn advance(&mut self, d: Duration) {
        self.now = self.now + d;
        for (_, ep) in &mut self.nodes {
            ep.handle_timeout(self.now);
        }
        self.deliver();
    }

    fn settle(&mut self) {
        for _ in 0..20 {
            self.advance(Duration::from_millis(30));
        }
    }

    fn connected(&self, i: usize, j: usize, ids: &[PeerId]) -> bool {
        self.nodes[i].1.is_connected(&ids[j]) && self.nodes[j].1.is_connected(&ids[i])
    }

    /// `data` sent reliably from `i` arrives at `j`.
    fn reaches(&mut self, i: usize, j: usize, ids: &[PeerId], data: &[u8]) -> bool {
        self.events.clear();
        self.nodes[i].1.send(&ids[j], Lane::Reliable, data).unwrap();
        self.settle();
        self.events.iter().any(|(n, e)| *n == j && matches!(e, Event::Message { data: d, .. } if d == data))
    }
}

#[test]
fn fuzz_hostile_peer() {
    // An authenticated peer M sends A sealed packets of random frames: valid ones
    // with edge-case fields, garbage, FWD to A's other peer C, and RELAYED
    // packets that nest further packets sealed on the same link. A must not
    // panic, must drop M on a violation (and let it back in), and must keep
    // serving C.
    let keys = identities(3);
    let ids: Vec<PeerId> = keys.iter().map(Identity::peer_id).collect();
    let (a, m, c) = (0, 1, 2);
    let mut net = Net::new(&keys);
    for i in [m, c] {
        net.nodes[i].1.connect(net.now, Net::addr(a), ids[a]).unwrap();
    }
    net.settle();
    assert!(net.connected(a, m, &ids) && net.connected(a, c, &ids));
    for (i, j) in [(c, a), (a, c), (m, a), (a, m)] {
        assert!(net.reaches(i, j, &ids, &[i as u8; 3000]));
    }
    let mut rng = Rng::for_target("hostile");
    let (mut sent, mut nested, mut reconnects, mut messages) = (0usize, 0usize, 0usize, 0usize);
    for i in 0..cases(6_000) {
        if !net.connected(a, m, &ids) {
            // A dropped M (a violation or a CLOSE); M dials again.
            net.nodes[m].1.disconnect(&ids[a]);
            net.nodes[m].1.connect(net.now, Net::addr(a), ids[a]).unwrap();
            for _ in 0..4 {
                net.advance(Duration::from_millis(5));
            }
            reconnects += 1;
            continue;
        }
        let mut pt = random_frames(&mut rng, 300, &ids, &net.captured);
        if rng.one_in(4) {
            mutate(&mut rng, &mut pt, MAX_PLAINTEXT);
        }
        if rng.one_in(6) {
            for _ in 0..1 + rng.below(12) {
                let Some(inner) = net.nodes[m].1.seal_raw(&ids[a], &pt) else { break };
                if inner.len() > MAX_RELAYED_PACKET {
                    break;
                }
                pt.clear();
                wire::put_relayed(&mut pt, &[], &inner);
                nested += 1;
            }
        }
        pt.truncate(MAX_PLAINTEXT);
        let pkt = net.nodes[m].1.seal_raw(&ids[a], &pt).unwrap();
        let from = match rng.one_in(12) {
            true => SocketAddr::from(([127, 0, 0, 1], 5000 + rng.below(50) as u16)),
            false => Net::addr(m),
        };
        if let Err(e) = panic::catch_unwind(AssertUnwindSafe(|| net.nodes[a].1.handle_datagram(net.now, from, &pkt))) {
            eprintln!("fuzz hostile: case {i} of seed {:#x} failed on plaintext {}", seed(), hex(&pt));
            panic::resume_unwind(e);
        }
        sent += 1;
        net.deliver();
        for (n, e) in net.events.drain(..) {
            if let (0, Event::Message { peer, data, .. }) = (n, &e) {
                assert!(data.len() <= Config::default().max_message);
                messages += (*peer == ids[m]) as usize;
            }
        }
        let d = match rng.below(40) {
            0 => Duration::from_millis(1000 * (1 + rng.below(12) as u64)),
            _ => Duration::from_millis(rng.below(60) as u64),
        };
        net.advance(d);
    }
    let st = net.nodes[a].1.stats();
    println!(
        "fuzz hostile: {sent} packets ({nested} nested), {reconnects} reconnects, {messages} messages, {} sessions, {st:?}",
        net.nodes[a].1.session_count()
    );
    assert!(st.rx_malformed > 0 && st.relayed > 0 && st.roams > 0 && messages > 0, "the fuzzer must reach those paths");
    assert!(net.nodes[a].1.session_count() <= 64, "half-open sessions are bounded");
    net.settle();
    if !net.connected(a, m, &ids) {
        net.nodes[m].1.disconnect(&ids[a]);
        net.nodes[m].1.connect(net.now, Net::addr(a), ids[a]).unwrap();
        net.settle();
    }
    assert!(net.connected(a, c, &ids), "the bystander's link survived");
    for (i, j) in [(c, a), (a, c), (m, a), (a, m)] {
        assert!(net.reaches(i, j, &ids, b"still serving"), "{i} -> {j} after the fuzz");
    }
}

#[test]
fn hostile_peer_pins_bounded_receive_memory() {
    // An authenticated peer M makes A hold as much as it can on each stream:
    // full fragments in order without a final one, so a message is partly
    // assembled, then the next sequence number held back and everything after
    // it sent, so the out-of-order buffer fills. The urgent stream holds at most
    // URGENT_RECV_WINDOW - 1 fragments and MAX_URGENT bytes of message, and one
    // fragment past that cap is a violation.
    let keys = identities(2);
    let ids: Vec<PeerId> = keys.iter().map(Identity::peer_id).collect();
    let (a, m) = (0, 1);
    let mut net = Net::new(&keys);
    net.nodes[m].1.connect(net.now, Net::addr(a), ids[a]).unwrap();
    net.settle();
    assert!(net.connected(a, m, &ids));
    let send_len = |net: &mut Net, urgent: bool, seq: u32, len: usize| {
        let mut pt = Vec::new();
        wire::put_reliable(&mut pt, seq, false, &vec![seq as u8; len], urgent);
        let pkt = net.nodes[m].1.seal_raw(&ids[a], &pt).unwrap();
        net.nodes[a].1.handle_datagram(net.now, Net::addr(m), &pkt);
    };
    let send = |net: &mut Net, urgent: bool, seq: u32| send_len(net, urgent, seq, FRAGMENT);
    let mut bound = [0; 2];
    for (u, urgent, window, max) in
        [(0, false, RECV_WINDOW, Config::default().max_message), (1, true, URGENT_RECV_WINDOW, MAX_URGENT)]
    {
        let assembled = (max / FRAGMENT) as u32;
        for seq in (0..assembled).chain(assembled + 1..assembled + 2 * window) {
            send(&mut net, urgent, seq);
        }
        bound[u] = (window as usize - 1) * FRAGMENT + max;
        let held = net.nodes[a].1.rx_held(&ids[m]).unwrap()[u];
        assert_eq!(held, (window as usize - 1 + assembled as usize) * FRAGMENT, "stream {u}");
        assert!(held <= bound[u]);
    }
    println!("receive memory a peer can pin per link: {} B (urgent {} B)", bound[0] + bound[1], bound[1]);
    assert!(bound[1] < 140_000 && bound[0] + bound[1] < 2_320_000);
    // The held-back fragment completes more than MAX_URGENT of message.
    send(&mut net, true, (MAX_URGENT / FRAGMENT) as u32);
    net.deliver();
    let dropped =
        |e: &Event<SocketAddr>| matches!(e, Event::Disconnected { reason: DisconnectReason::ProtocolViolation, .. });
    assert!(net.events.iter().any(|(n, e)| *n == a && dropped(e)));
    assert!(!net.nodes[a].1.is_connected(&ids[m]));

    // An honest sender may send MAX_URGENT bytes, and not one more.
    net.nodes[m].1.disconnect(&ids[a]);
    net.nodes[m].1.connect(net.now, Net::addr(a), ids[a]).unwrap();
    net.settle();
    assert_eq!(net.nodes[m].1.send(&ids[a], Lane::Urgent, &vec![1; MAX_URGENT + 1]), Err(SendError::TooLarge));
    net.events.clear();
    net.nodes[m].1.send(&ids[a], Lane::Urgent, &vec![2; MAX_URGENT]).unwrap();
    net.settle();
    let got = |e: &Event<SocketAddr>| matches!(e, Event::Message { data, .. } if data.len() == MAX_URGENT);
    assert!(net.events.iter().any(|(n, e)| *n == a && got(e)));

    // A fragment longer than FRAGMENT, which the parser would accept, is a
    // violation on either stream, so the bound above holds.
    for urgent in [false, true] {
        net.nodes[m].1.disconnect(&ids[a]);
        net.nodes[m].1.connect(net.now, Net::addr(a), ids[a]).unwrap();
        net.settle();
        assert!(net.connected(a, m, &ids));
        net.events.clear();
        send_len(&mut net, urgent, 1, FRAGMENT + 1);
        net.deliver();
        assert!(net.events.iter().any(|(n, e)| *n == a && dropped(e)), "urgent {urgent}");
    }
}
