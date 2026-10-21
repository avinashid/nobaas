//! NB-041: relay peers (`Node::set_relay_peers`) on real loopback sockets, with an
//! in-process signaling hub. Whether a symmetric pair links through one is in
//! `peerwire-sim/tests/traversal.rs`; here: the links themselves.
use peerwire::{Config, Error, Event, Identity, MemoryHub, Node, PeerId, Status};
use rand_core::OsRng;
use std::time::{Duration, Instant};

/// Polls every node until `done`, keeping their events; false on timeout.
fn pump(
    nodes: &mut [&mut Node],
    events: &mut Vec<Vec<Event>>,
    secs: u64,
    mut done: impl FnMut(&[&mut Node]) -> bool,
) -> bool {
    events.resize_with(nodes.len(), Vec::new);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        for (i, n) in nodes.iter_mut().enumerate() {
            n.poll();
            events[i].extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        if done(nodes) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

fn about(e: &Event) -> PeerId {
    match e {
        Event::AddrObserved { by, .. } => *by,
        Event::Connected { peer, .. }
        | Event::Message { peer, .. }
        | Event::Disconnected { peer, .. }
        | Event::PathChanged { peer, .. }
        | Event::ConnectFailed { peer } => *peer,
    }
}

fn relay_linked(n: &Node, relay: PeerId) -> bool {
    n.relay_peers().contains(&(relay, Status::Connected))
}

/// Dials that give up in well under a second, for a relay that isn't there yet.
fn quick() -> Config {
    Config { handshake_retry: Duration::from_millis(50), handshake_attempts: 3, ..Config::default() }
}

#[test]
fn relay_links_stay_up_out_of_sight_and_come_back() {
    let hub = MemoryHub::new();
    // The relay, as `peerwire-peer` runs it: no signaling, no room.
    let identity = Identity::generate(&mut OsRng);
    let mut relay = Node::bind_with("127.0.0.1:0", identity.clone(), Config::default()).unwrap();
    let (rid, at) = (relay.peer_id(), relay.local_addr());
    let invite = relay.invite();
    let node = || Node::builder().signaling(hub.connect()).relay_peers([invite.as_str()]).bind("127.0.0.1:0").unwrap();
    let (mut a, mut b) = (node(), node());
    let (aid, bid) = (a.peer_id(), b.peer_id());
    assert_eq!(a.relay_peers(), vec![(rid, Status::Connecting)], "dialled at bind");
    let mut ev = Vec::new();
    let both = |n: &[&mut Node]| relay_linked(n[1], rid) && relay_linked(n[2], rid) && n[0].peers().count() == 2;
    assert!(pump(&mut [&mut relay, &mut a, &mut b], &mut ev, 5, both));

    // In a room, the relay links stay (the room admits them as dialled), and
    // the pair links as before.
    a.join_room("pw-test", "relayed");
    b.join_room("pw-test", "relayed");
    let up = |n: &[&mut Node]| n[1].status(&bid) == Status::Connected && n[2].status(&aid) == Status::Connected;
    assert!(pump(&mut [&mut relay, &mut a, &mut b], &mut ev, 5, up));
    pump(&mut [&mut relay, &mut a, &mut b], &mut ev, 1, |_| false);
    assert!(relay_linked(&a, rid) && relay_linked(&b, rid));
    // The app sees the other player only: no event about the relay, nor in `peers()`.
    for (n, other) in [(&a, bid), (&b, aid)] {
        assert_eq!(n.peers().copied().collect::<Vec<_>>(), vec![other]);
    }
    for e in ev[1].iter().chain(&ev[2]) {
        assert_ne!(about(e), rid, "{e:?}");
    }
    assert!(ev[1].iter().any(|e| matches!(e, Event::Connected { peer, .. } if *peer == bid)));

    // The relay closes A's link: A dials it again at once.
    relay.disconnect(&aid);
    assert!(pump(&mut [&mut relay, &mut a, &mut b], &mut ev, 1, |n| !relay_linked(n[1], rid)));
    assert!(pump(&mut [&mut relay, &mut a, &mut b], &mut ev, 3, |n| relay_linked(n[1], rid)));

    // A relay that is down: the dial fails, and is tried again until it is up.
    drop(relay);
    let mut c = Node::builder().config(quick()).relay_peers([invite.as_str()]).bind("127.0.0.1:0").unwrap();
    let (mut alone, mut ev) = (Vec::new(), Vec::new());
    assert!(pump(&mut [&mut c], &mut alone, 3, |n| n[0].relay_peers() == vec![(rid, Status::Disconnected)]));
    assert!(c.next_timeout().is_some_and(|t| t <= Duration::from_secs(2)), "wakes for the next dial");
    let mut relay = Node::bind_with(at, identity, Config::default()).unwrap();
    assert!(pump(&mut [&mut relay, &mut c], &mut ev, 10, |n| relay_linked(n[1], rid)));
    assert!(alone[0].iter().chain(&ev[1]).all(|e| about(e) != rid), "no event on C either");

    // Dropped from the list: the link is closed. A bad invite changes nothing.
    assert_eq!(c.set_relay_peers(&["hello"]), Err(Error::InvalidInvite));
    assert!(relay_linked(&c, rid));
    c.set_relay_peers::<&str>(&[]).unwrap();
    assert_eq!(c.relay_peers(), vec![]);
    assert!(pump(&mut [&mut relay, &mut c], &mut ev, 3, |n| n[0].peers().count() == 0));
    assert!(Node::builder().relay_peers(["pw:nonsense"]).bind("127.0.0.1:0").is_err());
}

/// A relay-only node links through its own relay only: it never dials a relay
/// peer, not even through that relay.
#[test]
fn relay_only_nodes_dial_no_relay_peer() {
    let (mut relay, mut own) = (Node::bind("127.0.0.1:0").unwrap(), Node::bind("127.0.0.1:0").unwrap());
    let (rid, oid) = (relay.peer_id(), own.peer_id());
    let invite = relay.invite();
    let mut n = Node::builder()
        .privacy(peerwire::Privacy::RelayOnly { via: Some(oid) })
        .relay_peers([invite.as_str()])
        .bind("127.0.0.1:0")
        .unwrap();
    n.connect(&own.invite()).unwrap();
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut relay, &mut own, &mut n], &mut ev, 3, |n| n[2].status(&oid) == Status::Connected));
    pump(&mut [&mut relay, &mut own, &mut n], &mut ev, 1, |_| false);
    assert_eq!(n.relay_peers(), vec![(rid, Status::Disconnected)], "not dialled, directly or through its relay");
    assert_eq!(relay.stats().rx_packets, 0, "not even a handshake");
    // Out of relay-only, it dials.
    n.set_privacy(peerwire::Privacy::Direct);
    assert!(pump(&mut [&mut relay, &mut own, &mut n], &mut ev, 3, |n| relay_linked(n[2], rid)));
}
