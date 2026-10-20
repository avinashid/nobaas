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
//! 4. A symmetric NAT picks a new port for each peer, so the address it offers is
//!    not the one its packets come from. If it hands ports out in sequence, it
//!    offers a prediction and the other side probes the ports around it.
//! 5. Where punching can't work (symmetric against port-filtering NATs), each also
//!    dials through a peer both are connected to ([`Path::Via`]); core moves the
//!    link off the relay if a direct path turns up. With no such peer the dial
//!    fails with [`Failure::Symmetric`]: there is no TURN server, by design.
//!
//! Rooms are closed by their name. The room *topic* is a hash every relay sees, so
//! it proves nothing; a separate *room key*, hashed from the name under another
//! label, is known only to those given the name. Hellos and room offers carry an
//! HMAC under that key, and a node in a room links only with peers that proved it
//! (or that it dialled itself, from an invite). A room whose name can be guessed is
//! a public lobby. Hellos carry the sender's clock and are dropped when stale.
//!
//! Outside rooms, a node acts on an offer only from a peer that holds its invite
//! (the offer carries an HMAC under the invite's key) or that it dialled itself:
//! knowing a [`PeerId`] is not enough to make a node dial back or reveal its
//! addresses.
//!
//! [`Privacy`] decides which addresses a node gives out: all of them, public ones
//! only, or none (every link but the one to its relay goes through the relay).

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
/// Offer versions: 1 plain (only to peers that dialled us), 2 in a room, with a
/// room proof, 3 with an invite proof (from a peer dialling us from our invite).
const OFFER: u8 = 1;
const ROOM_OFFER: u8 = 2;
const INVITE_OFFER: u8 = 3;
const PROOF_LEN: usize = 32;
/// A symmetric NAT whose two STUN mappings are 1 to `MAX_STEP` ports apart, upwards,
/// looks sequential: its next mapping is predictable.
const MAX_STEP: u16 = 8;
/// Ports probed around a predicted one. The NAT may have handed out a few more
/// since STUN asked, and ports go up, so the window leans forward.
const GUESS_BEFORE: i32 = 4;
const GUESS_AFTER: i32 = 27;
/// Starts the port prediction entry in an offer's address list. Decoders that
/// predate it skip it, as an address they can't read.
const PREDICTION_TAG: [u8; 2] = [0xfe, 1];

/// What STUN says about this node's NAT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NatType {
    /// No STUN answer yet, or only one server answered.
    #[default]
    Unknown,
    /// No NAT: the public address is the socket's own.
    Open,
    /// The NAT keeps one public port for every destination (full, restricted or
    /// port-restricted cone). Punching works with other cone and open peers, and
    /// usually with symmetric ones, but not always: a port-restricted cone only
    /// lets in packets from ports it has sent to, so it can't punch a symmetric
    /// NAT that picks its ports at random. STUN can't tell the cone kinds apart.
    Cone,
    /// The NAT picks a new public port per destination: punching works towards
    /// open, full-cone and restricted-cone peers, towards port-restricted ones
    /// only if the ports come in a predictable sequence, and two symmetric peers
    /// need a relay.
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

/// Which of its addresses a node gives out. Peers it links with directly still see
/// the address its packets come from; only [`Privacy::RelayOnly`] hides that.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Privacy {
    /// Every candidate (public, LAN, loopback): the lowest latency, and peers on
    /// one LAN link over it.
    #[default]
    Direct,
    /// Public addresses only: LAN, loopback, link-local, CGNAT (100.64/10) and
    /// other private ranges are left out of invites and offers. Two peers behind
    /// one NAT then link only if it hairpins, or through a relay.
    PublicOnly,
    /// No addresses at all, except to the relay: every other link goes through
    /// it ([`Path::Via`]), and never moves to a direct path. The relay is `via`,
    /// or, in a host/client room, the host. Costs the relay's upload (it carries
    /// every packet twice) and a detour in latency.
    RelayOnly { via: Option<PeerId> },
}

/// Why a dial to a peer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No handshake answer on any address: the peer is gone, or a firewall drops UDP.
    Unreachable,
    /// The NAT on `Side` is symmetric (a new public port per destination) and the
    /// other side's router only lets in packets from ports it has sent to, so
    /// punching can't work, and no peer connected to both could relay. Nobaas has
    /// no TURN server; a third peer in the room that both can reach fixes it.
    Symmetric(Side),
}

/// Which end of a link a [`Failure`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// This node.
    Ours,
    /// The peer.
    Theirs,
    /// Both.
    Both,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sym = "is symmetric (a new public port per destination)";
        let filters = "router only lets in packets from ports it has sent to";
        let end = "so hole punching can't work, and no peer linked to both could relay";
        match self {
            Failure::Unreachable => write!(f, "no answer on any address: the peer is gone, or a firewall drops UDP"),
            Failure::Symmetric(Side::Ours) => write!(f, "this side's NAT {sym} and the peer's {filters}, {end}"),
            Failure::Symmetric(Side::Theirs) => write!(f, "the peer's NAT {sym} and this side's {filters}, {end}"),
            Failure::Symmetric(Side::Both) => write!(f, "both NATs are symmetric (a new port per destination), {end}"),
        }
    }
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
    /// We dialled it from an invite: its plain offers are welcome (in a room too).
    invited: bool,
    /// Its invite's key, which our offers to it prove we hold.
    key: Option<[u8; 32]>,
    /// The role it proved in the current room (with the room key).
    member: Option<Role>,
    nat: NatType,
    failure: Option<Failure>,
    /// The links it named in its last offer acted on: if one links with us later,
    /// we dial it through that one (we never name ours to it to make that happen).
    relays: Vec<PeerId>,
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
    /// The port a sequential symmetric NAT will likely pick next, at `public`'s IP.
    prediction: Option<Prediction>,
    observed: Vec<A>,
    room: Option<Room>,
    remotes: BTreeMap<PeerId, Remote>,
    nonces: HashMap<(PeerId, u64), Instant>,
    hellos: Bucket,
    offers: Bucket,
    privacy: Privacy,
    /// The room Host a relay-only node picked as its relay (with no `via` and no
    /// pinned host): the first it dialled. Kept while that dial or link lasts.
    picked: Option<PeerId>,
    /// Whoever holds this, from our invite, may ask us to dial back.
    invite_key: [u8; 32],
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
        let mut rng = ChaCha20Rng::from_seed(seed);
        let mut invite_key = [0u8; 32];
        rng.fill_bytes(&mut invite_key);
        Traversal {
            me: identity.clone(),
            id,
            inbox,
            rng,
            unix_ms,
            loopback: !local.is_empty() && local.iter().all(is_loopback),
            local,
            servers,
            next_round: Instant::ZERO,
            public: None,
            nat: NatType::Unknown,
            prediction: None,
            observed: Vec::new(),
            room: None,
            remotes: BTreeMap::new(),
            nonces: HashMap::new(),
            hellos: Bucket::default(),
            offers: Bucket::default(),
            privacy: Privacy::Direct,
            picked: None,
            invite_key,
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

    /// Where a symmetric NAT that hands out ports in sequence will likely map
    /// this node's next new destination, from the step between its two STUN
    /// mappings. Offers carry it, and cone peers probe the ports around it.
    pub fn predicted_addr(&self) -> Option<A> {
        let (public, p) = (self.public.as_ref()?, self.prediction?);
        public.port_offset(i32::from(p.next) - i32::from(public.to_socket_addr()?.port()))
    }

    /// Why the last dial to `peer` failed, until it connects.
    pub fn failure(&self, peer: &PeerId) -> Option<Failure> {
        self.remotes.get(peer).and_then(|r| r.failure)
    }

    /// The addresses given out (in invites), as [`Privacy`] allows: public (STUN),
    /// observed by peers, local. None under [`Privacy::RelayOnly`].
    pub fn candidates(&self) -> Vec<A> {
        match self.privacy {
            Privacy::Direct => self.addresses(false),
            Privacy::PublicOnly => self.addresses(true),
            Privacy::RelayOnly { .. } => Vec::new(),
        }
    }

    /// The addresses offered to `to`: [`candidates`](Traversal::candidates), but
    /// a relay-only node's relay gets its public ones (it links directly).
    fn candidates_for(&self, to: &PeerId) -> Vec<A> {
        match self.privacy {
            Privacy::RelayOnly { .. } if self.relay() == Some(*to) => self.addresses(true),
            _ => self.candidates(),
        }
    }

    fn addresses(&self, public_only: bool) -> Vec<A> {
        let mut out: Vec<A> = Vec::new();
        for a in self.public.iter().chain(&self.observed).chain(&self.local) {
            if !out.contains(a) && out.len() < MAX_CANDIDATES && (!public_only || is_public(a)) {
                out.push(a.clone());
            }
        }
        out
    }

    /// Which addresses this node gives out.
    pub fn privacy(&self) -> Privacy {
        self.privacy
    }

    /// Sets which addresses this node gives out from now on (offers and invites
    /// made later), and, for [`Privacy::RelayOnly`], keeps `ep`'s relayed links
    /// relayed. Links already up keep their path.
    pub fn set_privacy(&mut self, ep: &mut Endpoint<A>, privacy: Privacy) {
        self.privacy = privacy;
        ep.set_relay_only(matches!(privacy, Privacy::RelayOnly { .. }));
    }

    /// The peer a [`Privacy::RelayOnly`] node links through: `via`, else the room's
    /// pinned host, else the first peer that proved the Host role and was dialled.
    /// That pick sticks while its dial or link lasts: another Host can't take over.
    pub fn relay(&self) -> Option<PeerId> {
        let Privacy::RelayOnly { via } = self.privacy else { return None };
        via.or_else(|| self.room.as_ref()?.expect).or(self.picked)
    }

    /// The key this node's invites carry: an offer proved with it is from a peer
    /// holding one, which may ask this node to dial back.
    pub fn invite_key(&self) -> [u8; 32] {
        self.invite_key
    }

    /// Joins a room: announces this node on the room topic now and now and then,
    /// and dials the peers that answer (as `role` allows) once they prove they know
    /// the room's name. With `host`, links with that peer only. Leaves any other room.
    pub fn join_room(&mut self, now: Instant, app_id: &str, room: &str, role: Role, host: Option<PeerId>) {
        self.leave_room();
        self.picked = None;
        let topic = Topic::room(app_id, room);
        let key = room_key(app_id, room);
        self.signals.push_back(Signal::Subscribe(topic));
        self.signals.push_back(Signal::Publish(topic, hello(&key, &self.id, role, self.wall_ms(now))));
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
        r.invited
            || r.member.is_some_and(|role| allowed(Some(room.role), role) && room.expect.is_none_or(|h| h == *peer))
    }

    /// Sends `peer` an offer asking for one back, so a dial to it punches through
    /// NATs: call it after dialling a peer from an invite, with the invite's `key`
    /// (without one, the peer ignores the offer unless it dialled us too). Its
    /// offers are welcome from then on, in a room too, though it never proved the
    /// room key.
    pub fn introduce(&mut self, now: Instant, ep: &Endpoint<A>, peer: PeerId, key: Option<[u8; 32]>) {
        let r = self.remotes.entry(peer).or_default();
        (r.invited, r.key) = (true, key.or(r.key));
        self.offer(now, ep, peer, true, false);
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
            let Some((from, role, ts, proof)) = parse_hello(payload) else { return };
            let fresh = self.wall_ms(now).abs_diff(ts) <= FRESH_MS;
            if fresh && mac_ok(&room.key, &[b"nb-hello", from.as_bytes(), &[role as u8], &ts.to_le_bytes()], &proof) {
                self.on_hello(now, ep, from, role);
            }
        } else if topic == self.inbox {
            let Some((from, body)) = sealed::open(&self.me, payload) else { return };
            // Room and invite offers end in an HMAC over sender, recipient and body,
            // under the room key or this node's invite key.
            let (body, proof) = match body.first() {
                Some(&(ROOM_OFFER | INVITE_OFFER)) if body.len() > PROOF_LEN => {
                    let (b, p) = body.split_at(body.len() - PROOF_LEN);
                    (b, Some(p))
                }
                _ => (&body[..], None),
            };
            let parts = |label: &'static [u8]| [label, from.as_bytes(), self.id.as_bytes(), body];
            let welcome = match (body.first(), proof) {
                (Some(&ROOM_OFFER), Some(p)) => {
                    self.room.as_ref().is_some_and(|r| mac_ok(&r.key, &parts(b"nb-offer"), p))
                }
                (Some(&INVITE_OFFER), Some(p)) => mac_ok(&self.invite_key, &parts(b"nb-invite"), p),
                // Plain offers only from peers this node dialled itself.
                (Some(&OFFER), None) => self.remotes.get(&from).is_some_and(|r| r.invited),
                // A room or invite offer too short to carry a proof.
                _ => false,
            };
            if let Some(offer) = Offer::decode(body).filter(|_| welcome) {
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
                        *p != peer
                            && r.heard
                            && !ep.is_connected(p)
                            && r.offered_at.is_none_or(|t| now - t >= OFFER_GAP)
                    })
                    .map(|(p, _)| *p)
                    .collect();
                for p in waiting {
                    let room = self.room_role(&p).is_some();
                    self.offer(now, ep, p, true, room);
                }
                // A peer that named this link as a relay is still waiting: dial it through
                // the link (a relay-only node dials only through its relay, below).
                if !matches!(self.privacy, Privacy::RelayOnly { .. }) {
                    let waiting: Vec<PeerId> = self
                        .remotes
                        .iter()
                        .filter(|(p, r)| r.heard && r.relays.contains(peer) && !ep.is_connected(p))
                        .map(|(p, _)| *p)
                        .collect();
                    for p in waiting {
                        let _ = ep.connect_with(now, p, &[Path::Via(*peer)]);
                    }
                }
                // A relay-only node's relay just linked: dial the peers waiting for it.
                if self.relay() == Some(*peer) {
                    let waiting: Vec<PeerId> =
                        self.remotes.iter().filter(|(p, r)| r.heard && !ep.is_connected(p)).map(|(p, _)| *p).collect();
                    for p in waiting {
                        let _ = ep.connect_with(now, p, &[Path::Via(*peer)]);
                    }
                }
            }
            Event::ConnectFailed { peer } => {
                if self.picked == Some(*peer) {
                    self.picked = None; // another Host may be tried
                }
                let mine = self.nat;
                let r = self.remotes.entry(*peer).or_default();
                r.seen_at = now;
                let sym = |a: NatType, b: NatType| a == NatType::Symmetric && b != NatType::Open;
                r.failure = Some(match (sym(mine, r.nat), sym(r.nat, mine)) {
                    (true, true) => Failure::Symmetric(Side::Both),
                    (true, false) => Failure::Symmetric(Side::Ours),
                    (false, true) => Failure::Symmetric(Side::Theirs),
                    (false, false) => Failure::Unreachable,
                });
            }
            Event::Disconnected { peer, .. } if self.picked == Some(*peer) => self.picked = None,
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
            let h = hello(&r.key, &self.id, r.role, self.unix_ms + now.as_micros() / 1000);
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
        self.prediction = match (self.nat, &mapped[..]) {
            (NatType::Symmetric, [a, b, ..]) => Prediction::from_mappings(*a, *b),
            _ => None,
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
        let room = o.kind == Proof::Room;
        let role_ok = if room { self.links_with(&from, o.role) } else { from != self.id };
        if !role_ok || self.wall_ms(now).abs_diff(o.ts) > FRESH_MS || self.nonces.contains_key(&(from, o.nonce)) {
            return; // not for us, stale, or a replay
        }
        // The sender's own limit, then the node's, which peers we dialled don't
        // draw on (answers to our own dials can't be crowded out); the nonce is
        // kept only for offers acted on.
        let (mut mine, invited) = self.remotes.get(&from).map_or((Bucket::default(), false), |r| (r.offers, r.invited));
        if !mine.ready(now, SENDER_BURST, SENDER_REFILL) || !(invited || self.offers.take(now, BURST, REFILL)) {
            return;
        }
        mine.take(now, SENDER_BURST, SENDER_REFILL);
        if self.nonces.len() >= MAX_NONCES {
            return;
        }
        self.nonces.insert((from, o.nonce), now);
        let r = self.remotes.entry(from).or_default();
        (r.heard, r.nat, r.seen_at, r.offers) = (true, o.nat, now, mine);
        r.relays = o.relays.iter().filter(|p| **p != from && **p != self.id).take(MAX_RELAYS).copied().collect();
        if room {
            r.member = Some(o.role);
        }
        let loopback = self.loopback;
        // A sequential symmetric NAT maps the sender's packets to us at a port
        // near its prediction: probe those too (pointless from an open or
        // symmetric NAT, which take any packet or would burn a port per probe).
        // Only around a public address: senders predict only for theirs.
        let guesses: Vec<A> = match (o.predict, o.addrs.first()) {
            (Some(p), Some(base))
                if !matches!(self.nat, NatType::Open | NatType::Symmetric)
                    && is_public(base)
                    && dialable(base, loopback) =>
            {
                p.window(base).into_iter().filter(|a| dialable(a, loopback) && !o.addrs.contains(a)).collect()
            }
            _ => Vec::new(),
        };
        let addrs: Vec<A> = o.addrs.into_iter().filter(|a| dialable(a, loopback) && !self.local.contains(a)).collect();
        let mut relay = self.relay();
        let reachable = ep.is_connected(&from) || !addrs.is_empty();
        if relay.is_none()
            && room
            && reachable
            && o.role == Role::Host
            && matches!(self.privacy, Privacy::RelayOnly { .. })
        {
            // The first proven Host this relay-only node dials becomes its relay, and stays.
            // A Host offer with nothing to dial is not picked: no dial means no
            // ConnectFailed to clear the pick, and the real Host would be locked out.
            (self.picked, relay) = (Some(from), Some(from));
        }
        if matches!(self.privacy, Privacy::RelayOnly { .. }) && relay != Some(from) {
            // Relay-only: anyone but the relay is dialled through it, never directly
            // (or once it links, from `handle_event`).
            if let Some(r) = relay.filter(|r| ep.is_connected(r) && !ep.is_connected(&from)) {
                let _ = ep.connect_with(now, from, &[Path::Via(r)]);
            }
        } else {
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
            if !guesses.is_empty() {
                ep.set_guesses(now, &from, &guesses);
            }
        }
        if o.reply {
            self.offer(now, ep, from, false, room);
        }
    }

    /// Seals an offer to `to` and publishes it on its topic. `room`: a room offer,
    /// with a proof under the room key (sent only while in a room); otherwise
    /// one proved with `to`'s invite key, if we have it.
    fn offer(&mut self, now: Instant, ep: &Endpoint<A>, to: PeerId, reply: bool, room: bool) {
        let invite = self.remotes.get(&to).and_then(|r| r.key);
        let (kind, key, label): (_, _, &[u8]) = match (&self.room, room, invite) {
            (Some(r), true, _) => (Proof::Room, Some(r.key), b"nb-offer"),
            (None, true, _) => return,
            (_, false, Some(k)) => (Proof::Invite, Some(k), b"nb-invite"),
            (_, false, None) => (Proof::None, None, b""),
        };
        // A relay-only node names only its relay, not every peer it links with. Others
        // name their links and LAN addresses only to a room member or a peer they
        // dialled; anyone else, such as a stranger holding our invite, gets public
        // addresses only, even after its dial failed (a holder can make that happen).
        // Relay fallback for it still works: it names its links, and we dial through
        // whichever of them we link with (`handle_event`).
        let full = room || self.remotes.get(&to).is_some_and(|r| r.invited);
        let (addrs, relays) = match self.privacy {
            Privacy::RelayOnly { .. } => {
                let relay = self.relay().filter(|r| *r != to && ep.is_connected(r));
                (self.candidates_for(&to), relay.into_iter().collect())
            }
            _ if full => {
                let links = ep.connected_peers().filter(|p| **p != to).take(MAX_RELAYS);
                (self.candidates_for(&to), links.copied().collect())
            }
            _ => (self.addresses(true), Vec::new()),
        };
        // The prediction is for the public address, so only when that leads.
        let predict = self.prediction.filter(|_| self.public.is_some() && addrs.first() == self.public.as_ref());
        let o = Offer {
            kind,
            reply,
            role: self.role().unwrap_or(Role::Peer),
            nat: self.nat,
            ts: self.wall_ms(now),
            nonce: self.rng.next_u64(),
            addrs,
            predict,
            relays,
        };
        let mut body = o.encode();
        if let Some(k) = key {
            let proof = mac(&k, &[label, self.id.as_bytes(), to.as_bytes(), &body]);
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
        (None, _)
            | (Some(Role::Peer), Role::Peer | Role::Host)
            | (Some(Role::Host), Role::Peer | Role::Client)
            | (Some(Role::Client), Role::Host)
    )
}

/// Whether an address from a peer or STUN is worth dialling: never port 0, an
/// unspecified, multicast, broadcast or link-local address, and loopback only for a
/// node bound to loopback (so an offer can't aim a node at services on its own host).
fn dialable<A: Address>(a: &A, loopback: bool) -> bool {
    use std::net::IpAddr;
    let Some(s) = a.to_socket_addr() else { return true };
    let ip = s.ip().to_canonical();
    let special = match ip {
        IpAddr::V4(v) => v.is_broadcast() || v.is_link_local(),
        IpAddr::V6(v) => v.segments()[0] & 0xffc0 == 0xfe80,
    };
    s.port() != 0 && !ip.is_unspecified() && !ip.is_multicast() && !special && (loopback || !ip.is_loopback())
}

/// Whether an address is on the public internet: not loopback, private (RFC 1918,
/// IPv6 ULA), carrier-grade NAT (100.64/10), link-local or otherwise special.
/// Addresses that aren't IP addresses count as public.
fn is_public<A: Address>(a: &A) -> bool {
    use std::net::IpAddr;
    let Some(s) = a.to_socket_addr() else { return true };
    let ip = s.ip().to_canonical();
    let local = match ip {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            v.is_private() || v.is_link_local() || v.is_broadcast() || (a == 100 && b & 0xc0 == 64) || a == 0
        }
        IpAddr::V6(v) => {
            let s0 = v.segments()[0];
            s0 & 0xfe00 == 0xfc00 || s0 & 0xffc0 == 0xfe80
        }
    };
    !(local || ip.is_loopback() || ip.is_unspecified() || ip.is_multicast())
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
const HELLO_TS: &str = r#"","ts":"#;
const HELLO_PROOF: &str = r#","p":""#;
const HELLO_TAIL: &str = r#""}}"#;

/// The room presence message, in the JSON shape `nostr.js` uses (`{t, from, meta}`).
/// `meta.nb` marks a UDP peer, so browsers' WebRTC hellos are told apart; `meta.ts`
/// is the sender's Unix time in ms; `meta.p` is the room proof, an HMAC under the
/// room key over the sender, its role and `ts` (so a captured hello goes stale).
fn hello(key: &[u8; 32], id: &PeerId, role: Role, ts: u64) -> Vec<u8> {
    let proof = mac(key, &[b"nb-hello", id.as_bytes(), &[role as u8], &ts.to_le_bytes()]);
    let (id, name, proof) = (hex(id.as_bytes()), role_name(role), hex(&proof));
    format!("{HELLO_HEAD}{id}{HELLO_MID}{name}{HELLO_TS}{ts}{HELLO_PROOF}{proof}{HELLO_TAIL}").into_bytes()
}

fn parse_hello(b: &[u8]) -> Option<(PeerId, Role, u64, [u8; 32])> {
    let s = std::str::from_utf8(b).ok()?;
    let rest = s.strip_prefix(HELLO_HEAD)?;
    let (id_hex, rest) = (rest.get(..64)?, rest.get(64..)?);
    let (role, rest) = rest.strip_prefix(HELLO_MID)?.strip_suffix(HELLO_TAIL)?.split_once(HELLO_TS)?;
    let (ts, proof_hex) = rest.split_once(HELLO_PROOF)?;
    let role = [Role::Peer, Role::Host, Role::Client].into_iter().find(|r| role_name(*r) == role)?;
    if ts.is_empty() || ts.len() > 19 || !ts.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (mut id, mut proof) = ([0u8; 32], [0u8; 32]);
    (unhex(id_hex, &mut id) && unhex(proof_hex, &mut proof)).then_some((PeerId(id), role, ts.parse().ok()?, proof))
}

/// What an offer's proof is under (it follows the body, inside the seal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proof {
    /// Version 1: none.
    None,
    /// Version 2: the room key.
    Room,
    /// Version 3: the recipient's invite key.
    Invite,
}

/// Candidates for one peer. Sealed to it, so only it reads them.
#[derive(Debug, PartialEq)]
struct Offer<A> {
    kind: Proof,
    /// The sender wants an offer back (it will dial when that arrives).
    reply: bool,
    role: Role,
    nat: NatType,
    /// Sender's Unix time in ms, and a random nonce: replays are dropped.
    ts: u64,
    nonce: u64,
    addrs: Vec<A>,
    /// The sender's NAT's likely next port, at the first address's IP.
    predict: Option<Prediction>,
    /// Peers the sender is connected to, which could relay.
    relays: Vec<PeerId>,
}

/// The port a sequential symmetric NAT will likely hand out next, and its step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Prediction {
    next: u16,
    step: u16,
}

impl Prediction {
    /// From the mappings two STUN servers saw, in the order they were asked.
    fn from_mappings<A: Address>(a: &A, b: &A) -> Option<Prediction> {
        let (a, b) = (a.to_socket_addr()?, b.to_socket_addr()?);
        let step = b.port().wrapping_sub(a.port());
        let ok = a.ip() == b.ip() && (1..=MAX_STEP).contains(&step);
        ok.then_some(Prediction { next: b.port().checked_add(step)?, step })
    }

    /// The addresses to probe: `base`'s IP, at ports around `next`.
    fn window<A: Address>(&self, base: &A) -> Vec<A> {
        let Some(port) = base.to_socket_addr().map(|s| i32::from(s.port())) else { return Vec::new() };
        let at = |k: i32| base.port_offset(i32::from(self.next) + k * i32::from(self.step) - port);
        (-GUESS_BEFORE..=GUESS_AFTER).filter_map(at).collect()
    }

    // [tag: fe 01] [step u8] [next u16 le]
    fn encode(&self) -> [u8; 5] {
        let [lo, hi] = self.next.to_le_bytes();
        [PREDICTION_TAG[0], PREDICTION_TAG[1], self.step as u8, lo, hi]
    }

    fn decode(b: &[u8]) -> Option<Prediction> {
        let &[t0, t1, step, lo, hi] = b else { return None };
        let p = Prediction { next: u16::from_le_bytes([lo, hi]), step: step.into() };
        ([t0, t1] == PREDICTION_TAG && (1..=MAX_STEP).contains(&p.step)).then_some(p)
    }
}

impl<A: Address> Offer<A> {
    // [1|2|3] [flags] [role] [nat] [ts u64 le] [nonce u64 le] [n] n*([len] addr) [m] m*(32 B peer id)
    // A room (2) or invite (3) offer is followed, outside this body, by its 32-byte proof.
    // One of the n entries may be a port prediction (see `Prediction::encode`), last.
    fn encode(&self) -> Vec<u8> {
        let role = [Role::Peer, Role::Host, Role::Client].iter().position(|r| *r == self.role).unwrap() as u8;
        let nat =
            [NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].iter().position(|n| *n == self.nat);
        let version = match self.kind {
            Proof::None => OFFER,
            Proof::Room => ROOM_OFFER,
            Proof::Invite => INVITE_OFFER,
        };
        let mut v = vec![version, self.reply as u8, role, nat.unwrap() as u8];
        v.extend_from_slice(&self.ts.to_le_bytes());
        v.extend_from_slice(&self.nonce.to_le_bytes());
        let room = MAX_CANDIDATES - self.predict.is_some() as usize;
        let n = self.addrs.len().min(room);
        v.push((n + self.predict.is_some() as usize) as u8);
        for a in self.addrs.iter().take(n) {
            let mut e = Vec::new();
            a.encode(&mut e);
            v.push(e.len() as u8);
            v.extend_from_slice(&e);
        }
        if let Some(p) = self.predict {
            v.push(5);
            v.extend_from_slice(&p.encode());
        }
        v.push(self.relays.len().min(MAX_RELAYS) as u8);
        for p in self.relays.iter().take(MAX_RELAYS) {
            v.extend_from_slice(p.as_bytes());
        }
        v
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let (&[version @ (OFFER | ROOM_OFFER | INVITE_OFFER), flags, role, nat], rest) = b.split_first_chunk::<4>()?
        else {
            return None;
        };
        let role = *[Role::Peer, Role::Host, Role::Client].get(role as usize)?;
        let nat = *[NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].get(nat as usize)?;
        let (ts, rest) = rest.split_first_chunk::<8>()?;
        let (nonce, rest) = rest.split_first_chunk::<8>()?;
        let (&n, mut rest) = rest.split_first()?;
        if n as usize > MAX_CANDIDATES || flags > 1 {
            return None;
        }
        let (mut addrs, mut predict) = (Vec::new(), None);
        for _ in 0..n {
            let (&len, r) = rest.split_first()?;
            let (a, r) = (r.get(..len as usize)?, r.get(len as usize..)?);
            match a.starts_with(&PREDICTION_TAG) {
                true => predict = predict.or(Prediction::decode(a)),
                false => addrs.extend(A::decode(a)),
            }
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
            kind: [Proof::None, Proof::Room, Proof::Invite][version as usize - 1],
            reply: flags == 1,
            role,
            nat,
            ts: u64::from_le_bytes(*ts),
            nonce: u64::from_le_bytes(*nonce),
            addrs,
            predict,
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
            let ts = 1_800_000_000_123u64;
            let (from, r, t, proof) = parse_hello(&hello(&key, &p, role, ts)).unwrap();
            assert_eq!((from, r, t), (p, role, ts));
            let parts =
                |role: u8, ts: u64| mac_ok(&key, &[b"nb-hello", p.as_bytes(), &[role], &ts.to_le_bytes()], &proof);
            assert!(parts(role as u8, ts));
            assert!(!parts(9, ts), "bound to the role");
            assert!(!parts(role as u8, ts + 1), "bound to the time");
            let other = room_key("app", "other");
            assert!(!mac_ok(&other, &[b"nb-hello", p.as_bytes(), &[role as u8], &ts.to_le_bytes()], &proof));
        }
        let js = format!(r#"{{"t":"hello","from":"{}","meta":{{"role":"player"}}}}"#, hex(p.as_bytes()));
        // Not a number, signed, or too long: refused.
        let h = String::from_utf8(hello(&key, &p, Role::Peer, 5)).unwrap();
        for bad in ["\"ts\":x,", "\"ts\":+5,", "\"ts\":,", "\"ts\":99999999999999999999,"] {
            assert_eq!(parse_hello(h.replace("\"ts\":5,", bad).as_bytes()), None, "{bad}");
        }
        assert_eq!(parse_hello(js.as_bytes()), None);
        assert_eq!(parse_hello(b"{}"), None);
        let h = hello(&key, &p, Role::Peer, 0);
        for cut in 0..h.len() {
            assert_eq!(parse_hello(&h[..cut]), None);
        }
        // The key is not the topic, nor derivable from it.
        assert_ne!(key, Topic::room("app", "room").0);
    }

    #[test]
    fn offer_round_trips_and_rejects_garbage() {
        let o: Offer<SocketAddr> = Offer {
            kind: Proof::None,
            reply: true,
            role: Role::Client,
            nat: NatType::Symmetric,
            ts: 1_700_000_000_123,
            nonce: 42,
            addrs: vec!["203.0.113.5:4000".parse().unwrap(), "[2001:db8::1]:9".parse().unwrap()],
            predict: None,
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
        for (kind, version) in [(Proof::Room, ROOM_OFFER), (Proof::Invite, INVITE_OFFER)] {
            let o: Offer<SocketAddr> = Offer { kind, ..Offer::decode(&b).unwrap() };
            let ob = o.encode();
            assert_eq!(ob[0], version);
            assert_eq!(Offer::decode(&ob), Some(o));
        }
        let mut v4 = b.clone();
        v4[0] = 4;
        assert_eq!(Offer::<SocketAddr>::decode(&v4), None);
    }

    const ROOM: (&str, &str) = ("app", "room");

    /// A sealed offer from `from` to `to`, with a room proof under `key` if given.
    fn offer_from(
        from: &Identity,
        to: &PeerId,
        key: Option<&[u8; 32]>,
        nonce: u64,
        role: Role,
        addrs: Vec<SocketAddr>,
    ) -> Vec<u8> {
        proved_offer(from, to, key.map(|k| (Proof::Room, k)), nonce, role, addrs)
    }

    /// A sealed offer from `from` to `to`, with a proof of `kind` under its key.
    fn proved_offer(
        from: &Identity,
        to: &PeerId,
        proof: Option<(Proof, &[u8; 32])>,
        nonce: u64,
        role: Role,
        addrs: Vec<SocketAddr>,
    ) -> Vec<u8> {
        let kind = proof.map_or(Proof::None, |p| p.0);
        let nat = NatType::Cone;
        let o = Offer { kind, reply: true, role, nat, ts: 0, nonce, addrs, predict: None, relays: vec![] };
        let mut body = o.encode();
        if let Some((kind, k)) = proof {
            let label: &[u8] = if kind == Proof::Room { b"nb-offer" } else { b"nb-invite" };
            body.extend(mac(k, &[label, from.peer_id().as_bytes(), to.as_bytes(), &body]));
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
        t.handle_signal(now, &mut ep, topic, &hello(&topic.0, &ap, Role::Host, 0));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        assert_eq!((t.room_role(&ap), t.admits(&ap)), (None, false));
        // A member's offer: dialled, answered, and its role is known.
        let host = id(2);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 3, Role::Host, vec![there]));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![there], 1));
        assert_eq!((t.room_role(&host.peer_id()), t.admits(&host.peer_id())), (Some(Role::Host), true));
        // A member's hello in a role this client doesn't link with: no offer.
        t.handle_signal(now, &mut ep, topic, &hello(&key, &id(3).peer_id(), Role::Client, 0));
        assert_eq!(published(&mut t), 0);
        // Leaving the room forgets who proved what; outside a room, links are anyone's call.
        t.leave_room();
        assert_eq!((t.room_role(&host.peer_id()), t.admits(&ap)), (None, true));

        // With an expected host, other members are ignored.
        let (mut t, mut ep) = in_room(Role::Client, Some(id(3).peer_id()), local);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 4, Role::Host, vec![there]));
        t.handle_signal(now, &mut ep, topic, &hello(&key, &host.peer_id(), Role::Host, 0));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        t.handle_signal(now, &mut ep, topic, &hello(&key, &id(3).peer_id(), Role::Host, 0));
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
        t.introduce(now, &ep, id(2).peer_id(), None);
        assert_eq!(published(&mut t), 1);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &offer_from(&id(2), &me, None, 1, Role::Peer, vec![there]));
        assert_eq!(dials(&mut ep, now), vec![there]);
        assert!(t.admits(&id(2).peer_id()) && t.room_role(&id(2).peer_id()).is_none());
    }

    /// NB-031: a v2 (room) offer without a verified proof, from an invited peer, is
    /// dropped: it must not give that peer a room role such as Host. Security's
    /// repro is the 22-byte one, short enough to skip the proof split.
    #[test]
    fn short_room_offer_without_proof_is_dropped() {
        let me = id(1).peer_id();
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        let now = Instant::from_millis(10);
        t.introduce(now, &ep, id(2).peer_id(), None);
        while t.poll_signal().is_some() {}
        let offer = |kind, nonce| {
            let o = Offer::<SocketAddr> {
                kind,
                reply: true,
                role: Role::Host,
                nat: NatType::Cone,
                ts: 0,
                nonce,
                addrs: vec![],
                predict: None,
                relays: vec![],
            };
            o.encode()
        };
        let mut send = |t: &mut Traversal<SocketAddr>, body: &[u8]| {
            t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed::seal(&id(2), &me, [7; 32], body).unwrap());
            (t.room_role(&id(2).peer_id()), published(t))
        };
        let short = offer(Proof::Room, 1);
        assert_eq!(short.len(), 22);
        assert_eq!(send(&mut t, &short), (None, 0));
        // Padded with 32 bytes that aren't the proof, or as a short v3 (invite) offer: dropped too.
        let padded = [offer(Proof::Room, 2), vec![0; PROOF_LEN]].concat();
        assert_eq!(send(&mut t, &padded), (None, 0));
        assert_eq!(send(&mut t, &offer(Proof::Invite, 3)), (None, 0));
        // With the real proof, the same offer does make it the Host.
        let body = offer(Proof::Room, 4);
        let proof = mac(&room_key(ROOM.0, ROOM.1), &[b"nb-offer", id(2).peer_id().as_bytes(), me.as_bytes(), &body]);
        assert_eq!(send(&mut t, &[body, proof.to_vec()].concat()), (Some(Role::Host), 1));
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
            t.handle_signal(t0, &mut ep, topic, &hello(&topic.0, &member(n), Role::Peer, 0));
        }
        assert_eq!(published(&mut t), 0);
        // Real hellos from 100 members: a burst of answers, then none.
        for n in 0..100u8 {
            t.handle_signal(t0, &mut ep, topic, &hello(&key, &member(n), Role::Peer, 0));
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
            t.handle_signal(later, &mut ep, topic, &hello(&key, &member(n), Role::Peer, 0));
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
        let k = t.invite_key();
        for n in 0..200u8 {
            let o = proved_offer(&id(n.wrapping_add(2)), &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![]);
            t.handle_signal(Instant::ZERO, &mut ep, Topic::peer(&me), &o);
        }
        assert_eq!(t.nonces.len(), BURST as usize);
        assert!(
            MAX_NONCES >= (BURST as usize + (2 * FRESH_MS as usize / REFILL.as_millis() as usize)),
            "covers the window"
        );
    }

    /// NB-033(a): an offer (or an observed address) can't aim a node at its own host,
    /// multicast, broadcast, link-local, the unspecified address or port 0; loopback
    /// only between loopback nodes.
    #[test]
    fn offers_cannot_aim_at_special_addresses() {
        let me = id(1).peer_id();
        let addrs: Vec<SocketAddr> = [
            "198.51.100.7:9",
            "127.0.0.1:5",
            "[::ffff:127.0.0.1]:5",
            "0.0.0.0:5",
            "224.0.0.1:5",
            "[ff02::1]:5",
            "255.255.255.255:5",
            "198.51.100.7:0",
            "169.254.1.1:5",
            "[fe80::1]:5",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        let now = Instant::from_millis(10);
        let run = |local: &str| {
            let ia = id(1);
            let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
            let mut t = Traversal::new(&ia, [3; 32], 0, vec![local.parse().unwrap()], vec![]);
            let k = t.invite_key();
            let o = proved_offer(&id(2), &me, Some((Proof::Invite, &k)), 1, Role::Peer, addrs.clone());
            t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
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

    /// Links `ep` (at `at`) with a fresh endpoint for `peer` (at `peer_at`), in memory.
    fn link(ep: &mut Endpoint<SocketAddr>, at: SocketAddr, peer: &Identity, peer_at: SocketAddr, now: Instant) {
        let mut other = Endpoint::new(peer, Config::default(), [9; 32]);
        ep.connect_with(now, peer.peer_id(), &[Path::Direct(peer_at)]).unwrap();
        for _ in 0..8 {
            while let Some(t) = ep.poll_transmit(now) {
                other.handle_datagram(now, at, &t.payload);
            }
            while let Some(t) = other.poll_transmit(now) {
                ep.handle_datagram(now, peer_at, &t.payload);
            }
        }
        assert!(ep.is_connected(&peer.peer_id()));
    }

    /// NB-033(b): a node names its links and LAN addresses only to a room member or a
    /// peer it dialled. An invite holder it never dialled gets its public addresses
    /// and no relays.
    #[test]
    fn invite_holders_get_public_addresses_and_no_relays() {
        let ia = id(1);
        let me = ia.peer_id();
        let (lan, public): (SocketAddr, SocketAddr) =
            ("10.0.0.1:7000".parse().unwrap(), "203.0.113.5:7000".parse().unwrap());
        let (relay, holder, dialled) = (id(3), id(2), id(5));
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan], vec![]);
        let now = Instant::from_millis(10);
        link(&mut ep, lan, &relay, "198.51.100.3:9".parse().unwrap(), now);
        t.handle_event(now, &mut ep, &Event::AddrObserved { by: relay.peer_id(), addr: public });
        let k = t.invite_key();
        let o = proved_offer(&holder, &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        let answer = |t: &mut Traversal<SocketAddr>, to: &Identity| {
            let o = sent_offers(t, to);
            assert_eq!(o.len(), 1);
            (o[0].addrs.clone(), o[0].relays.clone())
        };
        assert_eq!(answer(&mut t, &holder), (vec![public], vec![]), "public only, no relays");
        // A peer this node dials gets everything.
        t.introduce(now, &ep, dialled.peer_id(), None);
        assert_eq!(answer(&mut t, &dialled), (vec![public, lan], vec![relay.peer_id()]));
        // Room members get everything too.
        let (mut t, mut ep) = in_room(Role::Peer, None, lan);
        link(&mut ep, lan, &relay, "198.51.100.3:9".parse().unwrap(), now);
        let key = room_key(ROOM.0, ROOM.1);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &offer_from(&holder, &me, Some(&key), 1, Role::Peer, vec![]));
        assert_eq!(answer(&mut t, &holder), (vec![lan], vec![relay.peer_id()]));
    }

    /// NB-033 N33-1: an invite holder can make its own dial fail (an address that never
    /// answers), and still gets public addresses and no relays afterwards, both in
    /// answers and in the re-offer when a link comes up.
    #[test]
    fn invite_holder_cannot_force_full_offer() {
        let ia = id(1);
        let me = ia.peer_id();
        let (lan, public): (SocketAddr, SocketAddr) =
            ("10.0.0.1:7000".parse().unwrap(), "203.0.113.5:7000".parse().unwrap());
        let (relay, holder) = (id(3), id(2));
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan], vec![]);
        let mut now = Instant::from_millis(10);
        link(&mut ep, lan, &relay, "198.51.100.3:9".parse().unwrap(), now);
        t.handle_event(now, &mut ep, &Event::AddrObserved { by: relay.peer_id(), addr: public });
        let k = t.invite_key();
        let dead: SocketAddr = "192.0.2.77:9".parse().unwrap();
        let o = proved_offer(&holder, &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![dead]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        let answer = |t: &mut Traversal<SocketAddr>| {
            let o = sent_offers(t, &holder);
            assert_eq!(o.len(), 1);
            (o[0].addrs.clone(), o[0].relays.clone())
        };
        assert_eq!(answer(&mut t), (vec![public], vec![]));
        // The real endpoint gives up on the dead address.
        let mut failed = false;
        while !failed {
            now = ep.poll_timeout().expect("a dial pending");
            ep.handle_timeout(now);
            while ep.poll_transmit(now).is_some() {}
            while let Some(e) = ep.poll_event() {
                failed |= matches!(e, Event::ConnectFailed { peer } if peer == holder.peer_id());
                t.handle_event(now, &mut ep, &e);
            }
        }
        assert!(t.failure(&holder.peer_id()).is_some() && ep.is_connected(&relay.peer_id()));
        let o = proved_offer(&holder, &me, Some((Proof::Invite, &k)), 2, Role::Peer, vec![dead]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        assert_eq!(answer(&mut t), (vec![public], vec![]), "a failed dial opens nothing");
        let up = Event::Connected { peer: relay.peer_id(), path: Path::Direct(lan), initiator: true };
        t.handle_event(now + OFFER_GAP, &mut ep, &up);
        assert_eq!(answer(&mut t), (vec![public], vec![]), "nor does the re-offer");
    }

    /// NB-033 N33-1: an invite holder that named R as a relay is dialled through R
    /// as soon as this node links with R, though R was not linked when the offer came.
    #[test]
    fn holder_is_dialled_via_a_relay_it_listed_once_linked() {
        let ia = id(1);
        let me = ia.peer_id();
        let lan: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let (relay, holder, other) = (id(3), id(2), id(4));
        let (at_relay, at_other): (SocketAddr, SocketAddr) =
            ("198.51.100.3:9".parse().unwrap(), "198.51.100.4:9".parse().unwrap());
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan], vec![]);
        let now = Instant::from_millis(10);
        let k = t.invite_key();
        let relays = vec![relay.peer_id()];
        let o: Offer<SocketAddr> = Offer {
            kind: Proof::Invite,
            reply: false,
            role: Role::Peer,
            nat: NatType::Cone,
            ts: 0,
            nonce: 1,
            addrs: vec![],
            predict: None,
            relays,
        };
        let mut body = o.encode();
        body.extend(mac(&k, &[b"nb-invite", holder.peer_id().as_bytes(), me.as_bytes(), &body]));
        t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed::seal(&holder, &me, [7; 32], &body).unwrap());
        assert_eq!(dials(&mut ep, now), vec![], "nothing to dial yet");
        // A link it didn't name changes nothing.
        link(&mut ep, lan, &other, at_other, now);
        t.handle_event(
            now,
            &mut ep,
            &Event::Connected { peer: other.peer_id(), path: Path::Direct(at_other), initiator: true },
        );
        assert_eq!(dials(&mut ep, now), vec![]);
        // R links: the holder is dialled through it.
        link(&mut ep, lan, &relay, at_relay, now);
        t.handle_event(
            now,
            &mut ep,
            &Event::Connected { peer: relay.peer_id(), path: Path::Direct(at_relay), initiator: true },
        );
        assert_eq!(dials(&mut ep, now), vec![at_relay]);
    }

    /// The offers `t` published, opened with `to`'s key and decoded.
    fn sent_offers(t: &mut Traversal<SocketAddr>, to: &Identity) -> Vec<Offer<SocketAddr>> {
        std::iter::from_fn(|| t.poll_signal())
            .filter_map(|s| match s {
                Signal::Publish(topic, p) if topic == Topic::peer(&to.peer_id()) => {
                    let (_, body) = sealed::open(to, &p)?;
                    let cut = if body[0] == OFFER { body.len() } else { body.len() - PROOF_LEN };
                    Offer::decode(&body[..cut])
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn public_addresses() {
        let public = ["198.51.100.7:9", "8.8.8.8:53", "[2001:db8::1]:9", "100.128.0.1:1", "172.32.0.1:1"];
        let local = [
            "10.0.0.1:1",
            "192.168.1.2:1",
            "172.16.0.1:1",
            "127.0.0.1:1",
            "169.254.1.1:1",
            "100.64.0.1:1",
            "100.127.255.1:1",
            "0.0.0.0:1",
            "255.255.255.255:1",
            "224.0.0.1:1",
            "[::1]:1",
            "[fe80::1]:1",
            "[fd00::1]:1",
            "[::ffff:10.0.0.1]:1",
        ];
        for a in public {
            assert!(is_public(&a.parse::<SocketAddr>().unwrap()), "{a}");
        }
        for a in local {
            assert!(!is_public(&a.parse::<SocketAddr>().unwrap()), "{a}");
        }
    }

    /// NB-034: what each mode puts in an offer, read back by its recipient.
    #[test]
    fn privacy_decides_what_offers_carry() {
        let (ia, peer, relay) = (id(1), id(2), id(3));
        let lan: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let lo: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let public: SocketAddr = "198.51.100.1:7000".parse().unwrap();
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan, lo], vec![]);
        t.public = Some(public);
        t.handle_event(
            Instant::ZERO,
            &mut ep,
            &Event::AddrObserved { by: relay.peer_id(), addr: "192.168.0.9:1".parse().unwrap() },
        );
        let now = Instant::from_millis(10);
        let offer_to = |t: &mut Traversal<SocketAddr>, ep: &mut Endpoint<SocketAddr>, to: &Identity| {
            while t.poll_signal().is_some() {}
            t.introduce(now, ep, to.peer_id(), Some([5; 32]));
            let o = sent_offers(t, to);
            assert_eq!(o.len(), 1);
            assert_eq!(o[0].kind, Proof::Invite, "proved with the invite key");
            (o[0].addrs.clone(), o[0].relays.clone())
        };
        assert_eq!(offer_to(&mut t, &mut ep, &peer).0, vec![public, "192.168.0.9:1".parse().unwrap(), lan, lo]);
        t.set_privacy(&mut ep, Privacy::PublicOnly);
        assert_eq!(offer_to(&mut t, &mut ep, &peer).0, vec![public]);
        assert_eq!(t.candidates(), vec![public]);
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: Some(relay.peer_id()) });
        assert!(ep.relay_only());
        assert_eq!(offer_to(&mut t, &mut ep, &peer), (vec![], vec![]), "nothing, not even the relay while unlinked");
        assert_eq!(offer_to(&mut t, &mut ep, &relay).0, vec![public], "the relay gets the public address");
        assert_eq!(t.candidates(), vec![]);
        t.set_privacy(&mut ep, Privacy::Direct);
        assert!(!ep.relay_only());
    }

    /// A relay-only node dials others only through its relay, whatever their
    /// offers carry, and in a host/client room the host is the relay.
    #[test]
    fn relay_only_dials_through_the_relay() {
        let me = id(1).peer_id();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let (key, inbox) = (room_key(ROOM.0, ROOM.1), Topic::peer(&me));
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        assert_eq!(t.relay(), None);
        let now = Instant::from_millis(10);
        // The host proves its role: it is the relay, and dialled directly.
        let host = id(2);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 1, Role::Host, vec![there]));
        assert_eq!(t.relay(), Some(host.peer_id()));
        assert_eq!(dials(&mut ep, now), vec![there]);
        // Pinning a host makes it the relay before it says anything.
        let (mut t, mut ep) = in_room(Role::Client, Some(id(4).peer_id()), "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        assert_eq!(t.relay(), Some(id(4).peer_id()));
        // Anyone else is never dialled at its addresses (the relay isn't linked: no dial at all).
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: Some(id(3).peer_id()) });
        t.handle_signal(now, &mut ep, inbox, &offer_from(&id(5), &me, Some(&key), 1, Role::Peer, vec![there]));
        assert_eq!(dials(&mut ep, now), vec![]);
        assert_eq!(published(&mut t), 1, "answered, with no address");
    }

    /// NB-034 R1: a relay-only client's relay is the first Host it dials, and stays
    /// so: a second Host (here with the lower PeerId, which used to win) is never
    /// dialled directly, until the first's dial fails.
    #[test]
    fn relay_only_client_dials_exactly_one_host_directly() {
        let me = id(1).peer_id();
        let (key, inbox) = (room_key(ROOM.0, ROOM.1), Topic::peer(&me));
        let (first_at, second_at): (SocketAddr, SocketAddr) =
            ("198.51.100.9:4000".parse().unwrap(), "198.51.100.10:4000".parse().unwrap());
        let (mut first, mut second) = (id(2), id(3));
        if first.peer_id() < second.peer_id() {
            std::mem::swap(&mut first, &mut second);
        }
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        let now = Instant::from_millis(10);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&first, &me, Some(&key), 1, Role::Host, vec![first_at]));
        assert_eq!(t.relay(), Some(first.peer_id()));
        assert_eq!(dials(&mut ep, now), vec![first_at]);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&second, &me, Some(&key), 1, Role::Host, vec![second_at]));
        assert_eq!(t.relay(), Some(first.peer_id()), "the relay doesn't switch");
        assert_eq!(dials(&mut ep, now), vec![], "the second Host is not dialled at its address");
        assert!(sent_offers(&mut t, &second).iter().all(|o| o.addrs.is_empty() && o.relays.is_empty()));
        // Once the first dial fails, the next Host may be picked.
        t.handle_event(now, &mut ep, &Event::ConnectFailed { peer: first.peer_id() });
        assert_eq!(t.relay(), None);
        let later = now + SENDER_REFILL;
        t.handle_signal(later, &mut ep, inbox, &offer_from(&second, &me, Some(&key), 2, Role::Host, vec![second_at]));
        assert_eq!(t.relay(), Some(second.peer_id()));
        assert_eq!(dials(&mut ep, later), vec![second_at]);
        // A pinned `via` overrides any pick.
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: Some(first.peer_id()) });
        assert_eq!(t.relay(), Some(first.peer_id()));
    }

    /// A Host offer with no address to dial isn't picked as the relay: nothing
    /// would ever clear the pick, locking the real Host out.
    #[test]
    fn relay_only_client_skips_a_host_it_cannot_dial() {
        let me = id(1).peer_id();
        let (key, inbox) = (room_key(ROOM.0, ROOM.1), Topic::peer(&me));
        let real_at: SocketAddr = "198.51.100.10:4000".parse().unwrap();
        let (empty, real) = (id(2), id(3));
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        let now = Instant::from_millis(10);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&empty, &me, Some(&key), 1, Role::Host, vec![]));
        assert_eq!(t.relay(), None);
        assert_eq!(dials(&mut ep, now), vec![]);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&real, &me, Some(&key), 1, Role::Host, vec![real_at]));
        assert_eq!(t.relay(), Some(real.peer_id()));
        assert_eq!(dials(&mut ep, now), vec![real_at]);
    }

    /// NB-032: a stale or replayed-later hello is dropped.
    #[test]
    fn stale_hellos_are_dropped() {
        let (key, topic) = (room_key(ROOM.0, ROOM.1), Topic::room(ROOM.0, ROOM.1));
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let late = Instant::ZERO + Duration::from_millis(FRESH_MS + 1);
        t.handle_signal(late, &mut ep, topic, &hello(&key, &id(5).peer_id(), Role::Peer, 0));
        assert_eq!(published(&mut t), 0);
        let ts = FRESH_MS + 1;
        t.handle_signal(late, &mut ep, topic, &hello(&key, &id(5).peer_id(), Role::Peer, ts));
        assert_eq!(published(&mut t), 1);
    }

    /// NB-032: outside a room, a plain offer from a peer this node never dialled
    /// is ignored (knowing a PeerId gets no dial-back and no addresses), and peers
    /// it dialled are answered even when strangers spent the node's budget.
    #[test]
    fn only_invite_holders_and_dialled_peers_get_answers() {
        let ia = id(1);
        let me = ia.peer_id();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec!["10.0.0.1:7000".parse().unwrap()], vec![]);
        while t.poll_signal().is_some() {}
        let now = Instant::from_millis(10);
        let inbox = Topic::peer(&me);
        t.handle_signal(now, &mut ep, inbox, &proved_offer(&id(2), &me, None, 1, Role::Peer, vec![there]));
        let wrong = proved_offer(&id(2), &me, Some((Proof::Invite, &[0; 32])), 2, Role::Peer, vec![there]);
        t.handle_signal(now, &mut ep, inbox, &wrong);
        // Nor does a room offer outside a room.
        let room =
            proved_offer(&id(2), &me, Some((Proof::Room, &room_key(ROOM.0, ROOM.1))), 3, Role::Peer, vec![there]);
        t.handle_signal(now, &mut ep, inbox, &room);
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        // Invite holders spend the node's whole offer budget...
        let k = t.invite_key();
        for n in 0..BURST as u8 + 5 {
            t.handle_signal(
                now,
                &mut ep,
                inbox,
                &proved_offer(&id(100 + n), &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![]),
            );
        }
        assert_eq!(published(&mut t), BURST as usize);
        // ...but a peer this node dialled is still answered (dialled, no reply asked twice).
        t.introduce(now, &ep, id(9).peer_id(), None);
        assert_eq!(published(&mut t), 1);
        let answer = proved_offer(&id(9), &me, None, 1, Role::Peer, vec![there]);
        t.handle_signal(now, &mut ep, inbox, &answer);
        assert_eq!(dials(&mut ep, now), vec![there]);
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
        ta.introduce(now, &ea, ib.peer_id(), Some(tb.invite_key()));
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
            (t.nat_type(), t.public_addr().copied(), t.predicted_addr())
        };
        assert_eq!(run([4000, 4000]), (NatType::Cone, Some("203.0.113.9:4000".parse().unwrap()), None));
        // NB-035: mappings a small step apart, upwards, predict the next one.
        let at = |port: u16| Some(SocketAddr::new([203, 0, 113, 9].into(), port));
        assert_eq!(run([4000, 4001]), (NatType::Symmetric, at(4000), at(4002)));
        assert_eq!(run([4000, 4008]).2, at(4016));
        for random in [[4000, 4009], [4001, 4000], [4000, 51234], [65534, 65535]] {
            assert_eq!(run(random), (NatType::Symmetric, at(random[0]), None), "{random:?}");
        }
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
    /// NB-035: the prediction rides in the address list as an entry no address
    /// decoder reads, so offers stay readable by nodes that predate it, and a
    /// full list makes room for it.
    #[test]
    fn predictions_round_trip_in_offers() {
        let p = Prediction { next: 20_002, step: 1 };
        let entry = p.encode();
        assert_eq!(Prediction::decode(&entry), Some(p));
        assert_eq!(SocketAddr::decode(&entry), None);
        for bad in [[0xfe, 1, 0, 1, 2], [0xfe, 1, 9, 1, 2], [0xfe, 2, 1, 1, 2]] {
            assert_eq!(Prediction::decode(&bad), None);
        }
        let addrs: Vec<SocketAddr> = (0..8).map(|i| SocketAddr::from(([198, 51, 100, 7], 4000 + i))).collect();
        let o = Offer {
            kind: Proof::None,
            reply: false,
            role: Role::Peer,
            nat: NatType::Symmetric,
            ts: 5,
            nonce: 6,
            addrs: addrs.clone(),
            predict: Some(p),
            relays: vec![],
        };
        let b = o.encode();
        let got = Offer::<SocketAddr>::decode(&b).unwrap();
        assert_eq!((got.addrs, got.predict), (addrs[..7].to_vec(), Some(p)));
        assert_eq!(b[20], 8, "eight entries: the most an older decoder takes");
        // The window: the predicted port's IP, leaning forward.
        let w = p.window(&addrs[0]);
        assert_eq!(w.len(), 32);
        assert_eq!((w[0].port(), w[4].port(), w[31].port()), (19_998, 20_002, 20_029));
        assert!(w.iter().all(|a| a.ip() == addrs[0].ip()));
    }

    /// NB-035: a cone node probes the ports around a symmetric peer's
    /// prediction; an open or symmetric one doesn't.
    #[test]
    fn cone_nodes_probe_predicted_ports() {
        let me = id(1).peer_id();
        let there: SocketAddr = "198.51.100.9:20000".parse().unwrap();
        let (s1, s2): (SocketAddr, SocketAddr) = ("192.0.2.1:3478".parse().unwrap(), "192.0.2.2:3478".parse().unwrap());
        for (ports, probes) in [([5000, 5000], true), ([5000, 5000 + 1], false), ([7000, 7000], false)] {
            let local: SocketAddr = "10.0.0.2:7000".parse().unwrap();
            let mut t = Traversal::new(&id(1), [3; 32], 0, vec![local], vec![s1, s2]);
            let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
            t.handle_timeout(Instant::ZERO, &mut ep);
            let mapped = if ports[0] == 7000 { [10, 0, 0, 2] } else { [203, 0, 113, 9] };
            for (r, port) in std::iter::from_fn(|| t.poll_transmit()).collect::<Vec<_>>().iter().zip(ports) {
                let tx = stun::parse_binding_request(&r.payload).unwrap();
                t.handle_stun(&r.dst, &stun::binding_response(&tx, SocketAddr::new(mapped.into(), port)));
            }
            let peer = id(2);
            t.introduce(Instant::ZERO, &ep, peer.peer_id(), None);
            let o = Offer {
                kind: Proof::None,
                reply: false,
                role: Role::Peer,
                nat: NatType::Symmetric,
                ts: 0,
                nonce: 1,
                addrs: vec![there],
                predict: Some(Prediction { next: 20_002, step: 1 }),
                relays: vec![],
            };
            let sealed = sealed::seal(&peer, &me, [7; 32], &o.encode()).unwrap();
            let now = Instant::from_millis(10);
            t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed);
            let sent = dials(&mut ep, now);
            let nat = t.nat_type();
            assert_eq!(sent.len(), if probes { 32 } else { 1 }, "{nat:?}");
            assert_eq!(sent[0], there);
            if probes {
                // Not the advertised port again; the rest of the window.
                assert!(sent[1..].iter().all(|a| a.ip() == there.ip() && a.port() != 20_000));
                assert!(sent.contains(&"198.51.100.9:20002".parse().unwrap()));
            }
        }
    }

    /// NB-035 security review: repeating an offer adds no probes (S1), guesses go
    /// out on three handshake sends only (S3), and never around a private address (S2).
    #[test]
    fn guesses_are_probed_three_times_and_only_at_public_addresses() {
        let me = id(1).peer_id();
        let (s1, s2): (SocketAddr, SocketAddr) = ("192.0.2.1:3478".parse().unwrap(), "192.0.2.2:3478".parse().unwrap());
        // Every datagram the endpoint sends to `there`'s IP over 40 s, after `offers` copies of one offer.
        let sent_to = |there: SocketAddr, offers: u64| -> usize {
            let local: SocketAddr = "10.0.0.2:7000".parse().unwrap();
            let mut t = Traversal::new(&id(1), [3; 32], 0, vec![local], vec![s1, s2]);
            let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
            t.handle_timeout(Instant::ZERO, &mut ep);
            for r in std::iter::from_fn(|| t.poll_transmit()).collect::<Vec<_>>() {
                let tx = stun::parse_binding_request(&r.payload).unwrap();
                t.handle_stun(&r.dst, &stun::binding_response(&tx, "203.0.113.9:5000".parse().unwrap()));
            }
            assert_eq!(t.nat_type(), NatType::Cone);
            let peer = id(2);
            t.introduce(Instant::ZERO, &ep, peer.peer_id(), None);
            let mut sent = Vec::new();
            for nonce in 1..=offers {
                let o = Offer {
                    kind: Proof::None,
                    reply: false,
                    role: Role::Peer,
                    nat: NatType::Symmetric,
                    ts: 0,
                    nonce,
                    addrs: vec![there],
                    predict: Some(Prediction { next: there.port() + 2, step: 1 }),
                    relays: vec![],
                };
                let sealed = sealed::seal(&peer, &me, [7; 32], &o.encode()).unwrap();
                let now = Instant::from_millis(10 * nonce);
                t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed);
                sent.extend(dials(&mut ep, now));
            }
            for ms in (100..40_000).step_by(100) {
                ep.handle_timeout(Instant::from_millis(ms));
                sent.extend(dials(&mut ep, Instant::from_millis(ms)));
            }
            sent.iter().filter(|a| a.ip() == there.ip()).count()
        };
        let public: SocketAddr = "198.51.100.9:20000".parse().unwrap();
        let attempts = usize::from(Config::default().handshake_attempts);
        // The candidate on every handshake send, the 31 other guesses on three.
        assert_eq!(sent_to(public, 1), attempts + 31 * 3);
        assert_eq!(sent_to(public, 4), attempts + 31 * 3);
        // A private first address gets no guesses: only the candidate itself.
        assert_eq!(sent_to("192.168.1.50:20000".parse().unwrap(), 4), attempts);
    }

    /// NB-035: a failure reads as a sentence that names the symmetric side.
    #[test]
    fn failures_say_which_side_is_symmetric() {
        let say = |f: Failure| f.to_string();
        assert!(say(Failure::Unreachable).contains("firewall drops UDP"));
        let ours = say(Failure::Symmetric(Side::Ours));
        assert!(ours.starts_with("this side's NAT is symmetric") && ours.contains("the peer's router only lets in"));
        let theirs = say(Failure::Symmetric(Side::Theirs));
        assert!(
            theirs.starts_with("the peer's NAT is symmetric") && theirs.contains("this side's router only lets in")
        );
        assert!(say(Failure::Symmetric(Side::Both)).starts_with("both NATs are symmetric"));
    }
}
