//! Endpoint scenarios run in the deterministic simulator.
#![allow(clippy::needless_range_loop)]
use peerwire_core::{Config, DisconnectReason, Event, Lane, Path, MAX_DATAGRAM};
use peerwire_sim::{NetConfig, Sim, SimAddr};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

type Captured = (SimAddr, SimAddr, Vec<u8>);

/// Events other than address observations, which arrive with every keepalive.
fn link_events(sim: &mut Sim, i: usize) -> Vec<Event<SimAddr>> {
    sim.take_events(i).into_iter().filter(|e| !matches!(e, Event::AddrObserved { .. })).collect()
}

fn connected(sim: &Sim, i: usize, j: usize) -> bool {
    sim.nodes[i].endpoint.is_connected(&sim.peer_id(j)) && sim.nodes[j].endpoint.is_connected(&sim.peer_id(i))
}

fn pair(net: NetConfig, seed: u64) -> Sim {
    let mut sim = Sim::new(net, seed);
    sim.add_node(Config::default());
    sim.add_node(Config::default());
    sim.connect(0, 1).unwrap();
    assert!(sim.run_until(Duration::from_secs(20), |s| connected(s, 0, 1)), "pair never connected");
    sim
}

fn messages(events: &[Event<SimAddr>], lane: Lane) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Message { lane: l, data, .. } if *l == lane => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn count_connected(events: &[Event<SimAddr>]) -> usize {
    events.iter().filter(|e| matches!(e, Event::Connected { .. })).count()
}

#[test]
fn handshake_takes_one_round_trip() {
    let mut sim = Sim::new(NetConfig::default(), 1); // 20 ms one way
    sim.add_node(Config::default());
    sim.add_node(Config::default());
    sim.connect(0, 1).unwrap();
    sim.run_until(Duration::from_secs(1), |s| s.nodes[0].endpoint.is_connected(&s.peer_id(1)));
    assert_eq!(sim.now.as_micros(), 40_000, "initiator connects after 1 RTT");
    sim.run_until(Duration::from_secs(1), |s| s.nodes[1].endpoint.is_connected(&s.peer_id(0)));
    assert_eq!(sim.now.as_micros(), 60_000, "responder connects on the initiator's first data packet");
    let (e0, e1) = (link_events(&mut sim, 0), link_events(&mut sim, 1));
    assert!(matches!(e0[..], [Event::Connected { initiator: true, peer, .. }] if peer == sim.peer_id(1)));
    assert!(matches!(e1[..], [Event::Connected { initiator: false, peer, .. }] if peer == sim.peer_id(0)));
}

#[test]
fn reliable_lane_exact_and_ordered_under_loss() {
    let net = NetConfig {
        latency: Duration::from_millis(30),
        jitter: Duration::from_millis(40),
        loss: 0.10,
        duplicate: 0.05,
        in_order: false,
    };
    let mut sim = pair(net, 2);
    let mut rng = ChaCha20Rng::seed_from_u64(9);
    let mut sent: [Vec<Vec<u8>>; 2] = [Vec::new(), Vec::new()];
    for k in 0..1500u32 {
        for from in 0..2 {
            let len = match k % 50 {
                0 => 40_000, // multi-fragment
                1 => 0,
                _ => (rng.next_u32() % 900) as usize,
            };
            let mut m = vec![0u8; len];
            rng.fill_bytes(&mut m);
            m.extend_from_slice(&k.to_le_bytes());
            sim.send(from, 1 - from, Lane::Reliable, &m).unwrap();
            sent[from].push(m);
        }
        if k % 20 == 0 {
            sim.run_for(Duration::from_millis(5));
        }
    }
    let mut got: [Vec<Vec<u8>>; 2] = [Vec::new(), Vec::new()];
    let done = sim.run_until(Duration::from_secs(120), |s| {
        for i in 0..2 {
            got[i].extend(messages(&s.take_events(i), Lane::Reliable));
        }
        got[0].len() == sent[1].len() && got[1].len() == sent[0].len()
    });
    assert!(done, "only {} / {} delivered", got[1].len(), sent[0].len());
    assert_eq!(got[1], sent[0]);
    assert_eq!(got[0], sent[1]);
    let st = sim.nodes[0].endpoint.stats();
    println!(
        "reliable: 3000 msgs, virtual {:.2}s, wire lost {} dup {}, node0 retransmits {}, replays rejected {}",
        sim.now.as_micros() as f64 / 1e6,
        sim.stats.lost,
        sim.stats.duplicated,
        st.retransmits,
        st.rx_replayed
    );
    assert!(st.retransmits > 0 && st.rx_replayed > 0);
}

#[test]
fn unreliable_lane_at_most_once() {
    let net = NetConfig { loss: 0.10, duplicate: 0.10, jitter: Duration::from_millis(30), ..Default::default() };
    let mut sim = pair(net, 3);
    sim.take_events(1);
    for k in 0..2000u32 {
        sim.send(0, 1, Lane::Unreliable, &k.to_le_bytes()).unwrap();
        sim.run_for(Duration::from_millis(5)); // one packet per message
    }
    sim.run_for(Duration::from_secs(1));
    let got = messages(&sim.take_events(1), Lane::Unreliable);
    let mut ids: Vec<u32> = got.iter().map(|m| u32::from_le_bytes(m[..4].try_into().unwrap())).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicates delivered");
    println!("unreliable: {n} / 2000 delivered at 10% loss");
    assert!((1700..=1880).contains(&n), "{n}");
}

#[test]
fn tampered_packets_are_rejected() {
    let mut sim = pair(NetConfig::default(), 4);
    let flips = Rc::new(RefCell::new(0u32));
    let f = flips.clone();
    let mut rng = ChaCha20Rng::seed_from_u64(5);
    // Corrupt one byte in every third data packet.
    sim.hook = Some(Box::new(move |_, _, p| {
        if p[0] & 0x0f == 3 && rng.next_u32() % 3 == 0 {
            let i = 1 + (rng.next_u32() as usize % (p.len() - 1));
            p[i] ^= 1 << (rng.next_u32() % 8);
            *f.borrow_mut() += 1;
        }
        true
    }));
    let sent: Vec<Vec<u8>> = (0..300u32).map(|k| format!("message {k}").into_bytes()).collect();
    for m in &sent {
        sim.send(0, 1, Lane::Reliable, m).unwrap();
        sim.run_for(Duration::from_millis(2));
    }
    let mut got = Vec::new();
    assert!(sim.run_until(Duration::from_secs(60), |s| {
        got.extend(messages(&s.take_events(1), Lane::Reliable));
        got.len() == sent.len()
    }));
    assert_eq!(got, sent);
    let st = sim.nodes[1].endpoint.stats();
    let rejected = st.rx_auth_failed + st.rx_unknown_session + st.rx_replayed + st.rx_malformed;
    println!("tamper: {} packets corrupted, {} rejected at node1 ({:?})", flips.borrow(), rejected, st);
    assert!(rejected > 0);
}

#[test]
fn replayed_packets_are_rejected() {
    let mut sim = pair(NetConfig::default(), 6);
    let captured: Rc<RefCell<Vec<Captured>>> = Rc::default();
    let c = captured.clone();
    sim.hook = Some(Box::new(move |from, to, p| {
        c.borrow_mut().push((from, to, p.clone()));
        true
    }));
    for k in 0..50u32 {
        sim.send(0, 1, Lane::Reliable, &k.to_le_bytes()).unwrap();
        sim.send(0, 1, Lane::Unreliable, &k.to_le_bytes()).unwrap();
        sim.run_for(Duration::from_millis(3));
    }
    sim.run_for(Duration::from_millis(200));
    sim.hook = None;
    sim.take_events(1);
    let before = sim.nodes[1].endpoint.stats().rx_replayed;
    let to1: Vec<_> = captured.borrow().iter().filter(|(_, to, _)| *to == sim.addr(1)).cloned().collect();
    for (from, to, p) in &to1 {
        sim.inject(*from, *to, p);
    }
    let replayed = sim.nodes[1].endpoint.stats().rx_replayed - before;
    assert_eq!(replayed, to1.len() as u64);
    assert!(messages(&sim.take_events(1), Lane::Reliable).is_empty());
    assert!(messages(&sim.take_events(1), Lane::Unreliable).is_empty());
}

#[test]
fn dialling_the_wrong_identity_fails() {
    let mut sim = Sim::new(NetConfig::default(), 7);
    for _ in 0..3 {
        sim.add_node(Config::default());
    }
    // Node 0 dials node 1's address but expects node 2's identity.
    let (addr1, peer2) = (sim.addr(1), sim.peer_id(2));
    sim.nodes[0].endpoint.connect(sim.now, addr1, peer2).unwrap();
    sim.run_for(Duration::from_secs(30));
    let e0 = sim.take_events(0);
    assert!(matches!(e0[..], [Event::ConnectFailed { peer, .. }] if peer == peer2), "{e0:?}");
    assert_eq!(count_connected(&sim.take_events(1)), 0);
    assert_eq!(sim.nodes[1].endpoint.stats().rx_auth_failed, 6, "one per HS1 attempt");
}

#[test]
fn simultaneous_open_converges_to_one_link() {
    for seed in 0..20 {
        let net = NetConfig { jitter: Duration::from_millis(15), ..Default::default() };
        let mut sim = Sim::new(net, 100 + seed);
        sim.add_node(Config::default());
        sim.add_node(Config::default());
        sim.connect(0, 1).unwrap();
        sim.connect(1, 0).unwrap();
        assert!(sim.run_until(Duration::from_secs(10), |s| connected(s, 0, 1)));
        sim.run_for(Duration::from_secs(1));
        for i in 0..2 {
            assert_eq!(count_connected(&sim.take_events(i)), 1, "seed {seed}");
            assert_eq!(sim.nodes[i].endpoint.session_count(), 1, "seed {seed}");
        }
        sim.send(0, 1, Lane::Reliable, b"hi").unwrap();
        sim.send(1, 0, Lane::Reliable, b"yo").unwrap();
        sim.run_for(Duration::from_millis(200));
        assert_eq!(messages(&sim.take_events(1), Lane::Reliable), vec![b"hi".to_vec()]);
        assert_eq!(messages(&sim.take_events(0), Lane::Reliable), vec![b"yo".to_vec()]);
    }
}

#[test]
fn idle_link_times_out_and_keepalive_holds_it() {
    let mut sim = pair(NetConfig::default(), 8);
    sim.take_events(0);
    sim.take_events(1);
    sim.run_for(Duration::from_secs(120)); // no app traffic: keepalives only
    assert!(connected(&sim, 0, 1));
    sim.hook = Some(Box::new(|_, _, _| false)); // partition
    sim.run_for(Duration::from_secs(31));
    for i in 0..2 {
        let e = link_events(&mut sim, i);
        assert!(matches!(e[..], [Event::Disconnected { reason: DisconnectReason::Timeout, .. }]), "{e:?}");
    }
}

#[test]
fn idle_link_survives_heavy_loss() {
    // Three keepalives lost in a row used to time a quiet link out (~3% per
    // 30 s at 30% loss). A silent peer is now probed every second, and answers.
    for seed in 1..=4 {
        let net = NetConfig { loss: 0.3, ..NetConfig::default() };
        let mut sim = pair(net, seed);
        sim.take_events(0);
        sim.take_events(1);
        sim.run_for(Duration::from_secs(600));
        assert!(connected(&sim, 0, 1), "seed {seed}");
        for i in 0..2 {
            let e = link_events(&mut sim, i);
            assert!(e.is_empty(), "seed {seed}: {e:?}");
        }
    }
}

#[test]
fn peer_restart_replaces_the_stale_link() {
    let mut sim = pair(NetConfig::default(), 9);
    sim.take_events(0);
    sim.restart(1, Config::default());
    sim.connect(1, 0).unwrap();
    assert!(sim.run_until(Duration::from_secs(5), |s| connected(s, 0, 1)));
    sim.run_for(Duration::from_millis(100));
    let e0 = link_events(&mut sim, 0);
    assert!(
        matches!(
            e0[..],
            [Event::Disconnected { reason: DisconnectReason::Replaced, .. }, Event::Connected { initiator: false, .. }]
        ),
        "{e0:?}"
    );
    assert_eq!(sim.nodes[0].endpoint.session_count(), 1);
    sim.send(0, 1, Lane::Reliable, b"after restart").unwrap();
    sim.run_for(Duration::from_millis(100));
    assert_eq!(messages(&sim.take_events(1), Lane::Reliable), vec![b"after restart".to_vec()]);
}

#[test]
fn replayed_handshake_does_not_displace_a_live_link() {
    let mut sim = Sim::new(NetConfig::default(), 10);
    sim.add_node(Config::default());
    sim.add_node(Config::default());
    let hs1: Rc<RefCell<Option<Vec<u8>>>> = Rc::default();
    let h = hs1.clone();
    sim.hook = Some(Box::new(move |_, _, p| {
        if p[0] & 0x0f == 1 && h.borrow().is_none() {
            *h.borrow_mut() = Some(p.clone());
        }
        true
    }));
    sim.connect(0, 1).unwrap();
    assert!(sim.run_until(Duration::from_secs(5), |s| connected(s, 0, 1)));
    sim.take_events(1);
    let pkt = hs1.borrow().clone().unwrap();
    sim.run_for(Duration::from_secs(1));
    // Attacker replays node 0's HS1 from another address.
    sim.inject(SimAddr::new(999, 1), sim.addr(1), &pkt);
    sim.run_for(Duration::from_secs(15));
    assert!(link_events(&mut sim, 1).is_empty(), "replay must not cause Replaced/Connected");
    assert!(connected(&sim, 0, 1));
    assert_eq!(sim.nodes[1].endpoint.session_count(), 1, "half-open state expired");
}

#[test]
fn address_change_roams_the_link() {
    let mut sim = pair(NetConfig::default(), 11);
    sim.take_events(1);
    sim.set_addr(0, SimAddr::new(7777, 7000));
    sim.send(0, 1, Lane::Reliable, b"from new address").unwrap();
    sim.run_for(Duration::from_millis(100));
    assert_eq!(messages(&sim.take_events(1), Lane::Reliable), vec![b"from new address".to_vec()]);
    assert_eq!(sim.nodes[1].endpoint.stats().roams, 1);
    sim.send(1, 0, Lane::Reliable, b"reply").unwrap();
    sim.run_for(Duration::from_millis(100));
    assert_eq!(messages(&sim.take_events(0), Lane::Reliable), vec![b"reply".to_vec()]);
}

#[test]
fn fuzzed_datagrams_never_panic_or_break_the_link() {
    let mut sim = pair(NetConfig::default(), 12);
    let captured: Rc<RefCell<Vec<Vec<u8>>>> = Rc::default();
    let c = captured.clone();
    sim.hook = Some(Box::new(move |_, _, p| {
        c.borrow_mut().push(p.clone());
        true
    }));
    for k in 0..30u32 {
        sim.send(0, 1, Lane::Reliable, &vec![k as u8; 3000]).unwrap();
        sim.run_for(Duration::from_millis(10));
    }
    sim.run_for(Duration::from_millis(500));
    sim.hook = None;
    sim.take_events(1);
    let mut rng = ChaCha20Rng::seed_from_u64(13);
    let corpus = captured.borrow().clone();
    let target = sim.addr(1);
    for round in 0..200_000u32 {
        let pkt = match round % 4 {
            // pure noise
            0 => {
                let mut p = vec![0u8; (rng.next_u32() as usize) % (MAX_DATAGRAM + 64)];
                rng.fill_bytes(&mut p);
                p
            }
            // valid kind byte, random body
            1 => {
                let mut p = vec![0u8; 1 + (rng.next_u32() as usize) % 300];
                rng.fill_bytes(&mut p);
                p[0] = 0x10 | (1 + (rng.next_u32() % 3) as u8);
                p
            }
            // mutated real packet
            _ => {
                let mut p = corpus[(rng.next_u32() as usize) % corpus.len()].clone();
                for _ in 0..1 + rng.next_u32() % 4 {
                    let i = (rng.next_u32() as usize) % p.len();
                    p[i] = rng.next_u32() as u8;
                }
                if rng.next_u32() % 5 == 0 {
                    p.truncate((rng.next_u32() as usize) % (p.len() + 1));
                }
                p
            }
        };
        let from = if round % 2 == 0 { sim.addr(0) } else { SimAddr::new(5000 + rng.next_u32() % 100, 9) };
        sim.inject(from, target, &pkt);
    }
    let st = sim.nodes[1].endpoint.stats();
    println!("fuzz: 200000 datagrams -> {st:?}");
    assert!(sim.take_events(1).iter().all(|e| !matches!(e, Event::Message { .. })));
    sim.run_for(Duration::from_secs(12)); // let half-open handshakes from noise expire
    assert!(connected(&sim, 0, 1));
    sim.send(0, 1, Lane::Reliable, b"still alive").unwrap();
    sim.run_for(Duration::from_millis(200));
    assert_eq!(messages(&sim.take_events(1), Lane::Reliable), vec![b"still alive".to_vec()]);
}

#[test]
fn hub_with_300_peers() {
    let net = NetConfig {
        latency: Duration::from_millis(25),
        jitter: Duration::from_millis(10),
        loss: 0.02,
        ..Default::default()
    };
    let mut sim = Sim::new(net, 14);
    let n = 300;
    for _ in 0..=n {
        sim.add_node(Config::default());
    }
    let wall = std::time::Instant::now();
    for i in 1..=n {
        sim.connect(i, 0).unwrap();
    }
    assert!(sim.run_until(Duration::from_secs(30), |s| s.nodes[0].endpoint.connected_peers().count() == n));
    let t_connect = sim.now.as_micros() as f64 / 1e3;
    for i in 1..=n {
        for k in 0..10u32 {
            sim.send(i, 0, Lane::Reliable, &k.to_le_bytes()).unwrap();
        }
    }
    let mut got = 0;
    assert!(sim.run_until(Duration::from_secs(30), |s| {
        got += messages(&s.take_events(0), Lane::Reliable).len();
        got == n * 10
    }));
    println!(
        "hub: {n} peers connected by {t_connect:.0} ms virtual, {} msgs delivered, wall {:?}",
        n * 10,
        wall.elapsed()
    );
}

#[test]
fn same_seed_same_run() {
    let run = |seed| {
        let net = NetConfig { loss: 0.05, jitter: Duration::from_millis(20), ..Default::default() };
        let mut sim = pair(net, seed);
        for k in 0..200u32 {
            sim.send(0, 1, Lane::Reliable, &k.to_le_bytes()).unwrap();
            sim.send(1, 0, Lane::Unreliable, &k.to_le_bytes()).unwrap();
            sim.run_for(Duration::from_millis(3));
        }
        sim.run_for(Duration::from_secs(5));
        (sim.stats.clone(), sim.nodes[0].endpoint.stats(), sim.take_events(0), sim.take_events(1), sim.now)
    };
    assert_eq!(run(15), run(15));
    assert_ne!(run(15).0, run(16).0);
}

#[test]
fn bulk_transfer_keeps_the_queue_short_for_game_traffic() {
    // 2 Mbit/s uplink with an unbounded FIFO (never drops): a loss-only window
    // would fill it. 60 Hz game messages share the link with a 2 MB transfer.
    let mut sim = pair(NetConfig::default(), 21);
    sim.nodes[0].uplink_bps = Some(2_000_000);
    let start = sim.now;
    let bulk = vec![3u8; 1 << 20];
    sim.send(0, 1, Lane::Reliable, &bulk).unwrap();
    sim.send(0, 1, Lane::Reliable, &bulk).unwrap();
    let mut delays = Vec::new();
    let mut got_bulk = 0;
    for tick in 0..900u64 {
        let mut m = vec![0x47u8];
        m.extend_from_slice(&sim.now.as_micros().to_le_bytes());
        sim.send(0, 1, Lane::Unreliable, &m).unwrap();
        sim.run_for(Duration::from_micros(16_667));
        for (at, e) in sim.take_timed_events(1) {
            if let Event::Message { lane, data, .. } = e {
                match lane {
                    Lane::Unreliable => {
                        let sent = u64::from_le_bytes(data[1..9].try_into().unwrap());
                        delays.push((at.as_micros() - sent) / 1000);
                    }
                    Lane::Reliable | Lane::Urgent => got_bulk += 1,
                }
            }
        }
        if got_bulk == 2 && tick > 60 {
            break;
        }
    }
    let secs = (sim.now - start).as_secs_f64();
    delays.sort();
    let p = |q: f64| delays[((delays.len() - 1) as f64 * q) as usize];
    println!(
        "2 MB in {secs:.1} s ({:.2} Mbit/s of 2); game one-way delay p50 {} ms, p95 {} ms, max {} ms (20 ms base)",
        (2 << 20) as f64 * 8.0 / secs / 1e6,
        p(0.5),
        p(0.95),
        delays[delays.len() - 1]
    );
    assert_eq!(got_bulk, 2, "bulk transfer finished");
    assert!(p(0.95) < 100, "queueing delay stays bounded: p95 {} ms", p(0.95));
    assert!(secs < 15.0, "and the link is still used: {secs:.1} s");
}

/// Tags a numbered message with its lane: both arrive as `Lane::Reliable`.
fn tagged(tag: u8, n: u32, len: usize) -> Vec<u8> {
    let mut m = vec![tag; len.max(5)];
    m[1..5].copy_from_slice(&n.to_le_bytes());
    m
}

#[test]
fn urgent_overtakes_queued_reliable_and_each_lane_stays_in_order() {
    // 2 MiB of reliable data queued on a 2 Mbit/s uplink takes about 8 s to
    // drain; urgent messages sent meanwhile must not wait for it. 2% loss
    // exercises retransmission on both streams.
    let mut sim = pair(NetConfig { loss: 0.02, ..NetConfig::default() }, 22);
    sim.nodes[0].uplink_bps = Some(2_000_000);
    for n in 0..32 {
        sim.send(0, 1, Lane::Reliable, &tagged(b'R', n, 64 << 10)).unwrap();
    }
    let (mut sent_at, mut delays, mut order) = (Vec::new(), Vec::new(), (Vec::new(), Vec::new()));
    for tick in 0..1500u32 {
        if tick < 100 {
            sim.send(0, 1, Lane::Urgent, &tagged(b'U', tick, 200)).unwrap();
            sent_at.push(sim.now);
        }
        sim.run_for(Duration::from_millis(10));
        for (at, e) in sim.take_timed_events(1) {
            if let Event::Message { lane: Lane::Reliable, data, .. } = e {
                let n = u32::from_le_bytes(data[1..5].try_into().unwrap());
                if data[0] == b'U' {
                    delays.push((at - sent_at[n as usize]).as_millis());
                    order.1.push(n);
                } else {
                    order.0.push(n);
                }
            }
        }
        if order.0.len() == 32 && order.1.len() == 100 {
            break;
        }
    }
    assert_eq!(order.0, (0..32).collect::<Vec<_>>(), "reliable: all, in order");
    assert_eq!(order.1, (0..100).collect::<Vec<_>>(), "urgent: all, in order");
    // The first goes in the first packet, ahead of the initial window of reliable fragments
    // (200 B: it fits only if it goes first).
    let first = delays[0];
    delays.sort();
    let p99 = delays[98];
    println!("urgent one-way delay: first {first} ms, p50 {} ms, p99 {p99} ms (20 ms base)", delays[50]);
    assert!(first < 40, "urgent goes first in the packet: {first} ms");
    assert!(p99 < 250, "urgent doesn't wait behind 2 MiB of reliable data: p99 {p99} ms");
}

#[test]
fn urgent_stream_repairs_loss_on_its_own() {
    // Only urgent traffic: its retransmission timer and ACKs must work without
    // reliable traffic to wake the link.
    let mut sim = pair(NetConfig::default(), 24);
    sim.send(0, 1, Lane::Urgent, b"warm").unwrap();
    sim.run_for(Duration::from_secs(1));
    sim.take_events(1);
    let before = sim.nodes[0].endpoint.stats().retransmits;
    sim.hook = Some(Box::new(|_, _, _| false)); // both copies are lost
    sim.send(0, 1, Lane::Urgent, b"hit").unwrap();
    let start = sim.now;
    sim.run_for(Duration::from_millis(30));
    sim.hook = None;
    let delivered = |s: &mut Sim| s.take_events(1).iter().any(|e| matches!(e, Event::Message { .. }));
    assert!(sim.run_until(Duration::from_secs(5), delivered));
    let took = (sim.now - start).as_millis();
    sim.run_for(Duration::from_secs(5));
    let resent = sim.nodes[0].endpoint.stats().retransmits - before;
    println!("repaired after {took} ms; {resent} retransmissions");
    assert!(took < 300, "repaired within an RTO, not a keepalive: {took} ms");
    assert!(resent <= 2, "and acknowledged, so nothing more is sent: {resent}");
}

#[test]
fn urgent_flood_does_not_starve_reliable() {
    // Both lanes always have data queued on a 2 Mbit/s uplink: each stream has
    // its own congestion window, so they share the link.
    let mut sim = pair(NetConfig::default(), 23);
    sim.nodes[0].uplink_bps = Some(2_000_000);
    let (mut next, mut got) = ([0u32; 2], [0usize; 2]);
    let start = sim.now;
    while sim.now < start + Duration::from_secs(10) {
        for (i, (tag, lane)) in [(b'R', Lane::Reliable), (b'U', Lane::Urgent)].into_iter().enumerate() {
            while sim.send(0, 1, lane, &tagged(tag, next[i], 1000)).is_ok() {
                next[i] += 1;
            }
        }
        sim.run_for(Duration::from_millis(10));
        for e in sim.take_events(1) {
            if let Event::Message { data, .. } = e {
                got[(data[0] == b'U') as usize] += data.len();
            }
        }
    }
    let share = got[0] as f64 / (got[0] + got[1]) as f64;
    println!(
        "10 s: reliable {} kB, urgent {} kB delivered ({:.0}% reliable)",
        got[0] / 1000,
        got[1] / 1000,
        share * 100.0
    );
    assert!(share > 0.25, "reliable keeps a fair share under an urgent flood: {:.0}%", share * 100.0);
}

/// Fast lane 2 under 10% loss and 5 ms jitter: a link between a and b on a slow
/// address (a signalling pipe) moves through c once a probe and its answer get
/// through, and back when c closes its links (or, if those closes are lost, once
/// c has carried nothing for `direct_timeout`). A reliable stream from a to b
/// throughout arrives whole and in order, on the one link.
#[test]
fn slow_link_moves_through_a_relay_and_back_under_loss() {
    let net = NetConfig { loss: 0.1, jitter: Duration::from_millis(5), ..NetConfig::default() };
    let mut sim = Sim::new(net, 7);
    for _ in 0..3 {
        sim.add_node(Config::default());
    }
    sim.connect(0, 1).unwrap();
    sim.connect(2, 0).unwrap();
    sim.connect(2, 1).unwrap();
    let linked = |s: &mut Sim| connected(s, 0, 1) && connected(s, 2, 0) && connected(s, 2, 1);
    assert!(sim.run_until(Duration::from_secs(20), linked), "trio never linked");
    let (a, b, c) = (sim.peer_id(0), sim.peer_id(1), sim.peer_id(2));
    let (at_a, at_b) = (sim.addr(0), sim.addr(1));
    sim.nodes[0].endpoint.set_slow(&b, Some(at_b));
    sim.nodes[1].endpoint.set_slow(&a, Some(at_a));
    let now = sim.now;
    sim.nodes[0].endpoint.add_relay(now, c, &b);
    link_events(&mut sim, 0);
    link_events(&mut sim, 1);

    let mut sent = 0u32;
    let mut until = |sim: &mut Sim, what: &str, done: &dyn Fn(&Sim) -> bool| {
        for _ in 0..200 {
            if done(sim) {
                return;
            }
            sim.send(0, 1, Lane::Reliable, &sent.to_le_bytes()).unwrap();
            sent += 1;
            sim.run_for(Duration::from_millis(100));
        }
        panic!("{what} after 20 s: {:?} {:?}", sim.path(0, 1), sim.path(1, 0));
    };
    let via_c = Some(Path::Via(c));
    until(&mut sim, "not through c", &|s| s.path(0, 1) == via_c && s.path(1, 0) == via_c);
    let relayed = sim.nodes[2].endpoint.stats().relayed;
    sim.run_for(Duration::from_secs(1));
    assert!(sim.nodes[2].endpoint.stats().relayed > relayed, "c relays the link");
    sim.nodes[2].endpoint.disconnect(&a);
    sim.nodes[2].endpoint.disconnect(&b);
    let back = |s: &Sim| s.path(0, 1) == Some(Path::Direct(at_b)) && s.path(1, 0) == Some(Path::Direct(at_a));
    until(&mut sim, "not back on the slow address", &back);
    sim.run_for(Duration::from_secs(5));

    let (ea, eb) = (link_events(&mut sim, 0), link_events(&mut sim, 1));
    let moves = |e: &[Event<_>], peer| {
        e.iter().filter(|e| matches!(e, Event::PathChanged { peer: p, .. } if *p == peer)).count()
    };
    assert_eq!((moves(&ea, b), moves(&eb, a)), (2, 2), "one move there and one back each: {ea:?} {eb:?}");
    let lost_link = |e: &[Event<_>], peer| {
        e.iter()
            .any(|e| matches!(e, Event::Connected { peer: p, .. } | Event::Disconnected { peer: p, .. } if *p == peer))
    };
    assert!(!lost_link(&ea, b) && !lost_link(&eb, a), "the same link throughout");
    let got: Vec<u32> =
        messages(&eb, Lane::Reliable).iter().map(|m| u32::from_le_bytes(m[..4].try_into().unwrap())).collect();
    assert_eq!(got, (0..sent).collect::<Vec<_>>(), "every message once, in order");
}
