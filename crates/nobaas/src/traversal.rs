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
//!
//! Rooms are closed by their name. The room *topic* is a hash every relay sees, so
//! it proves nothing; a separate *room key*, hashed from the name under another
//! label, is known only to those given the name. Hellos and room offers carry an
//! HMAC under that key, and a node in a room links only with peers that proved it
//! (or that it dialled itself, from an invite). A room whose name can be guessed is
//! a public lobby.

use crate::signaling::{hex, unhex, Topic};
use hmac::{Hmac, Mac};
use nobaas_core::{sealed, stun, Address, Endpoint, Event, Identity, Instant, Path, PeerId, Transmit};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use sha2::{Digest, Sha256};
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
/// Hellos answered and offers acted on each have a budget: a burst of `BURST`, then
/// one per `REFILL`, so a flood can't make a node dial or publish without bound,
/// and a flood of one kind doesn't starve the other. Proofs are checked first, so
/// forged ones cost no token.
const BURST: u32 = 30;
const REFILL: Duration = Duration::from_secs(2);
/// Offers acted on per sender: a burst, then one per `SENDER_REFILL`, so one peer
/// can't spend the whole offer budget.
const SENDER_BURST: u32 = 8;
const SENDER_REFILL: Duration = Duration::from_secs(1);
/// Most offer nonces remembered (they are only kept for offers acted on).
const MAX_NONCES: usize = 1024;
const MAX_CANDIDATES: usize = 8;
const MAX_RELAYS: usize = 8;
/// Relays tried per dial.
const VIA_PER_DIAL: usize = 2;
const MAX_OBSERVED: usize = 4;
/// Peers forgotten after this long without news (unless connected).
const FORGET: Duration = Duration::from_secs(600);
/// Offer versions: 1 outside rooms (invites), 2 in a room, with a room proof.
const OFFER: u8 = 1;
const ROOM_OFFER: u8 = 2;
const PROOF_LEN: usize = 32;

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

/// A token bucket that starts full: `spent` tokens come back one per refill period.
#[derive(Clone, Copy, Default)]
struct Bucket {
    spent: u32,
    at: Instant,
}

impl Bucket {
    /// Refills, then says whether a token is left.
    fn ready(&mut self, now: Instant, burst: u32, every: Duration) -> bool {
        while self.spent > 0 && now >= self.at + every {
            self.spent -= 1;
            self.at = self.at + every;
        }
        if self.spent == 0 {
            self.at = now;
        }
        self.spent < burst
    }

    fn take(&mut self, now: Instant, burst: u32, every: Duration) -> bool {
        let ok = self.ready(now, burst, every);
        self.spent += ok as u32;
        ok
    }
}

#[derive(Default)]
struct Remote {
    offered_at: Option<Instant>,
    /// We received an offer from it.
    heard: bool,
    /// We dialled it from an invite: its plain (non-room) offers are welcome in a room.
    invited: bool,
    /// The role it proved in the current room (with the room key).
    member: Option<Role>,
    nat: NatType,
    failure: Option<Failure>,
    seen_at: Instant,
    /// Its offers acted on.
    offers: Bucket,
}

struct Room {
    topic: Topic,
    key: [u8; 32],
    role: Role,
    /// Link only with this peer (a client that knows its host's key).
    expect: Option<PeerId>,
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
    /// The socket is bound to loopback: loopback candidates are real.
    loopback: bool,
    servers: Vec<Server<A>>,
    next_round: Instant,
    public: Option<A>,
    nat: NatType,
    observed: Vec<A>,
    room: Option<Room>,
    remotes: BTreeMap<PeerId, Remote>,
    nonces: HashMap<(PeerId, u64), Instant>,
    hellos: Bucket,
    offers: Bucket,
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
        let is_loopback = |a: &A| a.to_socket_addr().is_some_and(|s| s.ip().to_canonical().is_loopback());
        Traversal {
            me: identity.clone(),
            id,
            inbox,
            rng: ChaCha20Rng::from_seed(seed),
            unix_ms,
            loopback: !local.is_empty() && local.iter().all(is_loopback),
            local,
            servers,
            next_round: Instant::ZERO,
            public: None,
            nat: NatType::Unknown,
            observed: Vec::new(),
            room: None,
            remotes: BTreeMap::new(),
            nonces: HashMap::new(),
            hellos: Bucket::default(),
            offers: Bucket::default(),
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
    /// and dials the peers that answer (as `role` allows) once they prove they know
    /// the room's name. With `host`, links with that peer only. Leaves any other room.
    pub fn join_room(&mut self, now: Instant, app_id: &str, room: &str, role: Role, host: Option<PeerId>) {
        self.leave_room();
        let topic = Topic::room(app_id, room);
        let key = room_key(app_id, room);
        self.signals.push_back(Signal::Subscribe(topic));
        self.signals.push_back(Signal::Publish(topic, hello(&key, &self.id, role)));
        self.room = Some(Room { topic, key, role, expect: host, next_hello: now + HELLO_FIRST, interval: HELLO_FIRST });
    }

    /// Stops announcing and answering in the current room. Links stay up.
    pub fn leave_room(&mut self) {
        if let Some(r) = self.room.take() {
            self.signals.push_back(Signal::Unsubscribe(r.topic));
        }
        for r in self.remotes.values_mut() {
            r.member = None;
        }
    }

    /// The role `peer` proved in the current room, if it did.
    pub fn room_role(&self, peer: &PeerId) -> Option<Role> {
        self.room.as_ref()?;
        self.remotes.get(peer)?.member
    }

    /// Whether a link with `peer` belongs: outside a room, any; in one, a peer that
    /// proved the room key in a role this node links with, or one it dialled itself.
    pub fn admits(&self, peer: &PeerId) -> bool {
        let Some(room) = &self.room else { return true };
        let Some(r) = self.remotes.get(peer) else { return false };
        r.invited || r.member.is_some_and(|role| allowed(Some(room.role), role) && room.expect.is_none_or(|h| h == *peer))
    }

    /// Sends `peer` an offer asking for one back, so a dial to it punches through
    /// NATs: call it after dialling a peer from an invite. In a room, it also marks
    /// `peer` as welcome though it never proved the room key.
    pub fn introduce(&mut self, now: Instant, ep: &Endpoint<A>, peer: PeerId) {
        self.offer(now, ep, peer, true, false);
        self.remotes.entry(peer).or_default().invited = true;
    }

    /// Takes a datagram if it is STUN (returns `true`); otherwise it is for the endpoint.
    pub fn handle_stun(&mut self, from: &A, data: &[u8]) -> bool {
        if !stun::is_stun(data) {
            return false;
        }
        let loopback = self.loopback;
        for s in self.servers.iter_mut().filter(|s| s.pending && &s.addr == from) {
            let m = stun::parse_binding_response(data, &s.tx).and_then(A::from_socket_addr);
            if let Some(m) = m.filter(|m| dialable(m, loopback)) {
                s.mapped = Some(m);
                s.pending = false;
            }
        }
        self.classify();
        true
    }

    /// Handles a message from the signaling carrier.
    pub fn handle_signal(&mut self, now: Instant, ep: &mut Endpoint<A>, topic: Topic, payload: &[u8]) {
        if let Some(room) = self.room.as_ref().filter(|r| r.topic == topic) {
            let Some((from, role, proof)) = parse_hello(payload) else { return };
            if mac_ok(&room.key, &[b"nb-hello", from.as_bytes(), &[role as u8]], &proof) {
                self.on_hello(now, ep, from, role);
            }
        } else if topic == self.inbox {
            let Some((from, body)) = sealed::open(&self.me, payload) else { return };
            // A room offer ends in an HMAC, under the room key, over sender, recipient and body.
            let (body, proof) = match body.first() {
                Some(&ROOM_OFFER) if body.len() > PROOF_LEN => {
                    let (b, p) = body.split_at(body.len() - PROOF_LEN);
                    (b, Some(p))
                }
                _ => (&body[..], None),
            };
            let welcome = match (&self.room, proof) {
                (Some(r), Some(p)) => mac_ok(&r.key, &[b"nb-offer", from.as_bytes(), self.id.as_bytes(), body], p),
                (None, None) => true,
                // In a room, plain offers only from peers this node dialled from an invite.
                (Some(_), None) => self.remotes.get(&from).is_some_and(|r| r.invited),
                (None, Some(_)) => false,
            };
            // A room offer too short to carry a proof is not a room offer.
            if let Some(offer) = Offer::decode(body).filter(|o| welcome && o.room == proof.is_some()) {
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
                    let room = self.room_role(&p).is_some();
                    self.offer(now, ep, p, true, room);
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
                let real = dialable(addr, false); // a peer that sees us on loopback is on this host
                if real && !self.observed.contains(addr) && !self.local.contains(addr) {
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
            let h = hello(&r.key, &self.id, r.role);
            self.signals.push_back(Signal::Publish(r.topic, h));
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

    /// Whether this node, in its room (if any), links with `peer` announcing `role`.
    fn links_with(&self, peer: &PeerId, role: Role) -> bool {
        let expect = self.room.as_ref().and_then(|r| r.expect);
        *peer != self.id && allowed(self.role(), role) && expect.is_none_or(|h| h == *peer)
    }

    /// A hello whose room proof checked out.
    fn on_hello(&mut self, now: Instant, ep: &Endpoint<A>, from: PeerId, role: Role) {
        if !self.links_with(&from, role) {
            return;
        }
        if let Some(r) = self.remotes.get_mut(&from) {
            r.member = Some(role);
        }
        if ep.is_connected(&from) {
            return;
        }
        let recent = self.remotes.get(&from).and_then(|r| r.offered_at).is_some_and(|t| now - t < OFFER_GAP);
        if !recent && self.hellos.take(now, BURST, REFILL) {
            self.offer(now, ep, from, true, true);
            self.remotes.entry(from).or_default().member = Some(role);
        }
    }

    /// An offer that opened, and whose room proof (in a room) checked out.
    fn on_offer(&mut self, now: Instant, ep: &mut Endpoint<A>, from: PeerId, o: Offer<A>) {
        let room = o.room;
        let role_ok = if room { self.links_with(&from, o.role) } else { from != self.id };
        if !role_ok || self.wall_ms(now).abs_diff(o.ts) > FRESH_MS || self.nonces.contains_key(&(from, o.nonce)) {
            return; // not for us, stale, or a replay
        }
        // The sender's own limit, then the node's; the nonce is kept only for offers acted on.
        let mut mine = self.remotes.get(&from).map(|r| r.offers).unwrap_or_default();
        if !mine.ready(now, SENDER_BURST, SENDER_REFILL) || !self.offers.take(now, BURST, REFILL) {
            return;
        }
        mine.take(now, SENDER_BURST, SENDER_REFILL);
        if self.nonces.len() >= MAX_NONCES {
            return;
        }
        self.nonces.insert((from, o.nonce), now);
        let r = self.remotes.entry(from).or_default();
        (r.heard, r.nat, r.seen_at, r.offers) = (true, o.nat, now, mine);
        if room {
            r.member = Some(o.role);
        }
        let loopback = self.loopback;
        let addrs: Vec<A> = o.addrs.into_iter().filter(|a| dialable(a, loopback) && !self.local.contains(a)).collect();
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
            self.offer(now, ep, from, false, room);
        }
    }

    /// Seals an offer to `to` and publishes it on its topic. `room`: a room offer,
    /// with a proof under the room key (sent only while in a room).
    fn offer(&mut self, now: Instant, ep: &Endpoint<A>, to: PeerId, reply: bool, room: bool) {
        let key = match (&self.room, room) {
            (Some(r), true) => Some(r.key),
            (None, true) => return,
            (_, false) => None,
        };
        let relays = ep.connected_peers().filter(|p| **p != to).take(MAX_RELAYS).copied().collect();
        let o = Offer {
            room,
            reply,
            role: self.role().unwrap_or(Role::Peer),
            nat: self.nat,
            ts: self.wall_ms(now),
            nonce: self.rng.next_u64(),
            addrs: self.candidates(),
            relays,
        };
        let mut body = o.encode();
        if let Some(k) = key {
            let proof = mac(&k, &[b"nb-offer", self.id.as_bytes(), to.as_bytes(), &body]);
            body.extend_from_slice(&proof);
        }
        let mut seed = [0u8; 32];
        self.rng.fill_bytes(&mut seed);
        if let Some(s) = sealed::seal(&self.me, &to, seed, &body) {
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

/// Whether an address from a peer or STUN is worth dialling: never port 0, an
/// unspecified, multicast or broadcast address, and loopback only for a node bound
/// to loopback (so an offer can't aim a node at services on its own host).
fn dialable<A: Address>(a: &A, loopback: bool) -> bool {
    let Some(s) = a.to_socket_addr() else { return true };
    let ip = s.ip().to_canonical();
    let broadcast = matches!(ip, std::net::IpAddr::V4(v4) if v4.is_broadcast());
    s.port() != 0 && !ip.is_unspecified() && !ip.is_multicast() && !broadcast && (loopback || !ip.is_loopback())
}

/// The key that proves knowledge of a room's name. Not the topic, which every relay
/// sees: the same name hashed under another label.
fn room_key(app_id: &str, room: &str) -> [u8; 32] {
    Sha256::digest(format!("nobaas/room-key/{app_id}/{room}")).into()
}

fn hmac(key: &[u8; 32], parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m
}

/// HMAC-SHA256 of `parts`, concatenated, under `key`.
fn mac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    hmac(key, parts).finalize().into_bytes().into()
}

/// Checks a proof from [`mac`] in constant time.
fn mac_ok(key: &[u8; 32], parts: &[&[u8]], proof: &[u8]) -> bool {
    hmac(key, parts).verify_slice(proof).is_ok()
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
const HELLO_PROOF: &str = r#"","p":""#;
const HELLO_TAIL: &str = r#""}}"#;

/// The room presence message, in the JSON shape `nostr.js` uses (`{t, from, meta}`).
/// `meta.nb` marks a UDP peer, so browsers' WebRTC hellos are told apart; `meta.p`
/// is the room proof, an HMAC under the room key over the sender and its role.
fn hello(key: &[u8; 32], id: &PeerId, role: Role) -> Vec<u8> {
    let proof = mac(key, &[b"nb-hello", id.as_bytes(), &[role as u8]]);
    let (id, name, proof) = (hex(id.as_bytes()), role_name(role), hex(&proof));
    format!("{HELLO_HEAD}{id}{HELLO_MID}{name}{HELLO_PROOF}{proof}{HELLO_TAIL}").into_bytes()
}

fn parse_hello(b: &[u8]) -> Option<(PeerId, Role, [u8; 32])> {
    let s = std::str::from_utf8(b).ok()?;
    let rest = s.strip_prefix(HELLO_HEAD)?;
    let (id_hex, rest) = (rest.get(..64)?, rest.get(64..)?);
    let (role, proof_hex) = rest.strip_prefix(HELLO_MID)?.strip_suffix(HELLO_TAIL)?.split_once(HELLO_PROOF)?;
    let role = [Role::Peer, Role::Host, Role::Client].into_iter().find(|r| role_name(*r) == role)?;
    let (mut id, mut proof) = ([0u8; 32], [0u8; 32]);
    (unhex(id_hex, &mut id) && unhex(proof_hex, &mut proof)).then_some((PeerId(id), role, proof))
}

/// Candidates for one peer. Sealed to it, so only it reads them.
#[derive(Debug, PartialEq)]
struct Offer<A> {
    /// A room offer (version 2): the sealed body is followed by a room proof.
    room: bool,
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
    // [1|2] [flags] [role] [nat] [ts u64 le] [nonce u64 le] [n] n*([len] addr) [m] m*(32 B peer id)
    // A room offer (2) is followed, outside this body, by its 32-byte proof.
    fn encode(&self) -> Vec<u8> {
        let role = [Role::Peer, Role::Host, Role::Client].iter().position(|r| *r == self.role).unwrap() as u8;
        let nat = [NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].iter().position(|n| *n == self.nat);
        let version = if self.room { ROOM_OFFER } else { OFFER };
        let mut v = vec![version, self.reply as u8, role, nat.unwrap() as u8];
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
        let (&[version @ (OFFER | ROOM_OFFER), flags, role, nat], rest) = b.split_first_chunk::<4>()? else { return None };
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
            room: version == ROOM_OFFER,
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
        let key = room_key("app", "room");
        for role in [Role::Peer, Role::Host, Role::Client] {
            let (from, r, proof) = parse_hello(&hello(&key, &p, role)).unwrap();
            assert_eq!((from, r), (p, role));
            assert!(mac_ok(&key, &[b"nb-hello", p.as_bytes(), &[role as u8]], &proof));
            assert!(!mac_ok(&room_key("app", "other"), &[b"nb-hello", p.as_bytes(), &[role as u8]], &proof));
            assert!(!mac_ok(&key, &[b"nb-hello", p.as_bytes(), &[9]], &proof), "bound to the role");
        }
        let js = format!(r#"{{"t":"hello","from":"{}","meta":{{"role":"player"}}}}"#, hex(p.as_bytes()));
        assert_eq!(parse_hello(js.as_bytes()), None);
        assert_eq!(parse_hello(b"{}"), None);
        let h = hello(&key, &p, Role::Peer);
        for cut in 0..h.len() {
            assert_eq!(parse_hello(&h[..cut]), None);
        }
        // The key is not the topic, nor derivable from it.
        assert_ne!(key, Topic::room("app", "room").0);
    }

    #[test]
    fn offer_round_trips_and_rejects_garbage() {
        let o: Offer<SocketAddr> = Offer {
            room: false,
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
        let room: Offer<SocketAddr> = Offer { room: true, ..Offer::decode(&b).unwrap() };
        let rb = room.encode();
        assert_eq!(rb[0], ROOM_OFFER);
        assert_eq!(Offer::decode(&rb), Some(room));
        let mut v3 = rb.clone();
        v3[0] = 3;
        assert_eq!(Offer::<SocketAddr>::decode(&v3), None);
    }

    const ROOM: (&str, &str) = ("app", "room");

    /// A sealed offer from `from` to `to`, with a room proof under `key` if given.
    fn offer_from(from: &Identity, to: &PeerId, key: Option<&[u8; 32]>, nonce: u64, role: Role, addrs: Vec<SocketAddr>) -> Vec<u8> {
        let o = Offer { room: key.is_some(), reply: true, role, nat: NatType::Cone, ts: 0, nonce, addrs, relays: vec![] };
        let mut body = o.encode();
        if let Some(k) = key {
            body.extend(mac(k, &[b"nb-offer", from.peer_id().as_bytes(), to.as_bytes(), &body]));
        }
        sealed::seal(from, to, [7; 32], &body).unwrap()
    }

    /// A traversal (`id(1)`) in `ROOM` as `role`, with its endpoint and drained signals.
    fn in_room(role: Role, host: Option<PeerId>, local: SocketAddr) -> (Traversal<SocketAddr>, Endpoint<SocketAddr>) {
        let ia = id(1);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![local], vec![]);
        t.join_room(Instant::ZERO, ROOM.0, ROOM.1, role, host);
        while t.poll_signal().is_some() {}
        (t, Endpoint::new(&ia, Config::default(), [1; 32]))
    }

    fn published(t: &mut Traversal<SocketAddr>) -> usize {
        std::iter::from_fn(|| t.poll_signal()).filter(|s| matches!(s, Signal::Publish(..))).count()
    }

    fn dials(ep: &mut Endpoint<SocketAddr>, now: Instant) -> Vec<SocketAddr> {
        std::iter::from_fn(|| ep.poll_transmit(now)).map(|t| t.dst).collect()
    }

    /// NB-031: knowing the room topic (what relays see) is not enough to be linked
    /// with, nor to be taken for the host.
    #[test]
    fn room_offers_and_hellos_need_the_room_key() {
        let me = id(1).peer_id();
        let local: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let (key, topic, inbox) = (room_key(ROOM.0, ROOM.1), Topic::room(ROOM.0, ROOM.1), Topic::peer(&me));
        let now = Instant::from_millis(10);
        let (mut t, mut ep) = in_room(Role::Client, None, local);
        let attacker = id(9);
        // A plain offer, and one "proved" with the topic hash as key: dropped.
        t.handle_signal(now, &mut ep, inbox, &offer_from(&attacker, &me, None, 1, Role::Host, vec![there]));
        t.handle_signal(now, &mut ep, inbox, &offer_from(&attacker, &me, Some(&topic.0), 2, Role::Host, vec![there]));
        // Hellos without a proof (the old format), or with a wrong one: ignored.
        let ap = attacker.peer_id();
        let old = format!(r#"{{"t":"hello","from":"{}","meta":{{"nb":1,"role":"host"}}}}"#, hex(ap.as_bytes()));
        t.handle_signal(now, &mut ep, topic, old.as_bytes());
        t.handle_signal(now, &mut ep, topic, &hello(&topic.0, &ap, Role::Host));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        assert_eq!((t.room_role(&ap), t.admits(&ap)), (None, false));
        // A member's offer: dialled, answered, and its role is known.
        let host = id(2);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 3, Role::Host, vec![there]));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![there], 1));
        assert_eq!((t.room_role(&host.peer_id()), t.admits(&host.peer_id())), (Some(Role::Host), true));
        // A member's hello in a role this client doesn't link with: no offer.
        t.handle_signal(now, &mut ep, topic, &hello(&key, &id(3).peer_id(), Role::Client));
        assert_eq!(published(&mut t), 0);
        // Leaving the room forgets who proved what; outside a room, links are anyone's call.
        t.leave_room();
        assert_eq!((t.room_role(&host.peer_id()), t.admits(&ap)), (None, true));

        // With an expected host, other members are ignored.
        let (mut t, mut ep) = in_room(Role::Client, Some(id(3).peer_id()), local);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 4, Role::Host, vec![there]));
        t.handle_signal(now, &mut ep, topic, &hello(&key, &host.peer_id(), Role::Host));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        t.handle_signal(now, &mut ep, topic, &hello(&key, &id(3).peer_id(), Role::Host));
        assert_eq!(published(&mut t), 1);
        assert!(t.admits(&id(3).peer_id()) && !t.admits(&host.peer_id()));
    }

    /// An invite dial still punches from inside a room: the dialled peer's plain
    /// offer is welcome, nobody else's.
    #[test]
    fn invited_peers_may_answer_with_plain_offers_in_a_room() {
        let me = id(1).peer_id();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let now = Instant::from_millis(10);
        t.introduce(now, &ep, id(2).peer_id());
        assert_eq!(published(&mut t), 1);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &offer_from(&id(2), &me, None, 1, Role::Peer, vec![there]));
        assert_eq!(dials(&mut ep, now), vec![there]);
        assert!(t.admits(&id(2).peer_id()) && t.room_role(&id(2).peer_id()).is_none());
    }

    /// A v2 (room) offer short enough to have no proof, from an invited peer, is
    /// dropped: it must not give that peer a room role such as Host.
    #[test]
    fn short_room_offer_without_proof_is_dropped() {
        let me = id(1).peer_id();
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        let now = Instant::from_millis(10);
        t.introduce(now, &ep, id(2).peer_id());
        while t.poll_signal().is_some() {}
        let o = Offer::<SocketAddr> {
            room: true, reply: true, role: Role::Host, nat: NatType::Cone, ts: 0, nonce: 1, addrs: vec![], relays: vec![],
        };
        let body = o.encode();
        assert!(body.len() <= PROOF_LEN, "short enough to skip the proof split");
        let sealed = sealed::seal(&id(2), &me, [7; 32], &body).unwrap();
        t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed);
        assert_eq!(t.room_role(&id(2).peer_id()), None);
        assert_eq!(published(&mut t), 0);
    }

    /// NB-032: forged hellos cost nothing, and hellos, offers and each sender
    /// spend separate budgets.
    #[test]
    fn hellos_offers_and_senders_have_separate_budgets() {
        let me = id(1).peer_id();
        let (key, topic, inbox) = (room_key(ROOM.0, ROOM.1), Topic::room(ROOM.0, ROOM.1), Topic::peer(&me));
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let t0 = Instant::ZERO;
        let member = |n: u8| id(n.wrapping_add(10)).peer_id();
        // 200 forged hellos: nothing sent, no token spent.
        for n in 0..200u8 {
            t.handle_signal(t0, &mut ep, topic, &hello(&topic.0, &member(n), Role::Peer));
        }
        assert_eq!(published(&mut t), 0);
        // Real hellos from 100 members: a burst of answers, then none.
        for n in 0..100u8 {
            t.handle_signal(t0, &mut ep, topic, &hello(&key, &member(n), Role::Peer));
        }
        assert_eq!(published(&mut t), BURST as usize);
        // Offers have their own budget: still acted on (each answered).
        for n in 0..5u8 {
            let from = id(200 + n);
            t.handle_signal(t0, &mut ep, inbox, &offer_from(&from, &me, Some(&key), 1, Role::Peer, vec![]));
        }
        assert_eq!(published(&mut t), 5);
        // One sender gets SENDER_BURST, and leaves the rest of the budget to others.
        let loud = id(220);
        for nonce in 0..50 {
            t.handle_signal(t0, &mut ep, inbox, &offer_from(&loud, &me, Some(&key), nonce, Role::Peer, vec![]));
        }
        assert_eq!(published(&mut t), SENDER_BURST as usize);
        t.handle_signal(t0, &mut ep, inbox, &offer_from(&id(221), &me, Some(&key), 1, Role::Peer, vec![]));
        assert_eq!(published(&mut t), 1);
        // The sender's limit refills.
        let later = t0 + SENDER_REFILL * 2;
        for nonce in 100..110 {
            t.handle_signal(later, &mut ep, inbox, &offer_from(&loud, &me, Some(&key), nonce, Role::Peer, vec![]));
        }
        assert_eq!(published(&mut t), 2);
        // And the hello budget at one per REFILL.
        let later = t0 + REFILL * 3;
        for n in 100..150u8 {
            t.handle_signal(later, &mut ep, topic, &hello(&key, &member(n), Role::Peer));
        }
        assert_eq!(published(&mut t), 3);
    }

    /// NB-032(b): nonces are kept only for offers acted on, and the map is capped.
    #[test]
    fn nonces_are_kept_only_for_offers_acted_on() {
        let ia = id(1);
        let me = ia.peer_id();
        let mut ep: Endpoint<SocketAddr> = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec!["10.0.0.1:7000".parse().unwrap()], vec![]);
        for n in 0..200u8 {
            t.handle_signal(Instant::ZERO, &mut ep, Topic::peer(&me), &offer_from(&id(n.wrapping_add(2)), &me, None, 1, Role::Peer, vec![]));
        }
        assert_eq!(t.nonces.len(), BURST as usize);
        assert!(MAX_NONCES >= (BURST as usize + (2 * FRESH_MS as usize / REFILL.as_millis() as usize)), "covers the window");
    }

    /// NB-033(a): an offer can't aim a node at its own host, multicast, broadcast,
    /// the unspecified address or port 0; loopback only between loopback nodes.
    #[test]
    fn offers_cannot_aim_at_special_addresses() {
        let me = id(1).peer_id();
        let addrs: Vec<SocketAddr> = ["198.51.100.7:9", "127.0.0.1:5", "[::ffff:127.0.0.1]:5", "0.0.0.0:5", "224.0.0.1:5",
            "[ff02::1]:5", "255.255.255.255:5", "198.51.100.7:0"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        let now = Instant::from_millis(10);
        let run = |local: &str| {
            let ia = id(1);
            let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
            let mut t = Traversal::new(&ia, [3; 32], 0, vec![local.parse().unwrap()], vec![]);
            t.handle_signal(now, &mut ep, Topic::peer(&me), &offer_from(&id(2), &me, None, 1, Role::Peer, addrs.clone()));
            let mut d = dials(&mut ep, now);
            d.sort();
            d.dedup();
            d
        };
        assert_eq!(run("10.0.0.1:7000"), vec!["198.51.100.7:9".parse().unwrap()]);
        let on_lo = run("127.0.0.1:7000");
        assert!(on_lo.contains(&"127.0.0.1:5".parse().unwrap()) && on_lo.len() == 3, "{on_lo:?}");
        // Observed addresses are filtered the same way.
        let ia = id(1);
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec!["10.0.0.1:7000".parse().unwrap()], vec![]);
        for a in &addrs {
            t.handle_event(now, &mut ep, &Event::AddrObserved { by: id(2).peer_id(), addr: *a });
        }
        assert_eq!(t.candidates(), vec!["198.51.100.7:9".parse().unwrap(), "10.0.0.1:7000".parse().unwrap()]);
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
