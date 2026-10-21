//! NB-034: privacy modes on real loopback sockets, with an in-process signaling
//! hub. What a node gives out is checked where it leaves: in its invites, and in
//! the sealed offers it publishes (opened here with the recipient's key).
use peerwire::{ConnectError, Error, Event, MemoryHub, Node, Path, PeerId, Privacy, Role, Signaling, Topic};
use peerwire_core::{sealed, stun, Identity, Invite};
use rand_core::OsRng;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
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

/// Polls the nodes until `done` or `secs` pass, keeping every event per node.
fn pump(
    nodes: &mut [&mut Node],
    events: &mut [Vec<Event>],
    secs: u64,
    mut done: impl FnMut(&[&mut Node]) -> bool,
) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        for (n, ev) in nodes.iter_mut().zip(events.iter_mut()) {
            n.poll();
            ev.extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        if done(nodes) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

fn addrs(invite: &str) -> Vec<SocketAddr> {
    Invite::<SocketAddr>::parse(invite).unwrap().addrs
}

fn private(a: &SocketAddr) -> bool {
    match a.ip() {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// A sealed offer's addresses and relays (the body layout from `traversal.rs`:
/// version, flags, role, nat, ts u64, nonce u64, then the counted lists).
fn offer_contents(body: &[u8]) -> (usize, Vec<PeerId>) {
    let n = body[20] as usize;
    let mut i = 21;
    for _ in 0..n {
        i += 1 + body[i] as usize;
    }
    let m = body[i] as usize;
    let relays = (0..m).map(|k| PeerId(body[i + 1 + 32 * k..i + 33 + 32 * k].try_into().unwrap())).collect();
    (n, relays)
}

#[test]
fn public_only_invites_hold_no_lan_or_loopback_address() {
    let public: SocketAddr = "203.0.113.7:4242".parse().unwrap();
    let servers = [mock_stun(public), mock_stun(public)].map(|a| a.to_string());
    let mut n = Node::builder().stun_servers(servers).bind("0.0.0.0:0").unwrap();
    assert!(pump(&mut [&mut n], &mut [vec![]], 5, |n| n[0].public_addr().is_some()));
    let lo = SocketAddr::from(([127, 0, 0, 1], n.local_addr().port()));
    assert_eq!(n.privacy(), Privacy::Direct);
    let direct = addrs(&n.invite());
    assert!(direct.contains(&public) && direct.contains(&lo), "{direct:?}");

    // Switched at runtime: the next invite has the public address only.
    n.set_privacy(Privacy::PublicOnly);
    assert_eq!(addrs(&n.invite()), vec![public]);
    assert_eq!(n.addresses(), vec![public]);
    n.set_privacy(Privacy::RelayOnly { via: None });
    let inv = Invite::<SocketAddr>::parse(&n.invite()).unwrap();
    assert_eq!((inv.addrs, inv.relays), (vec![], vec![]), "no address and, with no relay, no relay either");
    n.set_privacy(Privacy::Direct);
    assert!(addrs(&n.invite()).contains(&lo));

    // Without STUN, a public-only node bound to every interface gives out nothing.
    let quiet =
        Node::builder().stun_servers(Vec::<String>::new()).privacy(Privacy::PublicOnly).bind("0.0.0.0:0").unwrap();
    assert_eq!(addrs(&quiet.invite()), vec![]);
    let all = Node::builder().stun_servers(Vec::<String>::new()).bind("0.0.0.0:0").unwrap();
    assert!(addrs(&all.invite()).iter().all(private), "only LAN and loopback to leave out");
    assert!(Invite::<SocketAddr>::parse(&all.invite()).unwrap().key.is_some(), "invites carry the dial-back key");
}

#[test]
fn relay_only_connect_needs_a_connected_relay() {
    let mut r = Node::bind("127.0.0.1:0").unwrap();
    let other = Node::bind("127.0.0.1:0").unwrap();
    let mut a = Node::builder().privacy(Privacy::RelayOnly { via: None }).bind("127.0.0.1:0").unwrap();
    assert_eq!(a.connect(&other.invite()), Err(Error::Connect(ConnectError::NoRelay)));
    a.set_privacy(Privacy::RelayOnly { via: Some(r.peer_id()) });
    assert_eq!(a.connect(&other.invite()), Err(Error::Connect(ConnectError::NoRelay)), "the relay isn't linked yet");
    // The relay itself is dialled directly.
    let rid = a.connect(&r.invite()).unwrap();
    let mut ev = [vec![], vec![]];
    assert!(pump(&mut [&mut a, &mut r], &mut ev, 5, |n| n[0].path(&rid).is_some()));
    assert!(matches!(a.path(&rid), Some(Path::Direct(_))));
    assert!(a.connect(&other.invite()).is_ok(), "through the relay now");
}

/// Two relay-only peers in a room with a host link through it and never learn
/// each other's address: not from offers, the relay or the link.
#[test]
fn relay_only_room_peers_link_through_the_host() {
    let hub = MemoryHub::new();
    let ids: Vec<Identity> = (0..3).map(|_| Identity::generate(&mut OsRng)).collect();
    let node = |i: usize, p: Privacy| {
        Node::builder().identity(ids[i].clone()).privacy(p).signaling(hub.connect()).bind("127.0.0.1:0").unwrap()
    };
    let private = Privacy::RelayOnly { via: None };
    let (mut h, mut a, mut b) = (node(0, Privacy::Direct), node(1, private), node(2, private));
    let (hid, aid, bid) = (h.peer_id(), a.peer_id(), b.peer_id());
    // A tap on A's and B's inboxes, to read what each is sent.
    let mut tap = hub.connect();
    tap.subscribe(Topic::peer(&aid));
    tap.subscribe(Topic::peer(&bid));
    h.join_room_as("pw-test", "quiet", Role::Host);
    a.join_room_as("pw-test", "quiet", Role::Peer);
    b.join_room_as("pw-test", "quiet", Role::Peer);
    let mut ev = [vec![], vec![], vec![]];
    let via_h = |n: &Node, p: &PeerId| n.path(p) == Some(&Path::Via(hid));
    assert!(pump(&mut [&mut h, &mut a, &mut b], &mut ev, 10, |n| via_h(n[1], &bid) && via_h(n[2], &aid)));
    a.send(&bid, peerwire::Lane::Reliable, b"psst").unwrap();
    pump(&mut [&mut h, &mut a, &mut b], &mut ev, 2, |_| false);
    assert!(via_h(&a, &bid) && via_h(&b, &aid), "still relayed: never punched to a direct path");
    assert!(matches!(a.path(&hid), Some(Path::Direct(_))) && matches!(b.path(&hid), Some(Path::Direct(_))));
    let got: Vec<&[u8]> = ev[2]
        .iter()
        .filter_map(|e| match e {
            Event::Message { peer, data, .. } if *peer == aid => Some(&data[..]),
            _ => None,
        })
        .collect();
    assert_eq!(got, vec![&b"psst"[..]]);
    // Every link event between A and B names the relay, never an address.
    for (mine, theirs) in [(1, bid), (2, aid)] {
        for e in &ev[mine] {
            match e {
                Event::Connected { peer, path, .. } | Event::PathChanged { peer, path } if *peer == theirs => {
                    assert_eq!(path, &Path::Via(hid), "{e:?}");
                }
                Event::AddrObserved { by, .. } => assert_eq!(by, &hid),
                _ => {}
            }
        }
    }
    // The offers A and B sent each other: no address, and only the host as relay.
    let mut seen: HashMap<(PeerId, PeerId), usize> = HashMap::new();
    while let Some((topic, payload)) = tap.poll() {
        let me = if topic == Topic::peer(&aid) { &ids[1] } else { &ids[2] };
        let (from, body) = sealed::open(me, &payload).unwrap();
        let (n, relays) = offer_contents(&body);
        if from != hid {
            assert_eq!(n, 0, "a relay-only peer's offer carries no address");
            assert!(relays.iter().all(|r| *r == hid), "{relays:?}");
            *seen.entry((from, me.peer_id())).or_default() += 1;
        }
    }
    assert!(seen.contains_key(&(aid, bid)) || seen.contains_key(&(bid, aid)), "they did swap offers: {seen:?}");
    assert!(h.stats().relayed > 0, "the host carried their packets");
}

/// A relay-only node's invite names its relay instead of addresses; a holder that
/// is linked to the relay joins through it, and both ends see `Via`.
#[test]
fn relay_only_invites_go_through_the_relay() {
    let hub = MemoryHub::new();
    let mut r = Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    let rid = r.peer_id();
    let mut a = Node::builder()
        .privacy(Privacy::RelayOnly { via: Some(rid) })
        .signaling(hub.connect())
        .bind("127.0.0.1:0")
        .unwrap();
    let mut b = Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    let (aid, bid) = (a.peer_id(), b.peer_id());
    a.connect(&r.invite()).unwrap();
    b.connect(&r.invite()).unwrap();
    let mut ev = [vec![], vec![], vec![]];
    assert!(pump(&mut [&mut r, &mut a, &mut b], &mut ev, 5, |n| n[1].path(&rid).is_some() && n[2].path(&rid).is_some()));
    let inv = Invite::<SocketAddr>::parse(&a.invite()).unwrap();
    assert_eq!(inv.addrs, vec![]);
    assert_eq!(inv.relays, vec![(rid, r.local_addr())]);
    b.connect(&a.invite()).unwrap();
    let via = |n: &Node, p: &PeerId| n.path(p) == Some(&Path::Via(rid));
    assert!(pump(&mut [&mut r, &mut a, &mut b], &mut ev, 5, |n| via(n[1], &bid) && via(n[2], &aid)));
    pump(&mut [&mut r, &mut a, &mut b], &mut ev, 1, |_| false);
    assert!(via(&a, &bid) && via(&b, &aid));
}
