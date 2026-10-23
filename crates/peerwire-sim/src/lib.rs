//! `peerwire-sim`: a deterministic, single-threaded network simulator for
//! [`peerwire_core::Endpoint`].
//!
//! Every node is a real endpoint; only the network is simulated, with latency,
//! jitter (which reorders packets), loss, duplication and an optional per-node
//! uplink rate. Time is virtual, so a minute of traffic between thousands of
//! nodes runs as fast as the endpoints can process it, and the same seed always
//! produces the same run.
//!
//! Nodes can sit behind NATs ([`NatConfig`]), and NATs behind other NATs
//! (carrier-grade NAT). Each NAT's inside is its own address realm; the
//! internet is realm 0.
//!
//! STUN servers ([`Sim::add_stun_server`]) sit on the internet and answer Binding
//! requests with the address they saw, so hosts learn their NAT's mapping. STUN
//! answers reaching a node go to its [`Node::stun_inbox`], not its endpoint.

pub mod fuzz;
mod nat;

use nat::Nat;
pub use nat::{Dependence, NatConfig, NatStats, PortAlloc};
use peerwire_core::{
    stun, Address, Config, ConnectError, Endpoint, Event, Identity, Instant, Lane, Path, PeerId, SendError,
};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::time::Duration;

/// An IPv4-like address. IPs are unique across all realms, so an address
/// also says which realm it lives in. IPs `0x20xx_xxxx` stand for IPv6
/// addresses ([`Sim::add_ipv6`]), `2001:db8::` and the IP to real sockets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SimAddr {
    pub ip: u32,
    pub port: u16,
}

/// The top byte of the IPs that stand for IPv6 addresses.
const V6_NET: u32 = 0x2000_0000;
/// The /96 prefix they map to.
const V6_PREFIX: [u16; 6] = [0x2001, 0xdb8, 0, 0, 0, 0];

impl SimAddr {
    pub const fn new(ip: u32, port: u16) -> Self {
        SimAddr { ip, port }
    }

    /// Whether this stands for an IPv6 address.
    pub const fn is_v6(&self) -> bool {
        self.ip & 0xff00_0000 == V6_NET
    }
}

impl std::fmt::Display for SimAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_socket_addr().expect("always an IP"))
    }
}

impl Address for SimAddr {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ip.to_le_bytes());
        out.extend_from_slice(&self.port.to_le_bytes());
    }

    fn decode(b: &[u8]) -> Option<Self> {
        (b.len() == 6)
            .then(|| SimAddr::new(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), u16::from_le_bytes([b[4], b[5]])))
    }

    fn port_offset(&self, delta: i32) -> Option<Self> {
        let p = i32::from(self.port) + delta;
        (1..=65535).contains(&p).then(|| SimAddr::new(self.ip, p as u16))
    }

    fn to_socket_addr(&self) -> Option<std::net::SocketAddr> {
        if self.is_v6() {
            let [p0, p1, p2, p3, p4, p5] = V6_PREFIX;
            let ip = std::net::Ipv6Addr::new(p0, p1, p2, p3, p4, p5, (self.ip >> 16) as u16, self.ip as u16);
            return Some(std::net::SocketAddr::from((ip, self.port)));
        }
        Some(std::net::SocketAddr::from((self.ip.to_be_bytes(), self.port)))
    }

    fn from_socket_addr(addr: std::net::SocketAddr) -> Option<Self> {
        match addr {
            std::net::SocketAddr::V4(a) => Some(SimAddr::new(u32::from(*a.ip()), a.port())).filter(|s| !s.is_v6()),
            std::net::SocketAddr::V6(a) => {
                let s = a.ip().segments();
                let ip = u32::from(s[6]) << 16 | u32::from(s[7]);
                (s[..6] == V6_PREFIX && ip & 0xff00_0000 == V6_NET).then(|| SimAddr::new(ip, a.port()))
            }
        }
    }
}

/// The port every simulated host binds.
pub const HOST_PORT: u16 = 7000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Node(usize),
    /// A node's IPv6 address ([`Sim::add_ipv6`]), on the internet.
    Node6(usize),
    Nat(usize),
    Stun,
}

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub latency: Duration,
    /// Extra delay drawn uniformly from `0..jitter` per packet.
    pub jitter: Duration,
    pub loss: f64,
    pub duplicate: f64,
    /// Packets between two addresses never overtake each other: jitter varies
    /// the delay without reordering, as on one queue. Off, each packet's
    /// jitter is drawn alone, so fast senders see heavy reordering.
    pub in_order: bool,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            latency: Duration::from_millis(20),
            jitter: Duration::ZERO,
            loss: 0.0,
            duplicate: 0.0,
            in_order: false,
        }
    }
}

/// Called for every packet put on the wire; return `false` to drop it. May mutate it.
pub type Hook = Box<dyn FnMut(SimAddr, SimAddr, &mut Vec<u8>) -> bool>;

pub struct Node {
    pub endpoint: Endpoint<SimAddr>,
    pub identity: Identity,
    pub addr: SimAddr,
    /// 0 for the internet, `n + 1` for the inside of NAT `n`.
    pub realm: usize,
    /// A global IPv6 address behind a stateful firewall ([`Sim::add_ipv6`]).
    pub addr6: Option<SimAddr>,
    /// Addresses `addr6` has sent to: the only ones the firewall lets in.
    pub pinholes: HashSet<SimAddr>,
    /// Packets to `addr6` the firewall dropped.
    pub firewall_dropped: u64,
    pub events: Vec<Event<SimAddr>>,
    /// Virtual time each entry of `events` was emitted.
    pub event_times: Vec<Instant>,
    /// Bits per second; `None` means unlimited.
    pub uplink_bps: Option<u64>,
    /// The uplink's buffer, as queueing delay: a packet that would wait longer
    /// is dropped (tail drop, as a router does). `None` queues without limit.
    pub uplink_buffer: Option<Duration>,
    /// Packets dropped because the uplink buffer was full.
    pub uplink_dropped: u64,
    /// Wire counters, bytes including 28 bytes of IPv4/UDP header per packet.
    pub tx_packets: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub rx_bytes: u64,
    /// STUN datagrams that arrived for this node, with their source; the host takes them.
    pub stun_inbox: Vec<(SimAddr, Vec<u8>)>,
    /// Wall-clock time spent inside this node's endpoint calls, when
    /// [`Sim::measure_cpu`] is on. Not virtual time, so not deterministic.
    pub cpu: Duration,
    tx_free_at: Instant,
    /// Extra sockets ([`Sim::open_ports`]) by local port, on `addr`'s IP.
    extra: HashMap<u16, ExtraPort>,
    /// Which extra port sends to an address: the first that heard from it.
    routes: HashMap<SimAddr, u16>,
    next_port: u16,
}

/// An extra socket, as a host opens for birthday punching.
struct ExtraPort {
    /// Closed then unless it is a route.
    until: Instant,
    /// Last heard from an address it is the route for.
    last_recv: Option<Instant>,
}

/// A routed extra port is closed once it has heard nothing for this long.
pub const ROUTE_IDLE: Duration = Duration::from_secs(60);

impl ExtraPort {
    fn open(&self, now: Instant) -> bool {
        match self.last_recv {
            Some(t) => now < t + ROUTE_IDLE,
            None => now < self.until,
        }
    }
}

struct InFlight {
    at: Instant,
    seq: u64,
    from: SimAddr,
    to: SimAddr,
    /// The realm where the destination became visible.
    realm: usize,
    /// NATs the packet left through (for the hairpin check).
    exits: Vec<usize>,
    data: Vec<u8>,
}

impl PartialEq for InFlight {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for InFlight {}
impl PartialOrd for InFlight {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for InFlight {
    // Min-heap on (time, insertion order).
    fn cmp(&self, o: &Self) -> Ordering {
        (o.at, o.seq).cmp(&(self.at, self.seq))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimStats {
    pub sent: u64,
    pub bytes: u64,
    pub lost: u64,
    pub duplicated: u64,
    pub delivered: u64,
    pub undeliverable: u64,
}

pub struct Sim {
    pub now: Instant,
    pub nodes: Vec<Node>,
    pub net: NetConfig,
    pub hook: Option<Hook>,
    pub stats: SimStats,
    /// Time each node's endpoint calls with the wall clock (into `Node::cpu`), for benchmarks.
    pub measure_cpu: bool,
    owners: HashMap<u32, Owner>,
    nats: Vec<Nat>,
    queue: BinaryHeap<InFlight>,
    /// Latest arrival per (from, to), for `NetConfig::in_order`.
    last_arrival: HashMap<(SimAddr, SimAddr), Instant>,
    seq: u64,
    rng: ChaCha20Rng,
    next_ip: u32,
    /// Cached `poll_timeout` per node, refreshed whenever the node is touched.
    timers: Vec<Option<Instant>>,
    /// Nodes whose endpoint changed since the last flush.
    touched: Vec<usize>,
    is_touched: Vec<bool>,
}

impl Sim {
    pub fn new(net: NetConfig, seed: u64) -> Self {
        Sim {
            now: Instant::ZERO,
            nodes: Vec::new(),
            net,
            hook: None,
            stats: SimStats::default(),
            measure_cpu: false,
            owners: HashMap::new(),
            nats: Vec::new(),
            queue: BinaryHeap::new(),
            last_arrival: HashMap::new(),
            seq: 0,
            rng: ChaCha20Rng::seed_from_u64(seed),
            next_ip: 0x0a00_0001,
            timers: Vec::new(),
            touched: Vec::new(),
            is_touched: Vec::new(),
        }
    }

    fn touch(&mut self, i: usize) {
        if !self.is_touched[i] {
            self.is_touched[i] = true;
            self.touched.push(i);
        }
    }

    /// Marks every node touched, for when callers changed endpoints directly
    /// through `nodes`.
    fn touch_all(&mut self) {
        for i in 0..self.nodes.len() {
            self.touch(i);
        }
    }

    /// A fresh IP: public-looking (11.x) on the internet (realm 0), 100.64.x for a
    /// home NAT's outside inside a carrier-grade NAT, 10.x behind a NAT, so
    /// filters on private ranges behave as on real networks.
    fn alloc_ip(&mut self, realm: usize, carrier: bool) -> u32 {
        let n = self.next_ip;
        self.next_ip += 1;
        match (realm, carrier) {
            (0, _) => 0x0b00_0000 | (n & 0x00ff_ffff),
            (_, true) => 0x6440_0000 | (n & 0x003f_ffff),
            _ => n,
        }
    }

    /// Adds a NAT whose outside is the internet (`parent: None`) or the inside
    /// of NAT `parent` (a home router behind a carrier-grade NAT).
    pub fn add_nat(&mut self, config: NatConfig, parent: Option<usize>) -> usize {
        let n = self.nats.len();
        let outer = parent.map_or(0, |p| p + 1);
        let ip = self.alloc_ip(outer, true);
        self.nats.push(Nat::new(config, ip, outer, n + 1));
        self.owners.insert(ip, Owner::Nat(n));
        n
    }

    pub fn nat_stats(&self, n: usize) -> NatStats {
        self.nats[n].stats.clone()
    }

    /// The NAT's outside IP.
    pub fn nat_ip(&self, n: usize) -> u32 {
        self.nats[n].ip
    }

    /// Adds a node inside NAT `nat`.
    pub fn add_node_behind(&mut self, nat: usize, config: Config) -> usize {
        let identity = Identity::generate(&mut self.rng);
        self.add_node_in(identity, config, nat + 1)
    }

    pub fn add_node(&mut self, config: Config) -> usize {
        let identity = Identity::generate(&mut self.rng);
        self.add_node_with(identity, config)
    }

    /// Adds a node with a given identity (e.g. to simulate a restart).
    pub fn add_node_with(&mut self, identity: Identity, config: Config) -> usize {
        self.add_node_in(identity, config, 0)
    }

    fn add_node_in(&mut self, identity: Identity, config: Config, realm: usize) -> usize {
        let mut seed = [0u8; 32];
        self.rng.fill_bytes(&mut seed);
        let addr = SimAddr::new(self.alloc_ip(realm, false), HOST_PORT);
        let i = self.nodes.len();
        self.nodes.push(Node {
            endpoint: Endpoint::new(&identity, config, seed),
            identity,
            addr,
            realm,
            addr6: None,
            pinholes: HashSet::new(),
            firewall_dropped: 0,
            events: Vec::new(),
            event_times: Vec::new(),
            uplink_bps: None,
            uplink_buffer: None,
            uplink_dropped: 0,
            tx_packets: 0,
            tx_bytes: 0,
            rx_packets: 0,
            rx_bytes: 0,
            stun_inbox: Vec::new(),
            cpu: Duration::ZERO,
            tx_free_at: Instant::ZERO,
            extra: HashMap::new(),
            routes: HashMap::new(),
            next_port: HOST_PORT,
        });
        self.timers.push(None);
        self.is_touched.push(false);
        self.owners.insert(addr.ip, Owner::Node(i));
        i
    }

    /// Replaces node `i`'s endpoint with a fresh one (same identity and address),
    /// as if the process restarted and lost all state.
    pub fn restart(&mut self, i: usize, config: Config) {
        let mut seed = [0u8; 32];
        self.rng.fill_bytes(&mut seed);
        let n = &mut self.nodes[i];
        n.endpoint = Endpoint::new(&n.identity, config, seed);
        n.events.clear();
        n.event_times.clear();
        self.touch(i);
    }

    pub fn peer_id(&self, i: usize) -> PeerId {
        self.nodes[i].endpoint.peer_id()
    }

    pub fn addr(&self, i: usize) -> SimAddr {
        self.nodes[i].addr
    }

    /// Gives node `i` a global IPv6 address too (on the same port), as mobile
    /// carriers do beside a carrier-grade NAT: no NAT, but a stateful firewall
    /// that lets in only addresses the node has sent to from it. Packets to IPv6
    /// addresses leave from it; nodes without one can't send to them. Returns it.
    pub fn add_ipv6(&mut self, i: usize) -> SimAddr {
        let n = self.next_ip;
        self.next_ip += 1;
        let addr = SimAddr::new(V6_NET | (n & 0x00ff_ffff), HOST_PORT);
        self.nodes[i].addr6 = Some(addr);
        self.owners.insert(addr.ip, Owner::Node6(i));
        addr
    }

    /// Moves node `i` to a new address (a NAT rebinding or a Wi-Fi to cellular switch).
    pub fn set_addr(&mut self, i: usize, addr: SimAddr) {
        self.owners.remove(&self.nodes[i].addr.ip);
        self.nodes[i].addr = addr;
        self.owners.insert(addr.ip, Owner::Node(i));
    }

    pub fn connect(&mut self, from: usize, to: usize) -> Result<(), ConnectError> {
        let (addr, peer) = (self.addr(to), self.peer_id(to));
        self.touch(from);
        self.nodes[from].endpoint.connect(self.now, addr, peer)
    }

    /// Dials `to` through `relay`, with `candidates` as direct addresses to try.
    pub fn connect_via(
        &mut self,
        from: usize,
        relay: usize,
        to: usize,
        candidates: &[SimAddr],
    ) -> Result<(), ConnectError> {
        let (r, p) = (self.peer_id(relay), self.peer_id(to));
        self.touch(from);
        self.nodes[from].endpoint.connect_via(self.now, r, p, candidates)
    }

    pub fn connect_with(&mut self, from: usize, to: usize, targets: &[Path<SimAddr>]) -> Result<(), ConnectError> {
        let p = self.peer_id(to);
        self.touch(from);
        self.nodes[from].endpoint.connect_with(self.now, p, targets)
    }

    /// The path node `i` uses to reach node `j`, if connected.
    pub fn path(&self, i: usize, j: usize) -> Option<Path<SimAddr>> {
        self.nodes[i].endpoint.path(&self.peer_id(j)).cloned()
    }

    pub fn send(&mut self, from: usize, to: usize, lane: Lane, data: &[u8]) -> Result<(), SendError> {
        let peer = self.peer_id(to);
        self.touch(from);
        self.nodes[from].endpoint.send(&peer, lane, data)
    }

    /// Adds a STUN server on the internet; returns its address.
    pub fn add_stun_server(&mut self) -> SimAddr {
        let ip = self.alloc_ip(0, false);
        self.owners.insert(ip, Owner::Stun);
        SimAddr::new(ip, 3478)
    }

    /// Sends a datagram from node `i`'s socket (out through its NATs), as a host
    /// does for traffic that isn't the endpoint's, such as STUN.
    pub fn send_raw(&mut self, i: usize, to: SimAddr, data: Vec<u8>) {
        self.put_on_wire(i, to, data);
    }

    /// Opens `n` extra sockets on node `i`'s IPv4 address, as a host does for
    /// birthday punching (`peerwire`'s `Traversal::poll_birthday`); returns their
    /// ports. Like the host, the simulator hands the endpoint what they receive,
    /// and sends the endpoint's datagrams to an address one of them heard from out
    /// of the first that did (its route). One that is no address's route closes at
    /// `until`; a route, after [`ROUTE_IDLE`] without a packet.
    pub fn open_ports(&mut self, i: usize, n: u16, until: Instant) -> Vec<u16> {
        let now = self.now;
        let node = &mut self.nodes[i];
        node.extra.retain(|_, p| p.open(now));
        let extra = &node.extra;
        node.routes.retain(|_, p| extra.contains_key(p));
        let mut ports = Vec::new();
        while ports.len() < n as usize && node.extra.len() < 60_000 {
            node.next_port = if node.next_port == u16::MAX { 1024 } else { node.next_port + 1 };
            let p = node.next_port;
            if p != node.addr.port && !node.extra.contains_key(&p) {
                node.extra.insert(p, ExtraPort { until, last_recv: None });
                ports.push(p);
            }
        }
        ports
    }

    /// Sends a datagram from node `i`'s extra socket `port` ([`Sim::open_ports`]).
    pub fn send_from(&mut self, i: usize, port: u16, to: SimAddr, data: Vec<u8>) {
        if self.nodes[i].extra.get(&port).is_some_and(|p| p.open(self.now)) {
            self.put_on_wire_from(i, Some(port), to, data);
        }
    }

    /// Extra sockets of node `i` still open, and how many of them have a route.
    pub fn open_port_count(&self, i: usize) -> (usize, usize) {
        let n = &self.nodes[i];
        let open = n.extra.values().filter(|p| p.open(self.now)).count();
        (open, n.extra.values().filter(|p| p.open(self.now) && p.last_recv.is_some()).count())
    }

    /// Answers a Binding request that reached a STUN server with the address it came from.
    fn answer_stun(&mut self, p: &InFlight) {
        let (Some(tx), Some(seen)) = (stun::parse_binding_request(&p.data), p.from.to_socket_addr()) else { return };
        self.seq += 1;
        let data = stun::binding_response(&tx, seen);
        let at = self.now + self.net.latency;
        self.queue.push(InFlight { at, seq: self.seq, from: p.to, to: p.from, realm: 0, exits: Vec::new(), data });
    }

    /// Delivers a raw datagram to whatever node holds `to`, right now.
    pub fn inject(&mut self, from: SimAddr, to: SimAddr, data: &[u8]) {
        if let Some(&Owner::Node(i)) = self.owners.get(&to.ip) {
            self.nodes[i].endpoint.handle_datagram(self.now, from, data);
            self.touch(i);
            self.flush();
        }
    }

    pub fn take_events(&mut self, i: usize) -> Vec<Event<SimAddr>> {
        self.nodes[i].event_times.clear();
        std::mem::take(&mut self.nodes[i].events)
    }

    /// Events with the virtual time each was emitted.
    pub fn take_timed_events(&mut self, i: usize) -> Vec<(Instant, Event<SimAddr>)> {
        let times = std::mem::take(&mut self.nodes[i].event_times);
        times.into_iter().zip(std::mem::take(&mut self.nodes[i].events)).collect()
    }

    /// Drains packets and events from touched nodes and refreshes their timers.
    fn flush(&mut self) {
        let touched = std::mem::take(&mut self.touched);
        for &i in &touched {
            self.is_touched[i] = false;
            loop {
                let t0 = self.wall();
                let t = self.nodes[i].endpoint.poll_transmit(self.now);
                self.charge(i, t0);
                let Some(t) = t else { break };
                self.put_on_wire(i, t.dst, t.payload);
            }
            let t0 = self.wall();
            let n = &mut self.nodes[i];
            while let Some(e) = n.endpoint.poll_event() {
                n.events.push(e);
                n.event_times.push(self.now);
            }
            self.timers[i] = n.endpoint.poll_timeout();
            self.charge(i, t0);
        }
        self.touched = touched;
        self.touched.clear();
    }

    fn wall(&self) -> Option<std::time::Instant> {
        self.measure_cpu.then(std::time::Instant::now)
    }

    fn charge(&mut self, i: usize, since: Option<std::time::Instant>) {
        if let Some(t0) = since {
            self.nodes[i].cpu += t0.elapsed();
        }
    }

    fn uniform(&mut self) -> f64 {
        (self.rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn put_on_wire(&mut self, i: usize, to: SimAddr, data: Vec<u8>) {
        self.put_on_wire_from(i, None, to, data)
    }

    /// From `port`, an extra socket, or else the one routed to `to`, or else the main one.
    fn put_on_wire_from(&mut self, i: usize, port: Option<u16>, to: SimAddr, mut data: Vec<u8>) {
        let now = self.now;
        let n = &self.nodes[i];
        let routed = n.routes.get(&to).copied().filter(|p| n.extra.get(p).is_some_and(|x| x.open(now)));
        let from = match (to.is_v6(), self.nodes[i].addr6) {
            (false, _) => SimAddr::new(n.addr.ip, port.or(routed).unwrap_or(n.addr.port)),
            (true, Some(a6)) => {
                self.nodes[i].pinholes.insert(to);
                a6
            }
            (true, None) => {
                self.stats.undeliverable += 1; // no IPv6 route
                return;
            }
        };
        if let (Some(_), Some(buf)) = (self.nodes[i].uplink_bps, self.nodes[i].uplink_buffer) {
            if self.nodes[i].tx_free_at > self.now + buf {
                self.nodes[i].uplink_dropped += 1;
                return;
            }
        }
        self.stats.sent += 1;
        self.stats.bytes += data.len() as u64;
        self.nodes[i].tx_packets += 1;
        self.nodes[i].tx_bytes += data.len() as u64 + 28;
        if let Some(hook) = self.hook.as_mut() {
            if !hook(from, to, &mut data) {
                self.stats.lost += 1;
                return;
            }
        }
        let mut depart = self.now;
        if let Some(bps) = self.nodes[i].uplink_bps {
            let start = self.nodes[i].tx_free_at.max(self.now);
            let bits = (data.len() as u64 + 28) * 8; // + IPv4/UDP headers
            depart = start + Duration::from_micros(bits * 1_000_000 / bps);
            self.nodes[i].tx_free_at = depart;
        }
        if self.uniform() < self.net.loss {
            self.stats.lost += 1;
            return;
        }
        // Climb out through NATs until the destination IP is visible.
        let realm = if to.is_v6() { 0 } else { self.nodes[i].realm };
        let (mut realm, mut src, mut exits) = (realm, from, Vec::new());
        while !self.visible(realm, to.ip) {
            if realm == 0 {
                self.stats.undeliverable += 1;
                return;
            }
            let r = self.rng.next_u64();
            let n = realm - 1;
            src = self.nats[n].outbound(depart, src, to, r);
            exits.push(n);
            realm = self.nats[n].outer;
        }
        let from = src;
        let copies = if self.uniform() < self.net.duplicate { 2 } else { 1 };
        self.stats.duplicated += copies - 1;
        for _ in 0..copies {
            let jitter = self.net.jitter.mul_f64(self.uniform());
            let mut at = depart + self.net.latency + jitter;
            if self.net.in_order {
                let last = self.last_arrival.entry((from, to)).or_insert(at);
                at = at.max(*last);
                *last = at;
            }
            self.seq += 1;
            self.queue.push(InFlight { at, seq: self.seq, from, to, realm, exits: exits.clone(), data: data.clone() });
        }
    }

    /// Advances to the next packet arrival or timer. Returns `false` when nothing is left.
    pub fn step(&mut self) -> bool {
        let Some(next) = self.peek_next() else {
            return false;
        };
        self.now = self.now.max(next);
        while self.queue.peek().is_some_and(|p| p.at <= self.now) {
            let p = self.queue.pop().unwrap();
            if p.realm == 0 && self.owners.get(&p.to.ip) == Some(&Owner::Stun) {
                self.stats.delivered += 1;
                self.answer_stun(&p);
                continue;
            }
            match self.descend(&p) {
                Some((i, port)) => {
                    self.stats.delivered += 1;
                    let now = self.now;
                    let n = &mut self.nodes[i];
                    if n.extra.contains_key(&port) {
                        // The first open extra port to hear from an address is its route;
                        // only a route stays open past `until`.
                        let open = |r: &u16| n.extra.get(r).is_some_and(|x| x.open(now));
                        let route = n.routes.get(&p.from).copied().filter(open).unwrap_or(port);
                        n.routes.insert(p.from, route);
                        if route == port {
                            n.extra.get_mut(&port).expect("extra port").last_recv = Some(now);
                        }
                    }
                    n.rx_packets += 1;
                    n.rx_bytes += p.data.len() as u64 + 28;
                    if stun::is_stun(&p.data) {
                        n.stun_inbox.push((p.from, p.data));
                    } else {
                        let t0 = self.wall();
                        self.nodes[i].endpoint.handle_datagram(self.now, p.from, &p.data);
                        self.charge(i, t0);
                    }
                    self.touch(i);
                }
                None => self.stats.undeliverable += 1,
            }
        }
        for i in 0..self.nodes.len() {
            if self.timers[i].is_some_and(|t| t <= self.now) {
                let t0 = self.wall();
                self.nodes[i].endpoint.handle_timeout(self.now);
                self.charge(i, t0);
                self.touch(i);
            }
        }
        self.flush();
        true
    }

    /// Runs for `d` of virtual time.
    pub fn run_for(&mut self, d: Duration) {
        self.touch_all();
        let end = self.now + d;
        while self.peek_next().is_some_and(|t| t <= end) {
            self.step();
        }
        self.now = self.now.max(end);
        // Let timers that fired exactly at `end` emit their packets.
        self.flush();
    }

    /// Steps until `done` returns true or `limit` of virtual time passes. Returns whether `done` held.
    pub fn run_until(&mut self, limit: Duration, mut done: impl FnMut(&mut Sim) -> bool) -> bool {
        let end = self.now + limit;
        self.touch_all();
        loop {
            if done(self) {
                return true;
            }
            match self.peek_next() {
                Some(t) if t <= end => {
                    self.step();
                }
                _ => {
                    self.now = self.now.max(end);
                    return done(self);
                }
            }
        }
    }

    /// Whether `ip` can be addressed from inside `realm`.
    fn visible(&self, realm: usize, ip: u32) -> bool {
        match self.owners.get(&ip) {
            Some(&Owner::Node(i)) => self.nodes[i].realm == realm,
            Some(&Owner::Node6(_)) => realm == 0,
            Some(&Owner::Nat(n)) => self.nats[n].outer == realm,
            Some(&Owner::Stun) => realm == 0,
            None => false,
        }
    }

    /// Walks an arriving packet down through NATs to its node; returns it and the
    /// local port it arrived at.
    fn descend(&mut self, p: &InFlight) -> Option<(usize, u16)> {
        let (mut realm, mut dst) = (p.realm, p.to);
        let now = self.now;
        loop {
            match *self.owners.get(&dst.ip)? {
                Owner::Node(i) => {
                    let n = &self.nodes[i];
                    let port = dst.port == n.addr.port || n.extra.get(&dst.port).is_some_and(|x| x.open(now));
                    return (n.realm == realm && n.addr.ip == dst.ip && port).then_some((i, dst.port));
                }
                Owner::Node6(i) => {
                    let n = &mut self.nodes[i];
                    if realm != 0 || n.addr6 != Some(dst) {
                        return None;
                    }
                    if !n.pinholes.contains(&p.from) {
                        n.firewall_dropped += 1;
                        return None;
                    }
                    return Some((i, dst.port));
                }
                Owner::Nat(n) => {
                    if self.nats[n].outer != realm {
                        return None;
                    }
                    if p.exits.contains(&n) && !self.nats[n].config.hairpin {
                        self.nats[n].stats.hairpin_blocked += 1;
                        return None;
                    }
                    dst = self.nats[n].inbound(self.now, p.from, dst)?;
                    realm = self.nats[n].inner;
                }
                Owner::Stun => return None,
            }
        }
    }

    fn peek_next(&mut self) -> Option<Instant> {
        self.flush();
        let next_pkt = self.queue.peek().map(|p| p.at);
        let next_timer = self.timers.iter().flatten().min().copied();
        [next_pkt, next_timer].into_iter().flatten().min()
    }
}

// The README examples run as doc tests (`cargo test --doc`).
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
