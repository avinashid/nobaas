//! NB-042: a node bound to `[::]` links with IPv4 and IPv6 peers on one socket,
//! over real loopback UDP (127.0.0.1 and ::1).
use peerwire::{ConnectError, Error, Event, Invite, Lane, NatType, Node, Path, PeerId};
use peerwire_core::stun;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

fn quiet(addr: &str) -> Node {
    Node::builder().stun_servers(Vec::<String>::new()).bind(addr).unwrap()
}

/// Hosts without IPv6 (some CI containers) skip these tests.
fn has_ipv6() -> bool {
    UdpSocket::bind("[::1]:0").is_ok()
}

/// Polls every node until `done` holds (or 5 s pass), collecting their events.
fn pump(nodes: &mut [&mut Node], ev: &mut Vec<Vec<Event>>, mut done: impl FnMut(&[Vec<Event>]) -> bool) -> bool {
    ev.resize(nodes.len(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        for (n, e) in nodes.iter_mut().zip(ev.iter_mut()) {
            n.poll();
            e.extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        if done(ev) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

fn connected(ev: &[Event], peer: PeerId) -> bool {
    ev.iter().any(|e| matches!(e, Event::Connected { peer: p, .. } if *p == peer))
}

fn got(ev: &[Event], from: PeerId, data: &[u8]) -> bool {
    ev.iter().any(|e| matches!(e, Event::Message { peer, data: d, .. } if *peer == from && d == data))
}

fn at(addr: &str) -> SocketAddr {
    addr.parse().unwrap()
}

#[test]
fn one_dual_stack_socket_links_ipv4_and_ipv6_peers() {
    if !has_ipv6() {
        return eprintln!("skipped: no IPv6 on this host");
    }
    let mut dual = quiet("[::]:0");
    let port = dual.local_addr().port();
    assert!(dual.local_addr().is_ipv6());
    let addrs = dual.addresses();
    assert!(addrs.contains(&at(&format!("127.0.0.1:{port}"))), "{addrs:?}");
    assert!(addrs.contains(&at(&format!("[::1]:{port}"))), "{addrs:?}");

    let (mut v4, mut v6) = (quiet("127.0.0.1:0"), quiet("[::1]:0"));
    let (d, i4, i6) = (dual.peer_id(), v4.peer_id(), v6.peer_id());
    v4.connect(&dual.invite()).unwrap();
    v6.connect(&dual.invite()).unwrap();
    let mut ev = Vec::new();
    let all_up = |ev: &[Vec<Event>]| {
        connected(&ev[0], i4) && connected(&ev[0], i6) && connected(&ev[1], d) && connected(&ev[2], d)
    };
    assert!(pump(&mut [&mut dual, &mut v4, &mut v6], &mut ev, all_up));

    // Each side sees the other at its plain address: IPv4 peers are not ::ffff:a.b.c.d.
    assert_eq!(v4.path(&d), Some(&Path::Direct(at(&format!("127.0.0.1:{port}")))));
    assert_eq!(v6.path(&d), Some(&Path::Direct(at(&format!("[::1]:{port}")))));
    assert_eq!(dual.path(&i4), Some(&Path::Direct(v4.local_addr())));
    assert_eq!(dual.path(&i6), Some(&Path::Direct(v6.local_addr())));

    dual.send(&i4, Lane::Reliable, b"to v4").unwrap();
    dual.send(&i6, Lane::Reliable, b"to v6").unwrap();
    v4.send(&d, Lane::Unreliable, b"from v4").unwrap();
    v6.send(&d, Lane::Unreliable, b"from v6").unwrap();
    let delivered = |ev: &[Vec<Event>]| {
        got(&ev[1], d, b"to v4")
            && got(&ev[2], d, b"to v6")
            && got(&ev[0], i4, b"from v4")
            && got(&ev[0], i6, b"from v6")
    };
    assert!(pump(&mut [&mut dual, &mut v4, &mut v6], &mut ev, delivered));
}

#[test]
fn dials_skip_a_family_the_socket_cannot_reach() {
    if !has_ipv6() {
        return eprintln!("skipped: no IPv6 on this host");
    }
    let target = quiet("[::]:0");
    let port = target.local_addr().port();
    let only =
        |addr: &str| Invite { peer: target.peer_id(), key: None, addrs: vec![at(addr)], relays: vec![] }.to_link();
    let (v4, v6) = (only(&format!("[::1]:{port}")), only(&format!("127.0.0.1:{port}")));
    let no_path = Err(Error::Connect(ConnectError::NoPath));
    assert_eq!(quiet("127.0.0.1:0").connect(&v4), no_path, "an IPv4 socket can't send to ::1");
    assert_eq!(quiet("0.0.0.0:0").connect(&v4), no_path);
    assert_eq!(quiet("[::1]:0").connect(&v6), no_path, "nor one bound to ::1 to 127.0.0.1");
    // A dual-stack socket reaches both.
    assert_eq!(quiet("[::]:0").connect(&v4), Ok(target.peer_id()));
    assert_eq!(quiet("[::]:0").connect(&v6), Ok(target.peer_id()));
}

#[test]
fn two_dual_stack_nodes_link_on_whichever_family_answers_first() {
    if !has_ipv6() {
        return eprintln!("skipped: no IPv6 on this host");
    }
    let (mut a, mut b) = (quiet("[::]:0"), quiet("[::]:0"));
    let (ia, ib) = (a.peer_id(), b.peer_id());
    b.connect(&a.invite()).unwrap();
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, |ev| connected(&ev[0], ib) && connected(&ev[1], ia)));
    let offered = a.addresses();
    assert!(matches!(b.path(&ia), Some(Path::Direct(p)) if offered.contains(p)), "{:?}", b.path(&ia));
    a.send(&ib, Lane::Reliable, b"hi").unwrap();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, |ev| got(&ev[1], ia, b"hi")));
}

/// A STUN server on `bind` that tells every client it was seen at `mapped`.
fn mock_stun(bind: &str, mapped: SocketAddr) -> SocketAddr {
    let sock = UdpSocket::bind(bind).unwrap();
    let addr = sock.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if let Some(tx) = stun::parse_binding_request(&buf[..n]) {
                let _ = sock.send_to(&stun::binding_response(&tx, mapped), from);
            }
        }
    });
    addr
}

/// STUN goes over IPv4 only: an IPv6 server's answer, another address, would
/// read as a symmetric NAT.
#[test]
fn a_dual_stack_node_asks_stun_over_ipv4_only() {
    if !has_ipv6() {
        return eprintln!("skipped: no IPv6 on this host");
    }
    let public = at("203.0.113.7:4242");
    let servers = [mock_stun("[::1]:0", at("[2001:db8::7]:4242")), mock_stun("127.0.0.1:0", public)];
    let mut n = Node::builder().stun_servers(servers.map(|a| a.to_string())).bind("[::]:0").unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while n.public_addr().is_none() && Instant::now() < end {
        n.poll();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(n.public_addr(), Some(public), "the IPv4 server answered over the dual-stack socket");
    for _ in 0..50 {
        n.poll(); // time for an IPv6 answer, had it asked
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(n.nat_type(), NatType::Unknown, "one server: no NAT type, and no IPv6 answer mixed in");
    assert_eq!(n.addresses()[0], public);
}
