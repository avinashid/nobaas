//! STUN and signaling on real loopback sockets: local mock STUN servers and an
//! in-process signaling hub, no public network.
use peerwire::{MemoryHub, NatType, Node, PeerId, Role, Status};
use peerwire_core::stun;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// A STUN server on loopback that tells every client it was seen at `mapped`.
fn mock_stun(mapped: SocketAddr) -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
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

fn pump(nodes: &mut [&mut Node], secs: u64, mut done: impl FnMut(&[&mut Node]) -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        for n in nodes.iter_mut() {
            n.poll();
            while n.try_recv_event().is_some() {}
        }
        if done(nodes) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

#[test]
fn stun_public_address_goes_into_the_invite() {
    let public: SocketAddr = "203.0.113.7:4242".parse().unwrap();
    let servers = [mock_stun(public), mock_stun(public)].map(|a| a.to_string());
    let mut n = Node::builder().stun_servers(servers).bind("0.0.0.0:0").unwrap();
    assert!(pump(&mut [&mut n], 5, |n| n[0].nat_type() != NatType::Unknown));
    assert_eq!(n.public_addr(), Some(public));
    assert_eq!(n.nat_type(), NatType::Cone, "both servers saw the same mapping");
    let inv: peerwire_core::Invite<SocketAddr> = peerwire_core::Invite::parse(&n.invite()).unwrap();
    assert_eq!(inv.addrs[0], public, "public address first");
    assert!(inv.addrs.contains(&SocketAddr::from(([127, 0, 0, 1], n.local_addr().port()))));

    // Two servers seeing two ports: a symmetric NAT.
    let other: SocketAddr = "203.0.113.7:4243".parse().unwrap();
    let servers = [mock_stun(public), mock_stun(other)].map(|a| a.to_string());
    let mut s = Node::builder().stun_servers(servers).bind("0.0.0.0:0").unwrap();
    assert!(pump(&mut [&mut s], 5, |n| n[0].nat_type() != NatType::Unknown));
    assert_eq!(s.nat_type(), NatType::Symmetric);
}

fn room_node(hub: &MemoryHub) -> Node {
    Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap()
}

fn linked(a: &Node, b: PeerId) -> bool {
    a.status(&b) == Status::Connected
}

#[test]
fn room_peers_find_each_other_through_signaling() {
    let hub = MemoryHub::new();
    let (mut a, mut b, mut c) = (room_node(&hub), room_node(&hub), room_node(&hub));
    let ids = [a.peer_id(), b.peer_id(), c.peer_id()];
    for n in [&mut a, &mut b] {
        n.join_room("pw-test", "lobby");
    }
    assert!(pump(&mut [&mut a, &mut b], 5, |n| linked(n[0], ids[1]) && linked(n[1], ids[0])));
    // A late joiner is found by both, and nobody outside the room is dialled.
    let mut d = room_node(&hub);
    d.join_room("pw-test", "other room");
    c.join_room("pw-test", "lobby");
    assert!(pump(&mut [&mut a, &mut b, &mut c, &mut d], 5, |n| {
        linked(n[0], ids[2]) && linked(n[1], ids[2]) && linked(n[2], ids[0]) && linked(n[2], ids[1])
    }));
    assert_eq!(d.peers().count(), 0);
}

#[test]
fn star_room_links_clients_to_the_host_only() {
    let hub = MemoryHub::new();
    let (mut h, mut c1, mut c2) = (room_node(&hub), room_node(&hub), room_node(&hub));
    h.join_room_as("pw-test", "star", Role::Host);
    c1.join_room_as("pw-test", "star", Role::Client);
    c2.join_room_as("pw-test", "star", Role::Client);
    let hid = h.peer_id();
    assert!(pump(&mut [&mut h, &mut c1, &mut c2], 5, |n| linked(n[1], hid) && linked(n[2], hid)));
    // Give stray client-to-client offers time to show up.
    pump(&mut [&mut h, &mut c1, &mut c2], 1, |_| false);
    assert_eq!((c1.peers().count(), c2.peers().count()), (1, 1));
    assert!(linked(&c1, h.peer_id()) && linked(&c2, h.peer_id()));
}

#[test]
fn invite_join_dials_back_over_signaling() {
    // The invite's only address is dead (as a host's LAN address is from outside),
    // so the link can only come up because the host dials the joiner's addresses.
    let hub = MemoryHub::new();
    let (mut host, mut guest) = (room_node(&hub), room_node(&hub));
    let dead = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap(); // closed when dropped
    let real: peerwire_core::Invite<std::net::SocketAddr> = peerwire_core::Invite::parse(&host.invite()).unwrap();
    let key = real.key;
    assert!(key.is_some());
    let hid = host.peer_id();
    let invite_with = |key| peerwire_core::Invite { peer: hid, key, addrs: vec![dead], relays: vec![] }.to_link();

    // NB-032: knowing the host's PeerId (and an address) is not enough to make it
    // dial back: without the invite's key the offer is ignored.
    let mut stranger = room_node(&hub);
    let h = stranger.connect(&invite_with(None)).unwrap();
    pump(&mut [&mut host, &mut stranger], 2, |_| false);
    assert_eq!((stranger.status(&h), host.peers().count()), (Status::Connecting, 0));

    let invite = invite_with(key);
    let h = guest.connect(&invite).unwrap();
    let g = guest.peer_id();
    assert!(pump(&mut [&mut host, &mut guest], 5, |n| linked(n[0], g) && linked(n[1], h)));
    assert_eq!(guest.path(&h), Some(&peerwire::Path::Direct(host.local_addr())));

    // Without signaling the same invite gets nowhere.
    let mut alone = Node::bind("127.0.0.1:0").unwrap();
    alone.connect(&invite).unwrap();
    pump(&mut [&mut alone, &mut host], 2, |_| false);
    assert_eq!(alone.status(&h), Status::Connecting, "still dialling a dead address");
}
