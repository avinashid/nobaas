//! The `peerwire` NAT traversal (STUN, rooms, sealed offers, simultaneous dial,
//! relay fallback) driven over simulated NATs, exactly as `Node` drives it over UDP.
//! Signaling is a deterministic in-memory broker with relay-like latency; no
//! invite or address is exchanged out of band.

use peerwire::traversal::{Failure, NatType, Privacy, Role, Side, Signal, Traversal};
use peerwire::Topic;
use peerwire_core::{Config, Event, Lane, Path, PeerId};
use peerwire_sim::{NatConfig, NetConfig, Sim, SimAddr};
use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

/// Unix ms at virtual time zero: every peer's wall clock agrees.
const BASE_MS: u64 = 1_800_000_000_000;
const TICK: Duration = Duration::from_millis(10);

#[derive(Clone, Debug)]
enum Kind {
    Public,
    Nat(&'static str, NatConfig),
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

fn kind(name: &str) -> Kind {
    kinds().into_iter().find(|k| k.name() == name).unwrap()
}

struct Peer {
    node: usize,
    trav: Traversal<SimAddr>,
    topics: BTreeSet<Topic>,
    events: Vec<Event<SimAddr>>,
}

/// A simulated internet with two STUN servers and a signaling broker.
struct World {
    sim: Sim,
    stun: Vec<SimAddr>,
    peers: Vec<Peer>,
    /// (deliver at, peer, topic, payload)
    inflight: VecDeque<(peerwire_core::Instant, usize, Topic, Vec<u8>)>,
    signal_delay: Duration,
}

impl World {
    fn new(seed: u64) -> World {
        let mut sim = Sim::new(NetConfig::default(), seed);
        let stun = vec![sim.add_stun_server(), sim.add_stun_server()];
        World { sim, stun, peers: Vec::new(), inflight: VecDeque::new(), signal_delay: Duration::from_millis(150) }
    }

    fn add(&mut self, k: &Kind) -> usize {
        let node = self.node(k);
        self.attach(node)
    }

    /// A peer behind `k` on IPv4 with a firewalled global IPv6 address too (a phone).
    fn add6(&mut self, k: &Kind) -> usize {
        let node = self.node(k);
        self.sim.add_ipv6(node);
        self.attach(node)
    }

    fn node(&mut self, k: &Kind) -> usize {
        let config = Config::default();
        match k {
            Kind::Public => self.sim.add_node(config),
            Kind::Nat(_, n) => {
                let nat = self.sim.add_nat(n.clone(), None);
                self.sim.add_node_behind(nat, config)
            }
            Kind::Cgnat(_, home, carrier) => {
                let c = self.sim.add_nat(carrier.clone(), None);
                let h = self.sim.add_nat(home.clone(), Some(c));
                self.sim.add_node_behind(h, config)
            }
        }
    }

    /// Another peer behind the NAT that `p` is behind (a home LAN).
    fn add_beside(&mut self, nat: usize) -> usize {
        let node = self.sim.add_node_behind(nat, Config::default());
        self.attach(node)
    }

    fn attach(&mut self, node: usize) -> usize {
        let identity = self.sim.nodes[node].identity.clone();
        let seed = [node as u8 + 1; 32];
        let local = [Some(self.sim.addr(node)), self.sim.nodes[node].addr6].into_iter().flatten().collect();
        let trav = Traversal::new(&identity, seed, BASE_MS, local, self.stun.clone());
        self.peers.push(Peer { node, trav, topics: BTreeSet::new(), events: Vec::new() });
        self.peers.len() - 1
    }

    fn id(&self, p: usize) -> PeerId {
        self.sim.peer_id(self.peers[p].node)
    }

    fn set_privacy(&mut self, p: usize, privacy: Privacy) {
        let ep = &mut self.sim.nodes[self.peers[p].node].endpoint;
        self.peers[p].trav.set_privacy(ep, privacy);
    }

    fn join(&mut self, p: usize, room: &str, role: Role) {
        let now = self.sim.now;
        self.peers[p].trav.join_room(now, "pw-sim", room, role, None);
    }

    /// One host-loop iteration per peer, as `Node::poll` does, then `TICK` of network.
    fn tick(&mut self) {
        let now = self.sim.now;
        while self.inflight.front().is_some_and(|m| m.0 <= now) {
            let (_, to, topic, payload) = self.inflight.pop_front().unwrap();
            let node = self.peers[to].node;
            let ep = &mut self.sim.nodes[node].endpoint;
            self.peers[to].trav.handle_signal(now, ep, topic, &payload);
        }
        for p in 0..self.peers.len() {
            let node = self.peers[p].node;
            let events = self.sim.take_events(node);
            let n = &mut self.sim.nodes[node];
            let peer = &mut self.peers[p];
            for (from, data) in std::mem::take(&mut n.stun_inbox) {
                assert!(peer.trav.handle_stun(&from, &data));
            }
            if peer.trav.poll_timeout().is_some_and(|t| t <= now) {
                peer.trav.handle_timeout(now, &mut n.endpoint);
            }
            for e in events {
                peer.trav.handle_event(now, &mut n.endpoint, &e);
                peer.events.push(e);
            }
            let stun: Vec<_> = std::iter::from_fn(|| peer.trav.poll_transmit()).collect();
            let signals: Vec<_> = std::iter::from_fn(|| peer.trav.poll_signal()).collect();
            let birthdays: Vec<_> = std::iter::from_fn(|| peer.trav.poll_birthday()).collect();
            for t in stun {
                self.sim.send_raw(node, t.dst, t.payload);
            }
            for b in birthdays {
                for port in self.sim.open_ports(node, b.sockets, b.until) {
                    self.sim.send_from(node, port, b.dst, b.payload.clone());
                }
            }
            for s in signals {
                self.signal(p, s);
            }
        }
        self.sim.run_for(TICK);
    }

    fn signal(&mut self, from: usize, s: Signal) {
        match s {
            Signal::Subscribe(t) => {
                self.peers[from].topics.insert(t);
            }
            Signal::Unsubscribe(t) => {
                self.peers[from].topics.remove(&t);
            }
            Signal::Publish(t, payload) => {
                let at = self.sim.now + self.signal_delay;
                for (to, p) in self.peers.iter().enumerate() {
                    if to != from && p.topics.contains(&t) {
                        self.inflight.push_back((at, to, t, payload.clone()));
                    }
                }
            }
        }
    }

    fn run_until(&mut self, limit: Duration, mut done: impl FnMut(&World) -> bool) -> bool {
        let end = self.sim.now + limit;
        while self.sim.now < end {
            if done(self) {
                return true;
            }
            self.tick();
        }
        done(self)
    }

    fn connected(&self, a: usize, b: usize) -> bool {
        let (na, nb) = (self.peers[a].node, self.peers[b].node);
        self.sim.nodes[na].endpoint.is_connected(&self.id(b)) && self.sim.nodes[nb].endpoint.is_connected(&self.id(a))
    }

    fn path(&self, a: usize, b: usize) -> Option<Path<SimAddr>> {
        self.sim.path(self.peers[a].node, self.peers[b].node)
    }

    fn direct(&self, a: usize, b: usize) -> bool {
        matches!(self.path(a, b), Some(Path::Direct(_))) && matches!(self.path(b, a), Some(Path::Direct(_)))
    }

    fn failure(&self, a: usize, b: usize) -> Option<Failure> {
        self.peers[a].trav.failure(&self.id(b))
    }

    fn messages(&mut self, p: usize) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.peers[p].events)
            .into_iter()
            .filter_map(|e| match e {
                Event::Message { data, .. } => Some(data),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn stun_reports_the_nat_type() {
    let mut w = World::new(1);
    let names = ["public", "full-cone", "port-restr", "sym-seq", "sym-random", "cgnat(pr+pr)", "cgnat(pr+sym)"];
    let ps: Vec<usize> = names.iter().map(|n| w.add(&kind(n))).collect();
    assert!(w.run_until(Duration::from_secs(2), |w| ps.iter().all(|&p| w.peers[p].trav.nat_type() != NatType::Unknown)));
    let got: Vec<NatType> = ps.iter().map(|&p| w.peers[p].trav.nat_type()).collect();
    use NatType::*;
    assert_eq!(got, vec![Open, Cone, Cone, Symmetric, Symmetric, Cone, Symmetric]);
    // The public address is the NAT's outside one.
    let pr = ps[2];
    let public = *w.peers[pr].trav.public_addr().unwrap();
    assert_ne!(public, w.sim.addr(w.peers[pr].node));
    assert_eq!(w.peers[pr].trav.candidates()[0], public);
}

#[derive(Debug)]
struct Outcome {
    direct: bool,
    connected: bool,
    t_connect: Duration,
    failures: (Option<Failure>, Option<Failure>),
}

/// Two peers join a room; nothing else is shared. Runs until they connect or both give up.
fn room_pair(a: &Kind, b: &Kind, seed: u64) -> Outcome {
    room_pair_with(a, b, seed, (0, 0))
}

/// [`room_pair`], with `used.0` and `used.1` more ports used up on A's and B's NATs
/// after STUN (sent to addresses nobody holds), so their predictions are that far off.
fn room_pair_with(a: &Kind, b: &Kind, seed: u64, used: (u32, u32)) -> Outcome {
    let mut w = World::new(seed);
    let (pa, pb) = (w.add(a), w.add(b));
    w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
    for (p, n) in [(pa, used.0), (pb, used.1)] {
        for k in 0..n {
            w.sim.send_raw(w.peers[p].node, SimAddr::new(0x0c00_0000 + 0x100 * p as u32 + k, 9), vec![0]);
        }
    }
    let t0 = w.sim.now;
    w.join(pa, "pair", Role::Peer);
    w.join(pb, "pair", Role::Peer);
    let connected = w.run_until(Duration::from_secs(45), |w| {
        w.connected(pa, pb) || (w.failure(pa, pb).is_some() && w.failure(pb, pa).is_some())
    }) && w.connected(pa, pb);
    let t_connect = w.sim.now - t0;
    let direct = connected && w.run_until(Duration::from_secs(6), |w| w.direct(pa, pb));
    if connected {
        let (na, nb) = (w.peers[pa].node, w.peers[pb].node);
        w.sim.send(na, nb, Lane::Reliable, b"ping").unwrap();
        w.run_until(Duration::from_secs(1), |w| w.peers[pb].events.iter().any(|e| matches!(e, Event::Message { .. })));
        assert_eq!(w.messages(pb), vec![b"ping".to_vec()], "{} -> {}", a.name(), b.name());
    }
    Outcome { direct, connected, t_connect, failures: (w.failure(pa, pb), w.failure(pb, pa)) }
}

#[test]
fn nat_matrix_through_signaling() {
    let ks = kinds();
    println!("\nroom of two, signaling only: D = direct (ms to connect), F = failed (U: unreachable;");
    println!("S: symmetric NAT on A's side (S<), B's (S^) or both (S2), as A reports it)");
    print!("{:>14}", "A \\ B");
    for b in &ks {
        print!("{:>14}", b.name());
    }
    println!();
    let mut direct = 0;
    for (i, a) in ks.iter().enumerate() {
        print!("{:>14}", a.name());
        for (j, b) in ks.iter().enumerate() {
            let o = room_pair(a, b, 200 + (i * 16 + j) as u64);
            let cell = match (o.connected, o.failures) {
                (true, _) => format!("{} {}", if o.direct { "D" } else { "R?" }, o.t_connect.as_millis()),
                (false, (Some(Failure::Symmetric(side)), _)) => {
                    format!(
                        "F S{}",
                        match side {
                            Side::Ours => "<",
                            Side::Theirs => "^",
                            Side::Both => "2",
                        }
                    )
                }
                (false, _) => "F U".to_string(),
            };
            print!("{cell:>14}");
            direct += o.direct as u32;
            if !o.connected {
                // Only symmetric NATs stop two peers, both sides say so, and they
                // agree on which side is symmetric: the one whose NAT is.
                let sym = |k: &Kind| k.name().contains("sym");
                let (sa, sb) = match (sym(a), sym(b)) {
                    (true, true) => (Side::Both, Side::Both),
                    (true, false) => (Side::Ours, Side::Theirs),
                    (false, true) => (Side::Theirs, Side::Ours),
                    (false, false) => panic!("{} x {} failed without a symmetric NAT: {o:?}", a.name(), b.name()),
                };
                let want = (Some(Failure::Symmetric(sa)), Some(Failure::Symmetric(sb)));
                assert_eq!(o.failures, want, "{} x {}: {o:?}", a.name(), b.name());
            } else {
                assert!(o.direct, "two peers alone can only be direct: {} x {}: {o:?}", a.name(), b.name());
            }
        }
        println!();
    }
    println!("direct: {direct}/{}", ks.len() * ks.len());
    // Cone NATs (and a public peer) on both sides always punch.
    let cone = ["public", "full-cone", "restricted", "port-restr", "cgnat(pr+pr)"];
    for a in cone {
        for b in cone {
            let o = room_pair(&kind(a), &kind(b), 7);
            assert!(o.direct, "{a} x {b}: {o:?}");
            assert!(o.t_connect < Duration::from_secs(3), "{a} x {b}: {o:?}");
        }
    }
    // A symmetric NAT can still reach a public peer.
    for s in ["sym-seq", "sym-random", "cgnat(pr+sym)"] {
        assert!(room_pair(&kind(s), &Kind::Public, 8).direct, "{s} x public");
    }
    // Two random-port symmetric NATs can't punch, and say why.
    let o = room_pair(&kind("sym-random"), &kind("sym-random"), 9);
    assert!(!o.connected);
    let both = Some(Failure::Symmetric(Side::Both));
    assert_eq!(o.failures, (both, both));
    // Against a port-restricted cone, a birthday punch links them (NB-044).
    let o = room_pair(&kind("sym-random"), &kind("port-restr"), 9);
    assert!(o.direct, "{o:?}");
}

/// NB-044: a random-port symmetric NAT against a cone that filters by port links
/// directly by a birthday punch: the symmetric side opens 256 sockets towards the
/// cone, the cone sprays 1024 ports of the symmetric side's IP. Theoretical odds per
/// try 1 - (1 - 256/64512)^1024, about 98%.
#[test]
fn birthday_punch_links_random_symmetric_and_port_restricted() {
    let pairs = [("sym-random", "port-restr"), ("sym-random", "cgnat(pr+pr)")];
    let (mut direct, mut tries, mut times) = (0, 0, Vec::new());
    for (a, b) in pairs {
        for seed in 0..25 {
            for (x, y) in [(a, b), (b, a)] {
                let o = room_pair(&kind(x), &kind(y), 1300 + seed);
                tries += 1;
                if o.direct {
                    direct += 1;
                    times.push(o.t_connect);
                } else {
                    println!("{x} x {y}, seed {seed}: {o:?}");
                }
            }
        }
    }
    times.sort();
    println!(
        "birthday: {direct}/{tries} direct, connect median {:?}, max {:?}",
        times[times.len() / 2],
        times[times.len() - 1]
    );
    assert!(direct * 100 >= tries * 90, "{direct}/{tries}");
}

/// NB-035: a sequential symmetric NAT predicts its next port from its two STUN
/// mappings and offers it; a cone peer probes the ports around it, so the pair
/// punches though the cone side filters by port. Either side may dial first.
#[test]
fn sequential_symmetric_nats_punch_port_restricted_peers() {
    let pairs = [
        ("sym-seq", "port-restr"),
        ("sym-seq", "cgnat(pr+pr)"),
        ("cgnat(pr+sym)", "port-restr"),
        ("cgnat(pr+sym)", "full-cone"),
    ];
    let mut times = Vec::new();
    for (a, b) in pairs {
        for seed in 0..10 {
            for (x, y) in [(a, b), (b, a)] {
                let o = room_pair(&kind(x), &kind(y), 900 + seed);
                assert!(o.direct, "{x} x {y}, seed {seed}: {o:?}");
                times.push(o.t_connect);
            }
        }
    }
    times.sort();
    println!(
        "{} pairs direct, connect median {:?}, max {:?}",
        times.len(),
        times[times.len() / 2],
        times[times.len() - 1]
    );
    // Random ports can't be predicted: no prediction, so no probing.
    let mut w = World::new(3);
    let (r, s) = (w.add(&kind("sym-random")), w.add(&kind("sym-seq")));
    w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
    assert_eq!(w.peers[r].trav.predicted_addr(), None);
    let next = w.peers[s].trav.predicted_addr().expect("sequential");
    assert_eq!(next.port, w.peers[s].trav.public_addr().unwrap().port + 2);
}

#[test]
fn symmetric_pair_relays_through_a_third_room_peer() {
    let mut w = World::new(10);
    let (a, b) = (w.add(&kind("sym-random")), w.add(&kind("sym-random")));
    let r = w.add(&kind("full-cone"));
    w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
    for p in [a, b, r] {
        w.join(p, "trio", Role::Peer);
    }
    assert!(w.run_until(Duration::from_secs(30), |w| w.connected(a, b)), "a-b {:?}", w.failure(a, b));
    assert!(w.connected(a, r) && w.connected(b, r));
    let rid = w.id(r);
    assert_eq!(w.path(a, b), Some(Path::Via(rid)), "relayed through the cone peer");
    // The link carries data both ways, over the relay.
    w.peers[b].events.clear();
    let (na, nb) = (w.peers[a].node, w.peers[b].node);
    let big = vec![5u8; 20_000];
    w.sim.send(na, nb, Lane::Reliable, &big).unwrap();
    w.sim.send(nb, na, Lane::Reliable, b"back").unwrap();
    w.run_until(Duration::from_secs(3), |_| false);
    assert_eq!(w.messages(b), vec![big]);
    assert_eq!(w.messages(a), vec![b"back".to_vec()]);
    assert!(w.sim.nodes[w.peers[r].node].endpoint.stats().relayed > 0);
}

/// NB-041: two random-port symmetric NATs, alone in a room, can't link; with an
/// always-on relay peer outside the room (as `peerwire-peer` runs) that both
/// keep a link to (as `Node::set_relay_peers` does), they link through it, and
/// game traffic (1 KB unreliable every 10 ms, each way) fits its relay rate.
#[test]
fn symmetric_pair_links_through_a_relay_peer_outside_the_room() {
    symmetric_pair_through_a_relay_peer();
}

/// NB-045: two sequential symmetric NATs (alone, or a home NAT behind a sequential
/// carrier NAT) link directly by sweeping each other's predicted ports in lockstep,
/// also when each NAT handed out ports since its prediction. The lower PeerId's
/// probe `dl + 2·dh` meets the other's `dl + dh`, so the sweeps (96 and 64 probes)
/// cover `dl + 2·dh < 96`, where each `d` also counts the dial's own new destinations.
#[test]
fn lockstep_prediction_links_sequential_symmetric_nats() {
    let pairs = [("sym-seq", "sym-seq"), ("sym-seq", "cgnat(pr+sym)"), ("cgnat(pr+sym)", "cgnat(pr+sym)")];
    let (mut direct, mut tries, mut times) = (0, 0, Vec::new());
    for (a, b) in pairs {
        for seed in 0..10 {
            for (x, y) in [(a, b), (b, a)] {
                let o = room_pair(&kind(x), &kind(y), 1500 + seed);
                tries += 1;
                direct += o.direct as u32;
                times.push(o.t_connect);
                assert!(o.direct, "{x} x {y}, seed {seed}: {o:?}");
            }
        }
    }
    times.sort();
    println!("lockstep: {direct}/{tries} direct, connect median {:?}, max {:?}", times[tries / 2], times[tries - 1]);
    // Ports used up on both NATs since the predictions: linked within the sweeps' reach.
    print!("ports used up (A, B) -> direct:");
    for used in [(1, 1), (5, 0), (0, 5), (10, 10), (20, 5), (5, 20), (28, 28)] {
        let ok = (0..4).filter(|&seed| room_pair_with(&kind("sym-seq"), &kind("sym-seq"), 1600 + seed, used).direct);
        let ok = ok.count();
        print!(" {used:?} {ok}/4");
        assert_eq!(ok, 4, "{used:?}");
    }
    println!();
    // Far beyond reach, it fails as before.
    assert!(!room_pair_with(&kind("sym-seq"), &kind("sym-seq"), 1700, (60, 60)).connected);
}

/// NB-045: two sequential symmetric NATs linked through a relay peer move to a
/// direct path by the lockstep sweep (punches over the relayed link).
#[test]
fn lockstep_prediction_moves_a_relayed_link_to_direct() {
    for seed in 0..5 {
        let mut w = World::new(1800 + seed);
        let (a, b) = (w.add(&kind("sym-seq")), w.add(&kind("cgnat(pr+sym)")));
        let r = w.add(&Kind::Public);
        w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
        let (rid, at) = (w.id(r), w.sim.addr(w.peers[r].node));
        for p in [a, b] {
            let now = w.sim.now;
            w.sim.nodes[w.peers[p].node].endpoint.connect_with(now, rid, &[Path::Direct(at)]).unwrap();
            w.peers[p].trav.set_relay_peers(vec![rid]);
        }
        assert!(w.run_until(Duration::from_secs(2), |w| w.connected(a, r) && w.connected(b, r)));
        w.join(a, "pair", Role::Peer);
        w.join(b, "pair", Role::Peer);
        assert!(w.run_until(Duration::from_secs(5), |w| w.connected(a, b)), "seed {seed}");
        assert_eq!(w.path(a, b), Some(Path::Via(rid)), "seed {seed}: relayed first");
        let t0 = w.sim.now;
        assert!(w.run_until(Duration::from_secs(10), |w| w.direct(a, b)), "seed {seed}: {:?}", w.path(a, b));
        println!("seed {seed}: direct {:?} after relayed", w.sim.now - t0);
    }
}

/// NB-044: a link that came up through a relay peer moves to a direct path by a
/// birthday punch (punches sprayed over the live link), and the symmetric side's
/// extra sockets close but the one carrying the link.
#[test]
fn birthday_punch_moves_a_relayed_link_to_direct() {
    for seed in 0..5 {
        let mut w = World::new(1400 + seed);
        let (a, b) = (w.add(&kind("sym-random")), w.add(&kind("port-restr")));
        let r = w.add(&Kind::Public);
        w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
        let (rid, at) = (w.id(r), w.sim.addr(w.peers[r].node));
        for p in [a, b] {
            let now = w.sim.now;
            w.sim.nodes[w.peers[p].node].endpoint.connect_with(now, rid, &[Path::Direct(at)]).unwrap();
            w.peers[p].trav.set_relay_peers(vec![rid]);
        }
        assert!(w.run_until(Duration::from_secs(2), |w| w.connected(a, r) && w.connected(b, r)));
        w.join(a, "pair", Role::Peer);
        w.join(b, "pair", Role::Peer);
        assert!(w.run_until(Duration::from_secs(5), |w| w.connected(a, b)), "seed {seed}");
        let t0 = w.sim.now;
        assert_eq!(w.path(a, b), Some(Path::Via(rid)), "seed {seed}: relayed first");
        assert!(w.run_until(Duration::from_secs(10), |w| w.direct(a, b)), "seed {seed}: {:?}", w.path(a, b));
        let (na, nb) = (w.peers[a].node, w.peers[b].node);
        let stats = w.sim.nodes[nb].endpoint.stats();
        println!("seed {seed}: direct {:?} after relayed, {} probes", w.sim.now - t0, stats.spray_probes);
        assert!(stats.spray_probes > 0 && stats.punch_succeeded >= 1);
        // The link keeps working over the extra socket, and the rest close.
        w.run_until(Duration::from_secs(12), |_| false);
        assert!(w.direct(a, b));
        assert_eq!(w.sim.open_port_count(na), (1, 1), "seed {seed}");
        w.sim.send(na, nb, Lane::Reliable, b"over the birthday socket").unwrap();
        w.run_until(Duration::from_secs(1), |w| w.peers[b].events.iter().any(|e| matches!(e, Event::Message { .. })));
        assert_eq!(w.messages(b), vec![b"over the birthday socket".to_vec()]);
    }
}

fn symmetric_pair_through_a_relay_peer() {
    for with_relay in [false, true] {
        let mut w = World::new(11);
        let (a, b) = (w.add(&kind("sym-random")), w.add(&kind("sym-random")));
        let r = w.add(&Kind::Public);
        w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
        let (rid, at) = (w.id(r), w.sim.addr(w.peers[r].node));
        if with_relay {
            for p in [a, b] {
                let now = w.sim.now;
                w.sim.nodes[w.peers[p].node].endpoint.connect_with(now, rid, &[Path::Direct(at)]).unwrap();
                w.peers[p].trav.set_relay_peers(vec![rid]);
            }
            assert!(w.run_until(Duration::from_secs(2), |w| w.connected(a, r) && w.connected(b, r)));
        }
        w.join(a, "pair", Role::Peer);
        w.join(b, "pair", Role::Peer);
        let done = w.run_until(Duration::from_secs(45), |w| {
            w.connected(a, b) || (w.failure(a, b).is_some() && w.failure(b, a).is_some())
        });
        assert!(done);
        if !with_relay {
            let both = Some(Failure::Symmetric(Side::Both));
            assert_eq!((w.failure(a, b), w.failure(b, a)), (both, both));
            continue;
        }
        assert!(w.connected(a, b), "a-b {:?}", w.failure(a, b));
        assert_eq!((w.path(a, b), w.path(b, a)), (Some(Path::Via(rid)), Some(Path::Via(w.id(r)))));
        let (na, nb) = (w.peers[a].node, w.peers[b].node);
        w.peers[a].events.clear();
        w.peers[b].events.clear();
        for _ in 0..500 {
            w.sim.send(na, nb, Lane::Unreliable, &[1u8; 1000]).unwrap();
            w.sim.send(nb, na, Lane::Unreliable, &[2u8; 1000]).unwrap();
            w.tick();
        }
        w.run_until(Duration::from_millis(500), |_| false);
        let (got_b, got_a) = (w.messages(b).len(), w.messages(a).len());
        let relay = w.sim.nodes[w.peers[r].node].endpoint.stats();
        println!(
            "relayed 5 s of 100 KB/s each way: {got_b} and {got_a} of 500 delivered, {} relay drops",
            relay.relay_dropped
        );
        assert!(got_b >= 490 && got_a >= 490, "{got_b} {got_a}");
        assert_eq!(relay.relay_dropped, 0);
    }
}

/// NB-042: peers behind symmetric IPv4 NATs, which can't punch, link directly
/// over IPv6 when both have it (through stateful firewalls, as on mobile
/// carriers); with IPv6 on one side only, nothing changes: they fail as before.
#[test]
fn symmetric_ipv4_pair_links_directly_over_ipv6() {
    for (six_a, six_b) in [(true, true), (true, false)] {
        let mut w = World::new(12);
        let add = |w: &mut World, six: bool| if six { w.add6(&kind("sym-random")) } else { w.add(&kind("sym-random")) };
        let (a, b) = (add(&mut w, six_a), add(&mut w, six_b));
        w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
        assert_eq!(w.peers[a].trav.nat_type(), NatType::Symmetric, "STUN still sees the IPv4 NAT");
        w.join(a, "pair", Role::Peer);
        w.join(b, "pair", Role::Peer);
        let done = w.run_until(Duration::from_secs(45), |w| {
            w.direct(a, b) || (w.failure(a, b).is_some() && w.failure(b, a).is_some())
        });
        assert!(done);
        if !six_b {
            let both = Some(Failure::Symmetric(Side::Both));
            assert_eq!((w.failure(a, b), w.failure(b, a)), (both, both));
            continue;
        }
        let at = |p: usize| w.sim.nodes[w.peers[p].node].addr6;
        assert_eq!((w.path(a, b), w.path(b, a)), (at(b).map(Path::Direct), at(a).map(Path::Direct)));
        let dropped = |p: usize| w.sim.nodes[w.peers[p].node].firewall_dropped;
        assert!(dropped(a) + dropped(b) > 0, "a firewall turned away the first handshake: the dial punched it");
        let (na, nb) = (w.peers[a].node, w.peers[b].node);
        w.sim.send(na, nb, Lane::Reliable, b"over v6").unwrap();
        let got = |w: &World| {
            w.peers[b].events.iter().any(|e| matches!(e, Event::Message { data, .. } if data == b"over v6"))
        };
        assert!(w.run_until(Duration::from_secs(1), got));
    }
}

/// Where IPv4 punches too, both families are dialled at once and the link takes
/// whichever answers first; a peer's IPv6 is offered next to its IPv4.
#[test]
fn dual_stack_offers_both_families_and_links_on_the_first_answer() {
    let mut w = World::new(13);
    let (a, b) = (w.add6(&kind("port-restr")), w.add6(&kind("full-cone")));
    w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
    let a6 = w.sim.nodes[w.peers[a].node].addr6.unwrap();
    let offered = w.peers[a].trav.candidates();
    assert!(offered.contains(&a6) && offered.iter().any(|c| !c.is_v6()), "{offered:?}");
    w.join(a, "pair", Role::Peer);
    w.join(b, "pair", Role::Peer);
    assert!(w.run_until(Duration::from_secs(10), |w| w.direct(a, b)));
    // Both families answer on these NATs: either one may carry the link.
    let addrs = [w.peers[b].trav.candidates(), vec![w.sim.addr(w.peers[b].node)]].concat();
    assert!(matches!(w.path(a, b), Some(Path::Direct(p)) if addrs.contains(&p)), "{:?}", w.path(a, b));
}

#[test]
fn invite_join_punches_only_with_signaling() {
    // The joiner has the host's invite (its STUN-learned candidates). Both sit behind
    // port-restricted NATs, so the host's NAT drops the joiner's dial unless the host
    // dials back at the same time, which the signaled offer makes it do.
    for signaled in [false, true] {
        let mut w = World::new(11);
        let (h, j) = (w.add(&kind("port-restr")), w.add(&kind("port-restr")));
        w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.public_addr().is_some()));
        let (now, hid) = (w.sim.now, w.id(h));
        let targets: Vec<_> = w.peers[h].trav.candidates().into_iter().map(Path::Direct).collect();
        let nj = w.peers[j].node;
        w.sim.nodes[nj].endpoint.connect_with(now, hid, &targets).unwrap();
        if signaled {
            let ep = &w.sim.nodes[nj].endpoint;
            let key = w.peers[h].trav.invite_key();
            w.peers[j].trav.introduce(now, ep, hid, Some(key));
        }
        let ok = w.run_until(Duration::from_secs(10), |w| w.connected(h, j));
        assert_eq!(ok, signaled, "signaled: {signaled}");
        if ok {
            assert!(w.direct(h, j));
        }
    }
}

/// How the A-B link of a room ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Direct,
    Relayed,
    Failed,
}

/// A public host H and two peers A and B (of kinds `a`, `b`, or both behind one
/// port-restricted NAT with no hairpinning when `a` is `None`) meet in a room,
/// A and B in `mode`. How does A-B link?
fn room_trio(a: Option<(&Kind, &Kind)>, mode: Privacy, seed: u64) -> Link {
    let mut w = World::new(seed);
    let h = w.add(&Kind::Public);
    let (pa, pb) = match a {
        Some((a, b)) => (w.add(a), w.add(b)),
        None => {
            let nat = w.sim.add_nat(NatConfig::port_restricted(), None);
            let pa = w.add_beside(nat);
            (pa, w.add_beside(nat))
        }
    };
    w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
    for p in [pa, pb] {
        w.set_privacy(p, mode);
    }
    w.join(h, "trio", Role::Host);
    w.join(pa, "trio", Role::Peer);
    w.join(pb, "trio", Role::Peer);
    if !w.run_until(Duration::from_secs(45), |w| w.connected(pa, pb)) {
        return Link::Failed;
    }
    // Time to punch, if anything will.
    w.run_until(Duration::from_secs(6), |w| w.direct(pa, pb));
    let (na, nb) = (w.peers[pa].node, w.peers[pb].node);
    w.peers[pb].events.clear();
    w.sim.send(na, nb, Lane::Reliable, b"ping").unwrap();
    w.run_until(Duration::from_secs(1), |w| w.peers[pb].events.iter().any(|e| matches!(e, Event::Message { .. })));
    assert_eq!(w.messages(pb), vec![b"ping".to_vec()]);
    if w.direct(pa, pb) {
        Link::Direct
    } else {
        let hid = w.id(h);
        assert_eq!((w.path(pa, pb), w.path(pb, pa)), (Some(Path::Via(hid)), Some(Path::Via(hid))));
        Link::Relayed
    }
}

/// NB-034: what each privacy mode costs in direct links, over every pair of NAT
/// kinds (each unordered pair once) plus two peers on one LAN, in a room with a
/// public host that relays. Prints the counts per mode.
#[test]
fn privacy_modes_matrix() {
    let ks = kinds();
    let mut pairs: Vec<Option<(&Kind, &Kind)>> = vec![None];
    for (i, a) in ks.iter().enumerate() {
        for b in &ks[i..] {
            pairs.push(Some((a, b)));
        }
    }
    println!(
        "\nroom of a public host and two peers A, B in each mode: A-B links ({} pairs of NAT kinds + one LAN pair)",
        pairs.len() - 1
    );
    let modes = [Privacy::Direct, Privacy::PublicOnly, Privacy::RelayOnly { via: None }];
    let mut lan = Vec::new();
    for mode in modes {
        let mut count = [0u32; 3];
        for (k, pair) in pairs.iter().enumerate() {
            let link = room_trio(*pair, mode, 500 + k as u64);
            count[link as usize] += 1;
            if pair.is_none() {
                lan.push(link);
            }
            if mode != Privacy::Direct {
                // No mode loses a link: what can't be direct goes through the host.
                assert_ne!(link, Link::Failed, "{mode:?} {:?}", pair.map(|(a, b)| (a.name(), b.name())));
            }
            if matches!(mode, Privacy::RelayOnly { .. }) {
                assert_eq!(link, Link::Relayed);
            }
        }
        println!("{:>34}: direct {:>2}, relayed {:>2}, failed {}", format!("{mode:?}"), count[0], count[1], count[2]);
    }
    // On one LAN, Direct links over the LAN; PublicOnly gave the LAN address to no
    // one, and the NAT doesn't hairpin, so the pair goes through the host.
    assert_eq!(lan, vec![Link::Direct, Link::Relayed, Link::Relayed]);
}
