//! Nostr signaling: against the in-memory relay from `examples/rust/nostr_relay.rs`
//! on loopback, and (opt-in, `scripts/gate.sh --nostr`) against public relays.
#[path = "../../../examples/rust/nostr_relay.rs"]
mod relay;

use nobaas::{NostrSignaling, Node, Signaling, Status, Topic};
use std::time::{Duration, Instant};

fn wait(secs: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn carriers_exchange_topic_messages_through_a_relay() {
    let url = format!("ws://{}", relay::spawn("127.0.0.1:0").unwrap());
    let (mut a, mut b) = (NostrSignaling::new([&url]), NostrSignaling::new([&url]));
    let t = Topic::room("nb-test", "carrier");
    a.subscribe(t);
    b.subscribe(t);
    assert!(wait(5, || a.stats().relays_open == 1 && b.stats().relays_open == 1));
    std::thread::sleep(Duration::from_millis(100)); // let both REQs land
    a.publish(t, b"hello \x00\xff");
    let mut got = None;
    assert!(wait(5, || {
        got = b.poll();
        got.is_some()
    }));
    assert_eq!(got, Some((t, b"hello \x00\xff".to_vec())));
    assert_eq!(a.poll(), None, "not echoed to the author");
    // Another topic is not delivered.
    a.publish(Topic::room("nb-test", "elsewhere"), b"x");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(b.poll(), None);
}

fn pump(nodes: &mut [&mut Node], secs: u64, mut done: impl FnMut(&[&mut Node]) -> bool) -> bool {
    wait(secs, || {
        for n in nodes.iter_mut() {
            n.poll();
            while n.try_recv_event().is_some() {}
        }
        done(nodes)
    })
}

#[test]
fn nodes_meet_in_a_room_over_a_local_relay() {
    let url = format!("ws://{}", relay::spawn("127.0.0.1:0").unwrap());
    let node = || Node::builder().signaling(NostrSignaling::new([&url])).bind("127.0.0.1:0").unwrap();
    let (mut a, mut b) = (node(), node());
    let (ia, ib) = (a.peer_id(), b.peer_id());
    a.join_room("nb-test", "nostr-room");
    b.join_room("nb-test", "nostr-room");
    assert!(pump(&mut [&mut a, &mut b], 15, |n| {
        n[0].status(&ib) == Status::Connected && n[1].status(&ia) == Status::Connected
    }));
}

/// Two nodes on this host find each other through relay.damus.io and nos.lol and
/// learn their public address from Google's and Cloudflare's STUN. A few events.
#[test]
#[ignore = "touches public Nostr relays and STUN: run with scripts/gate.sh --nostr"]
fn public_relays_and_stun() {
    let room = format!("t-{:x}", Instant::now().elapsed().as_nanos() ^ u128::from(std::process::id()) << 32);
    let node = || Node::builder().signaling(NostrSignaling::default()).bind("0.0.0.0:0").unwrap();
    let (mut a, mut b) = (node(), node());
    let (ia, ib) = (a.peer_id(), b.peer_id());
    let t0 = Instant::now();
    a.join_room("nb-public-test", &room);
    b.join_room("nb-public-test", &room);
    let ok = pump(&mut [&mut a, &mut b], 45, |n| {
        n[0].status(&ib) == Status::Connected && n[1].status(&ia) == Status::Connected
    });
    println!(
        "connected {ok} in {:?}; public {:?} / {:?}; NAT {:?}; path {:?}",
        t0.elapsed(),
        a.public_addr(),
        b.public_addr(),
        a.nat_type(),
        a.path(&ib)
    );
    assert!(ok, "no connection through public relays within 45 s");
    assert!(a.public_addr().is_some(), "no STUN answer");
}
