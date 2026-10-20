//! Two nodes over real loopback UDP, driven by plain poll loops.
use nobaas::{Event, Lane, Node, PeerId, Status};
use std::time::{Duration, Instant};

/// Polls both nodes until `done` holds (or 5 s pass), collecting their events.
fn pump(a: &mut Node, b: &mut Node, ev: &mut [Vec<Event>; 2], mut done: impl FnMut(&[Vec<Event>; 2]) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        a.poll();
        b.poll();
        ev[0].extend(std::iter::from_fn(|| a.try_recv_event()));
        ev[1].extend(std::iter::from_fn(|| b.try_recv_event()));
        if done(ev) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

fn messages(ev: &[Event], lane: Lane) -> Vec<Vec<u8>> {
    ev.iter()
        .filter_map(|e| match e {
            Event::Message { lane: l, data, .. } if *l == lane => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn connected(ev: &[Event], peer: PeerId) -> bool {
    ev.iter().any(|e| matches!(e, Event::Connected { peer: p, .. } if *p == peer))
}

#[test]
fn connect_by_invite_and_exchange_messages() {
    let mut host = Node::bind("0.0.0.0:0").unwrap();
    let mut guest = Node::bind("127.0.0.1:0").unwrap();
    let (h, g) = (host.peer_id(), guest.peer_id());
    assert_eq!(guest.connect(&host.invite()).unwrap(), h);
    assert_eq!(guest.status(&h), Status::Connecting);

    let mut ev = [Vec::new(), Vec::new()];
    assert!(pump(&mut host, &mut guest, &mut ev, |ev| connected(&ev[0], g) && connected(&ev[1], h)));
    assert_eq!(guest.status(&h), Status::Connected);
    assert_eq!(host.status(&g), Status::Connected);
    assert_eq!(host.peers().copied().collect::<Vec<_>>(), vec![g]);

    let big = vec![9u8; 100_000];
    for i in 0..50u8 {
        guest.send(&h, Lane::Reliable, &[i]).unwrap();
    }
    guest.send(&h, Lane::Reliable, &big).unwrap();
    host.send(&g, Lane::Unreliable, b"pos").unwrap();
    assert!(pump(&mut host, &mut guest, &mut ev, |ev| {
        messages(&ev[0], Lane::Reliable).len() == 51 && !messages(&ev[1], Lane::Unreliable).is_empty()
    }));
    let got = messages(&ev[0], Lane::Reliable);
    assert_eq!(got[..50], (0..50u8).map(|i| vec![i]).collect::<Vec<_>>()[..], "reliable lane is ordered");
    assert_eq!(got[50], big);
    assert_eq!(messages(&ev[1], Lane::Unreliable), vec![b"pos".to_vec()]);

    let st = guest.peer_stats(&h).unwrap();
    assert!(st.rtt.is_some() && st.min_rtt.is_some());
    assert!(st.fragments_sent >= 51);
    assert!((0.0..=1.0).contains(&st.loss()));
    assert!(host.peer_stats(&PeerId([7; 32])).is_none());
}

#[test]
fn disconnect_reaches_the_peer() {
    let mut a = Node::bind("127.0.0.1:0").unwrap();
    let mut b = Node::bind("127.0.0.1:0").unwrap();
    let (ia, ib) = (a.peer_id(), b.peer_id());
    b.connect(&a.invite()).unwrap();
    let mut ev = [Vec::new(), Vec::new()];
    assert!(pump(&mut a, &mut b, &mut ev, |ev| connected(&ev[0], ib) && connected(&ev[1], ia)));
    b.disconnect(&ia);
    assert_eq!(b.status(&ia), Status::Disconnected);
    assert!(pump(&mut a, &mut b, &mut ev, |ev| ev[0].iter().any(|e| matches!(e, Event::Disconnected { .. }))));
    assert_eq!(a.status(&ib), Status::Disconnected);
    assert_eq!(a.peers().count(), 0);
}

#[test]
fn poll_never_blocks() {
    let mut n = Node::bind("127.0.0.1:0").unwrap();
    let t = Instant::now();
    for _ in 0..1000 {
        n.poll();
    }
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(n.try_recv_event().is_none());
}
