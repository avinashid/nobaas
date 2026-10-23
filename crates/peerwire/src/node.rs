use crate::portmap::{self, PortMapping, Worker};
use crate::signaling::Signaling;
use crate::traversal::{Birthday, Failure, NatType, Privacy, Role, Signal, Traversal};
use crate::Event;
use peerwire_core::{
    Config, ConnectError, Endpoint, Identity, Invite, Lane, Path, PeerId, PeerStats, SendError, Transmit,
};
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
/// A relay peer that drops is dialled again at once, then after 1 s, 2 s, ... up to 30 s; the
/// wait starts over once a link has held for 30 s.
const RELAY_REDIAL: Duration = Duration::from_secs(1);
const RELAY_REDIAL_MAX: Duration = Duration::from_secs(30);
/// Most extra sockets open at once, for birthday punching (two punches' worth).
const MAX_EXTRA_SOCKETS: usize = 512;
/// An extra socket that carries a route is closed once it has heard nothing for this long.
const ROUTE_IDLE: Duration = Duration::from_secs(60);
/// While extra sockets are open, a host sleeping on `next_timeout` wakes at least
/// this often to read them (it can only wait on the main socket).
const EXTRA_POLL: Duration = Duration::from_millis(5);

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
    /// The string is not an `pw:` invite.
    InvalidInvite,
    /// The invite names no address or relay this node can use.
    Connect(ConnectError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidInvite => f.write_str("not a valid pw: invite"),
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
    privacy: Privacy,
    port_mapping: bool,
    /// PCP/NAT-PMP server and SSDP target, instead of the default gateway's (tests).
    portmap_at: Option<(SocketAddr, SocketAddr)>,
    relay_peers: Vec<String>,
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

    /// Which addresses the node gives out. Default: [`Privacy::Direct`]. See
    /// [`Node::set_privacy`].
    pub fn privacy(mut self, privacy: Privacy) -> Self {
        self.privacy = privacy;
        self
    }

    /// Asks the LAN router to forward a public UDP port to this node, by PCP,
    /// NAT-PMP or UPnP-IGD (tried in that order), and offers the mapped address
    /// to peers like a public one: a peer can then reach this node through a
    /// symmetric NAT that hole punching can't get through. The mapping is renewed
    /// while the node lives and removed when it is dropped. It runs on a thread of
    /// its own, for IPv4 and dual-stack sockets not bound to loopback. It doesn't run while the
    /// node is under [`Privacy::RelayOnly`], which gives out no address (see
    /// [`PortMapping::RelayOnly`]). See [`Node::port_mapping`]. Default: off.
    pub fn port_mapping(mut self, on: bool) -> Self {
        self.port_mapping = on;
        self
    }

    /// Port mapping through a given PCP/NAT-PMP server and SSDP address, for
    /// tests against a fake gateway (feature `testing`).
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn port_mapping_at(mut self, gateway: SocketAddr, ssdp: SocketAddr) -> Self {
        (self.port_mapping, self.portmap_at) = (true, Some((gateway, ssdp)));
        self
    }

    /// Always-on peers to keep linked as relays, by their invites: see
    /// [`Node::set_relay_peers`]. [`bind`](Builder::bind) fails with
    /// [`ErrorKind::InvalidInput`] if one is not an invite. Default: none.
    pub fn relay_peers<S: Into<String>>(mut self, invites: impl IntoIterator<Item = S>) -> Self {
        self.relay_peers = invites.into_iter().map(Into::into).collect();
        self
    }

    /// Binds the UDP socket: `"[::]:0"` for any interface, IPv4 and IPv6, and a
    /// free port (see [`Node::bind`]).
    pub fn bind(self, addr: impl ToSocketAddrs) -> io::Result<Node> {
        let sock = bind_socket(addr)?;
        sock.set_nonblocking(true)?;
        let local = sock.local_addr()?;
        let dual = local.is_ipv6() && local.ip().is_unspecified();
        let identity = self.identity.unwrap_or_else(|| Identity::generate(&mut OsRng));
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let mut ep = Endpoint::new(&identity, self.config, seed);
        // A socket on loopback can't reach STUN servers anyway. A dual-stack one
        // asks over IPv4 only: its IPv6 address is global already, and answers
        // from both families would read as a symmetric NAT.
        let stun = if local.ip().is_loopback() { Vec::new() } else { resolve(&self.stun, local.is_ipv6() && !dual) };
        OsRng.fill_bytes(&mut seed);
        let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let mut trav = Traversal::new(&identity, seed, unix_ms, local_addresses(local), stun);
        trav.set_privacy(&mut ep, self.privacy);
        // Starts paused: sync_port_mapping starts it unless relay-only.
        let port_mapping = if self.port_mapping { PortMapping::RelayOnly } else { PortMapping::Off };
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
            portmap: None,
            port_mapping,
            portmap_at: self.portmap_at,
            releasing: None,
            relay_peers: Vec::new(),
            extra: HashMap::new(),
            routes: HashMap::new(),
            next_extra: 0,
        };
        node.sync_port_mapping();
        node.pump_signals();
        node.set_relay_peers(&self.relay_peers).map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
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
    portmap: Option<Worker>,
    port_mapping: PortMapping,
    portmap_at: Option<(SocketAddr, SocketAddr)>,
    /// A worker stopped by a switch to relay-only, deleting its mapping.
    releasing: Option<Worker>,
    relay_peers: Vec<RelayPeer>,
    /// Extra sockets for birthday punching ([`Traversal::poll_birthday`]), by id.
    extra: HashMap<u64, Extra>,
    /// The extra socket that sends to an address: the first to hear from it.
    routes: HashMap<SocketAddr, u64>,
    next_extra: u64,
}

/// An extra socket, opened for birthday punching.
struct Extra {
    sock: UdpSocket,
    /// Closed then, unless it is some address's route.
    until: peerwire_core::Instant,
    /// Last heard from an address it is the route for.
    last_recv: Option<peerwire_core::Instant>,
}

impl Extra {
    fn open(&self, now: peerwire_core::Instant) -> bool {
        match self.last_recv {
            Some(t) => now < t + ROUTE_IDLE,
            None => now < self.until,
        }
    }
}

/// An always-on peer kept linked as a relay ([`Node::set_relay_peers`]).
struct RelayPeer {
    invite: String,
    peer: PeerId,
    /// While unlinked, dial again from then; the wait after that doubles.
    retry_at: Instant,
    wait: Duration,
    /// When the link last came up. The wait goes back to [`RELAY_REDIAL`] only
    /// once a link has held for [`RELAY_REDIAL_MAX`], so a relay that drops
    /// every link at once is dialled less and less often.
    linked_at: Option<Instant>,
}

impl Node {
    /// Binds a UDP socket with a fresh identity, the default [`Config`] and the
    /// [`DEFAULT_STUN_SERVERS`].
    ///
    /// `"[::]:0"` (any interface, a free port) binds one dual-stack socket: the
    /// node offers its IPv4 and its global IPv6 address, and a dial tries both at
    /// once, keeping whichever answers first. That links peers on mobile networks
    /// whose IPv4 is behind a carrier NAT hole punching can't get through, as their
    /// IPv6 has no NAT. On a host without IPv6 it binds `0.0.0.0` on the same port
    /// instead. `"0.0.0.0:0"` binds IPv4 only; a given address binds that one.
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
            privacy: Privacy::Direct,
            port_mapping: false,
            portmap_at: None,
            relay_peers: Vec::new(),
        }
    }

    /// This node's identity, and the key every peer authenticates it by.
    pub fn peer_id(&self) -> PeerId {
        self.ep.peer_id()
    }

    /// The address the socket is bound to (`0.0.0.0` when `[::]` fell back to IPv4).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// An `pw:` invite with this node's [`addresses`](Node::addresses) (under
    /// [`Privacy::RelayOnly`], none: its relay instead, for joiners linked to it).
    /// It also carries this node's invite key: a holder may ask it, over
    /// signaling, to dial back. Share it with those you want to link with.
    pub fn invite(&self) -> String {
        let relays = match self.trav.relay() {
            Some(r) => self.ep.path(&r).and_then(|p| p.direct()).map(|a| (r, *a)).into_iter().collect(),
            None => Vec::new(),
        };
        Invite { peer: self.peer_id(), key: Some(self.trav.invite_key()), addrs: self.addresses(), relays }.to_link()
    }

    /// Which addresses this node gives out.
    pub fn privacy(&self) -> Privacy {
        self.trav.privacy()
    }

    /// Changes which addresses this node gives out, from the next offer and
    /// invite on. Links already up are kept, on the path they have: to stop a
    /// peer knowing an address it already has, disconnect it. The exception is
    /// [relay peers](Node::set_relay_peers), which the app can't see: switching
    /// to [`Privacy::RelayOnly`] closes their links (unless one is the relay it
    /// names), and switching out of it dials them again.
    ///
    /// With [`Builder::port_mapping`] on, switching to [`Privacy::RelayOnly`]
    /// removes the router mapping (in the background), and switching out of it
    /// asks the router again; that waits, like dropping the node, up to 7 s for
    /// the last mapping's removal if the router stalls.
    pub fn set_privacy(&mut self, privacy: Privacy) {
        self.trav.set_privacy(&mut self.ep, privacy);
        if matches!(privacy, Privacy::RelayOnly { .. }) {
            let own = self.trav.relay();
            for r in self.relay_peers.iter().filter(|r| Some(r.peer) != own) {
                self.ep.disconnect(&r.peer);
                self.status.remove(&r.peer);
            }
        }
        self.sync_port_mapping();
    }

    /// Runs router port mapping, when asked for, only while not relay-only.
    fn sync_port_mapping(&mut self) {
        if self.port_mapping == PortMapping::Off {
            return;
        }
        if matches!(self.privacy(), Privacy::RelayOnly { .. }) {
            if let Some(w) = self.portmap.take() {
                w.stop();
                self.releasing = Some(w);
            }
            self.port_mapping = PortMapping::RelayOnly;
            self.trav.set_mapped(None);
        } else if self.port_mapping == PortMapping::RelayOnly {
            // The old mapping's delete goes out before the new request.
            drop(self.releasing.take());
            self.portmap = start_port_mapping(self.local, self.portmap_at);
            self.port_mapping = if self.portmap.is_some() { PortMapping::Trying } else { PortMapping::Unavailable };
        }
    }

    /// Addresses a peer may reach this node at, as put in [`invite`](Node::invite):
    /// the public address from STUN, any public address connected peers have
    /// reported, and the bound address (or, when bound to any interface, the LAN
    /// and loopback addresses), as the [`Privacy`] mode allows.
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

    /// How router port mapping ([`Builder::port_mapping`]) stands: the mapped
    /// address and the protocol that made it, once the router agreed. Updated by
    /// [`poll`](Node::poll).
    pub fn port_mapping(&self) -> PortMapping {
        self.port_mapping
    }

    /// Dials the peer named in an invite, at every address it lists (and through any
    /// listed relay this node is already connected to). With a signaling carrier it
    /// also sends the peer this node's addresses, so the peer dials back and the
    /// handshake punches through NATs on both sides. Returns the peer's id; an
    /// [`Event::Connected`] or [`Event::ConnectFailed`] follows from [`poll`](Node::poll).
    ///
    /// Under [`Privacy::RelayOnly`] it dials through the relay only (the relay
    /// itself directly), and fails with [`ConnectError::NoRelay`] while no relay
    /// is connected: it never falls back to a direct path.
    pub fn connect(&mut self, invite: &str) -> Result<PeerId, Error> {
        let inv: Invite<SocketAddr> = Invite::parse(invite).ok_or(Error::InvalidInvite)?;
        let reaches = |a: &&SocketAddr| self.trav.reaches(*a);
        let mut targets: Vec<Path<SocketAddr>> = inv.addrs.iter().filter(reaches).copied().map(Path::Direct).collect();
        for (relay, addr) in &inv.relays {
            if self.ep.is_connected(relay) {
                targets.push(Path::Via(*relay));
                targets.extend(Some(addr).filter(reaches).copied().map(Path::Direct));
            }
        }
        let relay = self.trav.relay();
        if matches!(self.privacy(), Privacy::RelayOnly { .. }) && relay != Some(inv.peer) && inv.peer != self.peer_id()
        {
            let r = relay.filter(|r| self.ep.is_connected(r)).ok_or(Error::Connect(ConnectError::NoRelay))?;
            targets = vec![Path::Via(r)];
        }
        let now = self.now();
        self.ep.connect_with(now, inv.peer, &targets).map_err(Error::Connect)?;
        if !self.ep.is_connected(&inv.peer) {
            self.status.insert(inv.peer, Status::Connecting);
            // Also marks the peer as welcome while in a room (the offer is dropped with no carrier).
            self.trav.introduce(now, &self.ep, inv.peer, inv.key);
            self.pump_signals();
        }
        Ok(inv.peer)
    }

    /// Keeps a link to each of these always-on peers (by their invites, such as
    /// the one `peerwire-peer` prints), dialling one again when its link drops or
    /// a dial fails, so that peers this node can't punch to (both behind
    /// symmetric NATs, say) still link with it, through one both are linked to
    /// ([`Path::Via`]). Offers name them first among this node's links. Direct
    /// paths are still dialled too, and a relayed link moves to a direct path
    /// when one turns out to work. Replaces the last list (links to peers no
    /// longer on it are closed); empty stops. Fails, changing nothing, if one is
    /// not an invite.
    ///
    /// Relay links carry no app traffic: they raise no [`Event`]s and are not in
    /// [`peers`](Node::peers); [`relay_peers`](Node::relay_peers) shows them.
    /// A room admits them, as peers this node dialled. Not dialled under
    /// [`Privacy::RelayOnly`], which links through its own relay only.
    pub fn set_relay_peers<S: AsRef<str>>(&mut self, invites: &[S]) -> Result<(), Error> {
        let mut new: Vec<RelayPeer> = Vec::new();
        for invite in invites {
            let invite = invite.as_ref().trim();
            let inv: Invite<SocketAddr> = Invite::parse(invite).ok_or(Error::InvalidInvite)?;
            if inv.peer != self.peer_id() && new.iter().all(|r| r.peer != inv.peer) {
                let now = Instant::now();
                let (retry_at, wait, linked_at) = (now, RELAY_REDIAL, None);
                new.push(RelayPeer { invite: invite.to_string(), peer: inv.peer, retry_at, wait, linked_at });
            }
        }
        for old in std::mem::take(&mut self.relay_peers) {
            if new.iter().all(|r| r.peer != old.peer) {
                self.ep.disconnect(&old.peer);
                self.status.remove(&old.peer);
            }
        }
        self.trav.set_relay_peers(new.iter().map(|r| r.peer).collect());
        self.relay_peers = new;
        self.poll(); // dials them
        Ok(())
    }

    /// The relay peers set with [`set_relay_peers`](Node::set_relay_peers), and
    /// where this node stands with each.
    pub fn relay_peers(&self) -> Vec<(PeerId, Status)> {
        self.relay_peers.iter().map(|r| (r.peer, self.status(&r.peer))).collect()
    }

    fn is_relay_peer(&self, peer: &PeerId) -> bool {
        self.relay_peers.iter().any(|r| r.peer == *peer)
    }

    /// Dials the relay peers that are neither linked nor being dialled, once their wait is over.
    fn keep_relay_links(&mut self, now: Instant) {
        if matches!(self.privacy(), Privacy::RelayOnly { .. }) {
            return;
        }
        for i in 0..self.relay_peers.len() {
            let (peer, at) = (self.relay_peers[i].peer, self.relay_peers[i].retry_at);
            if self.status(&peer) == Status::Connected {
                let r = &mut self.relay_peers[i];
                if r.linked_at.is_some_and(|t| now.saturating_duration_since(t) >= RELAY_REDIAL_MAX) {
                    r.wait = RELAY_REDIAL;
                }
            }
            if self.status(&peer) != Status::Disconnected || now < at {
                continue;
            }
            let invite = self.relay_peers[i].invite.clone();
            let _ = self.connect(&invite);
            let r = &mut self.relay_peers[i];
            r.retry_at = now + r.wait;
            r.wait = (r.wait * 2).min(RELAY_REDIAL_MAX);
        }
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

    /// Why the last dial to `peer` failed, after an [`Event::ConnectFailed`]. Its
    /// `Display` is a sentence for users, naming the symmetric side if there is one.
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

    /// Peers with a live link, except [relay peers](Node::set_relay_peers).
    pub fn peers(&self) -> impl Iterator<Item = &PeerId> {
        self.ep.connected_peers().filter(|p| !self.is_relay_peer(p))
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
    pub fn stats(&self) -> peerwire_core::Stats {
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
        let wall = now;
        let now = self.at(now);
        if let Some(s) = self.portmap.as_ref().and_then(|w| w.poll()) {
            self.port_mapping = s;
            self.trav.set_mapped(s.addr());
        }
        let mut drained = false;
        for _ in 0..MAX_READS_PER_POLL {
            match self.sock.recv_from(&mut self.buf[..]) {
                Ok((n, from)) => {
                    // A dual-stack socket sees IPv4 peers as ::ffff:a.b.c.d.
                    let from = SocketAddr::new(from.ip().to_canonical(), from.port());
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
        self.read_extra(now);
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
        while let Some(b) = self.trav.poll_birthday() {
            self.open_extra(&b);
        }
        self.close_extra(now);
        while let Some(e) = self.ep.poll_event() {
            let about = match &e {
                Event::Connected { peer, .. }
                | Event::Message { peer, .. }
                | Event::Disconnected { peer, .. }
                | Event::PathChanged { peer, .. } => Some(*peer),
                Event::AddrObserved { by, .. } => Some(*by),
                Event::ConnectFailed { .. } => None,
            };
            if let Some(peer) = about.filter(|p| self.status(p) != Status::Connected && !self.admits(p, &e)) {
                // In a room, a link from a peer that never proved the room key, or a
                // relay-only node's new direct link to anyone but its relay: closed unseen.
                if matches!(e, Event::Connected { .. }) {
                    self.ep.disconnect(&peer);
                }
                continue;
            }
            self.trav.handle_event(now, &mut self.ep, &e);
            self.track(&e);
            let subject = match &e {
                Event::AddrObserved { by, .. } => by,
                Event::Connected { peer, .. }
                | Event::Message { peer, .. }
                | Event::Disconnected { peer, .. }
                | Event::PathChanged { peer, .. }
                | Event::ConnectFailed { peer } => peer,
            };
            if let Some(r) = self.relay_peers.iter_mut().find(|r| r.peer == *subject) {
                if matches!(e, Event::Connected { .. }) {
                    r.linked_at = Some(wall);
                }
                continue; // a relay link is the node's own business, not the app's
            }
            self.events.push_back(e);
        }
        self.keep_relay_links(wall);
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
        let wall = Instant::now();
        let now = self.at(wall);
        let t = [self.ep.poll_timeout(), self.trav.poll_timeout()].into_iter().flatten().min();
        let t = t.map(|t| t.saturating_duration_since(now));
        let redial = self.relay_peers.iter().filter(|r| self.status(&r.peer) == Status::Disconnected);
        let redial = redial.map(|r| r.retry_at.saturating_duration_since(wall)).min();
        let redial = redial.filter(|_| !matches!(self.privacy(), Privacy::RelayOnly { .. }));
        let extra = (!self.extra.is_empty()).then_some(EXTRA_POLL);
        let t = [t, redial, extra].into_iter().flatten().min();
        match self.signaling {
            Some(_) => Some(t.map_or(SIGNAL_POLL, |t| t.min(SIGNAL_POLL))),
            None => t,
        }
    }

    /// The UDP socket, for hosts that wait on its readiness (it is non-blocking).
    /// While a birthday punch keeps extra sockets open (see
    /// [`Traversal::poll_birthday`]), [`next_timeout`](Node::next_timeout) is at most
    /// 5 ms, so a host waiting on this socket still reads them.
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }

    fn now(&self) -> peerwire_core::Instant {
        self.at(Instant::now())
    }

    fn at(&self, now: Instant) -> peerwire_core::Instant {
        peerwire_core::Instant::from_micros(now.saturating_duration_since(self.epoch).as_micros() as u64)
    }

    fn flush(&mut self, now: peerwire_core::Instant) {
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

    /// Whether an event about a not yet connected `peer` belongs (see `poll_drained`).
    fn admits(&self, peer: &PeerId, e: &Event) -> bool {
        let direct = matches!(e, Event::Connected { path: Path::Direct(_), .. });
        let relay_only = matches!(self.privacy(), Privacy::RelayOnly { .. });
        self.trav.admits(peer) && !(relay_only && direct && self.trav.relay() != Some(*peer))
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

    /// `false` only when the socket's send buffer is full. Goes out of the extra
    /// socket that is `t.dst`'s route, if any, else the main one.
    fn try_send(&self, t: &Transmit<SocketAddr>) -> bool {
        let sock = self.routes.get(&t.dst).and_then(|id| self.extra.get(id)).map_or(&self.sock, |x| &x.sock);
        match sock.send_to(&t.payload, self.wire_addr(t.dst)) {
            Ok(_) => true,
            Err(e) => e.kind() != ErrorKind::WouldBlock, // unreachable addresses etc.: drop it
        }
    }

    /// `dst` as the socket takes it: IPv4 as `::ffff:a.b.c.d` on an IPv6 socket.
    fn wire_addr(&self, dst: SocketAddr) -> SocketAddr {
        match dst {
            SocketAddr::V4(a) if self.local.is_ipv6() => SocketAddr::new(a.ip().to_ipv6_mapped().into(), a.port()),
            dst => dst,
        }
    }

    /// Opens a birthday punch's sockets, on the main socket's address but any
    /// port, and sends its payload from each (see [`Traversal::poll_birthday`]).
    fn open_extra(&mut self, b: &Birthday<SocketAddr>) {
        let any = SocketAddr::new(self.local.ip(), 0);
        let n = (b.sockets as usize).min(MAX_EXTRA_SOCKETS.saturating_sub(self.extra.len()));
        for _ in 0..n {
            let Ok(sock) = bind_socket(any) else { break };
            if sock.set_nonblocking(true).is_err() {
                continue;
            }
            let _ = sock.send_to(&b.payload, self.wire_addr(b.dst));
            self.next_extra += 1;
            self.extra.insert(self.next_extra, Extra { sock, until: b.until, last_recv: None });
        }
    }

    /// Hands the endpoint what the extra sockets heard. The first open one to hear
    /// from an address becomes its route.
    fn read_extra(&mut self, now: peerwire_core::Instant) {
        let ids: Vec<u64> = self.extra.keys().copied().collect();
        for id in ids {
            for _ in 0..MAX_READS_PER_POLL {
                let Some(x) = self.extra.get(&id) else { break };
                let (n, from) = match x.sock.recv_from(&mut self.buf[..]) {
                    Ok(r) => r,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => continue,
                };
                let from = SocketAddr::new(from.ip().to_canonical(), from.port());
                if peerwire_core::stun::is_stun(&self.buf[..n]) {
                    continue; // STUN is asked on the main socket only
                }
                let open = |r: &u64| self.extra.get(r).is_some_and(|x| x.open(now));
                let route = self.routes.get(&from).copied().filter(open).unwrap_or(id);
                self.routes.insert(from, route);
                if route == id {
                    if let Some(x) = self.extra.get_mut(&id) {
                        x.last_recv = Some(now);
                    }
                }
                self.ep.handle_datagram(now, from, &self.buf[..n]);
            }
        }
    }

    /// Closes the extra sockets that are no route by their `until`, and routes
    /// that have heard nothing for [`ROUTE_IDLE`].
    fn close_extra(&mut self, now: peerwire_core::Instant) {
        if self.extra.is_empty() {
            return;
        }
        self.extra.retain(|_, x| x.open(now));
        let extra = &self.extra;
        self.routes.retain(|_, id| extra.contains_key(id));
    }

    /// The local addresses of the open extra sockets, and how many are routes.
    #[cfg(test)]
    fn extra_sockets(&self) -> (Vec<SocketAddr>, usize) {
        let addrs = self.extra.values().filter_map(|x| x.sock.local_addr().ok()).collect();
        (addrs, self.extra.values().filter(|x| x.last_recv.is_some()).count())
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

/// Binds `addr`, the first of its addresses that binds. `[::]` binds a dual-stack
/// socket, or `0.0.0.0` on the same port where the host has no IPv6.
fn bind_socket(addr: impl ToSocketAddrs) -> io::Result<UdpSocket> {
    let mut last = io::Error::new(ErrorKind::InvalidInput, "no address to bind");
    for a in addr.to_socket_addrs()? {
        let bound = match a {
            SocketAddr::V6(v6) if v6.ip().is_unspecified() => match dual_stack(a) {
                Err(e) if e.kind() == ErrorKind::AddrInUse => Err(e),
                Err(_) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, v6.port())),
                ok => ok,
            },
            _ => UdpSocket::bind(a),
        };
        match bound {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// An IPv6 socket that also takes IPv4 (as v4-mapped addresses).
fn dual_stack(addr: SocketAddr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_only_v6(false)?;
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// The socket's own addresses: the bound one, or, when bound to any interface, the
/// address of each family's default route and loopback (both families for a
/// dual-stack socket; IPv6 only with a route out, which in practice means global),
/// then up to [`MAX_PRIVATE`] other private IPv4 interface addresses. Those are a
/// phone's hotspot side (its default route is cellular) or a second LAN: a peer
/// tethered to it reaches the socket there.
fn local_addresses(local: SocketAddr) -> Vec<SocketAddr> {
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let families: &[bool] = if local.is_ipv6() { &[false, true] } else { &[false] };
    let at = |ip| SocketAddr::new(ip, local.port());
    let mut out: Vec<SocketAddr> = families.iter().filter_map(|&v6| primary_ip(v6)).map(at).collect();
    out.push(at(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    if local.is_ipv6() {
        out.push(at(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    let private = private_v4(ifaces.iter().map(|i| (i.name.as_str(), i.ip())));
    for ip in private.into_iter().map(at) {
        if !out.contains(&ip) && out.len() < families.len() + 2 + MAX_PRIVATE {
            out.push(ip);
        }
    }
    out
}

/// How many extra private interface addresses a socket offers (the offer holds 8).
const MAX_PRIVATE: usize = 3;

/// The private IPv4 addresses (RFC 1918) of `(interface, address)` pairs, without
/// container and VM bridges, which no other device can reach.
fn private_v4<'a>(ifaces: impl Iterator<Item = (&'a str, IpAddr)>) -> Vec<IpAddr> {
    const BRIDGES: [&str; 6] = ["docker", "br-", "veth", "virbr", "vmnet", "cni"];
    let mut out = Vec::new();
    for (name, ip) in ifaces {
        let IpAddr::V4(v4) = ip else { continue };
        if v4.is_private() && !BRIDGES.iter().any(|b| name.starts_with(b)) && !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// Starts router port mapping for the socket at `local`, through `at` or else the
/// default gateway. `None` when it can't run: no IPv4, loopback or no LAN address.
fn start_port_mapping(local: SocketAddr, at: Option<(SocketAddr, SocketAddr)>) -> Option<Worker> {
    let (gateway, ssdp) = match at {
        Some(at) => at,
        None => {
            let lan = match local.ip() {
                ip if ip.is_unspecified() => primary_ip(false)?, // dual-stack sockets map their IPv4
                ip => ip,
            };
            let IpAddr::V4(lan) = lan else { return None };
            if lan.is_loopback() {
                return None;
            }
            (SocketAddr::from((portmap::default_gateway(lan), portmap::GATEWAY_PORT)), portmap::SSDP)
        }
    };
    let mut seed = [0u8; 20];
    OsRng.fill_bytes(&mut seed);
    Worker::spawn(local.port(), gateway, ssdp, seed).ok()
}

/// Resolves `host:port` names, keeping the first address of the socket's family.
fn resolve(servers: &[String], v6: bool) -> Vec<SocketAddr> {
    servers.iter().filter_map(|s| s.to_socket_addrs().ok()?.find(|a| a.is_ipv6() == v6)).collect()
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

    /// NB-044: a birthday punch's extra sockets. A peer that reaches one (as a
    /// spray probe would) links through it, every reply leaves from it, so the peer
    /// never sees the main port, and the others close at `until`.
    #[test]
    fn birthday_sockets_carry_the_link_they_hear() {
        let (mut a, mut b) = (quiet("127.0.0.1:0"), quiet("127.0.0.1:0"));
        let until = a.now() + Duration::from_millis(300);
        let opened = Birthday { peer: b.peer_id(), dst: b.local_addr(), sockets: 4, payload: b"x".to_vec(), until };
        a.open_extra(&opened);
        let (socks, _) = a.extra_sockets();
        assert_eq!(socks.len(), 4);
        assert!(socks.iter().all(|s| s.port() != a.local_addr().port()));
        let hit = socks[2];
        let now = b.now();
        b.ep.connect(now, hit, a.peer_id()).unwrap();
        let t0 = Instant::now();
        while !(a.status(&b.peer_id()) == Status::Connected && b.status(&a.peer_id()) == Status::Connected) {
            assert!(t0.elapsed() < Duration::from_secs(5), "no link");
            b.poll();
            a.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(b.path(&a.peer_id()), Some(&Path::Direct(hit)));
        a.send(&b.peer_id(), Lane::Reliable, b"from the extra socket").unwrap();
        let mut got = Vec::new();
        while t0.elapsed() < Duration::from_secs(5) && got.is_empty() {
            a.poll();
            b.poll();
            while let Some(e) = b.try_recv_event() {
                if let Event::Message { data, .. } = e {
                    got.push(data);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(got, vec![b"from the extra socket".to_vec()]);
        assert_eq!(b.path(&a.peer_id()), Some(&Path::Direct(hit)), "A's replies came from the extra socket");
        assert!(a.next_timeout().is_some_and(|t| t <= EXTRA_POLL), "wakes to read the extra sockets");
        while t0.elapsed() < Duration::from_millis(400) {
            a.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        let (socks, routes) = a.extra_sockets();
        assert_eq!((socks, routes), (vec![hit], 1), "only the route is left");
    }

    /// NB-042: any interface lists IPv6 only on a dual-stack socket.
    #[test]
    fn any_interface_lists_both_families_when_dual_stack() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        let v4 = local_addresses(at("0.0.0.0:7"));
        assert!(v4.contains(&at("127.0.0.1:7")) && v4.iter().all(|a| a.is_ipv4()), "{v4:?}");
        let dual = local_addresses(at("[::]:7"));
        assert!(dual.contains(&at("127.0.0.1:7")) && dual.contains(&at("[::1]:7")), "{dual:?}");
        assert_eq!(dual.iter().filter(|a| a.is_ipv4()).count(), v4.len(), "the same IPv4 addresses");
        assert_eq!(local_addresses(at("[::1]:7")), vec![at("[::1]:7")]);
    }

    /// A phone sharing mobile data: its hotspot address is offered, bridges are not.
    #[test]
    fn private_interface_addresses_are_offered() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let ifaces = [
            ("rmnet_data0", ip("192.0.0.4")),
            ("wlan1", ip("10.129.132.170")),
            ("lo", ip("127.0.0.1")),
            ("docker0", ip("172.17.0.1")),
            ("br-1a2b", ip("172.18.0.1")),
            ("eth0", ip("192.168.1.5")),
            ("eth0", ip("2001:db8::5")),
            ("tun0", ip("100.74.51.161")),
            ("wlan1", ip("10.129.132.170")),
        ];
        assert_eq!(private_v4(ifaces.into_iter()), vec![ip("10.129.132.170"), ip("192.168.1.5")]);
        // On this host: the default route's address and loopback come first, at most 3 more.
        let any = local_addresses("0.0.0.0:7".parse().unwrap());
        assert!(any.len() <= 2 + MAX_PRIVATE && any.iter().all(|a| a.port() == 7), "{any:?}");
    }

    #[test]
    fn bad_invites_are_refused() {
        let mut n = Node::bind("127.0.0.1:0").unwrap();
        assert_eq!(n.connect("hello"), Err(Error::InvalidInvite));
        let own = n.invite();
        assert_eq!(n.connect(&own), Err(Error::Connect(ConnectError::SelfConnect)));
        let empty = Invite::<SocketAddr> {
            peer: Node::bind("127.0.0.1:0").unwrap().peer_id(),
            key: None,
            addrs: vec![],
            relays: vec![],
        };
        assert_eq!(n.connect(&empty.to_link()), Err(Error::Connect(ConnectError::NoPath)));
        assert_eq!(n.status(&empty.peer), Status::Disconnected);
    }

    /// NB-038: off by default; once the router maps a port, the node offers it, and
    /// dropping the node removes the mapping.
    #[test]
    fn port_mapping_is_offered_once_mapped_and_removed_on_drop() {
        use crate::portmap::tests::{eventually, lifetime_of, pcp_gateway};
        assert_eq!(quiet("127.0.0.1:0").port_mapping(), PortMapping::Off);
        let on_loopback = Node::builder().stun_servers(Vec::<String>::new()).port_mapping(true).bind("127.0.0.1:0");
        assert_eq!(on_loopback.unwrap().port_mapping(), PortMapping::Unavailable, "no gateway for loopback");
        let ext = Ipv4Addr::new(203, 0, 113, 7);
        let (gw, log) = pcp_gateway(ext, 120);
        let b = Node::builder().stun_servers(Vec::<String>::new()).port_mapping_at(gw, "127.0.0.1:9".parse().unwrap());
        let mut n = b.bind("127.0.0.1:0").unwrap();
        assert_eq!(n.port_mapping(), PortMapping::Trying);
        let mapped = SocketAddr::from((ext, 41000));
        assert!(eventually(|| {
            n.poll();
            n.port_mapping().addr() == Some(mapped)
        }));
        assert_eq!(n.addresses(), vec![mapped, n.local_addr()]);
        let inv: Invite<SocketAddr> = Invite::parse(&n.invite()).unwrap();
        assert!(inv.addrs.contains(&mapped));
        drop(n);
        assert!(eventually(|| log.lock().unwrap().last().is_some_and(|r| lifetime_of(r) == 0)));
    }

    /// NPM-1: a relay-only node gives out no address, so it asks the router for
    /// none; switching to relay-only removes a mapping, and switching back maps again.
    #[test]
    fn port_mapping_is_skipped_under_relay_only() {
        use crate::portmap::tests::{eventually, lifetime_of, pcp_gateway};
        let ext = Ipv4Addr::new(203, 0, 113, 7);
        let (gw, log) = pcp_gateway(ext, 120);
        let mut n = Node::builder()
            .stun_servers(Vec::<String>::new())
            .privacy(Privacy::RelayOnly { via: None })
            .port_mapping_at(gw, "127.0.0.1:9".parse().unwrap())
            .bind("127.0.0.1:0")
            .unwrap();
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        std::thread::sleep(std::time::Duration::from_millis(300));
        n.poll();
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        assert!(log.lock().unwrap().is_empty(), "no request to the router");

        let mapped = SocketAddr::from((ext, 41000));
        let map = |n: &mut Node| {
            eventually(|| {
                n.poll();
                n.port_mapping().addr() == Some(mapped)
            })
        };
        n.set_privacy(Privacy::Direct);
        assert_eq!(n.port_mapping(), PortMapping::Trying);
        assert!(map(&mut n));
        assert!(n.addresses().contains(&mapped));

        n.set_privacy(Privacy::RelayOnly { via: None });
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        assert!(eventually(|| log.lock().unwrap().last().is_some_and(|r| lifetime_of(r) == 0)), "deleted");
        n.poll();
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        n.set_privacy(Privacy::RelayOnly { via: Some(n.peer_id()) });
        let requests = log.lock().unwrap().len();

        n.set_privacy(Privacy::PublicOnly);
        assert!(map(&mut n), "mapped again");
        assert!(log.lock().unwrap().len() > requests);
        assert_ne!(lifetime_of(log.lock().unwrap().last().unwrap()), 0);
        assert_eq!(n.addresses(), vec![mapped]);
    }
}
