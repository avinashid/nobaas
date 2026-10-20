use crate::addr::{Address, Path};
use crate::crypto::{self, ReplayWindow};
use crate::identity::{Identity, PeerId};
use crate::reliable::{PushError, RelRecv, RelSend};
use crate::time::Instant;
use crate::wire::{
    self, Frame, Packet, FWD_OVERHEAD, MAX_DATAGRAM, MAX_PLAINTEXT, MAX_RELAYED_PACKET, MAX_UNRELIABLE,
    RELAYED_OVERHEAD, RELAYED_PLAINTEXT, UNREL_OVERHEAD,
};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use snow::{HandshakeState, StatelessTransportState};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

/// Most port guesses kept per link (see [`Endpoint::set_guesses`]).
pub const MAX_GUESSES: usize = 64;
/// While relayed, guesses are probed every this many punch rounds: there are many.
const GUESS_ROUNDS: u32 = 5;
/// Handshake sends, or punch rounds, that probe a new set of guesses. A punch to a
/// predicted port lands within the first second or never; later rounds would only
/// spray the peer's IP.
const GUESS_PROBES: u32 = 3;

#[derive(Clone, Debug)]
pub struct Config {
    /// Send a PING if nothing was sent for this long (also keeps NAT mappings open).
    pub keepalive: Duration,
    /// Drop a link if nothing was received for this long.
    pub idle_timeout: Duration,
    /// Once nothing was received for `keepalive`, ping this often (each PING is
    /// answered) until the peer is heard again or `idle_timeout` passes, so a
    /// lossy link isn't declared dead after a few lost keepalives.
    pub probe_interval: Duration,
    /// First handshake retry interval; doubles on each attempt.
    pub handshake_retry: Duration,
    pub handshake_attempts: u8,
    /// How long a responder keeps a half-open handshake waiting for the first data packet.
    pub handshake_timeout: Duration,
    /// Cap on half-open responder handshakes (flood protection).
    pub max_pending_handshakes: usize,
    pub max_message: usize,
    /// Per-link cap on reliable bytes queued or unacknowledged.
    pub max_send_buffer: usize,
    /// Reliable fragments in flight per link.
    pub reliable_window: usize,
    /// Unreliable messages queued per link; the oldest is dropped when full.
    pub unreliable_queue: usize,
    /// Forward packets between connected peers that ask us to (FWD frames).
    pub relay: bool,
    /// Bytes per second each peer may have us relay, with a burst of `relay_burst`.
    pub relay_rate: u64,
    pub relay_burst: u64,
    /// Relayed packets queued per outgoing link; the oldest is dropped when full.
    pub relay_queue: usize,
    /// While a link is relayed and has direct candidates, probe them this often...
    pub punch_interval: Duration,
    /// ...for this long, then give up and stay relayed.
    pub punch_duration: Duration,
    /// Also probe this many ports above each candidate, for NATs that allocate
    /// ports in sequence. 0 disables.
    pub punch_spray: u16,
    pub max_candidates: usize,
    /// A direct path that has been silent this long falls back to the relay, if any.
    pub direct_timeout: Duration,
    /// Tell peers the addresses we see (OBSERVED frames, relay origin tags).
    /// Turn off when addresses are local handles rather than network addresses.
    pub share_addresses: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            keepalive: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            probe_interval: Duration::from_secs(1),
            handshake_retry: Duration::from_millis(500),
            handshake_attempts: 6,
            handshake_timeout: Duration::from_secs(10),
            max_pending_handshakes: 4096,
            max_message: 1 << 20,
            max_send_buffer: 4 << 20,
            reliable_window: 1024,
            unreliable_queue: 256,
            relay: true,
            relay_rate: 256 << 10,
            relay_burst: 64 << 10,
            relay_queue: 256,
            punch_interval: Duration::from_millis(100),
            punch_duration: Duration::from_secs(5),
            punch_spray: 0,
            max_candidates: 8,
            direct_timeout: Duration::from_secs(15),
            share_addresses: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Delivered exactly once, in order. Messages up to `Config::max_message`.
    Reliable,
    /// Delivered at most once, possibly out of order. Messages up to [`MAX_UNRELIABLE`].
    Unreliable,
    /// Reliable and ordered together with `Reliable`, but each fragment is sent
    /// twice a few ms apart, so a single loss costs nothing instead of a
    /// retransmission round trip. For small, latency-critical events; arrives
    /// as `Lane::Reliable`.
    Urgent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    Timeout,
    ClosedByPeer,
    /// The peer completed a new handshake (it restarted or re-dialled); a
    /// `Connected` event for the new link follows. Reliable data in flight on
    /// the old link is lost.
    Replaced,
    ProtocolViolation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<A> {
    Connected {
        peer: PeerId,
        path: Path<A>,
        initiator: bool,
    },
    Message {
        peer: PeerId,
        lane: Lane,
        data: Vec<u8>,
    },
    Disconnected {
        peer: PeerId,
        reason: DisconnectReason,
    },
    ConnectFailed {
        peer: PeerId,
    },
    /// A link moved: relayed to direct after a hole punch, direct to relayed
    /// after the direct path went silent, or to a new address (roaming).
    PathChanged {
        peer: PeerId,
        path: Path<A>,
    },
    /// A directly connected peer reports the address it sees us at (our
    /// public, server-reflexive address when we are behind a NAT).
    AddrObserved {
        by: PeerId,
        addr: A,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transmit<A> {
    pub dst: A,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectError {
    SelfConnect,
    InvalidPeerId,
    NoPath,
    /// The node is relay-only and no relay is connected to dial through.
    NoRelay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    NotConnected,
    TooLarge,
    /// The reliable send buffer is full; retry after the peer acknowledges.
    WouldBlock,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConnectError::SelfConnect => "cannot connect to self",
            ConnectError::InvalidPeerId => "invalid peer id",
            ConnectError::NoPath => "no address or relay to dial",
            ConnectError::NoRelay => "relay-only, and no relay is connected",
        })
    }
}

impl std::error::Error for ConnectError {}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SendError::NotConnected => "peer not connected",
            SendError::TooLarge => "message too large for the lane",
            SendError::WouldBlock => "reliable send buffer full",
        })
    }
}

impl std::error::Error for SendError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub rx_malformed: u64,
    pub rx_unknown_session: u64,
    pub rx_auth_failed: u64,
    pub rx_replayed: u64,
    pub rx_handshake_dropped: u64,
    pub retransmits: u64,
    pub unreliable_dropped: u64,
    pub roams: u64,
    /// Packets we forwarded for other peers.
    pub relayed: u64,
    /// Relay requests refused (no such peer, rate limit, queue full, relaying off).
    pub relay_dropped: u64,
    pub punches_sent: u64,
    /// Links moved from relayed to direct.
    pub punch_succeeded: u64,
    /// Punch rounds that ran out without a direct path.
    pub punch_failed: u64,
    pub direct_fallbacks: u64,
}

/// Per-link counters for one connected peer; see [`Endpoint::peer_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerStats {
    /// Smoothed round-trip time, once measured.
    pub rtt: Option<Duration>,
    /// Lowest round-trip time seen (the handshake counts).
    pub min_rtt: Option<Duration>,
    /// Reliable and urgent fragments sent for the first time.
    pub fragments_sent: u64,
    /// Reliable fragments sent again (timeouts, fast retransmits, tail probes).
    pub retransmits: u64,
    /// Congestion window, in fragments (before the `reliable_window` cap).
    pub cwnd: u32,
    /// Reliable fragments sent and not yet acknowledged.
    pub inflight: u32,
    /// Window reductions caused by queueing delay rather than loss.
    pub delay_backoffs: u64,
}

impl PeerStats {
    /// Estimated packet loss in `[0, 1]`: the share of reliable transmissions
    /// that were repeats. Only the reliable lanes are acknowledged, so a link
    /// carrying only unreliable traffic reports 0. Spurious retransmits make
    /// this an upper bound.
    pub fn loss(&self) -> f32 {
        let total = self.fragments_sent + self.retransmits;
        if total == 0 {
            0.0
        } else {
            self.retransmits as f32 / total as f32
        }
    }
}

struct Dial<A> {
    hs: Box<HandshakeState>,
    seed: [u8; 32],
    hs1: Vec<u8>,
    /// Every path HS1 goes to; the first HS2 back decides the link's path.
    targets: Vec<Path<A>>,
    attempts: u8,
    sent_at: Instant,
    resend_at: Instant,
}

struct Punch {
    token: u64,
    next_at: Instant,
    until: Instant,
    round: u32,
}

struct Link {
    initiator: bool,
    /// `Connected` has been emitted. A responder link stays unannounced until the
    /// first authenticated data packet, so a replayed HS1 can't displace a live link.
    announced: bool,
    /// The initiator has received data from the responder.
    confirmed: bool,
    remote_idx: u32,
    transport: StatelessTransportState,
    /// Responder: cached HS2, resent if the HS1 is retransmitted.
    hs2: Option<Vec<u8>>,
    /// Responder: HS2 went out more than once, so created→first data is no RTT sample.
    hs2_resent: bool,
    created: Instant,
    send_nonce: u64,
    replay: ReplayWindow,
    last_recv: Instant,
    last_send: Instant,
    confirm_at: Instant,
    /// Next liveness probe while the peer is silent.
    probe_at: Instant,
    ping_pending: bool,
    tx: RelSend,
    rx: RelRecv,
    unrel: VecDeque<Vec<u8>>,
    /// Encoded FWD/RELAYED frames carried for other links.
    relay_q: VecDeque<Vec<u8>>,
    relay_tokens: u64,
    relay_refill: Instant,
    dirty: bool,
}

impl Link {
    fn new(initiator: bool, remote_idx: u32, transport: StatelessTransportState, now: Instant, burst: u64) -> Self {
        Link {
            initiator,
            announced: false,
            confirmed: !initiator,
            remote_idx,
            transport,
            hs2: None,
            hs2_resent: false,
            created: now,
            send_nonce: 0,
            replay: ReplayWindow::new(),
            last_recv: now,
            last_send: now,
            confirm_at: now,
            probe_at: now,
            ping_pending: false,
            tx: RelSend::new(),
            rx: RelRecv::new(),
            unrel: VecDeque::new(),
            relay_q: VecDeque::new(),
            relay_tokens: burst,
            relay_refill: now,
            dirty: false,
        }
    }

    fn has_output(&self, now: Instant, window: usize) -> bool {
        self.ping_pending
            || self.rx.ack_pending
            || !self.unrel.is_empty()
            || !self.relay_q.is_empty()
            || self.tx.has_sendable(now, window)
    }
}

enum State<A> {
    Dialing(Dial<A>),
    Live(Box<Link>),
}

struct Session<A> {
    path: Path<A>,
    peer: PeerId,
    state: State<A>,
    /// Peer to route through when no direct path works.
    relay: Option<PeerId>,
    /// Direct addresses to try while relayed.
    candidates: Vec<A>,
    /// Addresses only probed (port predictions), never taken as the path unless answered from.
    guesses: Vec<A>,
    /// Probes of `guesses` left: handshake sends while dialling, punch rounds while relayed.
    guess_probes: u32,
    punch: Option<Punch>,
    last_direct: Instant,
    /// The address this peer last said it sees us at.
    seen_as: Option<A>,
}

/// A sans-IO P2P endpoint: one identity, any number of encrypted links.
pub struct Endpoint<A: Address> {
    me: PeerId,
    noise_private: [u8; 32],
    config: Config,
    rng: ChaCha20Rng,
    sessions: BTreeMap<u32, Session<A>>,
    /// Announced links by peer.
    peers: BTreeMap<PeerId, u32>,
    /// Outstanding dials by peer.
    dialing: BTreeMap<PeerId, u32>,
    /// (initiator index, initiator ephemeral key) of half-open responder links,
    /// for HS1 retransmits, which may arrive over several paths.
    half_open: HashMap<(u32, [u8; 32]), u32>,
    outbox: VecDeque<Transmit<A>>,
    ready: VecDeque<u32>,
    events: VecDeque<Event<A>>,
    stats: Stats,
    relay_only: bool,
}

impl<A: Address> Endpoint<A> {
    /// `seed` drives index allocation and Noise ephemeral keys. Production hosts
    /// must pass 32 bytes from the OS CSPRNG; the simulator passes a fixed seed.
    pub fn new(identity: &Identity, config: Config, seed: [u8; 32]) -> Self {
        Endpoint {
            me: identity.peer_id(),
            noise_private: identity.noise_private(),
            config,
            rng: ChaCha20Rng::from_seed(seed),
            sessions: BTreeMap::new(),
            peers: BTreeMap::new(),
            dialing: BTreeMap::new(),
            half_open: HashMap::new(),
            outbox: VecDeque::new(),
            ready: VecDeque::new(),
            events: VecDeque::new(),
            stats: Stats::default(),
            relay_only: false,
        }
    }

    /// Keeps relayed links relayed, so peers reached through a relay never learn
    /// this endpoint's address from it: they are asked to forward without the
    /// origin tag (FWD_QUIET), dials that name a relay skip their direct
    /// targets, and relayed links take no direct candidates, never
    /// punch, never answer a punch and never move to a direct path. This endpoint
    /// also stops sharing the addresses it sees (OBSERVED frames, origin tags), as
    /// with `share_addresses` off, and answers a handshake at its own address
    /// only from a peer it is dialling directly or already has a direct link
    /// with. Direct links, such as the one to the relay, are unaffected; links
    /// already up keep their path.
    pub fn set_relay_only(&mut self, on: bool) {
        self.relay_only = on;
        if on {
            for sess in self.sessions.values_mut().filter(|s| matches!(s.path, Path::Via(_))) {
                sess.punch = None;
                sess.candidates.clear();
            }
        }
    }

    /// Whether [`set_relay_only`](Endpoint::set_relay_only) is on.
    pub fn relay_only(&self) -> bool {
        self.relay_only
    }

    fn shares_addresses(&self) -> bool {
        self.config.share_addresses && !self.relay_only
    }

    /// Whether a handshake from `peer` on `from` may be answered there: always,
    /// unless relay-only, which answers at its own address only peers it dials
    /// directly itself or already has a direct link with (its relay).
    fn answers_directly(&self, from: &Path<A>, peer: &PeerId) -> bool {
        if !self.relay_only || matches!(from, Path::Via(_)) {
            return true;
        }
        let direct_dial = self.dialing.get(peer).and_then(|i| self.sessions.get(i)).is_some_and(|s| match &s.state {
            State::Dialing(d) => d.targets.iter().any(|t| matches!(t, Path::Direct(_))),
            State::Live(_) => false,
        });
        direct_dial || self.path(peer).is_some_and(|p| matches!(p, Path::Direct(_)))
    }

    pub fn peer_id(&self) -> PeerId {
        self.me
    }

    pub fn stats(&self) -> Stats {
        let mut s = self.stats.clone();
        for sess in self.sessions.values() {
            if let State::Live(l) = &sess.state {
                s.retransmits += l.tx.retransmits;
            }
        }
        s
    }

    /// Number of sessions in any state (dialing, half-open, live).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn connected_peers(&self) -> impl Iterator<Item = &PeerId> {
        self.peers.keys()
    }

    pub fn is_connected(&self, peer: &PeerId) -> bool {
        self.peers.contains_key(peer)
    }

    /// The path a connected peer's link currently uses.
    pub fn path(&self, peer: &PeerId) -> Option<&Path<A>> {
        Some(&self.sessions.get(self.peers.get(peer)?)?.path)
    }

    /// Lowest round-trip time seen to a connected peer (the handshake counts):
    /// the path's delay without queueing. Compare with `rtt` or one-way delays
    /// to detect a backed-up uplink.
    pub fn min_rtt(&self, peer: &PeerId) -> Option<Duration> {
        match &self.sessions.get(self.peers.get(peer)?)?.state {
            State::Live(l) => l.tx.path_min_rtt(),
            State::Dialing(_) => None,
        }
    }

    /// Smoothed round-trip time to a connected peer, once measured.
    pub fn rtt(&self, peer: &PeerId) -> Option<Duration> {
        match &self.sessions.get(self.peers.get(peer)?)?.state {
            State::Live(l) => l.tx.srtt(),
            State::Dialing(_) => None,
        }
    }

    /// RTT and loss counters for a connected peer.
    pub fn peer_stats(&self, peer: &PeerId) -> Option<PeerStats> {
        match &self.sessions.get(self.peers.get(peer)?)?.state {
            State::Live(l) => Some(PeerStats {
                rtt: l.tx.srtt(),
                min_rtt: l.tx.path_min_rtt(),
                fragments_sent: l.tx.fragments_sent,
                retransmits: l.tx.retransmits,
                cwnd: l.tx.cwnd(),
                inflight: l.tx.inflight(),
                delay_backoffs: l.tx.delay_backoffs,
            }),
            State::Dialing(_) => None,
        }
    }

    fn seed(&mut self) -> [u8; 32] {
        let mut s = [0u8; 32];
        self.rng.fill_bytes(&mut s);
        s
    }

    fn alloc_index(&mut self) -> u32 {
        loop {
            let i = self.rng.next_u32();
            if i != 0 && !self.sessions.contains_key(&i) {
                return i;
            }
        }
    }

    /// Starts a handshake with `peer` at `addr`. A no-op if already connected or dialling.
    pub fn connect(&mut self, now: Instant, addr: A, peer: PeerId) -> Result<(), ConnectError> {
        self.connect_with(now, peer, &[Path::Direct(addr)])
    }

    /// Dials `peer` through `relay` (a connected peer that also has a link to
    /// `peer`), and directly at `candidates` if any. Once the link is up, both
    /// sides punch towards each other's addresses and move the link off the
    /// relay if a direct path works in both directions.
    pub fn connect_via(
        &mut self,
        now: Instant,
        relay: PeerId,
        peer: PeerId,
        candidates: &[A],
    ) -> Result<(), ConnectError> {
        let mut targets = vec![Path::Via(relay)];
        targets.extend(candidates.iter().cloned().map(Path::Direct));
        self.connect_with(now, peer, &targets)
    }

    /// Sends the handshake along every path in `targets` at once (all of a
    /// peer's known addresses, a relay, or both). If both peers do this at the
    /// same time towards each other's public addresses, the handshakes punch
    /// through cone NATs. Calling it again while dialling adds paths.
    pub fn connect_with(&mut self, now: Instant, peer: PeerId, targets: &[Path<A>]) -> Result<(), ConnectError> {
        if peer == self.me {
            return Err(ConnectError::SelfConnect);
        }
        peer.noise_static().ok_or(ConnectError::InvalidPeerId)?;
        if targets.is_empty() {
            return Err(ConnectError::NoPath);
        }
        // Relay-only: a dial that names a relay goes through it alone.
        let relayed: Vec<Path<A>>;
        let targets = if self.relay_only && targets.iter().any(|t| matches!(t, Path::Via(_))) {
            relayed = targets.iter().filter(|t| matches!(t, Path::Via(_))).cloned().collect();
            &relayed[..]
        } else {
            targets
        };
        if self.peers.contains_key(&peer) {
            return Ok(());
        }
        if let Some(&idx) = self.dialing.get(&peer) {
            let sess = self.sessions.get_mut(&idx).expect("dial session");
            let State::Dialing(d) = &mut sess.state else { return Ok(()) };
            let mut new = Vec::new();
            for t in targets {
                if !d.targets.contains(t) && d.targets.len() < 16 {
                    d.targets.push(t.clone());
                    new.push((t.clone(), d.hs1.clone()));
                }
                if let (Path::Via(r), None) = (t, sess.relay) {
                    sess.relay = Some(*r);
                }
                if let Path::Direct(a) = t {
                    if !sess.candidates.contains(a) && sess.candidates.len() < self.config.max_candidates {
                        sess.candidates.push(a.clone());
                    }
                }
            }
            for (path, hs1) in new {
                self.emit(path, peer, hs1);
            }
            return Ok(());
        }
        let idx = self.alloc_index();
        let seed = self.seed();
        let (hs, msg1) =
            initiator_hs(seed, &self.noise_private, &self.me, &peer).map_err(|_| ConnectError::InvalidPeerId)?;
        let hs1 = wire::encode_hs1(idx, &msg1);
        let targets: Vec<Path<A>> = targets.iter().take(16).cloned().collect();
        let relay = targets.iter().find_map(|t| match t {
            Path::Via(r) => Some(*r),
            Path::Direct(_) => None,
        });
        let mut candidates: Vec<A> = targets.iter().filter_map(|t| t.direct().cloned()).collect();
        candidates.truncate(self.config.max_candidates);
        for t in &targets {
            self.emit(t.clone(), peer, hs1.clone());
        }
        let dial = Dial {
            hs: Box::new(hs),
            seed,
            hs1,
            targets: targets.clone(),
            attempts: 1,
            sent_at: now,
            resend_at: now + self.config.handshake_retry,
        };
        self.sessions.insert(
            idx,
            Session {
                path: targets[0].clone(),
                peer,
                state: State::Dialing(dial),
                relay,
                candidates,
                guesses: Vec::new(),
                guess_probes: 0,
                punch: None,
                last_direct: now,
                seen_as: None,
            },
        );
        self.dialing.insert(peer, idx);
        Ok(())
    }

    /// Adds direct addresses (learned through signalling) for a dialling or
    /// relayed peer, and starts punching towards them.
    pub fn add_candidates(&mut self, now: Instant, peer: &PeerId, addrs: &[A]) {
        if let Some(&idx) = self.peers.get(peer) {
            for a in addrs {
                self.add_candidate(now, idx, a.clone());
            }
        } else if self.dialing.contains_key(peer) {
            let targets: Vec<_> = addrs.iter().cloned().map(Path::Direct).collect();
            let _ = self.connect_with(now, *peer, &targets);
        }
    }

    fn add_candidate(&mut self, now: Instant, idx: u32, a: A) {
        let (max, relay_only) = (self.config.max_candidates, self.relay_only);
        let Some(sess) = self.sessions.get_mut(&idx) else { return };
        if sess.path.direct() == Some(&a) || (relay_only && matches!(sess.path, Path::Via(_))) {
            return;
        }
        if !sess.candidates.contains(&a) {
            if sess.candidates.len() >= max {
                return;
            }
            sess.candidates.push(a);
        }
        self.start_punch(now, idx);
    }

    fn start_punch(&mut self, now: Instant, idx: u32) {
        if self.relay_only {
            return;
        }
        let until = now + self.config.punch_duration;
        let token = self.rng.next_u64();
        let Some(sess) = self.sessions.get_mut(&idx) else { return };
        let nothing = sess.candidates.is_empty() && sess.guesses.is_empty();
        if !matches!(sess.path, Path::Via(_)) || nothing || !matches!(sess.state, State::Live(_)) {
            return;
        }
        match &mut sess.punch {
            Some(p) => p.until = until,
            None => sess.punch = Some(Punch { token, next_at: now, until, round: 0 }),
        }
    }

    /// Aims handshakes and punches for `peer` at `addrs` too: guesses at the
    /// address a symmetric NAT in front of the peer will pick for its packets to
    /// us (port prediction). Unlike candidates, guesses are only probed, three
    /// times: on the first handshake sends while dialling, or every few punch
    /// rounds while relayed. One becomes the link's path only when the peer
    /// answers from it. Replaces the peer's earlier guesses; keeps at most
    /// [`MAX_GUESSES`]. The same guesses again change nothing, and new ones are
    /// sent to at once without repeating the old. Ignored for a peer a relay-only
    /// endpoint reaches through a relay, or linked directly.
    pub fn set_guesses(&mut self, now: Instant, peer: &PeerId, addrs: &[A]) {
        let Some(&idx) = self.peers.get(peer).or_else(|| self.dialing.get(peer)) else { return };
        let relay_only = self.relay_only;
        let Some(sess) = self.sessions.get_mut(&idx) else { return };
        let relayed = match &sess.state {
            State::Dialing(d) => d.targets.iter().any(|t| matches!(t, Path::Via(_))),
            State::Live(_) => matches!(sess.path, Path::Via(_)),
        };
        let direct = matches!(sess.state, State::Live(_)) && !relayed;
        if (relay_only && relayed) || direct {
            return;
        }
        let known = |a: &A| sess.candidates.contains(a) || sess.path.direct() == Some(a);
        let guesses: Vec<A> = addrs.iter().filter(|a| !known(a)).take(MAX_GUESSES).cloned().collect();
        let fresh: Vec<A> = guesses.iter().filter(|g| !sess.guesses.contains(g)).cloned().collect();
        sess.guesses = guesses;
        if fresh.is_empty() {
            return;
        }
        sess.guess_probes = GUESS_PROBES;
        if let State::Dialing(d) = &sess.state {
            // The send now is the first probe; retries carry the rest.
            sess.guess_probes -= 1;
            let hs1 = d.hs1.clone();
            for g in fresh {
                self.emit(Path::Direct(g), *peer, hs1.clone());
            }
        } else {
            self.start_punch(now, idx);
        }
    }

    /// Closes the link to `peer`, telling it so. No event is emitted locally.
    pub fn disconnect(&mut self, peer: &PeerId) {
        let Some(idx) = self.peers.remove(peer) else { return };
        if let Some(mut sess) = self.sessions.remove(&idx) {
            if let State::Live(link) = &mut sess.state {
                let mut pt = Vec::with_capacity(1);
                wire::put_close(&mut pt);
                if let Some(p) = seal(link, &pt) {
                    self.emit(sess.path, sess.peer, p);
                }
            }
        }
    }

    pub fn send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError> {
        let idx = *self.peers.get(peer).ok_or(SendError::NotConnected)?;
        let Some(Session { state: State::Live(link), .. }) = self.sessions.get_mut(&idx) else {
            return Err(SendError::NotConnected);
        };
        match lane {
            Lane::Reliable | Lane::Urgent => link
                .tx
                .push_message(data, self.config.max_send_buffer, self.config.max_message, lane == Lane::Urgent)
                .map_err(|e| match e {
                    PushError::BufferFull => SendError::WouldBlock,
                    PushError::TooLarge => SendError::TooLarge,
                })?,
            Lane::Unreliable => {
                if data.len() > MAX_UNRELIABLE {
                    return Err(SendError::TooLarge);
                }
                if link.unrel.len() >= self.config.unreliable_queue {
                    link.unrel.pop_front();
                    self.stats.unreliable_dropped += 1;
                }
                link.unrel.push_back(data.to_vec());
            }
        }
        mark_ready(&mut self.ready, idx, link);
        Ok(())
    }

    /// Sends `payload` for `peer` along `path`: straight to the outbox, or
    /// wrapped in a FWD frame on the relay's link.
    fn emit(&mut self, path: Path<A>, peer: PeerId, payload: Vec<u8>) {
        match path {
            Path::Direct(dst) => {
                self.stats.tx_packets += 1;
                self.outbox.push_back(Transmit { dst, payload });
            }
            Path::Via(relay) => self.relay_out(relay, &peer, payload),
        }
    }

    pub fn handle_datagram(&mut self, now: Instant, from: A, data: &[u8]) {
        self.on_packet(now, Path::Direct(from), data);
    }

    /// Returns the session the packet belonged to.
    fn on_packet(&mut self, now: Instant, from: Path<A>, data: &[u8]) -> Option<u32> {
        self.stats.rx_packets += 1;
        match wire::parse(data) {
            None => {
                self.stats.rx_malformed += 1;
                None
            }
            Some(Packet::Hs1 { sender, noise }) => self.on_hs1(now, from, sender, noise),
            Some(Packet::Hs2 { sender, receiver, noise }) => self.on_hs2(now, from, sender, receiver, noise),
            Some(Packet::Data { receiver, nonce, ciphertext }) => self.on_data(now, from, receiver, nonce, ciphertext),
        }
    }

    fn on_hs1(&mut self, now: Instant, from: Path<A>, sender: u32, noise: &[u8]) -> Option<u32> {
        if noise.len() < 32 {
            self.stats.rx_malformed += 1;
            return None;
        }
        let mut eph = [0u8; 32];
        eph.copy_from_slice(&noise[..32]);
        if let Some(&idx) = self.half_open.get(&(sender, eph)) {
            let quiet = self.sessions.get(&idx).is_some_and(|s| !self.answers_directly(&from, &s.peer));
            if let (Some(Session { state: State::Live(l), peer, .. }), false) = (self.sessions.get_mut(&idx), quiet) {
                if let Some(hs2) = l.hs2.clone() {
                    l.hs2_resent = true;
                    let peer = *peer;
                    self.emit(from, peer, hs2);
                }
            }
            return Some(idx);
        }
        if self.half_open.len() >= self.config.max_pending_handshakes {
            self.stats.rx_handshake_dropped += 1;
            return None;
        }
        let seed = self.seed();
        let mut hs = crypto::builder(seed, &self.noise_private).and_then(|b| b.build_responder()).ok()?;
        let mut payload = [0u8; 256];
        let n = match hs.read_message(noise, &mut payload) {
            Ok(n) => n,
            Err(_) => {
                self.stats.rx_auth_failed += 1;
                return None;
            }
        };
        if n != 32 {
            self.stats.rx_malformed += 1;
            return None;
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&payload[..32]);
        let peer = PeerId(id);
        // The claimed Ed25519 identity must be the one whose X25519 form ran the handshake.
        if peer.noise_static().as_ref().map(|k| &k[..]) != hs.get_remote_static() || peer == self.me {
            self.stats.rx_auth_failed += 1;
            return None;
        }
        if !self.answers_directly(&from, &peer) {
            self.stats.rx_handshake_dropped += 1;
            return None;
        }
        let mut inherited = (None, Vec::new());
        if let Some(&didx) = self.dialing.get(&peer) {
            // Simultaneous open: the dial from the smaller PeerId wins.
            if self.me < peer {
                let mut resend = None;
                if let Some(sess) = self.sessions.get_mut(&didx) {
                    if let State::Dialing(d) = &mut sess.state {
                        if !d.targets.contains(&from) && d.targets.len() < 16 {
                            // Their packet proves this path works; aim our dial along it too.
                            d.targets.push(from.clone());
                            resend = Some(d.hs1.clone());
                        }
                    }
                }
                if let Some(hs1) = resend {
                    self.emit(from, peer, hs1);
                }
                return Some(didx);
            }
            if let Some(s) = self.sessions.remove(&didx) {
                inherited = (s.relay, s.candidates);
            }
            self.dialing.remove(&peer);
        }
        let idx = self.alloc_index();
        let mut out = [0u8; 256];
        let m = hs.write_message(&[], &mut out).ok()?;
        let transport = hs.into_stateless_transport_mode().ok()?;
        let hs2 = wire::encode_hs2(idx, sender, &out[..m]);
        self.emit(from.clone(), peer, hs2.clone());
        let mut link = Link::new(false, sender, transport, now, self.config.relay_burst);
        link.hs2 = Some(hs2);
        self.half_open.insert((sender, eph), idx);
        let relay = match &from {
            Path::Via(r) => Some(*r),
            Path::Direct(_) => inherited.0,
        };
        self.sessions.insert(
            idx,
            Session {
                path: from,
                peer,
                state: State::Live(Box::new(link)),
                relay,
                candidates: inherited.1,
                guesses: Vec::new(),
                guess_probes: 0,
                punch: None,
                last_direct: now,
                seen_as: None,
            },
        );
        self.start_punch(now, idx);
        Some(idx)
    }

    fn on_hs2(&mut self, now: Instant, from: Path<A>, sender: u32, receiver: u32, noise: &[u8]) -> Option<u32> {
        let Some(sess) = self.sessions.get_mut(&receiver) else {
            self.stats.rx_unknown_session += 1;
            return None;
        };
        let State::Dialing(dial) = &mut sess.state else {
            return Some(receiver); // duplicate HS2 for a link that is already up
        };
        let mut payload = [0u8; 64];
        if dial.hs.read_message(noise, &mut payload).is_err() || !dial.hs.is_handshake_finished() {
            self.stats.rx_auth_failed += 1;
            // A forged HS2 may have advanced the handshake state; rebuild it from
            // the seed, which reproduces the same ephemeral key and HS1.
            if let Ok((hs, _)) = initiator_hs(dial.seed, &self.noise_private, &self.me, &sess.peer) {
                *dial.hs = hs;
            }
            return None;
        }
        let mut sess = self.sessions.remove(&receiver).expect("present above");
        let State::Dialing(dial) = sess.state else { unreachable!() };
        self.dialing.remove(&sess.peer);
        let transport = dial.hs.into_stateless_transport_mode().ok()?;
        let mut link = Link::new(true, sender, transport, now, self.config.relay_burst);
        if dial.attempts == 1 {
            link.tx.seed_rtt(now - dial.sent_at);
        }
        link.ping_pending = true; // lets the responder confirm and announce
        link.confirm_at = now + self.config.handshake_retry;
        mark_ready(&mut self.ready, receiver, &mut link);
        sess.state = State::Live(Box::new(link));
        match &from {
            Path::Via(r) => {
                sess.relay.get_or_insert(*r);
            }
            Path::Direct(_) => sess.guesses.clear(),
        }
        sess.path = from;
        self.sessions.insert(receiver, sess);
        self.announce(receiver);
        self.start_punch(now, receiver);
        Some(receiver)
    }

    fn on_data(&mut self, now: Instant, from: Path<A>, receiver: u32, nonce: u64, ciphertext: &[u8]) -> Option<u32> {
        let Some(sess) = self.sessions.get_mut(&receiver) else {
            self.stats.rx_unknown_session += 1;
            return None;
        };
        let State::Live(link) = &mut sess.state else {
            self.stats.rx_unknown_session += 1;
            return None;
        };
        if !link.replay.check(nonce) {
            self.stats.rx_replayed += 1;
            return None;
        }
        let mut pt = [0u8; MAX_DATAGRAM];
        let Ok(n) = link.transport.read_message(nonce, ciphertext, &mut pt) else {
            self.stats.rx_auth_failed += 1;
            return None;
        };
        link.replay.commit(nonce);
        let mut path_changed = false;
        let mut reflexive = None;
        match (&from, &sess.path) {
            (Path::Direct(x), Path::Direct(y)) => {
                sess.last_direct = now;
                if x != y {
                    sess.path = from.clone();
                    self.stats.roams += 1;
                    path_changed = true;
                }
            }
            // Direct traffic on a relayed link only proves one direction; it
            // makes the address a candidate, and PUNCH_ACK decides.
            (Path::Direct(x), Path::Via(_)) => reflexive = Some(x.clone()),
            (Path::Via(r), Path::Via(q)) if r != q => {
                sess.path = from.clone();
                path_changed = true;
            }
            // Relayed stragglers after an upgrade don't move a direct link back.
            _ => {}
        }
        link.last_recv = now;
        link.confirmed = true;

        let direct_from = from.direct().cloned();
        let mut reliable = Vec::new();
        let mut unreliable = Vec::new();
        let mut forwards = Vec::new();
        let mut relayed = Vec::new();
        let mut punch_acks = Vec::new();
        let mut upgraded = None;
        let mut observed = None;
        let mut violation = false;
        let mut closed = false;
        for f in wire::frames(&pt[..n]) {
            match f {
                // Answered with an ACK, so a silent-side probe gets a reply.
                Ok(Frame::Ping) => link.rx.ack_pending = true,
                Ok(Frame::Ack { cum, bits }) => link.tx.on_ack(now, cum, bits),
                Ok(Frame::Reliable { seq, fin, data }) => {
                    if link.rx.on_fragment(seq, fin, data, self.config.max_message, &mut reliable).is_err() {
                        violation = true;
                    }
                }
                Ok(Frame::Unreliable(d)) => unreliable.push(d.to_vec()),
                Ok(Frame::Close) => closed = true,
                Ok(Frame::Observed(a)) => {
                    if direct_from.is_some() {
                        observed = A::decode(a);
                    }
                }
                // Relaying happens only between directly connected peers.
                Ok(Frame::Fwd { to, packet, tag }) => {
                    if let Some(a) = &direct_from {
                        forwards.push((to, packet.to_vec(), tag.then(|| a.clone())));
                    }
                }
                Ok(Frame::Relayed { addr, packet }) => {
                    if direct_from.is_some() {
                        relayed.push((packet.to_vec(), A::decode(addr)));
                    }
                }
                // A relay-only endpoint never answers at its own address on a relayed link.
                Ok(Frame::Punch(t)) => {
                    if let (Some(a), false) = (&direct_from, self.relay_only && matches!(sess.path, Path::Via(_))) {
                        punch_acks.push((a.clone(), t));
                    }
                }
                Ok(Frame::PunchAck(t)) => {
                    if let (Some(a), Some(p)) = (&direct_from, &sess.punch) {
                        if p.token == t {
                            upgraded = Some(a.clone());
                        }
                    }
                }
                Err(()) => violation = true,
            }
        }
        for (dst, token) in punch_acks {
            let mut f = Vec::with_capacity(9);
            wire::put_punch(&mut f, true, token);
            if let Some(p) = seal(link, &f) {
                self.stats.tx_packets += 1;
                self.outbox.push_back(Transmit { dst, payload: p });
            }
        }
        if let Some(a) = upgraded {
            sess.path = Path::Direct(a);
            sess.punch = None;
            sess.guesses.clear();
            sess.last_direct = now;
            self.stats.punch_succeeded += 1;
            path_changed = true;
        }
        if violation {
            self.stats.rx_malformed += 1;
        }
        let peer = sess.peer;
        let announce = !link.announced;
        if announce && !link.initiator && !link.hs2_resent {
            link.tx.seed_rtt(now - link.created);
        }
        if let Some(a) = observed {
            if sess.seen_as.as_ref() != Some(&a) {
                sess.seen_as = Some(a.clone());
                self.events.push_back(Event::AddrObserved { by: peer, addr: a });
            }
        }
        let new_path = sess.path.clone();
        mark_ready(&mut self.ready, receiver, link);
        if announce {
            self.announce(receiver);
        } else if path_changed {
            self.events.push_back(Event::PathChanged { peer, path: new_path });
        }
        if let Some(x) = reflexive {
            self.add_candidate(now, receiver, x);
        }
        for (to, packet, origin) in forwards {
            self.forward(now, receiver, &to, &packet, origin.as_ref());
        }
        for (packet, hint) in relayed {
            if let Some(idx) = self.on_packet(now, Path::Via(peer), &packet) {
                if let Some(a) = hint {
                    self.add_candidate(now, idx, a);
                }
            }
        }
        if !self.peers.get(&peer).is_some_and(|&i| i == receiver) {
            return Some(receiver); // lost a race against a newer link to the same peer
        }
        for data in reliable {
            self.events.push_back(Event::Message { peer, lane: Lane::Reliable, data });
        }
        for data in unreliable {
            self.events.push_back(Event::Message { peer, lane: Lane::Unreliable, data });
        }
        if closed || violation {
            let reason = if closed { DisconnectReason::ClosedByPeer } else { DisconnectReason::ProtocolViolation };
            self.drop_link(receiver, Some(reason));
        }
        Some(receiver)
    }

    /// Relays `packet` from the peer on link `src` to peer `to`, tagged with the
    /// address we see the sender at (`origin`: none if it asked us not to).
    fn forward(&mut self, now: Instant, src: u32, to: &PeerId, packet: &[u8], origin: Option<&A>) {
        let ok = self.config.relay
            && self.sessions.get(&src).is_some_and(|s| s.peer != *to)
            && self
                .peers
                .get(to)
                .and_then(|i| self.sessions.get(i))
                .is_some_and(|s| matches!(s.path, Path::Direct(_)) && matches!(s.state, State::Live(_)));
        if !ok || packet.len() > MAX_RELAYED_PACKET {
            self.stats.relay_dropped += 1;
            return;
        }
        // Token bucket on the requesting link.
        let (rate, burst) = (self.config.relay_rate, self.config.relay_burst);
        let Some(Session { state: State::Live(sl), .. }) = self.sessions.get_mut(&src) else { return };
        let elapsed = now.saturating_duration_since(sl.relay_refill).as_micros() as u64;
        sl.relay_tokens = (sl.relay_tokens + elapsed * rate / 1_000_000).min(burst);
        sl.relay_refill = now;
        let cost = packet.len() as u64;
        if sl.relay_tokens < cost {
            self.stats.relay_dropped += 1;
            return;
        }
        sl.relay_tokens -= cost;
        let mut addr = Vec::new();
        if let (Some(o), true) = (origin, self.shares_addresses()) {
            o.encode(&mut addr);
        }
        if addr.len() > crate::addr::MAX_ADDR_LEN {
            addr.clear();
        }
        let mut f = Vec::with_capacity(RELAYED_OVERHEAD + packet.len());
        wire::put_relayed(&mut f, &addr, packet);
        let tidx = self.peers[to];
        let Some(Session { state: State::Live(tl), .. }) = self.sessions.get_mut(&tidx) else { return };
        push_relay_frame(tl, f, self.config.relay_queue, &mut self.stats.relay_dropped);
        self.stats.relayed += 1;
        mark_ready(&mut self.ready, tidx, tl);
    }

    /// Makes session `idx` the link for its peer and emits `Connected`.
    fn announce(&mut self, idx: u32) {
        let Some(sess) = self.sessions.get_mut(&idx) else { return };
        let State::Live(link) = &mut sess.state else { return };
        let peer = sess.peer;
        let path = sess.path.clone();
        let initiator = link.initiator;
        link.announced = true;
        link.hs2 = None;
        if !initiator {
            link.ping_pending = true; // confirms the link to the initiator
            mark_ready(&mut self.ready, idx, link);
        }
        self.half_open.retain(|_, &mut v| v != idx);
        if let Some(old) = self.peers.insert(peer, idx) {
            if old != idx {
                self.sessions.remove(&old);
                self.half_open.retain(|_, &mut v| v != old);
                self.events.push_back(Event::Disconnected { peer, reason: DisconnectReason::Replaced });
            }
        }
        self.events.push_back(Event::Connected { peer, path, initiator });
    }

    fn drop_link(&mut self, idx: u32, reason: Option<DisconnectReason>) {
        let Some(sess) = self.sessions.remove(&idx) else { return };
        self.half_open.retain(|_, &mut v| v != idx);
        if self.peers.get(&sess.peer) == Some(&idx) {
            self.peers.remove(&sess.peer);
            if let Some(reason) = reason {
                self.events.push_back(Event::Disconnected { peer: sess.peer, reason });
            }
        }
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        let mut failed_dials = Vec::new();
        let mut dead = Vec::new();
        let mut expired = Vec::new();
        let mut resend = Vec::new();
        let mut fallbacks = Vec::new();
        let spray = self.config.punch_spray as i32;
        for (&idx, sess) in self.sessions.iter_mut() {
            match &mut sess.state {
                State::Dialing(d) => {
                    if now < d.resend_at {
                        continue;
                    }
                    if d.attempts >= self.config.handshake_attempts {
                        failed_dials.push(idx);
                        continue;
                    }
                    d.attempts += 1;
                    let backoff = self.config.handshake_retry.saturating_mul(1 << (d.attempts - 1).min(4));
                    d.resend_at = now + backoff;
                    let guessed = if sess.guess_probes > 0 { &sess.guesses[..] } else { &[] };
                    sess.guess_probes = sess.guess_probes.saturating_sub(1);
                    for t in d.targets.iter().cloned().chain(guessed.iter().cloned().map(Path::Direct)) {
                        resend.push((t, sess.peer, d.hs1.clone()));
                    }
                }
                State::Live(link) => {
                    if let Some(p) = &mut sess.punch {
                        if now >= p.until {
                            sess.punch = None;
                            self.stats.punch_failed += 1;
                        } else if now >= p.next_at {
                            p.next_at = now + self.config.punch_interval;
                            let mut f = Vec::with_capacity(9);
                            wire::put_punch(&mut f, false, p.token);
                            let probe = p.round % GUESS_ROUNDS == 0 && sess.guess_probes > 0;
                            let guessed = if probe { &sess.guesses[..] } else { &[] };
                            sess.guess_probes -= probe as u32;
                            p.round += 1;
                            let sprayed = sess.candidates.iter().flat_map(|c| {
                                std::iter::once(Some(c.clone())).chain((1..=spray).map(|k| c.port_offset(k))).flatten()
                            });
                            for dst in sprayed.chain(guessed.iter().cloned()) {
                                if let Some(payload) = seal(link, &f) {
                                    self.stats.tx_packets += 1;
                                    self.stats.punches_sent += 1;
                                    self.outbox.push_back(Transmit { dst, payload });
                                }
                            }
                        }
                    }
                    if !link.announced {
                        if now >= link.created + self.config.handshake_timeout {
                            expired.push(idx);
                        }
                        continue;
                    }
                    if now >= link.last_recv + self.config.idle_timeout || link.tx.failed {
                        dead.push(idx);
                        continue;
                    }
                    if let (Path::Direct(_), Some(_)) = (&sess.path, sess.relay) {
                        if now >= sess.last_direct + self.config.direct_timeout {
                            fallbacks.push(idx);
                        }
                    }
                    if !link.confirmed && now >= link.confirm_at {
                        link.ping_pending = true;
                        link.confirm_at = now + self.config.handshake_retry;
                    }
                    if now >= link.last_send + self.config.keepalive {
                        link.ping_pending = true;
                    }
                    if now >= link.last_recv + self.config.keepalive && now >= link.probe_at {
                        link.ping_pending = true;
                        link.probe_at = now + self.config.probe_interval;
                    }
                    if link.has_output(now, self.config.reliable_window) {
                        mark_ready(&mut self.ready, idx, link);
                    }
                }
            }
        }
        for (path, peer, hs1) in resend {
            self.emit(path, peer, hs1);
        }
        for idx in fallbacks {
            let Some(sess) = self.sessions.get_mut(&idx) else { continue };
            let Some(relay) = sess.relay else { continue };
            if !self.peers.contains_key(&relay) {
                sess.last_direct = now; // no relay to fall back to; check again later
                continue;
            }
            if let Path::Direct(old) = std::mem::replace(&mut sess.path, Path::Via(relay)) {
                if !sess.candidates.contains(&old) {
                    sess.candidates.insert(0, old);
                    sess.candidates.truncate(self.config.max_candidates);
                }
            }
            if let State::Live(link) = &mut sess.state {
                link.ping_pending = true;
                mark_ready(&mut self.ready, idx, link);
            }
            let peer = sess.peer;
            self.stats.direct_fallbacks += 1;
            self.events.push_back(Event::PathChanged { peer, path: Path::Via(relay) });
            self.start_punch(now, idx);
        }
        for idx in failed_dials {
            if let Some(sess) = self.sessions.remove(&idx) {
                self.dialing.remove(&sess.peer);
                self.events.push_back(Event::ConnectFailed { peer: sess.peer });
            }
        }
        for idx in expired {
            self.drop_link(idx, None);
        }
        for idx in dead {
            self.drop_link(idx, Some(DisconnectReason::Timeout));
        }
    }

    /// The earliest time `handle_timeout` should be called.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut min = |x: Instant| t = Some(t.map_or(x, |t| t.min(x)));
        for sess in self.sessions.values() {
            if let Some(p) = &sess.punch {
                min(p.next_at.min(p.until));
            }
            match &sess.state {
                State::Dialing(d) => min(d.resend_at),
                State::Live(link) if !link.announced => min(link.created + self.config.handshake_timeout),
                State::Live(link) => {
                    min(link.last_recv + self.config.idle_timeout);
                    min(link.last_send + self.config.keepalive);
                    min((link.last_recv + self.config.keepalive).max(link.probe_at));
                    if !link.confirmed {
                        min(link.confirm_at);
                    }
                    if let Some(d) = link.tx.next_due() {
                        min(d);
                    }
                    if let (Path::Direct(_), Some(_)) = (&sess.path, sess.relay) {
                        min(sess.last_direct + self.config.direct_timeout);
                    }
                }
            }
        }
        t
    }

    /// The next datagram to send, if any. Call until it returns `None`.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit<A>> {
        let window = self.config.reliable_window;
        loop {
            if let Some(t) = self.outbox.pop_front() {
                return Some(t);
            }
            let idx = self.ready.pop_front()?;
            let Some(sess) = self.sessions.get_mut(&idx) else { continue };
            let State::Live(link) = &mut sess.state else { continue };
            link.dirty = false;
            if !link.has_output(now, window) {
                continue;
            }
            let direct = matches!(sess.path, Path::Direct(_));
            let cap = if direct { MAX_PLAINTEXT } else { RELAYED_PLAINTEXT };
            let mut pt = Vec::with_capacity(cap);
            if link.rx.ack_pending {
                let (cum, bits) = link.rx.ack();
                wire::put_ack(&mut pt, cum, bits);
                link.rx.ack_pending = false;
            }
            if link.ping_pending {
                wire::put_ping(&mut pt);
                link.ping_pending = false;
                // Tell the peer where we see it from (its reflexive address).
                if let (Path::Direct(a), true) = (&sess.path, self.config.share_addresses && !self.relay_only) {
                    let mut b = Vec::new();
                    a.encode(&mut b);
                    if b.len() <= crate::addr::MAX_ADDR_LEN {
                        wire::put_observed(&mut pt, &b);
                    }
                }
            }
            while let Some(f) = link.relay_q.front() {
                if pt.len() + f.len() > cap {
                    break;
                }
                pt.extend_from_slice(f);
                link.relay_q.pop_front();
            }
            while let Some(m) = link.unrel.front() {
                if pt.len() + UNREL_OVERHEAD + m.len() > cap {
                    break;
                }
                wire::put_unreliable(&mut pt, m);
                link.unrel.pop_front();
            }
            link.tx.write_frames(now, window, &mut pt, cap);
            let Some(payload) = seal(link, &pt) else { continue };
            link.last_send = now;
            if link.has_output(now, window) {
                mark_ready(&mut self.ready, idx, link);
            }
            let (path, peer) = (sess.path.clone(), sess.peer);
            self.emit(path, peer, payload);
        }
    }

    /// Queues `payload` for `dst` on the link to `relay`, as a FWD frame.
    fn relay_out(&mut self, relay: PeerId, dst: &PeerId, payload: Vec<u8>) {
        let Some(&ridx) = self.peers.get(&relay) else { return };
        let Some(sess) = self.sessions.get_mut(&ridx) else { return };
        let State::Live(link) = &mut sess.state else { return };
        if payload.len() > MAX_RELAYED_PACKET || !matches!(sess.path, Path::Direct(_)) {
            return;
        }
        let mut f = Vec::with_capacity(FWD_OVERHEAD + payload.len());
        wire::put_fwd(&mut f, dst, &payload, !self.relay_only);
        push_relay_frame(link, f, self.config.relay_queue, &mut self.stats.relay_dropped);
        mark_ready(&mut self.ready, ridx, link);
    }

    pub fn poll_event(&mut self) -> Option<Event<A>> {
        self.events.pop_front()
    }

    /// Seals any plaintext, well-formed or not, as the next DATA packet on the
    /// live link to `peer`: what a misbehaving peer that completed the
    /// handshake can send. For the fuzz tests.
    #[cfg(test)]
    pub(crate) fn seal_raw(&mut self, peer: &PeerId, plaintext: &[u8]) -> Option<Vec<u8>> {
        let sess = self.sessions.get_mut(self.peers.get(peer)?)?;
        let State::Live(link) = &mut sess.state else { return None };
        seal(link, plaintext)
    }
}

/// Builds the initiator handshake and writes HS1 (our Ed25519 identity as payload).
/// Deterministic in `seed`.
fn initiator_hs(
    seed: [u8; 32],
    local_private: &[u8; 32],
    me: &PeerId,
    peer: &PeerId,
) -> Result<(HandshakeState, Vec<u8>), snow::Error> {
    let remote_static = peer.noise_static().ok_or(snow::Error::Input)?;
    let mut hs = crypto::builder(seed, local_private)?.remote_public_key(&remote_static)?.build_initiator()?;
    let mut buf = [0u8; 256];
    let n = hs.write_message(me.as_bytes(), &mut buf)?;
    Ok((hs, buf[..n].to_vec()))
}

fn mark_ready(ready: &mut VecDeque<u32>, idx: u32, link: &mut Link) {
    if !link.dirty {
        link.dirty = true;
        ready.push_back(idx);
    }
}

fn push_relay_frame(link: &mut Link, frame: Vec<u8>, cap: usize, dropped: &mut u64) {
    if link.relay_q.len() >= cap {
        link.relay_q.pop_front();
        *dropped += 1;
    }
    link.relay_q.push_back(frame);
}

fn seal(link: &mut Link, pt: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(wire::DATA_HEADER + pt.len() + wire::TAG);
    wire::encode_data_header(link.remote_idx, link.send_nonce, &mut out);
    out.resize(wire::DATA_HEADER + pt.len() + wire::TAG, 0);
    let n = link.transport.write_message(link.send_nonce, pt, &mut out[wire::DATA_HEADER..]).ok()?;
    out.truncate(wire::DATA_HEADER + n);
    link.send_nonce += 1;
    Some(out)
}
