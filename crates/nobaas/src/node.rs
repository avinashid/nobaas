use crate::Event;
use nobaas_core::{Config, ConnectError, Endpoint, Identity, Invite, Lane, Path, PeerId, PeerStats, SendError, Transmit};
use rand_core::{OsRng, RngCore};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Most datagrams read in one [`Node::poll`], so a flood can't stall the caller's frame.
const MAX_READS_PER_POLL: usize = 4096;
/// Datagrams kept while the socket's send buffer is full; older ones are dropped, as UDP would.
const MAX_PENDING_SENDS: usize = 1024;
/// Public addresses learned from peers, offered in invites.
const MAX_OBSERVED: usize = 4;

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

/// One identity on one non-blocking UDP socket, driven by [`poll`](Node::poll).
pub struct Node {
    sock: UdpSocket,
    local: SocketAddr,
    ep: Endpoint<SocketAddr>,
    epoch: Instant,
    status: HashMap<PeerId, Status>,
    observed: Vec<SocketAddr>,
    events: VecDeque<Event>,
    pending: VecDeque<Transmit<SocketAddr>>,
    buf: Box<[u8; 2048]>,
}

impl Node {
    /// Binds a UDP socket (`"0.0.0.0:0"` for any interface and a free port) with a
    /// fresh identity and the default [`Config`].
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Node> {
        Node::bind_with(addr, Identity::generate(&mut OsRng), Config::default())
    }

    /// Binds with a given identity (to keep the same [`PeerId`] across runs) and config.
    pub fn bind_with(addr: impl ToSocketAddrs, identity: Identity, config: Config) -> io::Result<Node> {
        let sock = UdpSocket::bind(addr)?;
        sock.set_nonblocking(true)?;
        let local = sock.local_addr()?;
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        Ok(Node {
            sock,
            local,
            ep: Endpoint::new(&identity, config, seed),
            epoch: Instant::now(),
            status: HashMap::new(),
            observed: Vec::new(),
            events: VecDeque::new(),
            pending: VecDeque::new(),
            buf: Box::new([0; 2048]),
        })
    }

    /// This node's identity, and the key every peer authenticates it by.
    pub fn peer_id(&self) -> PeerId {
        self.ep.peer_id()
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// An `nb:` invite with this node's addresses: the bound address (or, when bound
    /// to any interface, the LAN and loopback addresses) and any public address
    /// connected peers have reported. It holds no secret.
    pub fn invite(&self) -> String {
        Invite { peer: self.peer_id(), addrs: self.addresses(), relays: Vec::new() }.to_link()
    }

    /// Addresses a peer may reach this node at, as put in [`invite`](Node::invite).
    pub fn addresses(&self) -> Vec<SocketAddr> {
        let port = self.local.port();
        let mut out = Vec::new();
        if self.local.ip().is_unspecified() {
            let v6 = self.local.is_ipv6();
            if let Some(ip) = primary_ip(v6) {
                out.push(SocketAddr::new(ip, port));
            }
            let lo = if v6 { IpAddr::V6(Ipv6Addr::LOCALHOST) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
            out.push(SocketAddr::new(lo, port));
        } else {
            out.push(self.local);
        }
        for a in &self.observed {
            if !out.contains(a) {
                out.push(*a);
            }
        }
        out
    }

    /// Dials the peer named in an invite, at every address it lists (and through any
    /// listed relay this node is already connected to). Returns the peer's id; an
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
        self.ep.connect_with(self.now(), inv.peer, &targets).map_err(Error::Connect)?;
        if !self.ep.is_connected(&inv.peer) {
            self.status.insert(inv.peer, Status::Connecting);
        }
        Ok(inv.peer)
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
    /// socket, fires due timers, sends what is queued and queues the resulting
    /// events. Call it often (every frame, or when [`next_timeout`](Node::next_timeout)
    /// expires or the socket is readable).
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
                Ok((n, from)) => self.ep.handle_datagram(now, from, &self.buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                // ICMP errors surface here on some platforms (ConnectionReset): keep reading.
                Err(_) => continue,
            }
        }
        if self.ep.poll_timeout().is_some_and(|t| t <= now) {
            self.ep.handle_timeout(now);
        }
        self.flush(now);
        while let Some(e) = self.ep.poll_event() {
            self.track(&e);
            self.events.push_back(e);
        }
        drained
    }

    /// The next event, if any is queued.
    pub fn try_recv_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// How long until a timer needs [`poll`](Node::poll), if one is armed. A host that
    /// sleeps should wake at this or when the socket is readable, whichever is first.
    pub fn next_timeout(&self) -> Option<Duration> {
        let now = self.at(Instant::now());
        self.ep.poll_timeout().map(|t| t.saturating_duration_since(now))
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
        while let Some(t) = self.ep.poll_transmit(now) {
            if !self.try_send(&t) {
                if self.pending.len() >= MAX_PENDING_SENDS {
                    self.pending.pop_front();
                }
                self.pending.push_back(t);
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
            Event::AddrObserved { addr, .. } => {
                if !addr.ip().is_loopback() && !self.observed.contains(addr) {
                    if self.observed.len() >= MAX_OBSERVED {
                        self.observed.remove(0);
                    }
                    self.observed.push(*addr);
                }
            }
            Event::Message { .. } | Event::PathChanged { .. } => {}
        }
    }
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

    #[test]
    fn invite_round_trips_and_lists_loopback() {
        let n = Node::bind("0.0.0.0:0").unwrap();
        let inv: Invite<SocketAddr> = Invite::parse(&n.invite()).unwrap();
        assert_eq!(inv.peer, n.peer_id());
        let port = n.local_addr().port();
        assert!(inv.addrs.contains(&SocketAddr::from(([127, 0, 0, 1], port))));
        let bound = Node::bind("127.0.0.1:0").unwrap();
        assert_eq!(bound.addresses(), vec![bound.local_addr()]);
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
