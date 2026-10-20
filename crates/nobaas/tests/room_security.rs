//! Rooms are closed by their name (NB-031): on loopback with a MemoryHub, a peer that
//! knows only a room's topic (what every Nostr relay sees in the `p` tag) can't get
//! linked in it, by hello, offer or straight dial.
use nobaas::{Event, Lane, MemoryHub, Node, Role, Signaling, Status, Topic};
use std::time::{Duration, Instant};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Polls `nodes` for `ms`, returning every (node index, event).
fn run(nodes: &mut [&mut Node], ms: u64, mut each: impl FnMut(&mut [&mut Node])) -> Vec<(usize, Event)> {
    let end = Instant::now() + Duration::from_millis(ms);
    let mut out = Vec::new();
    while Instant::now() < end {
        each(nodes);
        for (i, n) in nodes.iter_mut().enumerate() {
            n.poll();
            out.extend(std::iter::from_fn(|| n.try_recv_event()).map(|e| (i, e)));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    out
}

/// The security review's repro (`sec_room_topic_only`), which linked before the
/// fix: an attacker given only `Topic::room(..)` announces itself as a Host.
#[test]
fn sec_room_topic_only() {
    let hub = MemoryHub::new();
    let node =
        || Node::builder().stun_servers(Vec::<String>::new()).signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    let mut client = node();
    client.join_room_as("my-game", "k3J9-xQ72-Lm0p-Zt8w-R4vB", Role::Client);
    let topic = Topic::room("my-game", "k3J9-xQ72-Lm0p-Zt8w-R4vB");
    let mut attacker = node();
    let mut side = hub.connect();
    let id = hex(attacker.peer_id().as_bytes());
    // The old hello, and one with a made-up proof.
    let hellos = [
        format!(r#"{{"t":"hello","from":"{id}","meta":{{"nb":1,"role":"host"}}}}"#),
        format!(r#"{{"t":"hello","from":"{id}","meta":{{"nb":1,"role":"host","p":"{}"}}}}"#, "ab".repeat(32)),
    ];
    let mut last = Instant::now() - Duration::from_secs(10);
    let events = run(&mut [&mut client, &mut attacker], 3000, |_| {
        if last.elapsed() > Duration::from_millis(500) {
            for h in &hellos {
                side.publish(topic, h.as_bytes());
            }
            last = Instant::now();
        }
    });
    let linked = events.iter().any(|(_, e)| matches!(e, Event::Connected { .. }));
    assert!(!linked, "a topic-only attacker linked: {events:?}");
    assert_eq!(client.peers().count(), 0);
}

/// Knowing a room member's address is not enough either: a node in a room closes
/// links from peers that never proved the room key, and their messages never
/// surface. Outside the room, the same dial works.
#[test]
fn room_nodes_close_links_from_non_members() {
    let hub = MemoryHub::new();
    let node =
        || Node::builder().stun_servers(Vec::<String>::new()).signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    let mut member = node();
    member.join_room("app", "private");
    let mut outsider = Node::builder().stun_servers(Vec::<String>::new()).bind("127.0.0.1:0").unwrap();
    let them = outsider.connect(&member.invite()).unwrap();
    let events = run(&mut [&mut member, &mut outsider], 1500, |n| {
        if n[1].status(&them) == Status::Connected {
            let _ = n[1].send(&them, Lane::Reliable, b"let me in");
        }
    });
    assert!(!events.iter().any(|(i, _)| *i == 0), "the member saw the outsider: {events:?}");
    assert_eq!(member.status(&outsider.peer_id()), Status::Disconnected);

    member.leave_room();
    let them = outsider.connect(&member.invite()).unwrap();
    let events = run(&mut [&mut member, &mut outsider], 1500, |n| {
        if n[1].status(&them) == Status::Connected {
            let _ = n[1].send(&them, Lane::Reliable, b"hi");
        }
    });
    assert!(events.iter().any(|(i, e)| *i == 0 && matches!(e, Event::Message { .. })), "{events:?}");
}

/// Members link; with an expected host, a client links with that host only.
#[test]
fn members_link_and_clients_can_pin_their_host() {
    let hub = MemoryHub::new();
    let node =
        || Node::builder().stun_servers(Vec::<String>::new()).signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    let (mut h1, mut h2, mut c) = (node(), node(), node());
    h1.join_room_as("app", "room", Role::Host);
    h2.join_room_as("app", "room", Role::Host);
    c.join_room_with("app", "room", Role::Client, Some(h2.peer_id()));
    let (p1, p2) = (h1.peer_id(), h2.peer_id());
    let events = run(&mut [&mut h1, &mut h2, &mut c], 2500, |_| {});
    let linked: Vec<_> = events
        .iter()
        .filter_map(|(i, e)| match e {
            Event::Connected { peer, .. } if *i == 2 => Some(*peer),
            _ => None,
        })
        .collect();
    assert_eq!(linked, vec![p2]);
    assert_eq!((c.room_role(&p2), c.room_role(&p1)), (Some(Role::Host), None));
}
