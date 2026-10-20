//! NAT traversal, sans-IO: STUN, room presence and candidate exchange over a
//! [`Signaling`](crate::Signaling) carrier, next to a core [`Endpoint`].
//!
//! [`Node`](crate::Node) drives one of these on its UDP socket; the simulator drives
//! the same code over simulated NATs. Like the core it never reads a clock or a
//! socket: the host passes time, datagrams and signaling messages in, and drains
//! STUN datagrams and signaling actions out.
//!
//! How two peers behind NATs meet:
//! 1. Each asks STUN servers, on the session's own socket, which public address
//!    its NAT gave it (`public_addr`), and from two servers guesses the NAT type.
//! 2. A peer that learns of another (a room `hello`, or an invite) sends it an
//!    *offer*: its candidate addresses, NAT type and connected peers, sealed to
//!    the recipient's [`PeerId`] and published to that peer's topic.
//! 3. The recipient dials every candidate and answers with its own; the first
//!    side dials on the answer. Both dial at once, so each NAT has already let
//!    the other side's packets in: the handshake punches through cone NATs.
//! 4. Where punching can't work (symmetric NATs on both sides), each also dials
//!    through a peer both are connected to ([`Path::Via`]); core moves the link
//!    off the relay if a direct path turns up. With no such peer the dial fails
//!    with [`Failure::Symmetric`]: there is no TURN server, by design.

use crate::signaling::{hex, unhex, Topic};
use nobaas_core::{sealed, stun, Address, Endpoint, Event, Identity, Instant, Path, PeerId, Transmit};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

/// How often to ask STUN servers again (keeps the NAT mapping and `public_addr` fresh).
const STUN_INTERVAL: Duration = Duration::from_secs(25);
/// First STUN retransmit; doubles, `STUN_TRIES` sends in all.
const STUN_RTO: Duration = Duration::from_millis(500);
const STUN_TRIES: u32 = 3;
/// Room hellos repeat at 2 s, 4 s, ... up to every 30 s, for peers that missed one.
const HELLO_FIRST: Duration = Duration::from_secs(2);
const HELLO_MAX: Duration = Duration::from_secs(30);
/// Least time between two offers to one peer.
const OFFER_GAP: Duration = Duration::from_secs(2);
/// Offers older or newer than this (by the sender's clock) are dropped.
const FRESH_MS: u64 = 60_000;
/// Offers acted on: a burst of `DIAL_BURST`, then one per `DIAL_REFILL`.
const DIAL_BURST: u32 = 30;
const DIAL_REFILL: Duration = Duration::from_secs(2);
const MAX_CANDIDATES: usize = 8;
const MAX_RELAYS: usize = 8;
/// Relays tried per dial.
const VIA_PER_DIAL: usize = 2;
const MAX_OBSERVED: usize = 4;
/// Peers forgotten after this long without news (unless connected).
const FORGET: Duration = Duration::from_secs(600);
const OFFER: u8 = 1;

/// What STUN says about this node's NAT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NatType {
    /// No STUN answer yet, or only one server answered.
    #[default]
    Unknown,
    /// No NAT: the public address is the socket's own.
    Open,
    /// The NAT keeps one public port for every destination (full, restricted or
    /// port-restricted cone): hole punching works.
    Cone,
    /// The NAT picks a new public port per destination: punching works only
    /// towards cone peers, and two symmetric peers need a relay.
    Symmetric,
}

/// How a node takes part in a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A full mesh: connect to every peer and host in the room.
    Peer,
    /// A star centre: connect to every client and peer.
    Host,
    /// A star leaf: connect to hosts only.
    Client,
}

/// Why a dial to a peer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No handshake answer on any address: the peer is gone, or a firewall drops UDP.
    Unreachable,
    /// One peer is behind a symmetric NAT and the other behind one that only lets
    /// in replies (port-restricted or symmetric), so punching can't work, and no
    /// peer connected to both could relay. Nobaas has no TURN server; a third peer
    /// in the room that both can reach fixes it.
    Symmetric,
}

/// Something the host must do on its signaling carrier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    Subscribe(Topic),
    Unsubscribe(Topic),
    Publish(Topic, Vec<u8>),
}

struct Server<A> {
    addr: A,
    tx: stun::TxId,
    sent_at: Instant,
    tries: u32,
    pending: bool,
    mapped: Option<A>,
}

#[derive(Default)]
struct Remote {
    offered_at: Option<Instant>,
    /// We received an offer from it.
    heard: bool,
    nat: NatType,
    failure: Option<Failure>,
    seen_at: Instant,
}

struct Room {
    topic: Topic,
    role: Role,
    next_hello: Instant,
    interval: Duration,
}

/// STUN, presence and candidate exchange for one [`Endpoint`]. See the module docs.
pub struct Traversal<A: Address> {
    me: Identity,
    id: PeerId,
    inbox: Topic,
    rng: ChaCha20Rng,
    unix_ms: u64,
    local: Vec<A>,
    servers: Vec<Server<A>>,
    next_round: Instant,
    public: Option<A>,
    nat: NatType,
    observed: Vec<A>,
    room: Option<Room>,
    remotes: BTreeMap<PeerId, Remote>,
    nonces: HashMap<(PeerId, u64), Instant>,
    budget: u32,
    refill_at: Instant,
    transmits: VecDeque<Transmit<A>>,
    signals: VecDeque<Signal>,
}

impl<A: Address> Traversal<A> {
    /// `local`: the addresses the socket can be reached at on its own networks.
    /// `stun_servers`: servers to ask (empty: no STUN). `unix_ms`: wall-clock time,
    /// in Unix milliseconds, at [`Instant::ZERO`] (offers carry it, against replay).
    /// `seed`: fresh random bytes.
    pub fn new(identity: &Identity, seed: [u8; 32], unix_ms: u64, local: Vec<A>, stun_servers: Vec<A>) -> Self {
        let id = identity.peer_id();
        let inbox = Topic::peer(&id);
        let servers = stun_servers
            .into_iter()
            .map(|addr| Server { addr, tx: [0; 12], sent_at: Instant::ZERO, tries: 0, pending: false, mapped: None })
            .collect();
        Traversal {
            me: identity.clone(),
            id,
            inbox,
            rng: ChaCha20Rng::from_seed(seed),
            unix_ms,
            local,
            servers,
            next_round: Instant::ZERO,
            public: None,
            nat: NatType::Unknown,
            observed: Vec::new(),
            room: None,
            remotes: BTreeMap::new(),
            nonces: HashMap::new(),
            budget: DIAL_BURST,
            refill_at: Instant::ZERO,
            transmits: VecDeque::new(),
            signals: VecDeque::from([Signal::Subscribe(inbox)]),
        }
    }

    /// The public address STUN reported, if any.
    pub fn public_addr(&self) -> Option<&A> {
        self.public.as_ref()
    }

    /// What STUN says about the NAT.
    pub fn nat_type(&self) -> NatType {
        self.nat
    }

    /// Why the last dial to `peer` failed, until it connects.
    pub fn failure(&self, peer: &PeerId) -> Option<Failure> {
        self.remotes.get(peer).and_then(|r| r.failure)
    }

    /// The addresses offered to peers: public (STUN), observed by peers, local.
    pub fn candidates(&self) -> Vec<A> {
        let mut out: Vec<A> = Vec::new();
        for a in self.public.iter().chain(&self.observed).chain(&self.local) {
            if !out.contains(a) && out.len() < MAX_CANDIDATES {
                out.push(a.clone());
            }
        }
        out
    }

    /// Joins a room: announces this node on the room topic now and now and then,
    /// and dials the peers that answer (as `role` allows). Leaves any other room.
    pub fn join_room(&mut self, now: Instant, app_id: &str, room: &str, role: Role) {
        self.leave_room();
        let topic = Topic::room(app_id, room);
        self.signals.push_back(Signal::Subscribe(topic));
        self.signals.push_back(Signal::Publish(topic, hello(&self.id, role)));
        self.room = Some(Room { topic, role, next_hello: now + HELLO_FIRST, interval: HELLO_FIRST });
    }

    /// Stops announcing and answering in the current room. Links stay up.
    pub fn leave_room(&mut self) {
        if let Some(r) = self.room.take() {
            self.signals.push_back(Signal::Unsubscribe(r.topic));
        }
    }

    /// Sends `peer` an offer asking for one back, so a dial to it punches through
    /// NATs: call it after dialling a peer from an invite.
    pub fn introduce(&mut self, now: Instant, ep: &Endpoint<A>, peer: PeerId) {
        self.offer(now, ep, peer, true);
    }

    /// Takes a datagram if it is STUN (returns `true`); otherwise it is for the endpoint.
    pub fn handle_stun(&mut self, from: &A, data: &[u8]) -> bool {
        if !stun::is_stun(data) {
            return false;
        }
        for s in self.servers.iter_mut().filter(|s| s.pending && &s.addr == from) {
            if let Some(m) = stun::parse_binding_response(data, &s.tx).and_then(A::from_socket_addr) {
                s.mapped = Some(m);
                s.pending = false;
            }
        }
        self.classify();
        true
    }

    /// Handles a message from the signaling carrier.
    pub fn handle_signal(&mut self, now: Instant, ep: &mut Endpoint<A>, topic: Topic, payload: &[u8]) {
        if self.room.as_ref().is_some_and(|r| r.topic == topic) {
            if let Some((from, role)) = parse_hello(payload) {
                self.on_hello(now, ep, from, role);
            }
        } else if topic == self.inbox {
            let Some((from, body)) = sealed::open(&self.me, payload) else { return };
            if let Some(offer) = Offer::decode(&body) {
                self.on_offer(now, ep, from, offer);
            }
        }
    }

    /// Feeds an event from the endpoint (call for every one, before handling it).
    pub fn handle_event(&mut self, now: Instant, ep: &mut Endpoint<A>, event: &Event<A>) {
        match event {
            Event::Connected { peer, .. } => {
                if let Some(r) = self.remotes.get_mut(peer) {
                    r.failure = None;
                }
                // A new link may be the relay a waiting dial needed: offer again, with it listed.
                let waiting: Vec<PeerId> = self
                    .remotes
                    .iter()
                    .filter(|(p, r)| {
                        *p != peer && r.heard && !ep.is_connected(p) && r.offered_at.is_none_or(|t| now - t >= OFFER_GAP)
                    })
                    .map(|(p, _)| *p)
                    .collect();
                for p in waiting {
                    self.offer(now, ep, p, true);
                }
            }
            Event::ConnectFailed { peer } => {
                let mine = self.nat;
                let r = self.remotes.entry(*peer).or_default();
                r.seen_at = now;
                let sym = |a: NatType, b: NatType| a == NatType::Symmetric && b != NatType::Open;
                r.failure = Some(if sym(mine, r.nat) || sym(r.nat, mine) { Failure::Symmetric } else { Failure::Unreachable });
            }
            Event::AddrObserved { addr, .. } => {
                let loopback = addr.to_socket_addr().is_some_and(|s| s.ip().is_loopback());
                if !loopback && !self.observed.contains(addr) && !self.local.contains(addr) {
                    if self.observed.len() >= MAX_OBSERVED {
                        self.observed.remove(0);
                    }
                    self.observed.push(addr.clone());
                }
            }
            _ => {}
        }
    }

    /// Fires due timers: STUN queries and retransmits, room hellos.
    pub fn handle_timeout(&mut self, now: Instant, ep: &mut Endpoint<A>) {
        let _ = ep;
        if !self.servers.is_empty() && now >= self.next_round {
            self.next_round = now + STUN_INTERVAL;
            for i in 0..self.servers.len() {
                let mut tx = [0u8; 12];
                self.rng.fill_bytes(&mut tx);
                let s = &mut self.servers[i];
                (s.tx, s.sent_at, s.tries, s.pending) = (tx, now, 1, true);
                self.transmits.push_back(Transmit { dst: s.addr.clone(), payload: stun::binding_request(&tx) });
            }
        }
        for s in self.servers.iter_mut().filter(|s| s.pending) {
            if now >= s.sent_at + STUN_RTO * (1 << (s.tries - 1)) {
                if s.tries >= STUN_TRIES {
                    s.pending = false;
                } else {
                    (s.tries, s.sent_at) = (s.tries + 1, now);
                    self.transmits.push_back(Transmit { dst: s.addr.clone(), payload: stun::binding_request(&s.tx) });
                }
            }
        }
        if let Some(r) = self.room.as_mut().filter(|r| now >= r.next_hello) {
            r.interval = (r.interval * 2).min(HELLO_MAX);
            r.next_hello = now + r.interval;
            let (topic, role) = (r.topic, r.role);
            self.signals.push_back(Signal::Publish(topic, hello(&self.id, role)));
        }
        self.remotes.retain(|p, r| now - r.seen_at < FORGET || ep.is_connected(p));
        self.nonces.retain(|_, t| now - *t < Duration::from_millis(2 * FRESH_MS));
    }

    /// When [`handle_timeout`](Traversal::handle_timeout) is next due.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let stun = (!self.servers.is_empty()).then_some(self.next_round);
        let retry = self.servers.iter().filter(|s| s.pending).map(|s| s.sent_at + STUN_RTO * (1 << (s.tries - 1)));
        let hello = self.room.as_ref().map(|r| r.next_hello);
        stun.into_iter().chain(retry).chain(hello).min()
    }

    /// The next STUN datagram to send from the endpoint's socket.
    pub fn poll_transmit(&mut self) -> Option<Transmit<A>> {
        self.transmits.pop_front()
    }

    /// The next action for the signaling carrier.
    pub fn poll_signal(&mut self) -> Option<Signal> {
        self.signals.pop_front()
    }

    fn wall_ms(&self, now: Instant) -> u64 {
        self.unix_ms + now.as_micros() / 1000
    }

    fn role(&self) -> Option<Role> {
        self.room.as_ref().map(|r| r.role)
    }

    fn classify(&mut self) {
        let mapped: Vec<&A> = self.servers.iter().filter_map(|s| s.mapped.as_ref()).collect();
        self.public = mapped.first().map(|a| (*a).clone());
        self.nat = match mapped.first() {
            None => NatType::Unknown,
            Some(_) if mapped.iter().any(|m| self.local.contains(m)) => NatType::Open,
            Some(_) if mapped.len() < 2 => NatType::Unknown,
            Some(first) if mapped.iter().all(|m| m == first) => NatType::Cone,
            Some(_) => NatType::Symmetric,
        };
    }

    fn on_hello(&mut self, now: Instant, ep: &Endpoint<A>, from: PeerId, role: Role) {
        if from == self.id || !allowed(self.role(), role) || ep.is_connected(&from) {
            return;
        }
        let recent = self.remotes.get(&from).and_then(|r| r.offered_at).is_some_and(|t| now - t < OFFER_GAP);
        if !recent {
            self.offer(now, ep, from, true);
        }
    }

    fn on_offer(&mut self, now: Instant, ep: &mut Endpoint<A>, from: PeerId, o: Offer<A>) {
        if from == self.id || self.wall_ms(now).abs_diff(o.ts) > FRESH_MS || !allowed(self.role(), o.role) {
            return;
        }
        if self.nonces.insert((from, o.nonce), now).is_some() || !self.take_token(now) {
            return; // a replay, or more offers than we act on
        }
        let r = self.remotes.entry(from).or_default();
        (r.heard, r.nat, r.seen_at) = (true, o.nat, now);
        let addrs: Vec<A> = o.addrs.into_iter().filter(|a| !self.local.contains(a)).collect();
        if ep.is_connected(&from) {
            ep.add_candidates(now, &from, &addrs);
        } else {
            let vias = o.relays.iter().filter(|p| **p != from && **p != self.id && ep.is_connected(p));
            let mut targets: Vec<Path<A>> = vias.take(VIA_PER_DIAL).map(|p| Path::Via(*p)).collect();
            targets.extend(addrs.into_iter().map(Path::Direct));
            if !targets.is_empty() {
                let _ = ep.connect_with(now, from, &targets);
            }
        }
        if o.reply {
            self.offer(now, ep, from, false);
        }
    }

    fn take_token(&mut self, now: Instant) -> bool {
        while self.budget < DIAL_BURST && now >= self.refill_at + DIAL_REFILL {
            self.budget += 1;
            self.refill_at = self.refill_at + DIAL_REFILL;
        }
        if self.budget == DIAL_BURST {
            self.refill_at = now;
        }
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        true
    }

    fn offer(&mut self, now: Instant, ep: &Endpoint<A>, to: PeerId, reply: bool) {
        let relays = ep.connected_peers().filter(|p| **p != to).take(MAX_RELAYS).copied().collect();
        let o = Offer {
            reply,
            role: self.role().unwrap_or(Role::Peer),
            nat: self.nat,
            ts: self.wall_ms(now),
            nonce: self.rng.next_u64(),
            addrs: self.candidates(),
            relays,
        };
        let mut seed = [0u8; 32];
        self.rng.fill_bytes(&mut seed);
        if let Some(s) = sealed::seal(&self.me, &to, seed, &o.encode()) {
            self.signals.push_back(Signal::Publish(Topic::peer(&to), s));
        }
        let r = self.remotes.entry(to).or_default();
        (r.offered_at, r.seen_at) = (Some(now), now);
    }
}

/// Whether a node in a room as `mine` (or in none) links with one announcing `theirs`.
fn allowed(mine: Option<Role>, theirs: Role) -> bool {
    matches!(
        (mine, theirs),
        (None, _) | (Some(Role::Peer), Role::Peer | Role::Host) | (Some(Role::Host), Role::Peer | Role::Client)
            | (Some(Role::Client), Role::Host)
    )
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Peer => "peer",
        Role::Host => "host",
        Role::Client => "client",
    }
}

const HELLO_HEAD: &str = r#"{"t":"hello","from":""#;
const HELLO_MID: &str = r#"","meta":{"nb":1,"role":""#;
const HELLO_TAIL: &str = r#""}}"#;

/// The room presence message, in the JSON shape `nostr.js` uses (`{t, from, meta}`).
/// `meta.nb` marks a UDP peer, so browsers' WebRTC hellos are told apart.
fn hello(id: &PeerId, role: Role) -> Vec<u8> {
    format!("{HELLO_HEAD}{}{HELLO_MID}{}{HELLO_TAIL}", hex(id.as_bytes()), role_name(role)).into_bytes()
}

fn parse_hello(b: &[u8]) -> Option<(PeerId, Role)> {
    let s = std::str::from_utf8(b).ok()?;
    let rest = s.strip_prefix(HELLO_HEAD)?;
    let (id_hex, rest) = (rest.get(..64)?, rest.get(64..)?);
    let role = rest.strip_prefix(HELLO_MID)?.strip_suffix(HELLO_TAIL)?;
    let role = [Role::Peer, Role::Host, Role::Client].into_iter().find(|r| role_name(*r) == role)?;
    let mut id = [0u8; 32];
    unhex(id_hex, &mut id).then_some((PeerId(id), role))
}

/// Candidates for one peer. Sealed to it, so only it reads them.
#[derive(Debug, PartialEq)]
struct Offer<A> {
    /// The sender wants an offer back (it will dial when that arrives).
    reply: bool,
    role: Role,
    nat: NatType,
    /// Sender's Unix time in ms, and a random nonce: replays are dropped.
    ts: u64,
    nonce: u64,
    addrs: Vec<A>,
    /// Peers the sender is connected to, which could relay.
    relays: Vec<PeerId>,
}

impl<A: Address> Offer<A> {
    // [1] [flags] [role] [nat] [ts u64 le] [nonce u64 le] [n] n*([len] addr) [m] m*(32 B peer id)
    fn encode(&self) -> Vec<u8> {
        let role = [Role::Peer, Role::Host, Role::Client].iter().position(|r| *r == self.role).unwrap() as u8;
        let nat = [NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].iter().position(|n| *n == self.nat);
        let mut v = vec![OFFER, self.reply as u8, role, nat.unwrap() as u8];
        v.extend_from_slice(&self.ts.to_le_bytes());
        v.extend_from_slice(&self.nonce.to_le_bytes());
        v.push(self.addrs.len().min(MAX_CANDIDATES) as u8);
        for a in self.addrs.iter().take(MAX_CANDIDATES) {
            let mut e = Vec::new();
            a.encode(&mut e);
            v.push(e.len() as u8);
            v.extend_from_slice(&e);
        }
        v.push(self.relays.len().min(MAX_RELAYS) as u8);
        for p in self.relays.iter().take(MAX_RELAYS) {
            v.extend_from_slice(p.as_bytes());
        }
        v
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let (&[OFFER, flags, role, nat], rest) = b.split_first_chunk::<4>()? else { return None };
        let role = *[Role::Peer, Role::Host, Role::Client].get(role as usize)?;
        let nat = *[NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].get(nat as usize)?;
        let (ts, rest) = rest.split_first_chunk::<8>()?;
        let (nonce, rest) = rest.split_first_chunk::<8>()?;
        let (&n, mut rest) = rest.split_first()?;
        if n as usize > MAX_CANDIDATES || flags > 1 {
            return None;
        }
        let mut addrs = Vec::new();
        for _ in 0..n {
            let (&len, r) = rest.split_first()?;
            let (a, r) = (r.get(..len as usize)?, r.get(len as usize..)?);
            addrs.extend(A::decode(a));
            rest = r;
        }
        let (&m, mut rest) = rest.split_first()?;
        if m as usize > MAX_RELAYS || rest.len() != m as usize * 32 {
            return None;
        }
        let mut relays = Vec::new();
        while let Some((p, r)) = rest.split_first_chunk::<32>() {
            relays.push(PeerId(*p));
            rest = r;
        }
        Some(Offer {
            reply: flags == 1,
            role,
            nat,
            ts: u64::from_le_bytes(*ts),
            nonce: u64::from_le_bytes(*nonce),
            addrs,
            relays,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nobaas_core::Config;
    use std::net::SocketAddr;

    fn id(n: u8) -> Identity {
        Identity::from_secret_bytes(&[n; 32])
    }

    #[test]
    fn hello_round_trips_and_ignores_browser_hellos() {
        let p = id(1).peer_id();
        for role in [Role::Peer, Role::Host, Role::Client] {
            assert_eq!(parse_hello(&hello(&p, role)), Some((p, role)));
        }
        let js = format!(r#"{{"t":"hello","from":"{}","meta":{{"role":"player"}}}}"#, hex(p.as_bytes()));
        assert_eq!(parse_hello(js.as_bytes()), None);
        assert_eq!(parse_hello(b"{}"), None);
        let h = hello(&p, Role::Peer);
        for cut in 0..h.len() {
            assert_eq!(parse_hello(&h[..cut]), None);
        }
    }

    #[test]
    fn offer_round_trips_and_rejects_garbage() {
        let o: Offer<SocketAddr> = Offer {
            reply: true,
            role: Role::Client,
            nat: NatType::Symmetric,
            ts: 1_700_000_000_123,
            nonce: 42,
            addrs: vec!["203.0.113.5:4000".parse().unwrap(), "[2001:db8::1]:9".parse().unwrap()],
            relays: vec![id(2).peer_id()],
        };
        let b = o.encode();
        assert_eq!(Offer::decode(&b), Some(o));
        for cut in 0..b.len() {
            assert_eq!(Offer::<SocketAddr>::decode(&b[..cut]), None, "cut at {cut}");
        }
        let mut long = b.clone();
        long.push(0);
        assert_eq!(Offer::<SocketAddr>::decode(&long), None);
    }

    #[test]
    fn roles() {
        use Role::*;
        assert!(allowed(None, Client) && allowed(Some(Peer), Peer) && allowed(Some(Client), Host));
        assert!(allowed(Some(Host), Client) && allowed(Some(Host), Peer) && allowed(Some(Peer), Host));
        assert!(!allowed(Some(Client), Client) && !allowed(Some(Client), Peer) && !allowed(Some(Host), Host));
        assert!(!allowed(Some(Peer), Client));
    }

    /// Two traversals wired to each other by hand: offers are sealed, checked for
    /// freshness and replay, and turn into dials.
    #[test]
    fn offers_dial_and_replays_are_dropped() {
        let (ia, ib) = (id(1), id(2));
        let a_addr: SocketAddr = "198.51.100.1:5000".parse().unwrap();
        let b_addr: SocketAddr = "198.51.100.2:6000".parse().unwrap();
        let mut ea = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut eb = Endpoint::new(&ib, Config::default(), [2; 32]);
        let base = 1_800_000_000_000;
        let mut ta = Traversal::new(&ia, [3; 32], base, vec![a_addr], vec![]);
        let mut tb = Traversal::new(&ib, [4; 32], base, vec![b_addr], vec![]);
        let now = Instant::from_millis(10);
        assert_eq!(ta.poll_signal(), Some(Signal::Subscribe(Topic::peer(&ia.peer_id()))));
        ta.introduce(now, &ea, ib.peer_id());
        let Some(Signal::Publish(topic, sealed)) = ta.poll_signal() else { panic!() };
        assert_eq!(topic, Topic::peer(&ib.peer_id()));
        tb.handle_signal(now, &mut eb, topic, &sealed);
        let dial = eb.poll_transmit(now).expect("B dials A's candidate");
        assert_eq!(dial.dst, a_addr);
        tb.poll_signal(); // B's subscribe
        let Some(Signal::Publish(back, answer)) = tb.poll_signal() else { panic!("B answers") };
        assert_eq!(back, Topic::peer(&ia.peer_id()));
        ta.handle_signal(now, &mut ea, back, &answer);
        assert_eq!(ea.poll_transmit(now).map(|t| t.dst), Some(b_addr), "A dials B on the answer");
        assert_eq!(ta.poll_signal(), None, "an answer is not answered");

        // The same sealed offer again: dropped.
        let mut eb2 = Endpoint::new(&ib, Config::default(), [5; 32]);
        tb.handle_signal(now, &mut eb2, topic, &sealed);
        assert!(eb2.poll_transmit(now).is_none());
        // A stale one: dropped.
        let mut tb2 = Traversal::new(&ib, [6; 32], base + 2 * FRESH_MS, vec![b_addr], vec![]);
        tb2.handle_signal(now, &mut eb2, topic, &sealed);
        assert!(eb2.poll_transmit(now).is_none());
    }

    #[test]
    fn stun_classifies_the_nat() {
        let ia = id(1);
        let local: SocketAddr = "10.0.0.2:7000".parse().unwrap();
        let (s1, s2): (SocketAddr, SocketAddr) = ("192.0.2.1:3478".parse().unwrap(), "192.0.2.2:3478".parse().unwrap());
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let run = |ports: [u16; 2]| {
            let mut t = Traversal::new(&ia, [3; 32], 0, vec![local], vec![s1, s2]);
            let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
            assert_eq!(t.poll_timeout(), Some(Instant::ZERO));
            t.handle_timeout(Instant::ZERO, &mut ep);
            let reqs: Vec<_> = std::iter::from_fn(|| t.poll_transmit()).collect();
            assert_eq!(reqs.iter().map(|r| r.dst).collect::<Vec<_>>(), vec![s1, s2]);
            for (r, port) in reqs.iter().zip(ports) {
                let tx = stun::parse_binding_request(&r.payload).unwrap();
                let resp = stun::binding_response(&tx, SocketAddr::new([203, 0, 113, 9].into(), port));
                assert!(!t.handle_stun(&"192.0.2.99:3478".parse().unwrap(), b"not stun"));
                assert!(t.handle_stun(&r.dst, &resp));
            }
            (t.nat_type(), t.public_addr().copied())
        };
        assert_eq!(run([4000, 4000]), (NatType::Cone, Some("203.0.113.9:4000".parse().unwrap())));
        assert_eq!(run([4000, 4001]).0, NatType::Symmetric);
        // Unanswered: retransmitted twice, then given up until the next round.
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![local], vec![s1]);
        let mut sent = 0;
        while let Some(at) = t.poll_timeout().filter(|t| *t < Instant::from_millis(20_000)) {
            t.handle_timeout(at, &mut ep);
            sent += std::iter::from_fn(|| t.poll_transmit()).count();
        }
        assert_eq!((sent, t.nat_type()), (3, NatType::Unknown));
        assert_eq!(t.poll_timeout(), Some(Instant::ZERO + STUN_INTERVAL));
    }
}
