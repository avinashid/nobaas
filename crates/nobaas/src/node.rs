use crate::signaling::Signaling;
use crate::traversal::{Failure, NatType, Role, Signal, Traversal};
use crate::Event;
use nobaas_core::{Config, ConnectError, Endpoint, Identity, Invite, Lane, Path, PeerId, PeerStats, SendError, Transmit};
use rand_core::{OsRng, RngCore};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Most datagrams read in one [`Node::poll`], so a flood can't stall the caller's frame.
const MAX_READS_PER_POLL: usize = 4096;
/// Most signaling messages handled in one poll.
const MAX_SIGNALS_PER_POLL: usize = 256;
/// Datagrams kept while the socket's send buffer is full; older ones are dropped, as UDP would.
const MAX_PENDING_SENDS: usize = 1024;
/// Signaling carriers are polled, so a host sleeping on `next_timeout` wakes at least this often.
const SIGNAL_POLL: Duration = Duration::from_millis(50);

/// Public STUN servers [`Node::bind`] asks for the node's public address.
pub const DEFAULT_STUN_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

/// Where a node stands with one peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not connected, and not dialling.
    Disconnected,
    /// Dialling: the handshake is under way.
    Connecting,
    /// The encrypted link is up.
    Connected,
}

/// Why [`Node::connect`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The string is not an `nb:` invite.
    InvalidInvite,
    /// The invite names no address or relay this node can use.
    Connect(ConnectError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidInvite => f.write_str("not a valid nb: invite"),
            Error::Connect(e) => write!(f, "cannot dial: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Options for [`Node`], from [`Node::builder`].
pub struct Builder {
    identity: Option<Identity>,
    config: Config,
    stun: Vec<String>,
    signaling: Option<Box<dyn Signaling>>,
}

impl Builder {
    /// The identity to use (to keep the same [`PeerId`] across runs). Default: a fresh one.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Session settings. Default: [`Config::default`].
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// STUN servers (`host:port`) to learn the public address from. Default:
    /// [`DEFAULT_STUN_SERVERS`]; empty turns STUN off. Names are resolved in
    /// [`bind`](Builder::bind), and servers that don't resolve are skipped.
    pub fn stun_servers<S: Into<String>>(mut self, servers: impl IntoIterator<Item = S>) -> Self {
        self.stun = servers.into_iter().map(Into::into).collect();
        self
    }

    /// A signaling carrier, for rooms ([`Node::join_room`]) and for invites that
    /// punch through NATs. Default: none (invites work where the host is reachable).
    pub fn signaling(mut self, signaling: impl Signaling + 'static) -> Self {
        self.signaling = Some(Box::new(signaling));
        self
    }

    /// Binds the UDP socket (`"0.0.0.0:0"` for any interface and a free port).
    pub fn bind(self, addr: impl ToSocketAddrs) -> io::Result<Node> {
        let sock = UdpSocket::bind(addr)?;
        sock.set_nonblocking(true)?;
        let local = sock.local_addr()?;
        let identity = self.identity.unwrap_or_else(|| Identity::generate(&mut OsRng));
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let ep = Endpoint::new(&identity, self.config, seed);
        // A socket on loopback can't reach STUN servers anyway.
        let stun = if local.ip().is_loopback() { Vec::new() } else { resolve(&self.stun, local.is_ipv6()) };
        OsRng.fill_bytes(&mut seed);
        let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let trav = Traversal::new(&identity, seed, unix_ms, local_addresses(local), stun);
        let mut node = Node {
            sock,
            local,
            ep,
            trav,
            signaling: self.signaling,
            epoch: Instant::now(),
            status: HashMap::new(),
            events: VecDeque::new(),
            pending: VecDeque::new(),
            buf: Box::new([0; 2048]),
        };
        node.pump_signals();
        Ok(node)
    }
}

/// One identity on one non-blocking UDP socket, driven by [`poll`](Node::poll).
pub struct Node {
    sock: UdpSocket,
    local: SocketAddr,
    ep: Endpoint<SocketAddr>,
    trav: Traversal<SocketAddr>,
    signaling: Option<Box<dyn Signaling>>,
    epoch: Instant,
    status: HashMap<PeerId, Status>,
    events: VecDeque<Event>,
    pending: VecDeque<Transmit<SocketAddr>>,
    buf: Box<[u8; 2048]>,
}

impl Node {
    /// Binds a UDP socket (`"0.0.0.0:0"` for any interface and a free port) with a
    /// fresh identity, the default [`Config`] and the [`DEFAULT_STUN_SERVERS`].
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Node> {
        Node::builder().bind(addr)
    }

    /// Binds with a given identity (to keep the same [`PeerId`] across runs) and config.
    pub fn bind_with(addr: impl ToSocketAddrs, identity: Identity, config: Config) -> io::Result<Node> {
        Node::builder().identity(identity).config(config).bind(addr)
    }

    /// Options: identity, config, STUN servers, signaling.
    pub fn builder() -> Builder {
        Builder {
            identity: None,
            config: Config::default(),
            stun: DEFAULT_STUN_SERVERS.iter().map(|s| s.to_string()).collect(),
            signaling: None,
        }
    }

    /// This node's identity, and the key every peer authenticates it by.
    pub fn peer_id(&self) -> PeerId {
        self.ep.peer_id()
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// An `nb:` invite with this node's [`addresses`](Node::addresses). It holds no secret.
    pub fn invite(&self) -> String {
        Invite { peer: self.peer_id(), addrs: self.addresses(), relays: Vec::new() }.to_link()
    }

    /// Addresses a peer may reach this node at, as put in [`invite`](Node::invite):
    /// the public address from STUN, any public address connected peers have
    /// reported, and the bound address (or, when bound to any interface, the LAN
    /// and loopback addresses).
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.trav.candidates()
    }

    /// The public address STUN reported for this socket, once a server has answered.
    pub fn public_addr(&self) -> Option<SocketAddr> {
        self.trav.public_addr().copied()
    }

    /// What STUN says about this node's NAT.
    pub fn nat_type(&self) -> NatType {
        self.trav.nat_type()
    }

    /// Dials the peer named in an invite, at every address it lists (and through any
    /// listed relay this node is already connected to). With a signaling carrier it
    /// also sends the peer this node's addresses, so the peer dials back and the
    /// handshake punches through NATs on both sides. Returns the peer's id; an
    /// [`Event::Connected`] or [`Event::ConnectFailed`] follows from [`poll`](Node::poll).
    pub fn connect(&mut self, invite: &str) -> Result<PeerId, Error> {
        let inv: Invite<SocketAddr> = Invite::parse(invite).ok_or(Error::InvalidInvite)?;
        let mut targets: Vec<Path<SocketAddr>> = inv.addrs.iter().copied().map(Path::Direct).collect();
        for (relay, addr) in &inv.relays {
            if self.ep.is_connected(relay) {
                targets.push(Path::Via(*relay));
                targets.push(Path::Direct(*addr));
            }
        }
        let now = self.now();
        self.ep.connect_with(now, inv.peer, &targets).map_err(Error::Connect)?;
        if !self.ep.is_connected(&inv.peer) {
            self.status.insert(inv.peer, Status::Connecting);
            // Also marks the peer as welcome while in a room (the offer is dropped with no carrier).
            self.trav.introduce(now, &self.ep, inv.peer);
            self.pump_signals();
        }
        Ok(inv.peer)
    }

    /// Joins room `room` of application `app_id` on the signaling carrier, as a mesh
    /// [`Role::Peer`]: this node connects to every other peer in it, and keeps
    /// announcing itself for those that join later. Leaves any other room. Needs a
    /// [`signaling`](Builder::signaling) carrier; without one nobody hears it.
    ///
    /// The room's name is its password: only peers that know it can link in it
    /// (relays see just a hash). While in a room, this node closes links from
    /// anyone else, except peers it dialled itself with [`connect`](Node::connect).
    /// A name others can guess makes a public lobby.
    pub fn join_room(&mut self, app_id: &str, room: &str) {
        self.join_room_as(app_id, room, Role::Peer);
    }

    /// [`join_room`](Node::join_room) with a role: a [`Role::Host`] links with clients
    /// and peers, a [`Role::Client`] with hosts only (a star, as a game server).
    pub fn join_room_as(&mut self, app_id: &str, room: &str, role: Role) {
        self.join_room_with(app_id, room, role, None);
    }

    /// [`join_room_as`](Node::join_room_as), linking with `host` only when given: a
    /// client that knows its host's key can't be taken in by another room member.
    pub fn join_room_with(&mut self, app_id: &str, room: &str, role: Role, host: Option<PeerId>) {
        let now = self.now();
        self.trav.join_room(now, app_id, room, role, host);
        self.pump_signals();
    }

    /// The role `peer` proved it holds in the current room (by knowing its name).
    pub fn room_role(&self, peer: &PeerId) -> Option<Role> {
        self.trav.room_role(peer)
    }

    /// Leaves the room: stops announcing and dialling. Links stay up.
    pub fn leave_room(&mut self) {
        self.trav.leave_room();
        self.pump_signals();
    }

    /// Why the last dial to `peer` failed, after an [`Event::ConnectFailed`].
    pub fn failure(&self, peer: &PeerId) -> Option<Failure> {
        self.trav.failure(peer)
    }

    /// Queues a message for `peer`; it goes out on the next [`poll`](Node::poll). Never
    /// blocks: when the reliable send buffer is full it returns
    /// [`SendError::WouldBlock`], and the unreliable lane drops its oldest message.
    pub fn send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError> {
        self.ep.send(peer, lane, data)
    }

    /// Closes the link to `peer`, telling it so. No event follows locally.
    pub fn disconnect(&mut self, peer: &PeerId) {
        self.ep.disconnect(peer);
        self.status.remove(peer);
        self.poll();
    }

    /// Where this node stands with `peer`.
    pub fn status(&self, peer: &PeerId) -> Status {
        self.status.get(peer).copied().unwrap_or(Status::Disconnected)
    }

    /// Peers with a live link.
    pub fn peers(&self) -> impl Iterator<Item = &PeerId> {
        self.ep.connected_peers()
    }

    /// RTT and loss for a connected peer.
    pub fn peer_stats(&self, peer: &PeerId) -> Option<PeerStats> {
        self.ep.peer_stats(peer)
    }

    /// The route a connected peer's link takes.
    pub fn path(&self, peer: &PeerId) -> Option<&crate::Path> {
        self.ep.path(peer)
    }

    /// Node-wide counters (packets, drops, relaying, hole punching).
    pub fn stats(&self) -> nobaas_core::Stats {
        self.ep.stats()
    }

    /// Does all pending work without blocking: reads every datagram waiting on the
    /// socket and every signaling message, fires due timers, sends what is queued
    /// and queues the resulting events. Call it often (every frame, or when
    /// [`next_timeout`](Node::next_timeout) expires or the socket is readable).
    pub fn poll(&mut self) {
        self.poll_at(Instant::now());
    }

    /// [`poll`](Node::poll) with the time supplied by the caller (it must never go backwards).
    pub fn poll_at(&mut self, now: Instant) {
        self.poll_drained(now);
    }

    /// [`poll_at`](Node::poll_at), returning whether the socket was read until empty
    /// (`false` when the per-poll read cap stopped it first).
    pub(crate) fn poll_drained(&mut self, now: Instant) -> bool {
        let now = self.at(now);
        let mut drained = false;
        for _ in 0..MAX_READS_PER_POLL {
            match self.sock.recv_from(&mut self.buf[..]) {
                Ok((n, from)) => {
                    if !self.trav.handle_stun(&from, &self.buf[..n]) {
                        self.ep.handle_datagram(now, from, &self.buf[..n]);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                // ICMP errors surface here on some platforms (ConnectionReset): keep reading.
                Err(_) => continue,
            }
        }
        if let Some(sig) = self.signaling.as_mut() {
            for _ in 0..MAX_SIGNALS_PER_POLL {
                let Some((topic, payload)) = sig.poll() else { break };
                self.trav.handle_signal(now, &mut self.ep, topic, &payload);
            }
        }
        if self.ep.poll_timeout().is_some_and(|t| t <= now) {
            self.ep.handle_timeout(now);
        }
        if self.trav.poll_timeout().is_some_and(|t| t <= now) {
            self.trav.handle_timeout(now, &mut self.ep);
        }
        while let Some(e) = self.ep.poll_event() {
            let about = match &e {
                Event::Connected { peer, .. }
                | Event::Message { peer, .. }
                | Event::Disconnected { peer, .. }
                | Event::PathChanged { peer, .. } => Some(*peer),
                Event::AddrObserved { by, .. } => Some(*by),
                Event::ConnectFailed { .. } => None,
            };
            if let Some(peer) = about.filter(|p| self.status(p) != Status::Connected && !self.trav.admits(p)) {
                // In a room, a link from a peer that never proved the room key: closed unseen.
                if matches!(e, Event::Connected { .. }) {
                    self.ep.disconnect(&peer);
                }
                continue;
            }
            self.trav.handle_event(now, &mut self.ep, &e);
            self.track(&e);
            self.events.push_back(e);
        }
        self.flush(now);
        self.pump_signals();
        drained
    }

    /// The next event, if any is queued.
    pub fn try_recv_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// How long until a timer needs [`poll`](Node::poll), if one is armed. A host that
    /// sleeps should wake at this or when the socket is readable, whichever is first.
    /// With a signaling carrier it is at most 50 ms, as carriers are polled.
    pub fn next_timeout(&self) -> Option<Duration> {
        let now = self.at(Instant::now());
        let t = [self.ep.poll_timeout(), self.trav.poll_timeout()].into_iter().flatten().min();
        let t = t.map(|t| t.saturating_duration_since(now));
        match self.signaling {
            Some(_) => Some(t.map_or(SIGNAL_POLL, |t| t.min(SIGNAL_POLL))),
            None => t,
        }
    }

    /// The UDP socket, for hosts that wait on its readiness (it is non-blocking).
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }

    fn now(&self) -> nobaas_core::Instant {
        self.at(Instant::now())
    }

    fn at(&self, now: Instant) -> nobaas_core::Instant {
        nobaas_core::Instant::from_micros(now.saturating_duration_since(self.epoch).as_micros() as u64)
    }

    fn flush(&mut self, now: nobaas_core::Instant) {
        while let Some(t) = self.pending.pop_front() {
            if !self.try_send(&t) {
                self.pending.push_front(t);
                return;
            }
        }
        while let Some(t) = self.ep.poll_transmit(now).or_else(|| self.trav.poll_transmit()) {
            if !self.try_send(&t) {
                if self.pending.len() >= MAX_PENDING_SENDS {
                    self.pending.pop_front();
                }
                self.pending.push_back(t);
            }
        }
    }

    /// Hands the traversal's signaling actions to the carrier (dropped without one).
    fn pump_signals(&mut self) {
        while let Some(s) = self.trav.poll_signal() {
            let Some(sig) = self.signaling.as_mut() else { continue };
            match s {
                Signal::Subscribe(t) => sig.subscribe(t),
                Signal::Unsubscribe(t) => sig.unsubscribe(t),
                Signal::Publish(t, payload) => sig.publish(t, &payload),
            }
        }
    }

    /// `false` only when the socket's send buffer is full.
    fn try_send(&self, t: &Transmit<SocketAddr>) -> bool {
        match self.sock.send_to(&t.payload, t.dst) {
            Ok(_) => true,
            Err(e) => e.kind() != ErrorKind::WouldBlock, // unreachable addresses etc.: drop it
        }
    }

    fn track(&mut self, e: &Event) {
        match e {
            Event::Connected { peer, .. } => {
                self.status.insert(*peer, Status::Connected);
            }
            Event::Disconnected { peer, .. } | Event::ConnectFailed { peer } => {
                self.status.remove(peer);
            }
            Event::Message { .. } | Event::PathChanged { .. } | Event::AddrObserved { .. } => {}
        }
    }
}

/// The socket's own addresses: the bound one, or, when bound to any interface, the
/// LAN address of the default route and loopback.
fn local_addresses(local: SocketAddr) -> Vec<SocketAddr> {
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let (port, v6) = (local.port(), local.is_ipv6());
    let mut out: Vec<SocketAddr> = primary_ip(v6).map(|ip| SocketAddr::new(ip, port)).into_iter().collect();
    let lo = if v6 { IpAddr::V6(Ipv6Addr::LOCALHOST) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
    out.push(SocketAddr::new(lo, port));
    out
}

/// Resolves `host:port` names, keeping the first address of the socket's family.
fn resolve(servers: &[String], v6: bool) -> Vec<SocketAddr> {
    servers
        .iter()
        .filter_map(|s| s.to_socket_addrs().ok()?.find(|a| a.is_ipv6() == v6))
        .collect()
}

/// The address of the interface the default route leaves by. `connect` on a UDP
/// socket only looks up the route: nothing is sent.
fn primary_ip(v6: bool) -> Option<IpAddr> {
    let (bind, probe): (SocketAddr, SocketAddr) = if v6 {
        ((Ipv6Addr::UNSPECIFIED, 0).into(), (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 9).into())
    } else {
        ((Ipv4Addr::UNSPECIFIED, 0).into(), (Ipv4Addr::new(192, 0, 2, 1), 9).into())
    };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(probe).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(addr: &str) -> Node {
        Node::builder().stun_servers(Vec::<String>::new()).bind(addr).unwrap()
    }

    #[test]
    fn invite_round_trips_and_lists_loopback() {
        let n = quiet("0.0.0.0:0");
        let inv: Invite<SocketAddr> = Invite::parse(&n.invite()).unwrap();
        assert_eq!(inv.peer, n.peer_id());
        let port = n.local_addr().port();
        assert!(inv.addrs.contains(&SocketAddr::from(([127, 0, 0, 1], port))));
        let bound = Node::bind("127.0.0.1:0").unwrap();
        assert_eq!(bound.addresses(), vec![bound.local_addr()]);
        assert_eq!((bound.public_addr(), bound.nat_type()), (None, NatType::Unknown));
        assert_eq!(bound.next_timeout(), None, "no STUN on loopback, no signaling: nothing to wake for");
    }

    #[test]
    fn bad_invites_are_refused() {
        let mut n = Node::bind("127.0.0.1:0").unwrap();
        assert_eq!(n.connect("hello"), Err(Error::InvalidInvite));
        let own = n.invite();
        assert_eq!(n.connect(&own), Err(Error::Connect(ConnectError::SelfConnect)));
        let empty = Invite::<SocketAddr> { peer: Node::bind("127.0.0.1:0").unwrap().peer_id(), addrs: vec![], relays: vec![] };
        assert_eq!(n.connect(&empty.to_link()), Err(Error::Connect(ConnectError::NoPath)));
        assert_eq!(n.status(&empty.peer), Status::Disconnected);
    }
}
