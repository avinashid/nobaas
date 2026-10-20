//! The `nobaas` NAT traversal (STUN, rooms, sealed offers, simultaneous dial,
//! relay fallback) driven over simulated NATs, exactly as `Node` drives it over UDP.
//! Signaling is a deterministic in-memory broker with relay-like latency; no
//! invite or address is exchanged out of band.

use nobaas::traversal::{Failure, NatType, Role, Signal, Traversal};
use nobaas::Topic;
use nobaas_core::{Config, Event, Lane, Path, PeerId};
use nobaas_sim::{NatConfig, NetConfig, Sim, SimAddr};
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
    inflight: VecDeque<(nobaas_core::Instant, usize, Topic, Vec<u8>)>,
    signal_delay: Duration,
}

impl World {
    fn new(seed: u64) -> World {
        let mut sim = Sim::new(NetConfig::default(), seed);
        let stun = vec![sim.add_stun_server(), sim.add_stun_server()];
        World { sim, stun, peers: Vec::new(), inflight: VecDeque::new(), signal_delay: Duration::from_millis(150) }
    }

    fn add(&mut self, k: &Kind) -> usize {
        let config = Config::default();
        let node = match k {
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
        };
        let identity = self.sim.nodes[node].identity.clone();
        let seed = [node as u8 + 1; 32];
        let trav = Traversal::new(&identity, seed, BASE_MS, vec![self.sim.addr(node)], self.stun.clone());
        self.peers.push(Peer { node, trav, topics: BTreeSet::new(), events: Vec::new() });
        self.peers.len() - 1
    }

    fn id(&self, p: usize) -> PeerId {
        self.sim.peer_id(self.peers[p].node)
    }

    fn join(&mut self, p: usize, room: &str, role: Role) {
        let now = self.sim.now;
        self.peers[p].trav.join_room(now, "nb-sim", room, role);
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
            for t in stun {
                self.sim.send_raw(node, t.dst, t.payload);
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
    let mut w = World::new(seed);
    let (pa, pb) = (w.add(a), w.add(b));
    w.run_until(Duration::from_secs(2), |w| w.peers.iter().all(|p| p.trav.nat_type() != NatType::Unknown));
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
    println!("\nroom of two, signaling only: D = direct (ms to connect), F = failed (S: symmetric NAT, U: unreachable)");
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
                (false, (Some(Failure::Symmetric), _)) => "F S".to_string(),
                (false, _) => "F U".to_string(),
            };
            print!("{cell:>14}");
            direct += o.direct as u32;
            if !o.connected {
                // Only symmetric NATs stop two peers, and both sides say so.
                let sym = Some(Failure::Symmetric);
                assert_eq!(o.failures, (sym, sym), "{} x {}: {o:?}", a.name(), b.name());
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
    assert_eq!(o.failures, (Some(Failure::Symmetric), Some(Failure::Symmetric)));
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
            w.peers[j].trav.introduce(now, ep, hid);
        }
        let ok = w.run_until(Duration::from_secs(10), |w| w.connected(h, j));
        assert_eq!(ok, signaled, "signaled: {signaled}");
        if ok {
            assert!(w.direct(h, j));
        }
    }
}
