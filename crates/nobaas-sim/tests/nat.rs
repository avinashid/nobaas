//! NAT traversal: reflexive addresses, relayed dials, hole punching and fallback.

use nobaas_core::{Config, Event, Lane, Path};
use nobaas_sim::{NatConfig, NetConfig, Sim, SimAddr};
use std::time::Duration;

#[derive(Clone, Debug)]
enum Kind {
    Public,
    Nat(&'static str, NatConfig),
    /// A home router behind a carrier-grade NAT.
    Cgnat(&'static str, NatConfig, NatConfig),
}

impl Kind {
    fn name(&self) -> &'static str {
        match self {
            Kind::Public => "public",
            Kind::Nat(n, _) | Kind::Cgnat(n, _, _) => n,
        }
    }
}

fn kinds() -> Vec<Kind> {
    vec![
        Kind::Public,
        Kind::Nat("full-cone", NatConfig::full_cone()),
        Kind::Nat("restricted", NatConfig::restricted_cone()),
        Kind::Nat("port-restr", NatConfig::port_restricted()),
        Kind::Nat("sym-seq", NatConfig::symmetric_sequential()),
        Kind::Nat("sym-random", NatConfig::symmetric_random()),
        Kind::Cgnat("cgnat(pr+pr)", NatConfig::port_restricted(), NatConfig::port_restricted()),
        Kind::Cgnat("cgnat(pr+sym)", NatConfig::port_restricted(), NatConfig::symmetric_sequential()),
    ]
}

fn place(sim: &mut Sim, k: &Kind, config: Config) -> usize {
    match k {
        Kind::Public => sim.add_node(config),
        Kind::Nat(_, n) => {
            let nat = sim.add_nat(n.clone(), None);
            sim.add_node_behind(nat, config)
        }
        Kind::Cgnat(_, home, carrier) => {
            let c = sim.add_nat(carrier.clone(), None);
            let h = sim.add_nat(home.clone(), Some(c));
            sim.add_node_behind(h, config)
        }
    }
}

fn connected(sim: &Sim, i: usize, j: usize) -> bool {
    sim.nodes[i].endpoint.is_connected(&sim.peer_id(j)) && sim.nodes[j].endpoint.is_connected(&sim.peer_id(i))
}

fn is_direct(sim: &Sim, i: usize, j: usize) -> bool {
    matches!(sim.path(i, j), Some(Path::Direct(_))) && matches!(sim.path(j, i), Some(Path::Direct(_)))
}

fn messages(events: &[Event<SimAddr>]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Message { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect()
}

/// A public rendezvous peer R plus two peers A and B, both linked to R.
fn trio(a: &Kind, b: &Kind, config: Config, seed: u64) -> (Sim, usize, usize, usize) {
    let mut sim = Sim::new(NetConfig::default(), seed);
    let r = sim.add_node(config.clone());
    let na = place(&mut sim, a, config.clone());
    let nb = place(&mut sim, b, config);
    sim.connect(na, r).unwrap();
    sim.connect(nb, r).unwrap();
    assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, na, r) && connected(s, nb, r)));
    sim.run_for(Duration::from_millis(100)); // let R's address reports land
    (sim, r, na, nb)
}

#[test]
fn port_restricted_nat_drops_unsolicited_and_expires_mappings() {
    let mut sim = Sim::new(NetConfig::default(), 1);
    let server = sim.add_node(Config::default());
    let nat = sim.add_nat(NatConfig::port_restricted(), None);
    let client = sim.add_node_behind(nat, Config::default());
    // The server can't reach in first, even at the NAT's public address.
    let (public, pc) = (SimAddr::new(sim.nat_ip(nat), nobaas_sim::HOST_PORT), sim.peer_id(client));
    let now = sim.now;
    sim.nodes[server].endpoint.connect(now, public, pc).unwrap();
    sim.run_for(Duration::from_secs(1));
    assert!(!connected(&sim, server, client));
    // Outbound works, and keepalives (10 s) hold the 30 s mapping open.
    sim.connect(client, server).unwrap();
    assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, client, server)));
    sim.run_for(Duration::from_secs(120));
    assert!(connected(&sim, client, server));
    let st = sim.nat_stats(nat);
    assert_eq!(st.mappings_created, 1, "{st:?}");
    assert!(st.inbound_filtered + st.inbound_no_mapping > 0, "the server's early dial was blocked");
}

#[test]
fn mapping_expires_without_keepalive() {
    let mut sim = Sim::new(NetConfig::default(), 2);
    let config = Config { keepalive: Duration::from_secs(40), idle_timeout: Duration::from_secs(120), ..Config::default() };
    let server = sim.add_node(config.clone());
    let nat = sim.add_nat(NatConfig::port_restricted(), None);
    let client = sim.add_node_behind(nat, config);
    sim.connect(client, server).unwrap();
    assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, client, server)));
    sim.run_for(Duration::from_secs(35)); // mapping (30 s) lapses before the 40 s keepalive
    sim.take_events(client);
    sim.send(server, client, Lane::Reliable, b"lost").unwrap();
    sim.run_for(Duration::from_secs(1));
    assert!(messages(&sim.take_events(client)).is_empty());
    assert!(sim.nat_stats(nat).inbound_no_mapping > 0);
}

#[test]
fn peers_learn_their_public_address() {
    let mut sim = Sim::new(NetConfig::default(), 3);
    let server = sim.add_node(Config::default());
    let nat = sim.add_nat(NatConfig::port_restricted(), None);
    let client = sim.add_node_behind(nat, Config::default());
    sim.connect(client, server).unwrap();
    sim.run_for(Duration::from_secs(1));
    let seen: Vec<_> = sim
        .take_events(client)
        .into_iter()
        .filter_map(|e| match e {
            Event::AddrObserved { addr, .. } => Some(addr),
            _ => None,
        })
        .collect();
    assert_eq!(seen, vec![SimAddr::new(sim.nat_ip(nat), nobaas_sim::HOST_PORT)], "port preserved by the NAT");
}

#[test]
fn relayed_dial_then_punch_goes_direct() {
    let pr = Kind::Nat("pr", NatConfig::port_restricted());
    let (mut sim, r, a, b) = trio(&pr, &pr, Config::default(), 4);
    let t0 = sim.now;
    sim.connect_via(a, r, b, &[]).unwrap();
    assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, a, b)));
    let t_conn = sim.now - t0;
    // Data flows straight away, over the relay or direct.
    sim.send(a, b, Lane::Reliable, b"hello").unwrap();
    assert!(sim.run_until(Duration::from_secs(5), |s| is_direct(s, a, b)));
    let t_direct = sim.now - t0;
    sim.run_for(Duration::from_millis(200));
    assert_eq!(messages(&sim.take_events(b)), vec![b"hello".to_vec()]);
    let relayed_before = sim.nodes[r].endpoint.stats().relayed;
    for k in 0..100u32 {
        sim.send(a, b, Lane::Reliable, &k.to_le_bytes()).unwrap();
        sim.send(b, a, Lane::Unreliable, &k.to_le_bytes()).unwrap();
    }
    sim.run_for(Duration::from_secs(1));
    assert_eq!(messages(&sim.take_events(b)).len(), 100);
    assert_eq!(sim.nodes[r].endpoint.stats().relayed, relayed_before, "no traffic through the relay once direct");
    println!("relayed connect {t_conn:?}, direct after {t_direct:?}, relay carried {relayed_before} packets");
}

/// NB-034: a relay-only endpoint's relayed links stay relayed, whichever side
/// dials, though a direct path would work: no packet ever goes between the two
/// peers' addresses, and the relay doesn't tag its packets with its address.
#[test]
fn relay_only_links_stay_relayed() {
    use std::cell::RefCell;
    use std::rc::Rc;
    for a_dials in [true, false] {
        let mut sim = Sim::new(NetConfig::default(), 11);
        let r = sim.add_node(Config::default());
        let a = sim.add_node(Config::default());
        let b = sim.add_node(Config::default());
        sim.nodes[a].endpoint.set_relay_only(true);
        sim.connect(a, r).unwrap();
        sim.connect(b, r).unwrap();
        assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, a, r) && connected(s, b, r)));
        let (aa, ba) = (sim.addr(a), sim.addr(b));
        let direct = Rc::new(RefCell::new(0));
        let seen = direct.clone();
        sim.hook = Some(Box::new(move |from, to, _| {
            if (from == aa && to == ba) || (from == ba && to == aa) {
                *seen.borrow_mut() += 1;
            }
            true
        }));
        let (x, y) = if a_dials { (a, b) } else { (b, a) };
        // The dialler also offers A's address; a relay-only A still doesn't punch.
        sim.connect_via(x, r, y, &[sim.addr(y)]).unwrap();
        assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, a, b)));
        for k in 0..50u32 {
            sim.send(a, b, Lane::Reliable, &k.to_le_bytes()).unwrap();
            sim.send(b, a, Lane::Reliable, &k.to_le_bytes()).unwrap();
        }
        sim.run_for(Duration::from_secs(20));
        assert!(connected(&sim, a, b));
        assert_eq!(sim.path(a, b), Some(Path::Via(sim.peer_id(r))), "a_dials {a_dials}");
        assert_eq!(sim.path(b, a), Some(Path::Via(sim.peer_id(r))), "a_dials {a_dials}");
        assert_eq!(messages(&sim.take_events(b)).len(), 50);
        let (sa, sb) = (sim.nodes[a].endpoint.stats(), sim.nodes[b].endpoint.stats());
        assert_eq!((sa.punches_sent, sa.punch_succeeded, sb.punch_succeeded), (0, 0, 0));
        if a_dials {
            // B learned no candidate for A: from the relay's tag or anywhere else.
            assert_eq!(sb.punches_sent, 0);
            assert_eq!(*direct.borrow(), 0, "no packet between A and B directly");
        } else {
            // B dialled A's address itself (it had it); A never answered there.
            assert!(sim.nodes[a].endpoint.stats().tx_packets > 0);
            assert_eq!(sim.path(a, b), Some(Path::Via(sim.peer_id(r))));
        }
    }
}

#[test]
fn relay_refuses_when_disabled() {
    let mut sim = Sim::new(NetConfig::default(), 5);
    let r = sim.add_node(Config { relay: false, ..Config::default() });
    let a = sim.add_node(Config::default());
    let b = sim.add_node(Config::default());
    sim.connect(a, r).unwrap();
    sim.connect(b, r).unwrap();
    sim.run_for(Duration::from_secs(1));
    sim.connect_via(a, r, b, &[]).unwrap();
    sim.run_for(Duration::from_secs(30));
    assert!(!connected(&sim, a, b));
    assert!(sim.take_events(a).iter().any(|e| matches!(e, Event::ConnectFailed { .. })));
    assert!(sim.nodes[r].endpoint.stats().relay_dropped > 0);
}

#[test]
fn symmetric_pair_stays_relayed_and_works() {
    let sym = Kind::Nat("sym", NatConfig::symmetric_random());
    let (mut sim, r, a, b) = trio(&sym, &sym, Config::default(), 6);
    sim.connect_via(a, r, b, &[]).unwrap();
    assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, a, b)));
    sim.run_for(Duration::from_secs(6)); // punching gives up after 5 s
    assert!(matches!(sim.path(a, b), Some(Path::Via(_))));
    assert!(sim.nodes[a].endpoint.stats().punch_failed >= 1);
    sim.take_events(b);
    let big = vec![7u8; 50_000]; // multi-fragment, sized for the relayed budget
    sim.send(a, b, Lane::Reliable, &big).unwrap();
    for k in 0..200u32 {
        sim.send(a, b, Lane::Unreliable, &k.to_le_bytes()).unwrap();
    }
    sim.run_for(Duration::from_secs(2));
    let got = messages(&sim.take_events(b));
    assert_eq!(got.len(), 201);
    assert!(got.contains(&big));
}

#[test]
fn direct_path_failure_falls_back_to_relay() {
    let pr = Kind::Nat("pr", NatConfig::port_restricted());
    let config = Config { keepalive: Duration::from_secs(2), direct_timeout: Duration::from_secs(5), ..Config::default() };
    let (mut sim, r, a, b) = trio(&pr, &pr, config, 7);
    sim.connect_via(a, r, b, &[]).unwrap();
    assert!(sim.run_until(Duration::from_secs(5), |s| connected(s, a, b) && is_direct(s, a, b)));
    // Cut only the direct A<->B path; both can still reach R.
    let rip = sim.addr(r).ip;
    sim.hook = Some(Box::new(move |from, to, _| from.ip == rip || to.ip == rip));
    sim.take_events(a);
    sim.take_events(b);
    assert!(sim.run_until(Duration::from_secs(10), |s| matches!(s.path(a, b), Some(Path::Via(_)))
        && matches!(s.path(b, a), Some(Path::Via(_)))));
    sim.send(a, b, Lane::Reliable, b"via relay").unwrap();
    sim.run_for(Duration::from_secs(1));
    assert!(connected(&sim, a, b));
    assert_eq!(messages(&sim.take_events(b)), vec![b"via relay".to_vec()]);
    assert!(sim.nodes[a].endpoint.stats().direct_fallbacks >= 1);
}

#[test]
fn signalled_simultaneous_open_punches_without_a_relay() {
    // Both peers learned their public address (from R) and swapped them out of
    // band (an invite, a Nostr note). Each dials the other at the same time.
    let pr = Kind::Nat("pr", NatConfig::port_restricted());
    let (mut sim, r, a, b) = trio(&pr, &pr, Config::default(), 8);
    let public = |sim: &mut Sim, i: usize| {
        sim.take_events(i)
            .into_iter()
            .find_map(|e| match e {
                Event::AddrObserved { addr, .. } => Some(addr),
                _ => None,
            })
            .unwrap()
    };
    let (pa, pb) = (public(&mut sim, a), public(&mut sim, b));
    let (ida, idb) = (sim.peer_id(a), sim.peer_id(b));
    sim.nodes[r].endpoint.disconnect(&ida);
    sim.nodes[r].endpoint.disconnect(&idb);
    sim.connect_with(a, b, &[Path::Direct(pb)]).unwrap();
    sim.connect_with(b, a, &[Path::Direct(pa)]).unwrap();
    assert!(sim.run_until(Duration::from_secs(5), |s| connected(s, a, b)));
    assert!(is_direct(&sim, a, b));
    assert_eq!(sim.nodes[r].endpoint.stats().relayed, 0);
}

#[test]
fn same_nat_peers_use_lan_candidates() {
    // Two peers behind one NAT without hairpinning: public candidates fail,
    // a LAN candidate shared by the app works.
    let mut sim = Sim::new(NetConfig::default(), 9);
    let r = sim.add_node(Config::default());
    let nat = sim.add_nat(NatConfig::port_restricted(), None);
    let a = sim.add_node_behind(nat, Config::default());
    let b = sim.add_node_behind(nat, Config::default());
    sim.connect(a, r).unwrap();
    sim.connect(b, r).unwrap();
    sim.run_for(Duration::from_secs(1));
    sim.connect_via(a, r, b, &[]).unwrap();
    assert!(sim.run_until(Duration::from_secs(2), |s| connected(s, a, b)));
    sim.run_for(Duration::from_secs(6));
    assert!(matches!(sim.path(a, b), Some(Path::Via(_))), "no hairpin: stays relayed");
    assert!(sim.nat_stats(nat).hairpin_blocked > 0);
    let (lan_b, pb) = (sim.addr(b), sim.peer_id(b));
    let now = sim.now;
    sim.nodes[a].endpoint.add_candidates(now, &pb, &[lan_b]);
    assert!(sim.run_until(Duration::from_secs(2), |s| is_direct(s, a, b)));
    assert_eq!(sim.path(a, b), Some(Path::Direct(lan_b)));
}

/// NB-035: a sequential symmetric NAT S maps its next new destination to the
/// port after its last one. Its cone peer C, behind a port-restricted NAT, can't
/// punch to the port S's relay saw, but can to guesses around the next one, while
/// relayed and while dialling. A relay-only C ignores guesses.
#[test]
fn guessed_ports_punch_a_sequential_symmetric_nat() {
    let sym = Kind::Nat("sym-seq", NatConfig::symmetric_sequential());
    let pr = Kind::Nat("pr", NatConfig::port_restricted());
    // S's NAT (nat 0) gave R port 20000; the next mapping is 20001.
    let guesses = |sim: &Sim| (0..8).map(|k| SimAddr::new(sim.nat_ip(0), 20_000 + k)).collect::<Vec<_>>();
    for (guess, relay_only) in [(false, false), (true, false), (true, true)] {
        let (mut sim, r, s, c) = trio(&sym, &pr, Config::default(), 12);
        sim.nodes[c].endpoint.set_relay_only(relay_only);
        sim.connect_via(c, r, s, &[]).unwrap();
        assert!(sim.run_until(Duration::from_secs(2), |sim| connected(sim, c, s)));
        if guess {
            let (now, ps, g) = (sim.now, sim.peer_id(s), guesses(&sim));
            sim.nodes[c].endpoint.set_guesses(now, &ps, &g);
        }
        sim.run_until(Duration::from_secs(6), |sim| is_direct(sim, c, s));
        let direct = is_direct(&sim, c, s);
        assert_eq!(direct, guess && !relay_only, "guess {guess}, relay-only {relay_only}");
        if direct {
            assert_eq!(sim.path(c, s), Some(Path::Direct(SimAddr::new(sim.nat_ip(0), 20_001))));
            // Guesses go out every fifth punch round only.
            let sent = sim.nodes[c].endpoint.stats().punches_sent;
            assert!(sent < 8 * 3, "{sent} punches");
        }
    }
    // No relay: S dials C's public address while C dials S's guesses.
    let (mut sim, r, s, c) = trio(&sym, &pr, Config::default(), 13);
    let (ps, pc) = (sim.peer_id(s), sim.peer_id(c));
    let c_public = SimAddr::new(sim.nat_ip(1), nobaas_sim::HOST_PORT);
    sim.nodes[r].endpoint.disconnect(&ps);
    sim.nodes[r].endpoint.disconnect(&pc);
    let (now, g) = (sim.now, guesses(&sim));
    sim.nodes[s].endpoint.connect(now, c_public, pc).unwrap();
    sim.nodes[c].endpoint.connect(now, g[0], ps).unwrap();
    sim.nodes[c].endpoint.set_guesses(now, &ps, &g);
    assert!(sim.run_until(Duration::from_secs(5), |sim| connected(sim, s, c)));
    assert!(is_direct(&sim, s, c));
    assert_eq!(sim.path(c, s), Some(Path::Direct(SimAddr::new(sim.nat_ip(0), 20_001))));
}

#[derive(Debug)]
struct Outcome {
    connected: bool,
    direct: bool,
    t_connect: Duration,
    t_direct: Option<Duration>,
}

fn run_pair(a: &Kind, b: &Kind, spray: u16, seed: u64) -> Outcome {
    let config = Config { punch_spray: spray, ..Config::default() };
    let (mut sim, r, na, nb) = trio(a, b, config, seed);
    let t0 = sim.now;
    sim.connect_via(na, r, nb, &[]).unwrap();
    let ok = sim.run_until(Duration::from_secs(3), |s| connected(s, na, nb));
    let t_connect = sim.now - t0;
    let direct = ok && sim.run_until(Duration::from_secs(6), |s| is_direct(s, na, nb));
    let t_direct = direct.then(|| sim.now - t0);
    // Whatever the path, the link must carry data both ways.
    if ok {
        sim.take_events(na);
        sim.take_events(nb);
        sim.send(na, nb, Lane::Reliable, b"ping").unwrap();
        sim.send(nb, na, Lane::Reliable, b"pong").unwrap();
        sim.run_for(Duration::from_secs(1));
        assert_eq!(messages(&sim.take_events(nb)), vec![b"ping".to_vec()], "{} -> {}", a.name(), b.name());
        assert_eq!(messages(&sim.take_events(na)), vec![b"pong".to_vec()], "{} <- {}", a.name(), b.name());
    }
    Outcome { connected: ok, direct, t_connect, t_direct }
}

#[test]
fn nat_matrix() {
    let ks = kinds();
    for spray in [0u16, 8] {
        println!("\npunch_spray = {spray}: D = direct (ms to direct), R = stays relayed");
        print!("{:>14}", "A \\ B");
        for b in &ks {
            print!("{:>14}", b.name());
        }
        println!();
        let mut direct = 0;
        for (i, a) in ks.iter().enumerate() {
            print!("{:>14}", a.name());
            for (j, b) in ks.iter().enumerate() {
                let o = run_pair(a, b, spray, 100 + (i * 16 + j) as u64);
                assert!(o.connected, "{} x {}: {o:?}", a.name(), b.name());
                assert!(o.t_connect < Duration::from_millis(200), "{o:?}");
                let cell = match o.t_direct {
                    Some(t) => format!("D {}", t.as_millis()),
                    None => "R".to_string(),
                };
                direct += o.direct as u32;
                print!("{cell:>14}");
            }
            println!();
        }
        println!("direct: {direct}/{}", ks.len() * ks.len());
    }
    // Cone NATs on both sides must always go direct.
    for a in ["full-cone", "restricted", "port-restr", "cgnat(pr+pr)"] {
        for b in ["public", "full-cone", "restricted", "port-restr", "cgnat(pr+pr)"] {
            let (ka, kb) = (
                ks.iter().find(|k| k.name() == a).unwrap(),
                ks.iter().find(|k| k.name() == b).unwrap(),
            );
            assert!(run_pair(ka, kb, 0, 7).direct, "{a} x {b} should go direct");
        }
    }
}
