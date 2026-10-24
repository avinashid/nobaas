use crate::addr::{Address, Path};
use crate::crypto::{self, ReplayWindow};
use crate::identity::{Identity, PeerId};
use crate::reliable::{PushError, RecoveryStats, RelRecv, RelSend, MAX_URGENT, RECV_WINDOW, URGENT_RECV_WINDOW};
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
/// A birthday spray ([`Endpoint::spray`]) sends this many probes per batch, a
/// batch every `SPRAY_GAP` (about 530 a second): 2048 probes take about 3.8 s.
const SPRAY_BATCH: u16 = 16;
const SPRAY_GAP: Duration = Duration::from_millis(30);
/// Most probes in one spray.
pub const MAX_SPRAY: u16 = 4096;
/// Sprayed ports: `SPRAY_LOW` and up, where NATs hand ports out.
const SPRAY_LOW: u32 = 1024;
const SPRAY_RANGE: u32 = 65536 - SPRAY_LOW;
/// A link on its slow address probes the relays offered for it ([`Endpoint::add_relay`])
/// every this many punch rounds: a relay path works or doesn't, no NAT needs opening.
const RELAY_ROUNDS: u32 = 5;
/// Most relays probed at once for one link, and held off at once.
const MAX_RELAYS: usize = 8;
/// A link on a relay, with a slow address to go back to, leaves the relay once
/// the oldest of its data still unacknowledged has waited this long there: the
/// relay carries the small packets (probes, acks, pings) but not the data.
const RELAY_STALL: Duration = Duration::from_secs(5);
/// A relay that stalled a link isn't taken for that link again for this long,
/// twice as long each time it stalls it again, up to 16 times as long.
const RELAY_HOLD: Duration = Duration::from_secs(60);

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
    /// Cap on announced links. At it a new HS1 is dropped before the
    /// responder's DH, and an inbound handshake that completes once the cap is
    /// reached is dropped on its first data packet, before any of it is acted
    /// on, so nothing is relayed past the cap. Half-open handshakes don't count
    /// (`max_pending_handshakes` bounds them). At the cap even a linked peer that
    /// re-dials waits until its old link times out, since its identity is only
    /// known after the DH. Links we dial ourselves are not capped.
    pub max_links: usize,
    /// Largest `Reliable` message, sent or received; `Urgent` ones are also
    /// capped at [`MAX_URGENT`]. A peer that sends a larger one is dropped.
    pub max_message: usize,
    /// Per-link cap on bytes queued or unacknowledged, for `Reliable` and for
    /// `Urgent` each, so a full reliable buffer never refuses an urgent message.
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
    /// Bytes per second we relay for all peers together, with a burst of a
    /// quarter of that (at least one relayed packet). 0: no node-wide limit.
    pub relay_total_rate: u64,
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

impl Config {
    /// Largest message on the urgent or the reliable lane.
    fn max_message(&self, urgent: bool) -> usize {
        match urgent {
            true => self.max_message.min(MAX_URGENT),
            false => self.max_message,
        }
    }
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
            max_links: usize::MAX,
            max_message: 1 << 20,
            max_send_buffer: 4 << 20,
            reliable_window: 1024,
            unreliable_queue: 256,
            relay: true,
            relay_rate: 256 << 10,
            relay_burst: 64 << 10,
            relay_total_rate: 0,
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
    /// Delivered exactly once, in order with other `Urgent` messages but not
    /// with `Reliable` ones: it has its own sequence, goes out ahead of queued
    /// `Reliable` data, and each fragment is sent twice a few ms apart, so a
    /// single loss costs nothing instead of a retransmission round trip. For
    /// small, latency-critical events, up to [`MAX_URGENT`] (64 KiB); arrives
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
    /// Probes sent by birthday sprays ([`Endpoint::spray`]).
    pub spray_probes: u64,
    /// Packets dropped unread for coming over another peer's channel
    /// ([`Endpoint::handle_datagram_from`]).
    pub rx_wrong_peer: u64,
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
    /// Reliable and urgent fragments sent again (timeouts, fast retransmits, tail probes).
    pub retransmits: u64,
    /// Congestion window, in fragments (before the `reliable_window` cap),
    /// shared by `Reliable` and `Urgent`.
    pub cwnd: u32,
    /// Window slots in use: reliable fragments in flight, and urgent ones by
    /// wire size (a small urgent message takes a fraction of a slot).
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
    /// When a peer's HS1 on a path we already dial may next be answered with
    /// ours (see `on_hs1`).
    echo_at: Instant,
}

/// Probes, paced: of every port of one IP but the low ones, or of a given list.
struct Spray<A> {
    targets: Targets<A>,
    sent: u16,
    count: u16,
    next_at: Instant,
}

enum Targets<A> {
    /// Ports of `base`'s IP in an order that visits each once:
    /// `SPRAY_LOW + (start + k * stride) mod SPRAY_RANGE`, `stride` coprime to the range.
    Ports { base: A, base_port: u16, start: u32, stride: u32 },
    /// These, in this order ([`Endpoint::sweep`]).
    List(Vec<A>),
}

impl<A: Address> Spray<A> {
    /// The next `n` probe addresses.
    fn next(&mut self, n: u16) -> Vec<A> {
        let mut out = Vec::new();
        while self.sent < self.count && out.len() < n as usize {
            let k = self.sent;
            self.sent += 1;
            match &self.targets {
                Targets::Ports { base, base_port, start, stride } => {
                    let k = u64::from(k) * u64::from(*stride) + u64::from(*start);
                    let port = SPRAY_LOW + (k % u64::from(SPRAY_RANGE)) as u32;
                    out.extend(base.port_offset(port as i32 - i32::from(*base_port)));
                }
                Targets::List(list) => out.extend(list.get(usize::from(k)).cloned()),
            }
        }
        out
    }

    fn done(&self) -> bool {
        self.sent >= self.count
    }
}

/// Probes of better paths: direct candidates while relayed, or the relays offered
/// for a link on its slow address. Each is a sealed PUNCH, answered with a PUNCH_ACK
/// along the same path.
struct Punch {
    token: u64,
    next_at: Instant,
    until: Instant,
    round: u32,
    /// Relays to probe through ([`Endpoint::add_relay`]).
    relays: Vec<PeerId>,
}

impl Punch {
    /// The token of probes through relays: never the direct one, so an answer a
    /// relay has carried can't pass for an answer from a direct address.
    fn relay_token(&self) -> u64 {
        !self.token
    }
}

/// A relay a link won't take for now: it stalled the link ([`RELAY_STALL`]).
struct Hold {
    relay: PeerId,
    until: Instant,
    /// Times it stalled this link.
    strikes: u32,
}

/// Whether `held` holds `relay` off at `now` (for borrows that can't take the session).
fn holds(held: &[Hold], relay: &PeerId, now: Instant) -> bool {
    held.iter().any(|h| h.relay == *relay && now < h.until)
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
    /// The urgent stream's receiver: its own sequence space.
    urx: RelRecv,
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
            rx: RelRecv::new(RECV_WINDOW),
            urx: RelRecv::new(URGENT_RECV_WINDOW),
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
            || self.urx.ack_pending
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
    /// A birthday spray under way ([`Endpoint::spray`]).
    spray: Option<Spray<A>>,
    punch: Option<Punch>,
    /// When a packet last came over `path`: a direct path silent for
    /// `direct_timeout` falls back to the relay, a relay to the slow address.
    last_path: Instant,
    /// The address this peer last said it sees us at.
    seen_as: Option<A>,
    /// A slow address of the peer's ([`Endpoint::set_slow`]), which ranks below
    /// a relay: the link leaves it for a relay that answers a probe, and goes back
    /// to it when that relay goes.
    slow: Option<A>,
    /// On a relay: the newest nonce of a packet other than a probe that came
    /// through it, so the peer is on it too. Only probes leave a path, so a newer
    /// one over the slow address means the peer went back there.
    relay_nonce: Option<u64>,
    /// When the link last moved onto a relay: a stall there is timed from then.
    via_since: Instant,
    /// Relays that stalled the link, at most `MAX_RELAYS`.
    held: Vec<Hold>,
}

impl<A: Address> Session<A> {
    /// Whether the link is on its slow address.
    fn on_slow(&self) -> bool {
        on_slow(&self.path, &self.slow)
    }

    /// Holds `relay` off for `RELAY_HOLD`, doubled for each earlier stall.
    fn hold(&mut self, relay: PeerId, now: Instant) {
        let i = match self.held.iter().position(|h| h.relay == relay) {
            Some(i) => i,
            None => {
                if self.held.len() >= MAX_RELAYS {
                    let mut first = 0; // the one that runs out first
                    for j in 1..self.held.len() {
                        if self.held[j].until < self.held[first].until {
                            first = j;
                        }
                    }
                    self.held.swap_remove(first);
                }
                self.held.push(Hold { relay, until: now, strikes: 0 });
                self.held.len() - 1
            }
        };
        let h = &mut self.held[i];
        h.strikes += 1;
        h.until = now + RELAY_HOLD * (1 << (h.strikes - 1).min(4));
    }
}

/// Whether `path` is the slow address `slow` (for borrows that can't take the session).
fn on_slow<A: Address>(path: &Path<A>, slow: &Option<A>) -> bool {
    slow.is_some() && path.direct() == slow.as_ref()
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
    /// Node-wide relay bucket (`Config::relay_total_rate`): tokens and last refill.
    relay_total: (u64, Option<Instant>),
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
            relay_total: (0, None),
        }
    }

    /// Keeps relayed links relayed, so peers reached through a relay never learn
    /// this endpoint's address from it: they are asked to forward without the
    /// origin tag (FWD_QUIET), dials through a relay alone go through it, and
    /// relayed links take no direct candidates, never punch, never answer a punch
    /// and never move to a direct path. A dial is through a relay alone if it is
    /// given the relay before any direct address of its own, or already has a
    /// relay when relay-only is turned on: its direct targets (also those given
    /// with the relay or later), port guesses and sprays are dropped, and an
    /// answer to it at this endpoint's own address is ignored. This endpoint also
    /// stops sharing the addresses it sees (OBSERVED frames, origin tags), as with
    /// `share_addresses` off, and answers a handshake at its own address only from
    /// a peer it is dialling directly or already has a direct link with. Direct
    /// links and dials, such as those to the relay, are unaffected, and so is a
    /// direct dial later given a relay: it keeps its direct addresses and may
    /// complete direct, though it takes no new ones. Links already up keep their
    /// path, and none moves between a slow address and a relay
    /// ([`add_relay`](Endpoint::add_relay)): relays offered are ignored, probes
    /// through them stop and aren't answered, and a relayed link doesn't fall back
    /// to its slow address.
    pub fn set_relay_only(&mut self, on: bool) {
        self.relay_only = on;
        if !on {
            return;
        }
        for sess in self.sessions.values_mut() {
            if let (State::Dialing(d), Some(r)) = (&mut sess.state, sess.relay) {
                d.targets.retain(|t| matches!(t, Path::Via(_)));
                if d.targets.is_empty() {
                    d.targets.push(Path::Via(r)); // the relay was named when the targets were full
                }
            } else if !matches!(sess.path, Path::Via(_)) {
                sess.punch = None; // on a direct link, a probe of relays
                continue;
            }
            sess.punch = None;
            sess.candidates.clear();
            sess.guesses.clear();
            sess.guess_probes = 0;
            sess.spray = None;
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
                s.retransmits += l.tx.retransmits();
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
                fragments_sent: l.tx.fragments_sent(),
                retransmits: l.tx.retransmits(),
                cwnd: l.tx.cwnd(),
                inflight: l.tx.inflight(),
                delay_backoffs: l.tx.delay_backoffs,
            }),
            State::Dialing(_) => None,
        }
    }

    /// How the reliable lanes to `peer` have recovered from loss, for the
    /// simulator's benchmarks; not a stable API.
    #[doc(hidden)]
    pub fn recovery_stats(&self, peer: &PeerId) -> Option<RecoveryStats> {
        match &self.sessions.get(self.peers.get(peer)?)?.state {
            State::Live(l) => Some(l.tx.recovery),
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
    /// through cone NATs. Calling it again while dialling adds paths, except,
    /// while relay-only, direct ones given with a relay or to a dial that has one.
    pub fn connect_with(&mut self, now: Instant, peer: PeerId, targets: &[Path<A>]) -> Result<(), ConnectError> {
        if peer == self.me {
            return Err(ConnectError::SelfConnect);
        }
        peer.noise_static().ok_or(ConnectError::InvalidPeerId)?;
        if targets.is_empty() {
            return Err(ConnectError::NoPath);
        }
        // Relay-only: direct targets given with a relay are dropped (a new dial goes through it alone).
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
                // Relay-only: a dial with a relay takes no new direct paths (a direct one keeps its own).
                if self.relay_only && sess.relay.is_some() && matches!(t, Path::Direct(_)) {
                    continue;
                }
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
            echo_at: now,
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
                spray: None,
                punch: None,
                last_path: now,
                seen_as: None,
                slow: None,
                relay_nonce: None,
                via_since: now,
                held: Vec::new(),
            },
        );
        self.dialing.insert(peer, idx);
        Ok(())
    }

    /// Adds direct addresses (learned through signalling) for a dialling or
    /// relayed peer, and starts punching towards them. Ignored while relay-only
    /// for a peer reached, or being dialled, through a relay.
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
        // The slow address isn't punched to: a relay beats it.
        let known = sess.path.direct() == Some(&a) || sess.slow.as_ref() == Some(&a);
        if known || (relay_only && matches!(sess.path, Path::Via(_))) {
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
        let nothing = sess.candidates.is_empty() && sess.guesses.is_empty() && sess.spray.is_none();
        if !matches!(sess.path, Path::Via(_)) || nothing || !matches!(sess.state, State::Live(_)) {
            return;
        }
        match &mut sess.punch {
            Some(p) => p.until = until,
            None => sess.punch = Some(Punch { token, next_at: now, until, round: 0, relays: Vec::new() }),
        }
    }

    /// Marks `addr` as a slow address of the linked `peer`, or clears the mark
    /// (`None`): a path that relaying through another peer beats, such as a
    /// signalling channel carrying datagrams. Paths rank slow address, then relay,
    /// then direct path. On its slow address a link takes a relay offered with
    /// [`add_relay`](Endpoint::add_relay) once a probe through it is answered, and
    /// goes back to it, with a `PathChanged` event, when that relay's link goes,
    /// when the peer's traffic comes over it again (newer than any through the
    /// relay: the peer went back), when the relay has carried nothing from the
    /// peer for `direct_timeout`, or when data sent there has gone unacknowledged
    /// for 5 s. Then the link holds that relay off for a minute, twice as long each
    /// time it stalls the link again (up to 16 minutes): no probing it, answering a
    /// probe through it, or moving or falling back to it. A slow address is never
    /// punched to, and a relay never forwards to a peer linked only over its slow
    /// address. Ignored for a peer that isn't linked.
    pub fn set_slow(&mut self, peer: &PeerId, addr: Option<A>) {
        let Some(sess) = self.peers.get(peer).and_then(|i| self.sessions.get_mut(i)) else { return };
        sess.candidates.retain(|c| Some(c) != addr.as_ref()); // a dial's own target is one
        sess.slow = addr;
        if matches!(sess.path, Path::Direct(_)) && !sess.on_slow() {
            sess.punch = None; // a probe of relays, and the path isn't slow any more
        }
    }

    /// Offers `relay`, a peer linked directly, to carry the link to `peer` while
    /// that link is on its slow address ([`set_slow`](Endpoint::set_slow)). A
    /// sealed PUNCH goes to `peer` through `relay` every `RELAY_ROUNDS` punch
    /// intervals for `punch_duration`, and `peer` answers it through `relay` only
    /// if it would use the relay too: it is on its own slow address, or on that
    /// relay already. The answer moves the link to the relay (a `PathChanged`
    /// event); several relays can be offered, and the first to answer wins. A
    /// probe arriving through a relay offers that relay back, so both ends move.
    /// Ignored while relay-only, for a link not on its slow address, for a relay
    /// not linked directly or linked only over its own slow address, and for one
    /// the link holds off for stalling it (see [`set_slow`](Endpoint::set_slow)).
    pub fn add_relay(&mut self, now: Instant, relay: PeerId, peer: &PeerId) {
        if let Some(&idx) = self.peers.get(peer) {
            self.offer_relay(now, idx, relay);
        }
    }

    fn offer_relay(&mut self, now: Instant, idx: u32, relay: PeerId) {
        if self.relay_only || !self.fast_link(&relay) {
            return;
        }
        let until = now + self.config.punch_duration;
        let Some(sess) = self.sessions.get_mut(&idx) else { return };
        let held = holds(&sess.held, &relay, now);
        if sess.peer == relay || !sess.on_slow() || !matches!(sess.state, State::Live(_)) || held {
            return;
        }
        let rng = &mut self.rng;
        let p = sess.punch.get_or_insert_with(|| Punch {
            token: rng.next_u64(),
            next_at: now,
            until,
            round: 0,
            relays: Vec::new(),
        });
        if p.relays.contains(&relay) || p.relays.len() >= MAX_RELAYS {
            return;
        }
        p.relays.push(relay);
        p.until = p.until.max(until);
        // Probe it at once (rounds are only relay probes on a slow address).
        p.next_at = now;
        p.round = 0;
    }

    /// Whether `peer` is linked over a direct path that isn't its slow address:
    /// a relay worth moving a link onto.
    fn fast_link(&self, peer: &PeerId) -> bool {
        self.peers
            .get(peer)
            .and_then(|i| self.sessions.get(i))
            .is_some_and(|s| matches!(s.path, Path::Direct(_)) && !s.on_slow() && matches!(s.state, State::Live(_)))
    }

    /// Whether a packet for session `idx` came over the channel of `owner`, a
    /// peer other than the session's: then it is counted, and dropped unread.
    fn on_others_channel(&mut self, owner: Option<PeerId>, idx: u32) -> bool {
        let other = owner.is_some_and(|o| self.sessions.get(&idx).is_some_and(|s| s.peer != o));
        self.stats.rx_wrong_peer += other as u64;
        other
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

    /// Birthday punching, from behind a NAT that keeps one port for every
    /// destination (a cone): probes `count` (at most [`MAX_SPRAY`]) ports of
    /// `addr`'s IP, each once, in a random order, `SPRAY_BATCH` every `SPRAY_GAP`.
    /// The peer is behind a symmetric NAT with random ports and has opened many
    /// sockets towards us ([`opener`](Endpoint::opener)), each with its own
    /// mapping; one probe that lands on one of them lets that socket's answer
    /// through our NAT, and the link takes that path. While dialling, the probes
    /// are the handshake; on a relayed link, punches. Replaces an earlier spray
    /// for the peer; ignored for a peer linked directly, or reached through a
    /// relay by a relay-only endpoint, and for addresses without ports.
    pub fn spray(&mut self, now: Instant, peer: &PeerId, addr: &A, count: u16) {
        let Some(base_port) = addr.to_socket_addr().map(|s| s.port()) else { return };
        let start = self.rng.next_u32() % SPRAY_RANGE;
        let mut stride = (self.rng.next_u32() % SPRAY_RANGE) | 1;
        while stride.is_multiple_of(3) || stride.is_multiple_of(7) {
            stride = (stride + 2) % SPRAY_RANGE; // 64512 = 2^10 * 3^2 * 7
        }
        let targets = Targets::Ports { base: addr.clone(), base_port, start, stride };
        self.start_spray(now, peer, targets, count);
    }

    /// Probes `addrs` (at most [`MAX_SPRAY`]) in this order, each once, paced as a
    /// [`spray`](Endpoint::spray), which it replaces, as are the cases it is
    /// ignored in. For two sequential symmetric NATs probing each other's
    /// predicted ports: every probe goes to a new address, so each costs the NAT
    /// the next port, and the order makes the two sides' mappings line up.
    pub fn sweep(&mut self, now: Instant, peer: &PeerId, addrs: &[A]) {
        let n = addrs.len().min(usize::from(MAX_SPRAY));
        self.start_spray(now, peer, Targets::List(addrs[..n].to_vec()), n as u16);
    }

    fn start_spray(&mut self, now: Instant, peer: &PeerId, targets: Targets<A>, count: u16) {
        let Some(&idx) = self.peers.get(peer).or_else(|| self.dialing.get(peer)) else { return };
        let relay_only = self.relay_only;
        let Some(sess) = self.sessions.get_mut(&idx) else { return };
        let relayed = match &sess.state {
            State::Dialing(d) => d.targets.iter().any(|t| matches!(t, Path::Via(_))),
            State::Live(_) => matches!(sess.path, Path::Via(_)),
        };
        if (relay_only && relayed) || (matches!(sess.state, State::Live(_)) && !relayed) {
            return;
        }
        let count = count.min(MAX_SPRAY);
        sess.spray = Some(Spray { targets, sent: 0, count, next_at: now });
        if matches!(sess.state, State::Live(_)) {
            self.start_punch(now, idx);
        }
    }

    /// Birthday punching, from behind a symmetric NAT with random ports: a
    /// packet for `peer` that the host sends from each of many extra sockets
    /// towards the peer's address, so that each gets a mapping the peer's NAT
    /// will answer through once the peer's [`spray`](Endpoint::spray) hits it.
    /// The host then hands the endpoint what each socket receives and sends
    /// replies to that sender from the socket that heard it. The handshake while
    /// dialling, a punch on a relayed link (which starts punching); `None` once
    /// linked directly, for a relay-only endpoint's relayed link, or with no link
    /// or dial.
    pub fn opener(&mut self, now: Instant, peer: &PeerId) -> Option<Vec<u8>> {
        let &idx = self.peers.get(peer).or_else(|| self.dialing.get(peer))?;
        let token = self.rng.next_u64();
        let until = now + self.config.punch_duration;
        let relay_only = self.relay_only;
        let sess = self.sessions.get_mut(&idx)?;
        match &mut sess.state {
            State::Dialing(d) => Some(d.hs1.clone()),
            State::Live(link) if matches!(sess.path, Path::Via(_)) && !relay_only => {
                let p = sess.punch.get_or_insert(Punch { token, next_at: now, until, round: 0, relays: Vec::new() });
                p.until = p.until.max(until);
                let mut f = Vec::with_capacity(9);
                wire::put_punch(&mut f, false, p.token);
                seal(link, &f)
            }
            State::Live(_) => None,
        }
    }

    /// Closes the link to `peer`, telling it so. No event is emitted for `peer`;
    /// links it relayed go back to their slow addresses (`PathChanged`).
    pub fn disconnect(&mut self, peer: &PeerId) {
        let Some(idx) = self.peers.remove(peer) else { return };
        if let Some(mut sess) = self.sessions.remove(&idx) {
            if let State::Live(link) = &mut sess.state {
                let mut pt = Vec::with_capacity(1);
                wire::put_close(&mut pt);
                let heard = link.last_recv;
                if let Some(p) = seal(link, &pt) {
                    self.emit(sess.path, sess.peer, p);
                }
                self.unrelay(*peer, heard);
            }
        }
    }

    /// `relay`'s link is gone (last heard from at `at`): the links it carried go
    /// back to their slow addresses.
    fn unrelay(&mut self, relay: PeerId, at: Instant) {
        let mut carried = Vec::new();
        for (&i, s) in &self.sessions {
            if matches!(s.path, Path::Via(r) if r == relay) {
                carried.push(i);
            }
        }
        for idx in carried {
            self.back_to_slow(idx, at);
        }
    }

    /// Moves link `idx` off its relay back to its slow address, if it has one,
    /// with a PING there at once. Not while relay-only.
    fn back_to_slow(&mut self, idx: u32, at: Instant) {
        if self.relay_only {
            return;
        }
        let Some(sess) = self.sessions.get_mut(&idx) else { return };
        if !matches!(sess.path, Path::Via(_)) {
            return;
        }
        let (Some(slow), State::Live(link)) = (&sess.slow, &mut sess.state) else { return };
        let path = Path::Direct(slow.clone());
        link.ping_pending = true;
        mark_ready(&mut self.ready, idx, link);
        sess.path = path.clone();
        sess.last_path = at;
        sess.punch = None;
        sess.relay_nonce = None;
        self.events.push_back(Event::PathChanged { peer: sess.peer, path });
    }

    pub fn send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError> {
        let idx = *self.peers.get(peer).ok_or(SendError::NotConnected)?;
        let Some(Session { state: State::Live(link), .. }) = self.sessions.get_mut(&idx) else {
            return Err(SendError::NotConnected);
        };
        match lane {
            Lane::Reliable | Lane::Urgent => link
                .tx
                .push_message(
                    data,
                    self.config.max_send_buffer,
                    self.config.max_message(lane == Lane::Urgent),
                    lane == Lane::Urgent,
                )
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
        self.on_packet(now, Path::Direct(from), None, data);
    }

    /// Like [`Endpoint::handle_datagram`], for a datagram from a channel that only
    /// `peer` sends on, such as a WebRTC DataChannel to it or a signalling pipe: a
    /// packet on it for another peer's link, or a handshake from anyone else, is
    /// dropped unread ([`Stats::rx_wrong_peer`]). So a peer that relays for others
    /// can't send their packets on its own channel to move their links onto it.
    /// Packets relayed through `peer` are unaffected. On addresses anyone can send
    /// from (UDP), use `handle_datagram`: a link roams to wherever its peer's
    /// packets come from.
    pub fn handle_datagram_from(&mut self, now: Instant, from: A, peer: &PeerId, data: &[u8]) {
        self.on_packet(now, Path::Direct(from), Some(*peer), data);
    }

    /// Like [`Endpoint::handle_datagram`], and additionally reports whether the
    /// datagram authenticated to a session whose direct path is now `from`.
    ///
    /// A birthday-punch host (`Node::read_extra`) adopts an extra socket as the
    /// route to `from` only when this holds, so an unauthenticated or spoofed
    /// source can neither redirect our egress nor hold an extra socket open past
    /// its deadline (NB44-2).
    pub fn handle_datagram_direct(&mut self, now: Instant, from: A, data: &[u8]) -> bool {
        match self.on_packet(now, Path::Direct(from.clone()), None, data) {
            Some(idx) => self.sessions.get(&idx).and_then(|s| s.path.direct()) == Some(&from),
            None => false,
        }
    }

    /// Returns the session the packet belonged to. `owner`: the only peer that
    /// sends on `from` (see `handle_datagram_from`).
    fn on_packet(&mut self, now: Instant, from: Path<A>, owner: Option<PeerId>, data: &[u8]) -> Option<u32> {
        self.stats.rx_packets += 1;
        match wire::parse(data) {
            None => {
                self.stats.rx_malformed += 1;
                None
            }
            Some(Packet::Hs1 { sender, noise }) => self.on_hs1(now, from, owner, sender, noise),
            Some(Packet::Hs2 { sender, receiver, noise }) => self.on_hs2(now, from, owner, sender, receiver, noise),
            Some(Packet::Data { receiver, nonce, ciphertext }) => {
                self.on_data(now, from, owner, receiver, nonce, ciphertext)
            }
        }
    }

    fn on_hs1(&mut self, now: Instant, from: Path<A>, owner: Option<PeerId>, sender: u32, noise: &[u8]) -> Option<u32> {
        if noise.len() < 32 {
            self.stats.rx_malformed += 1;
            return None;
        }
        let mut eph = [0u8; 32];
        eph.copy_from_slice(&noise[..32]);
        if let Some(&idx) = self.half_open.get(&(sender, eph)) {
            if self.on_others_channel(owner, idx) {
                return None;
            }
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
        if self.half_open.len() >= self.config.max_pending_handshakes || self.peers.len() >= self.config.max_links {
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
        if owner.is_some_and(|o| o != peer) {
            self.stats.rx_wrong_peer += 1;
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
                let retry = self.config.handshake_retry;
                if let Some(sess) = self.sessions.get_mut(&didx) {
                    if let State::Dialing(d) = &mut sess.state {
                        if !d.targets.contains(&from) && d.targets.len() < 16 {
                            // Their packet proves this path works; aim our dial along it too.
                            d.targets.push(from.clone());
                            resend = Some(d.hs1.clone());
                        } else if matches!(from, Path::Direct(_)) && now >= d.echo_at {
                            // A path we dial already, which may only now work (a birthday
                            // spray hit one of the host's extra sockets): answer now, not
                            // at the next retry, at most once per retry interval.
                            d.echo_at = now + retry;
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
                spray: None,
                punch: None,
                last_path: now,
                seen_as: None,
                slow: None,
                relay_nonce: None,
                via_since: now,
                held: Vec::new(),
            },
        );
        self.start_punch(now, idx);
        Some(idx)
    }

    fn on_hs2(
        &mut self,
        now: Instant,
        from: Path<A>,
        owner: Option<PeerId>,
        sender: u32,
        receiver: u32,
        noise: &[u8],
    ) -> Option<u32> {
        if self.on_others_channel(owner, receiver) {
            return None;
        }
        let Some(sess) = self.sessions.get_mut(&receiver) else {
            self.stats.rx_unknown_session += 1;
            return None;
        };
        let State::Dialing(dial) = &mut sess.state else {
            return Some(receiver); // duplicate HS2 for a link that is already up
        };
        // Relay-only: answered at our own address only if dialled directly (as
        // `answers_directly`), so a dial through a relay alone completes through
        // it, even if an HS1 sent direct before relay-only was turned on is
        // answered there.
        let direct_dial = dial.targets.iter().any(|t| matches!(t, Path::Direct(_)));
        if self.relay_only && matches!(from, Path::Direct(_)) && !direct_dial {
            self.stats.rx_handshake_dropped += 1;
            return None;
        }
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
            Path::Direct(_) => {
                sess.guesses.clear();
                sess.spray = None;
            }
        }
        sess.path = from;
        self.sessions.insert(receiver, sess);
        self.announce(receiver);
        self.start_punch(now, receiver);
        Some(receiver)
    }

    fn on_data(
        &mut self,
        now: Instant,
        from: Path<A>,
        owner: Option<PeerId>,
        receiver: u32,
        nonce: u64,
        ciphertext: &[u8],
    ) -> Option<u32> {
        if self.on_others_channel(owner, receiver) {
            return None;
        }
        // Relayed by a peer that is worth relaying through (see `add_relay`).
        let fast_from = matches!(&from, Path::Via(r) if self.fast_link(r));
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
        // The link cap again for an inbound handshake that completed while the
        // last slot filled: refuse it before any frame is acted on.
        if !link.announced
            && !link.initiator
            && self.peers.len() >= self.config.max_links
            && !self.peers.contains_key(&sess.peer)
        {
            self.stats.rx_handshake_dropped += 1;
            self.drop_link(receiver, None);
            return None;
        }
        let mut path_changed = false;
        let mut reflexive = None;
        let mut offered = None;
        match (&from, &sess.path) {
            (Path::Direct(x), Path::Direct(y)) => {
                sess.last_path = now;
                if x != y {
                    sess.path = from.clone();
                    self.stats.roams += 1;
                    path_changed = true;
                }
            }
            // Direct traffic on a relayed link only proves one direction; it
            // makes the address a candidate, and PUNCH_ACK decides.
            (Path::Direct(x), Path::Via(_)) => reflexive = Some(x.clone()),
            (Path::Via(r), Path::Via(q)) if r == q => sess.last_path = now,
            // The peer moved to another relay: so does the link, unless it holds that one off.
            (Path::Via(r), Path::Via(_)) => {
                if !holds(&sess.held, r, now) {
                    sess.last_path = now;
                    sess.via_since = now;
                    sess.path = from.clone();
                    sess.relay_nonce = None;
                    path_changed = true;
                }
            }
            // Relayed while on the slow address: that relay is probed, and its
            // PUNCH_ACK decides, as for a link given it with `add_relay`.
            (Path::Via(r), Path::Direct(_)) if fast_from && on_slow(&sess.path, &sess.slow) => offered = Some(*r),
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
        let mut relay_acks = Vec::new();
        let mut upgraded = None;
        let mut moved = None;
        let mut observed = None;
        let mut violation = false;
        let mut closed = false;
        let mut probes_only = true;
        for f in wire::frames(&pt[..n]) {
            probes_only &= matches!(f, Ok(Frame::Punch(_) | Frame::PunchAck(_)));
            match f {
                // Answered with an ACK, so a silent-side probe gets a reply.
                Ok(Frame::Ping) => link.rx.ack_pending = true,
                Ok(Frame::Ack { cum, bits, urgent }) => link.tx.on_ack(now, cum, bits, urgent),
                Ok(Frame::Reliable { seq, fin, data, urgent }) => {
                    let rx = if urgent { &mut link.urx } else { &mut link.rx };
                    if rx.on_fragment(seq, fin, data, self.config.max_message(urgent), &mut reliable).is_err() {
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
                    } else if let Path::Via(r) = &from {
                        // Through a relay: answered through it only if this end would
                        // use the relay too, so the prober moves only if both ends do.
                        let joins = (fast_from && on_slow(&sess.path, &sess.slow)) || sess.path == from;
                        if joins && !self.relay_only && !holds(&sess.held, r, now) {
                            relay_acks.push((*r, t));
                        }
                    }
                }
                Ok(Frame::PunchAck(t)) => match (&from, &sess.punch) {
                    (Path::Direct(a), Some(p)) if p.token == t && sess.slow.as_ref() != Some(a) => {
                        upgraded = Some(a.clone())
                    }
                    // A relay probe answered through the relay: it carries both ways.
                    (Path::Via(r), Some(p))
                        if p.relay_token() == t
                            && p.relays.contains(r)
                            && fast_from
                            && on_slow(&sess.path, &sess.slow) =>
                    {
                        moved = Some(*r);
                    }
                    _ => {}
                },
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
        let mut sealed = Vec::new();
        for (r, token) in relay_acks {
            let mut f = Vec::with_capacity(9);
            wire::put_punch(&mut f, true, token);
            if let Some(p) = seal(link, &f) {
                sealed.push((r, p));
            }
        }
        if let Some(a) = upgraded {
            sess.path = Path::Direct(a);
            sess.punch = None;
            sess.guesses.clear();
            sess.spray = None;
            sess.last_path = now;
            self.stats.punch_succeeded += 1;
            path_changed = true;
        }
        if let Some(r) = moved {
            sess.path = Path::Via(r);
            sess.relay = Some(r);
            sess.punch = None;
            sess.last_path = now;
            sess.via_since = now;
            sess.relay_nonce = None;
            // Through it at once: the peer, there too, answers, so each end has had a
            // packet through the relay that isn't a probe (`relay_nonce`) and follows the
            // other back to the slow address on its first one there.
            link.ping_pending = true;
            path_changed = true;
        }
        // Where the peer sends from: everything but probes goes along its path.
        let mut follow = false;
        if !probes_only {
            match (&from, &sess.path) {
                (Path::Via(r), Path::Via(q)) if r == q => sess.relay_nonce = sess.relay_nonce.max(Some(nonce)),
                (Path::Direct(x), Path::Via(_)) => {
                    follow = sess.slow.as_ref() == Some(x) && sess.relay_nonce.is_some_and(|n| nonce > n)
                }
                _ => {}
            }
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
        for (r, ack) in sealed {
            self.relay_out(r, &peer, ack);
        }
        if let Some(r) = offered {
            self.offer_relay(now, receiver, r);
        }
        if moved.is_some() {
            self.start_punch(now, receiver); // direct candidates, if any, beat the relay
        }
        if follow {
            self.back_to_slow(receiver, now); // the peer went back to the slow address
        }
        for (to, packet, origin) in forwards {
            self.forward(now, receiver, &to, &packet, origin.as_ref());
        }
        for (packet, hint) in relayed {
            if let Some(idx) = self.on_packet(now, Path::Via(peer), None, &packet) {
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
    /// address we see the sender at (`origin`: none if it asked us not to). Not
    /// to a peer linked only over its slow address: no relay path runs over one.
    fn forward(&mut self, now: Instant, src: u32, to: &PeerId, packet: &[u8], origin: Option<&A>) {
        let ok = self.config.relay && self.sessions.get(&src).is_some_and(|s| s.peer != *to) && self.fast_link(to);
        if !ok || packet.len() > MAX_RELAYED_PACKET {
            self.stats.relay_dropped += 1;
            return;
        }
        // Token buckets on the requesting link and on the whole node; both must have room.
        let (rate, burst) = (self.config.relay_rate, self.config.relay_burst);
        let Some(Session { state: State::Live(sl), .. }) = self.sessions.get_mut(&src) else { return };
        sl.relay_tokens = refill(sl.relay_tokens, sl.relay_refill, now, rate, burst);
        sl.relay_refill = now;
        let cost = packet.len() as u64;
        let total = self.config.relay_total_rate;
        if total > 0 {
            let (tokens, last) = self.relay_total;
            let burst = (total / 4).max(MAX_RELAYED_PACKET as u64);
            let tokens = last.map_or(burst, |t| refill(tokens, t, now, total, burst));
            self.relay_total = (tokens, Some(now));
        }
        if sl.relay_tokens < cost || (total > 0 && self.relay_total.0 < cost) {
            self.stats.relay_dropped += 1;
            return;
        }
        sl.relay_tokens -= cost;
        if total > 0 {
            self.relay_total.0 -= cost;
        }
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
            if let State::Live(link) = &sess.state {
                self.unrelay(sess.peer, link.last_recv);
            }
        }
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        let mut failed_dials = Vec::new();
        let mut dead = Vec::new();
        let mut expired = Vec::new();
        let mut resend = Vec::new();
        let mut fallbacks = Vec::new();
        let mut unrelayed = Vec::new();
        let mut stalled = Vec::new();
        let spray = self.config.punch_spray as i32;
        for (&idx, sess) in self.sessions.iter_mut() {
            if let Some(s) = sess.spray.as_mut().filter(|s| now >= s.next_at) {
                s.next_at = now + SPRAY_GAP;
                let batch = s.next(SPRAY_BATCH);
                let done = s.done();
                self.stats.spray_probes += batch.len() as u64;
                match &mut sess.state {
                    State::Dialing(d) => {
                        resend.extend(batch.into_iter().map(|a| (Path::Direct(a), sess.peer, d.hs1.clone())))
                    }
                    State::Live(link) => {
                        let Some(p) = &sess.punch else {
                            sess.spray = None; // punching ran out
                            continue;
                        };
                        let mut f = Vec::with_capacity(9);
                        wire::put_punch(&mut f, false, p.token);
                        for dst in batch {
                            if let Some(payload) = seal(link, &f) {
                                self.stats.tx_packets += 1;
                                self.outbox.push_back(Transmit { dst, payload });
                            }
                        }
                    }
                }
                if done {
                    sess.spray = None;
                }
            }
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
                            if p.round % RELAY_ROUNDS == 0 && !p.relays.is_empty() {
                                let mut f = Vec::with_capacity(9);
                                wire::put_punch(&mut f, false, p.relay_token());
                                for &r in &p.relays {
                                    if let Some(payload) = seal(link, &f) {
                                        resend.push((Path::Via(r), sess.peer, payload));
                                    }
                                }
                            }
                            let probe = p.round % GUESS_ROUNDS == 0 && sess.guess_probes > 0;
                            let guessed = if probe { &sess.guesses[..] } else { &[] };
                            sess.guess_probes -= probe as u32;
                            p.round += 1;
                            // Direct candidates are punched only while relayed.
                            let relayed = matches!(sess.path, Path::Via(_));
                            let candidates = if relayed { &sess.candidates[..] } else { &[] };
                            let sprayed = candidates.iter().flat_map(|c| {
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
                        if now >= sess.last_path + self.config.direct_timeout {
                            fallbacks.push(idx);
                        }
                    }
                    // A relay that carries nothing from the peer, or not our data: back to the
                    // slow address before `idle_timeout` or the retries would drop the link.
                    if let (Path::Via(r), Some(_), false) = (&sess.path, &sess.slow, self.relay_only) {
                        if link.tx.oldest_unacked().is_some_and(|s| now >= s.max(sess.via_since) + RELAY_STALL) {
                            stalled.push((idx, *r));
                        } else if now >= sess.last_path + self.config.direct_timeout {
                            unrelayed.push(idx);
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
            let Some(relay) = self.sessions.get(&idx).and_then(|s| s.relay) else { continue };
            let usable = self.fast_link(&relay);
            let Some(sess) = self.sessions.get_mut(&idx) else { continue };
            if !usable || holds(&sess.held, &relay, now) {
                sess.last_path = now; // no relay to fall back to; check again later
                continue;
            }
            if let Path::Direct(old) = std::mem::replace(&mut sess.path, Path::Via(relay)) {
                if !sess.candidates.contains(&old) && sess.slow.as_ref() != Some(&old) {
                    sess.candidates.insert(0, old);
                    sess.candidates.truncate(self.config.max_candidates);
                }
            }
            sess.last_path = now;
            sess.via_since = now;
            sess.relay_nonce = None;
            if let State::Live(link) = &mut sess.state {
                link.ping_pending = true;
                mark_ready(&mut self.ready, idx, link);
            }
            let peer = sess.peer;
            self.stats.direct_fallbacks += 1;
            self.events.push_back(Event::PathChanged { peer, path: Path::Via(relay) });
            self.start_punch(now, idx);
        }
        for idx in unrelayed {
            self.back_to_slow(idx, now);
        }
        for (idx, relay) in stalled {
            if let Some(sess) = self.sessions.get_mut(&idx) {
                sess.hold(relay, now);
                if let State::Live(link) = &mut sess.state {
                    link.tx.resend_all(now); // lost on the relay: over the slow address at once
                }
            }
            self.back_to_slow(idx, now);
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
            if let Some(s) = &sess.spray {
                min(s.next_at);
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
                        min(sess.last_path + self.config.direct_timeout);
                    }
                    if let (Path::Via(_), Some(_), false) = (&sess.path, &sess.slow, self.relay_only) {
                        min(sess.last_path + self.config.direct_timeout);
                        if let Some(s) = link.tx.oldest_unacked() {
                            min(s.max(sess.via_since) + RELAY_STALL);
                        }
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
            for (rx, urgent) in [(&mut link.rx, false), (&mut link.urx, true)] {
                if rx.ack_pending {
                    let (cum, bits) = rx.ack();
                    wire::put_ack(&mut pt, cum, bits, urgent);
                    rx.ack_pending = false;
                }
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

    /// Bytes the live link to `peer` holds for its reliable and urgent receive
    /// streams. For the tests.
    #[cfg(test)]
    pub(crate) fn rx_held(&self, peer: &PeerId) -> Option<[usize; 2]> {
        let sess = self.sessions.get(self.peers.get(peer)?)?;
        let State::Live(link) = &sess.state else { return None };
        Some([link.rx.held(), link.urx.held()])
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

/// A token bucket's `tokens`, last refilled at `last`, refilled at `rate` bytes/s up to `burst`.
fn refill(tokens: u64, last: Instant, now: Instant, rate: u64, burst: u64) -> u64 {
    let elapsed = now.saturating_duration_since(last).as_micros() as u64;
    tokens.saturating_add(elapsed.saturating_mul(rate) / 1_000_000).min(burst)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The direct addresses `ep` sends to from `t` until `t + 2 s`, its timers run every 50 ms.
    fn direct_sends(ep: &mut Endpoint<u32>, t: &mut Instant) -> Vec<u32> {
        let mut sent = Vec::new();
        for _ in 0..40 {
            while let Some(tx) = ep.poll_transmit(*t) {
                sent.push(tx.dst);
            }
            *t = *t + Duration::from_millis(50);
            ep.handle_timeout(*t);
        }
        sent
    }

    /// NB48b-4: relay-only, a dial through a relay takes no direct path added
    /// while it is pending, through `add_candidates`, `connect` or `connect_with`
    /// naming the relay too: nothing is sent there, at once or on a retry.
    /// Without relay-only all three are dialled, and a relay-only endpoint's
    /// direct dial still takes more direct paths.
    #[test]
    fn relay_only_dial_through_a_relay_takes_no_direct_paths() {
        let ids: Vec<Identity> = (1..=4u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let (b, r, c) = (ids[1].peer_id(), ids[2].peer_id(), ids[3].peer_id());
        for relay_only in [true, false] {
            let mut t = Instant::from_millis(1);
            let mut a = Endpoint::<u32>::new(&ids[0], Config::default(), [7; 32]);
            a.set_relay_only(relay_only);
            a.connect_via(t, r, b, &[]).unwrap();
            a.add_candidates(t, &b, &[11]);
            a.connect(t, 12, b).unwrap();
            a.connect_with(t, b, &[Path::Via(r), Path::Direct(13)]).unwrap();
            let sent = direct_sends(&mut a, &mut t);
            if relay_only {
                assert_eq!(sent, Vec::<u32>::new());
            } else {
                assert!([11, 12, 13].iter().all(|x| sent.contains(x)), "{sent:?}");
            }
            a.connect(t, 21, c).unwrap();
            a.add_candidates(t, &c, &[22]);
            let sent = direct_sends(&mut a, &mut t);
            assert!(sent.contains(&21) && sent.contains(&22), "relay_only {relay_only}: {sent:?}");
        }
    }

    /// Runs `eps`, endpoint `i` at address `i + 1`, for `ms` from `t` with timers
    /// every 10 ms, delivering every datagram between them except those to
    /// `down`; returns each datagram sent as (sender index, destination).
    fn route(eps: &mut [Endpoint<u32>], t: &mut Instant, ms: u64, down: Option<u32>) -> Vec<(usize, u32)> {
        route_with(eps, t, ms, |_, dst| Some(dst) == down)
    }

    /// As `route`, losing every datagram for which `lost(sender index, destination)` holds.
    fn route_with(
        eps: &mut [Endpoint<u32>],
        t: &mut Instant,
        ms: u64,
        lost: impl Fn(usize, u32) -> bool,
    ) -> Vec<(usize, u32)> {
        route_sized(eps, t, ms, |i, dst, _| lost(i, dst))
    }

    /// As `route_with`, `lost` also given the datagram.
    fn route_sized(
        eps: &mut [Endpoint<u32>],
        t: &mut Instant,
        ms: u64,
        lost: impl Fn(usize, u32, &[u8]) -> bool,
    ) -> Vec<(usize, u32)> {
        let mut sent = Vec::new();
        for _ in 0..ms / 10 {
            for _ in 0..100 {
                let mut quiet = true;
                for i in 0..eps.len() {
                    let out: Vec<_> = std::iter::from_fn(|| eps[i].poll_transmit(*t)).collect();
                    for tx in out {
                        quiet = false;
                        sent.push((i, tx.dst));
                        let to = (!lost(i, tx.dst, &tx.payload)).then(|| (tx.dst as usize).checked_sub(1)).flatten();
                        if let Some(ep) = to.and_then(|j| eps.get_mut(j)) {
                            ep.handle_datagram(*t, i as u32 + 1, &tx.payload);
                        }
                    }
                }
                if quiet {
                    break;
                }
            }
            *t = *t + Duration::from_millis(10);
            for ep in eps.iter_mut() {
                ep.handle_timeout(*t);
            }
        }
        sent
    }

    /// Relay-only turned on while a dial through a relay is under way: the dial
    /// drops its direct targets, port guesses and sweep, so nothing more goes
    /// direct, ignores the peer's answer at our address to an HS1 sent there
    /// before the switch, and completes through the relay. A direct dial goes on.
    #[test]
    fn relay_only_turned_on_mid_dial_dials_through_the_relay_alone() {
        let ids: Vec<Identity> = (1..=4u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let (b, r, c) = (ids[1].peer_id(), ids[2].peer_id(), ids[3].peer_id());
        // a, b and r at addresses 1, 2 and 3, a and b linked to r; c is never reached.
        let mut eps: Vec<Endpoint<u32>> =
            ids[..3].iter().zip(1u8..).map(|(id, s)| Endpoint::new(id, Config::default(), [s; 32])).collect();
        let mut t = Instant::from_millis(1);
        eps[0].connect(t, 3, r).unwrap();
        eps[1].connect(t, 3, r).unwrap();
        route(&mut eps, &mut t, 100, None);
        assert!(eps[2].is_connected(&ids[0].peer_id()) && eps[2].is_connected(&b));

        let a = &mut eps[0];
        a.connect_via(t, r, b, &[2, 12]).unwrap();
        a.set_guesses(t, &b, &[30, 31]);
        a.sweep(t, &b, &(100..300).collect::<Vec<u32>>());
        a.connect(t, 21, c).unwrap();
        a.add_candidates(t, &c, &[22]);
        a.set_guesses(t, &c, &[23]);
        a.sweep(t, &c, &(500..600).collect::<Vec<u32>>());
        // Drained once: b gets the direct HS1 and answers it at a's address.
        let first: Vec<_> = std::iter::from_fn(|| eps[0].poll_transmit(t)).collect();
        for tx in first.iter().filter(|tx| tx.dst == 2) {
            eps[1].handle_datagram(t, 1, &tx.payload);
        }
        assert_eq!(first.iter().filter(|tx| tx.dst == 2).count(), 1);

        eps[0].set_relay_only(true);
        // r out of reach for 2 s, so the dial is still pending; then back.
        let mut sent = route(&mut eps, &mut t, 2000, Some(3));
        sent.extend(route(&mut eps, &mut t, 3000, None));
        let to_c = |d: &u32| [21, 22, 23].contains(d) || (500..600).contains(d);
        let to_b: Vec<u32> = sent.iter().filter(|&&(i, d)| i == 0 && d != 3 && !to_c(&d)).map(|s| s.1).collect();
        assert_eq!(to_b, Vec::<u32>::new(), "direct sends from a after relay-only");
        assert_eq!(eps[0].stats().rx_handshake_dropped, 1, "b's HS2 at a's address is ignored");
        assert_eq!(eps[0].path(&b), Some(&Path::Via(r)), "the dial completes through r");
        let to_c: Vec<u32> = sent.iter().filter(|&&(i, d)| i == 0 && to_c(&d)).map(|s| s.1).collect();
        assert!([21, 22, 23].into_iter().chain(500..600).all(|x| to_c.contains(&x)), "{to_c:?}");
    }

    /// Relay-only, a direct dial also given a relay while pending keeps its
    /// direct target, so the peer's answer there is taken and the link is direct.
    #[test]
    fn relay_only_direct_dial_given_a_relay_takes_the_direct_answer() {
        let ids: Vec<Identity> = (1..=3u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let (b, r) = (ids[1].peer_id(), ids[2].peer_id());
        let mut eps: Vec<Endpoint<u32>> =
            ids.iter().zip(1u8..).map(|(id, s)| Endpoint::new(id, Config::default(), [s; 32])).collect();
        let mut t = Instant::from_millis(1);
        eps[0].connect(t, 3, r).unwrap();
        eps[1].connect(t, 3, r).unwrap();
        route(&mut eps, &mut t, 100, None);
        eps[0].set_relay_only(true);
        eps[0].connect(t, 2, b).unwrap();
        eps[0].connect_via(t, r, b, &[]).unwrap();
        route(&mut eps, &mut t, 1000, None);
        assert_eq!(eps[0].path(&b), Some(&Path::Direct(2)));
        assert_eq!(eps[0].stats().rx_handshake_dropped, 0);
    }

    /// a, b and c at addresses 1, 2 and 3, all linked directly, a and b marking
    /// each other's address slow as a room marks its signalling pipe; their
    /// events so far drained.
    fn slow_trio(t: &mut Instant) -> (Vec<Endpoint<u32>>, [PeerId; 3]) {
        let ids: Vec<Identity> = (1..=3u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let p = [ids[0].peer_id(), ids[1].peer_id(), ids[2].peer_id()];
        let mut eps: Vec<Endpoint<u32>> =
            ids.iter().zip(1u8..).map(|(id, s)| Endpoint::new(id, Config::default(), [s; 32])).collect();
        eps[0].connect(*t, 2, p[1]).unwrap();
        eps[2].connect(*t, 1, p[0]).unwrap();
        eps[2].connect(*t, 2, p[1]).unwrap();
        route(&mut eps, t, 200, None);
        assert!(eps[0].is_connected(&p[1]) && eps[2].is_connected(&p[0]) && eps[2].is_connected(&p[1]));
        eps[0].set_slow(&p[1], Some(2));
        eps[1].set_slow(&p[0], Some(1));
        for ep in eps.iter_mut() {
            events(ep);
        }
        (eps, p)
    }

    /// The events `ep` emitted since the last call.
    fn events(ep: &mut Endpoint<u32>) -> Vec<Event<u32>> {
        std::iter::from_fn(|| ep.poll_event()).collect()
    }

    /// Sends a message each way between `eps[0]` and `eps[1]`; whether both arrive.
    fn both_ways(
        eps: &mut [Endpoint<u32>],
        p: &[PeerId; 3],
        t: &mut Instant,
        lost: impl Fn(usize, u32) -> bool,
    ) -> bool {
        eps[0].send(&p[1], Lane::Reliable, b"to b").unwrap();
        eps[1].send(&p[0], Lane::Reliable, b"to a").unwrap();
        route_with(eps, t, 500, lost);
        let mut got = |i: usize, from: PeerId| {
            events(&mut eps[i]).into_iter().any(|e| matches!(e, Event::Message { peer, .. } if peer == from))
        };
        got(1, p[0]) & got(0, p[1])
    }

    /// A link on its slow address moves to a relay offered with `add_relay` once
    /// a probe through it is answered, at both ends (b probes back when a's probe
    /// reaches it), and goes back to the slow address as soon as the relay's link
    /// closes: the same link throughout, with only `PathChanged` events.
    #[test]
    fn slow_link_moves_to_an_answering_relay_and_back_when_it_closes() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        eps[0].add_relay(t, c, &b);
        route(&mut eps, &mut t, 500, None);
        assert_eq!(eps[0].path(&b), Some(&Path::Via(c)));
        assert_eq!(eps[1].path(&a), Some(&Path::Via(c)));
        assert_eq!(events(&mut eps[0]), vec![Event::PathChanged { peer: b, path: Path::Via(c) }]);
        assert_eq!(events(&mut eps[1]), vec![Event::PathChanged { peer: a, path: Path::Via(c) }]);
        eps[0].add_candidates(t, &b, &[2]); // its slow address: never punched to
        let relayed = eps[2].stats().relayed;
        assert!(both_ways(&mut eps, &p, &mut t, |_, _| false));
        assert!(eps[2].stats().relayed > relayed, "the messages went through c");
        assert_eq!(eps[0].stats().punches_sent, 0);

        eps[2].disconnect(&a);
        eps[2].disconnect(&b);
        route(&mut eps, &mut t, 100, None);
        assert_eq!(eps[0].path(&b), Some(&Path::Direct(2)));
        assert_eq!(eps[1].path(&a), Some(&Path::Direct(1)));
        let closed = Event::Disconnected { peer: c, reason: DisconnectReason::ClosedByPeer };
        assert_eq!(events(&mut eps[0]), vec![closed.clone(), Event::PathChanged { peer: b, path: Path::Direct(2) }]);
        assert_eq!(events(&mut eps[1]), vec![closed, Event::PathChanged { peer: a, path: Path::Direct(1) }]);
        let relayed = eps[2].stats().relayed;
        assert!(both_ways(&mut eps, &p, &mut t, |_, _| false));
        assert_eq!(eps[2].stats().relayed, relayed);
    }

    /// A relay path that works one way moves nothing: a's probes through c reach
    /// b, but nothing from c reaches a, so neither b's answers nor b's own probes
    /// do, and b, which had a's relayed packets, stays on its slow address too.
    #[test]
    fn a_one_way_relay_moves_no_link() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        eps[0].add_relay(t, c, &b);
        route_with(&mut eps, &mut t, 6000, |i, dst| i == 2 && dst == 1);
        // a's probes, b's answers and b's own probes, each every half second.
        assert!(eps[2].stats().relayed >= 20, "{:?}", eps[2].stats());
        assert_eq!(eps[0].path(&b), Some(&Path::Direct(2)));
        assert_eq!(eps[1].path(&a), Some(&Path::Direct(1)));
        assert_eq!(events(&mut eps[0]), vec![]);
        assert_eq!(events(&mut eps[1]), vec![]);
    }

    /// A relay that falls silent (it crashed, say) hands the link back to the slow
    /// address after `direct_timeout`, before `idle_timeout` would drop the link.
    #[test]
    fn a_silent_relay_hands_the_link_back_to_the_slow_address() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        eps[0].add_relay(t, c, &b);
        route(&mut eps, &mut t, 500, None);
        assert_eq!((eps[0].path(&b), eps[1].path(&a)), (Some(&Path::Via(c)), Some(&Path::Via(c))));
        events(&mut eps[0]);
        events(&mut eps[1]);
        let c_down = |i: usize, dst: u32| i == 2 || dst == 3;
        route_with(&mut eps, &mut t, 13_000, c_down);
        assert_eq!(eps[0].path(&b), Some(&Path::Via(c)), "not before direct_timeout");
        route_with(&mut eps, &mut t, 3_000, c_down);
        assert_eq!(events(&mut eps[0]), vec![Event::PathChanged { peer: b, path: Path::Direct(2) }]);
        assert_eq!(events(&mut eps[1]), vec![Event::PathChanged { peer: a, path: Path::Direct(1) }]);
        assert!(both_ways(&mut eps, &p, &mut t, c_down));
    }

    /// A relay that closes its link to one end only (its close to the other end
    /// lost, say): that end goes back to the slow address, and the other follows
    /// as soon as the peer's traffic comes over it, newer than any through the
    /// relay, rather than after `direct_timeout`.
    #[test]
    fn the_other_end_follows_its_peer_back_to_the_slow_address() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        eps[0].add_relay(t, c, &b);
        route(&mut eps, &mut t, 500, None);
        assert!(both_ways(&mut eps, &p, &mut t, |_, _| false), "through c, each end's traffic");
        events(&mut eps[0]);
        events(&mut eps[1]);
        eps[2].disconnect(&a);
        route(&mut eps, &mut t, 100, None);
        assert_eq!(eps[0].path(&b), Some(&Path::Direct(2)));
        assert_eq!(events(&mut eps[1]), vec![Event::PathChanged { peer: a, path: Path::Direct(1) }]);
        assert!(eps[1].is_connected(&c), "b heard nothing from c");
        assert!(both_ways(&mut eps, &p, &mut t, |_, _| false));
    }

    /// While a has moved to c and b, whose own probes are lost, is still on its
    /// slow address, b's traffic comes to a over the slow address: a stays on c,
    /// since none of b's traffic has come through c yet (b never was there).
    #[test]
    fn traffic_over_the_slow_address_before_the_peer_moved_leaves_the_relay() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        eps[0].add_relay(t, c, &b);
        // b to c: the first datagram, b's answer to a's probe, goes; b's own probes don't.
        let n = std::cell::Cell::new(0);
        let lost = |i: usize, dst: u32| i == 1 && dst == 3 && n.replace(n.get() + 1) > 0;
        route_with(&mut eps, &mut t, 300, lost);
        assert_eq!((eps[0].path(&b), eps[1].path(&a)), (Some(&Path::Via(c)), Some(&Path::Direct(1))));
        eps[1].send(&a, Lane::Reliable, b"over the slow address").unwrap();
        route_with(&mut eps, &mut t, 300, lost);
        let got = events(&mut eps[0]);
        assert!(got.iter().any(|e| matches!(e, Event::Message { peer, .. } if *peer == b)), "{got:?}");
        assert_eq!(eps[0].path(&b), Some(&Path::Via(c)));
    }

    /// Moves that never happen: on a path that isn't slow `add_relay` probes
    /// nothing; a relay-only end neither takes a relay nor answers a probe through
    /// one, so its peer stays put too; and a relay forwards nothing to a peer it
    /// reaches only over that peer's slow address.
    #[test]
    fn no_move_off_a_fast_path_while_relay_only_or_onto_a_slow_relay_leg() {
        let mut t = Instant::from_millis(1);
        let still = |eps: &mut [Endpoint<u32>], p: &[PeerId; 3]| {
            assert_eq!(eps[0].path(&p[1]), Some(&Path::Direct(2)));
            assert_eq!(eps[1].path(&p[0]), Some(&Path::Direct(1)));
            assert_eq!(events(&mut eps[0]), vec![]);
            assert_eq!(events(&mut eps[1]), vec![]);
        };
        let (mut eps, p) = slow_trio(&mut t);
        eps[0].set_slow(&p[1], None);
        eps[0].add_relay(t, p[2], &p[1]);
        eps[1].add_relay(t, p[2], &p[0]); // and a, not on a slow address, doesn't answer b's probes
        let sent = route(&mut eps, &mut t, 6000, None);
        assert!(eps[2].stats().relayed >= 10, "b's probes went through c");
        assert!(!sent.contains(&(0, 3)), "a sent nothing through c");
        still(&mut eps, &p);

        let (mut eps, p) = slow_trio(&mut t);
        eps[0].add_relay(t, p[2], &p[1]); // a probe under way stops
        eps[0].set_relay_only(true);
        eps[0].add_relay(t, p[2], &p[1]);
        eps[1].add_relay(t, p[2], &p[0]);
        route(&mut eps, &mut t, 6000, None);
        assert!(eps[2].stats().relayed >= 10, "b's probes went through c");
        still(&mut eps, &p);

        let (mut eps, p) = slow_trio(&mut t);
        eps[2].set_slow(&p[1], Some(2));
        eps[0].add_relay(t, p[2], &p[1]);
        route(&mut eps, &mut t, 6000, None);
        assert_eq!(eps[2].stats().relayed, 0);
        assert!(eps[2].stats().relay_dropped >= 10, "a's probes stopped at c");
        still(&mut eps, &p);
    }

    /// Whether any link in `got` dropped.
    fn dropped(got: &[Event<u32>]) -> bool {
        got.iter().any(|e| matches!(e, Event::Disconnected { .. }))
    }

    /// From `slow_trio`: moves the a-b link to c, then sends `data` from a to b,
    /// which c drops (it relays nothing over 200 bytes to b). Checks that a gives c
    /// up `RELAY_STALL` later, its timer due then, and sends the data again over
    /// the slow address at once, and that b gets it there and follows. Returns
    /// when a gave c up.
    fn stall_on_c(eps: &mut [Endpoint<u32>], t: &mut Instant, p: &[PeerId; 3], data: &[u8]) -> Instant {
        let [a, b, c] = *p;
        let paths = |eps: &[Endpoint<u32>]| (eps[0].path(&b).cloned(), eps[1].path(&a).cloned());
        eps[0].add_relay(*t, c, &b);
        route(eps, t, 500, None);
        assert_eq!(paths(eps), (Some(Path::Via(c)), Some(Path::Via(c))));
        assert!(!dropped(&events(&mut eps[0])) && !dropped(&events(&mut eps[1])));
        let drops_data = |i: usize, dst: u32, d: &[u8]| i == 2 && dst == 2 && d.len() > 200;
        eps[0].send(&b, Lane::Reliable, data).unwrap();
        let stall = *t + RELAY_STALL;
        route_sized(eps, t, 4_900, drops_data);
        assert_eq!(paths(eps), (Some(Path::Via(c)), Some(Path::Via(c))), "not before RELAY_STALL");
        assert!(eps[0].poll_timeout() <= Some(stall), "a's timer is due at the stall");
        route_sized(eps, t, 200, drops_data);
        assert_eq!(paths(eps), (Some(Path::Direct(2)), Some(Path::Direct(1))));
        assert_eq!(events(&mut eps[0]), vec![Event::PathChanged { peer: b, path: Path::Direct(2) }]);
        let got = events(&mut eps[1]);
        let message = Event::Message { peer: a, lane: Lane::Reliable, data: data.to_vec() };
        assert!(got.contains(&message) && !dropped(&got), "{got:?}");
        assert!(got.contains(&Event::PathChanged { peer: a, path: Path::Direct(1) }), "{got:?}");
        stall
    }

    /// FL2-1: a relay that carries the small packets (probes, acks, pings) but not
    /// the data hands the link back to the slow address `RELAY_STALL` after the
    /// data went out, rather than holding it until the retries drop it. a then
    /// holds c off: offered again by both ends, c isn't taken, nor fallen back to
    /// when the slow address goes quiet, until the hold runs out; a second stall
    /// holds it off twice as long.
    #[test]
    fn a_relay_that_stalls_the_data_hands_the_link_back_and_is_held_off() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        let slow = (Some(Path::Direct(2)), Some(Path::Direct(1)));
        let paths = |eps: &[Endpoint<u32>]| (eps[0].path(&b).cloned(), eps[1].path(&a).cloned());
        let route_to = |eps: &mut [Endpoint<u32>], t: &mut Instant, until: Instant| {
            route(eps, t, (until - *t).as_millis() as u64, None);
        };
        let offer = |eps: &mut [Endpoint<u32>], t: Instant| {
            eps[0].add_relay(t, c, &b);
            eps[1].add_relay(t, c, &a);
        };
        let s1 = stall_on_c(&mut eps, &mut t, &p, &[7; 400]);
        offer(&mut eps, t);
        route(&mut eps, &mut t, 5_000, None);
        assert_eq!(paths(&eps), slow, "held off");
        route_with(&mut eps, &mut t, 16_000, |i, dst| i == 1 && dst == 1);
        assert_eq!(paths(&eps), slow, "no fallback to c either");
        route_to(&mut eps, &mut t, s1 + (RELAY_HOLD - Duration::from_secs(5)));
        offer(&mut eps, t);
        route(&mut eps, &mut t, 4_900, None);
        assert_eq!(paths(&eps), slow, "held off until the hold runs out");
        let (ga, gb) = (events(&mut eps[0]), events(&mut eps[1]));
        assert!(ga.is_empty() && gb.is_empty(), "{ga:?} {gb:?}");
        assert_eq!(eps[0].stats().direct_fallbacks, 0);

        route(&mut eps, &mut t, 200, None);
        let s2 = stall_on_c(&mut eps, &mut t, &p, &[8; 400]);
        route_to(&mut eps, &mut t, s2 + RELAY_HOLD + Duration::from_secs(1));
        offer(&mut eps, t);
        route(&mut eps, &mut t, 5_000, None);
        assert_eq!(paths(&eps), slow, "held off twice as long");
        route_to(&mut eps, &mut t, s2 + RELAY_HOLD * 2);
        eps[0].add_relay(t, c, &b);
        route(&mut eps, &mut t, 500, None);
        assert_eq!(paths(&eps), (Some(Path::Via(c)), Some(Path::Via(c))));
        assert!(!dropped(&events(&mut eps[0])) && !dropped(&events(&mut eps[1])));
    }

    /// The stall is timed from the move onto the relay at the earliest: data stuck
    /// on the slow address before the move, its retries far apart by then, gets
    /// `RELAY_STALL` on the relay, and goes through there.
    #[test]
    fn a_stall_is_timed_from_the_move_onto_the_relay() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        let pipe_down = |i: usize, dst: u32| i == 0 && dst == 2;
        eps[0].send(&b, Lane::Reliable, &[5; 400]).unwrap();
        route_with(&mut eps, &mut t, 4_000, pipe_down);
        eps[0].add_relay(t, c, &b);
        route_with(&mut eps, &mut t, 500, pipe_down);
        assert_eq!((eps[0].path(&b), eps[1].path(&a)), (Some(&Path::Via(c)), Some(&Path::Via(c))));
        events(&mut eps[0]);
        route_with(&mut eps, &mut t, 4_000, pipe_down);
        assert_eq!(eps[0].path(&b), Some(&Path::Via(c)), "not given up 5 s after the data went out");
        assert_eq!(events(&mut eps[0]), vec![]);
        let got = events(&mut eps[1]);
        let message = Event::Message { peer: a, lane: Lane::Reliable, data: vec![5; 400] };
        assert!(got.contains(&message), "through c: {got:?}");
    }

    /// A relay the link holds off isn't roamed to either. b offers c, which a
    /// holds off, a offers d: both move to d, and b's probe through c, sent after
    /// a moved, doesn't take a's link onto c.
    #[test]
    fn a_held_relay_is_not_roamed_to() {
        let ids: Vec<Identity> = (1..=4u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let (a, b, c, d) = (ids[0].peer_id(), ids[1].peer_id(), ids[2].peer_id(), ids[3].peer_id());
        let mut eps: Vec<Endpoint<u32>> =
            ids.iter().zip(1u8..).map(|(id, s)| Endpoint::new(id, Config::default(), [s; 32])).collect();
        let mut t = Instant::from_millis(1);
        eps[0].connect(t, 2, b).unwrap();
        for r in [2, 3] {
            eps[r].connect(t, 1, a).unwrap();
            eps[r].connect(t, 2, b).unwrap();
        }
        route(&mut eps, &mut t, 200, None);
        eps[0].set_slow(&b, Some(2));
        eps[1].set_slow(&a, Some(1));
        events(&mut eps[0]);
        let idx = eps[0].peers[&b];
        eps[0].sessions.get_mut(&idx).unwrap().hold(c, t);
        eps[1].add_relay(t, c, &a);
        eps[0].add_relay(t, d, &b);
        let sent = route(&mut eps, &mut t, 2_000, None);
        assert!(sent.iter().filter(|&&(i, dst)| i == 2 && dst == 1).count() >= 2, "c relayed b's probes to a");
        assert_eq!((eps[0].path(&b), eps[1].path(&a)), (Some(&Path::Via(d)), Some(&Path::Via(d))));
        assert_eq!(events(&mut eps[0]), vec![Event::PathChanged { peer: b, path: Path::Via(d) }]);
    }

    /// Each stall by the same relay doubles its hold, up to 16 times `RELAY_HOLD`;
    /// a link holds at most `MAX_RELAYS` relays off, a new one replacing the hold
    /// that runs out first.
    #[test]
    fn relay_holds_double_and_stay_bounded() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let idx = eps[0].peers[&p[1]];
        let sess = eps[0].sessions.get_mut(&idx).unwrap();
        let r = PeerId([9; 32]);
        let mut held_for = Vec::new();
        for _ in 0..6 {
            sess.hold(r, t);
            held_for.push(sess.held[0].until - t);
        }
        assert_eq!(held_for, [1u32, 2, 4, 8, 16, 16].map(|k| RELAY_HOLD * k));
        for i in 0..MAX_RELAYS as u8 {
            sess.hold(PeerId([i; 32]), t + Duration::from_secs(u64::from(i)));
        }
        assert_eq!(sess.held.len(), MAX_RELAYS);
        assert!(!holds(&sess.held, &PeerId([0; 32]), t), "replaced: it ran out first");
        assert!(holds(&sess.held, &r, t) && holds(&sess.held, &PeerId([7; 32]), t));
    }

    /// FL2-2: b offers c as a relay for its link to a, which is on its slow
    /// address, and c keeps b's probe for a instead of relaying it, and sends it on
    /// its own channel. Told the channel is c's, a drops it unread and stays put;
    /// from an address anyone can send from (UDP), the same packet still moves the
    /// link there, as its nonce wasn't taken.
    #[test]
    fn a_relay_cannot_move_a_link_onto_its_own_channel() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        eps[1].add_relay(t, c, &a);
        eps[1].handle_timeout(t);
        let out: Vec<_> = std::iter::from_fn(|| eps[1].poll_transmit(t)).collect();
        for tx in out.iter().filter(|tx| tx.dst == 3) {
            eps[2].handle_datagram(t, 2, &tx.payload);
        }
        let to_a = eps[2].peers[&a];
        let Some(Session { state: State::Live(link), .. }) = eps[2].sessions.get_mut(&to_a) else { panic!() };
        let frame = link.relay_q.pop_front().expect("b's probe, to relay to a");
        let Some(Ok(Frame::Relayed { packet, .. })) = wire::frames(&frame).next() else { panic!() };
        let packet = packet.to_vec();

        eps[0].handle_datagram_from(t, 3, &c, &packet);
        assert_eq!(eps[0].stats().rx_wrong_peer, 1);
        assert_eq!(eps[0].path(&b), Some(&Path::Direct(2)));
        assert_eq!(events(&mut eps[0]), vec![]);
        let sealed = eps[1].seal_raw(&a, &[]).unwrap();
        eps[0].handle_datagram_from(t, 3, &c, &sealed);
        assert_eq!(eps[0].stats().rx_wrong_peer, 2, "any packet of b's");
        eps[0].handle_datagram(t, 3, &packet);
        assert_eq!(eps[0].path(&b), Some(&Path::Direct(3)));
        assert_eq!(events(&mut eps[0]), vec![Event::PathChanged { peer: b, path: Path::Direct(3) }]);
    }

    /// FL2-2, handshakes: an HS1 or HS2 on a channel only another peer sends on is
    /// dropped unread, the HS1 sent again to a half-open link too; on its peer's
    /// channel the handshake completes.
    #[test]
    fn a_handshake_on_another_peers_channel_is_dropped() {
        let ids: Vec<Identity> = (1..=3u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let (x, y, z) = (ids[0].peer_id(), ids[1].peer_id(), ids[2].peer_id());
        let mut ex = Endpoint::<u32>::new(&ids[0], Config::default(), [1; 32]);
        let mut ey = Endpoint::<u32>::new(&ids[1], Config::default(), [2; 32]);
        let t = Instant::from_millis(1);
        ex.connect(t, 2, y).unwrap();
        let hs1 = ex.poll_transmit(t).unwrap().payload;
        ey.handle_datagram_from(t, 1, &z, &hs1);
        assert_eq!((ey.stats().rx_wrong_peer, ey.session_count()), (1, 0));
        assert!(ey.poll_transmit(t).is_none());
        ey.handle_datagram_from(t, 1, &x, &hs1);
        let hs2 = ey.poll_transmit(t).unwrap().payload;
        ey.handle_datagram_from(t, 1, &z, &hs1);
        assert_eq!(ey.stats().rx_wrong_peer, 2);
        assert!(ey.poll_transmit(t).is_none(), "no HS2 again on z's channel");
        ex.handle_datagram_from(t, 2, &z, &hs2);
        assert_eq!(ex.stats().rx_wrong_peer, 1);
        assert!(!ex.is_connected(&y));
        ex.handle_datagram_from(t, 2, &y, &hs2);
        assert!(ex.is_connected(&y));
    }

    /// A relay probe's answer, sent on from an address of its own by a relay that
    /// kept it, answers no direct probe: relay probes carry a token of their own.
    /// (On an address anyone can send from, the link roams there all the same.)
    #[test]
    fn a_relayed_punch_ack_answers_no_direct_probe() {
        let mut t = Instant::from_millis(1);
        let (mut eps, p) = slow_trio(&mut t);
        let [a, b, c] = p;
        eps[0].add_relay(t, c, &b);
        eps[0].handle_timeout(t);
        // a's probe through c to b, and b's answer back to c, which keeps it.
        for (from, to) in [(0, 2), (2, 1), (1, 2)] {
            let out: Vec<_> = std::iter::from_fn(|| eps[from].poll_transmit(t)).collect();
            for tx in out.iter().filter(|tx| tx.dst == to as u32 + 1) {
                eps[to].handle_datagram(t, from as u32 + 1, &tx.payload);
            }
        }
        let to_a = eps[2].peers[&a];
        let Some(Session { state: State::Live(link), .. }) = eps[2].sessions.get_mut(&to_a) else { panic!() };
        let frame = link.relay_q.pop_front().expect("b's answer, to relay to a");
        let Some(Ok(Frame::Relayed { packet, .. })) = wire::frames(&frame).next() else { panic!() };
        let packet = packet.to_vec();
        eps[0].handle_datagram(t, 9, &packet);
        assert_eq!(eps[0].path(&b), Some(&Path::Direct(9)));
        assert_eq!(eps[0].stats().punch_succeeded, 0);
    }

    /// The direct path going quiet moves a link to its relay only if that relay is
    /// linked directly, not just over its slow address.
    #[test]
    fn a_link_falls_back_only_to_a_relay_linked_directly() {
        let ids: Vec<Identity> = (1..=3u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let (b, c) = (ids[1].peer_id(), ids[2].peer_id());
        for slow in [true, false] {
            let mut eps: Vec<Endpoint<u32>> =
                ids.iter().zip(1u8..).map(|(id, s)| Endpoint::new(id, Config::default(), [s; 32])).collect();
            let mut t = Instant::from_millis(1);
            eps[0].connect(t, 3, c).unwrap();
            eps[1].connect(t, 3, c).unwrap();
            route(&mut eps, &mut t, 100, None);
            eps[0].connect(t, 2, b).unwrap();
            eps[0].connect_via(t, c, b, &[]).unwrap();
            route(&mut eps, &mut t, 1000, None);
            assert_eq!(eps[0].path(&b), Some(&Path::Direct(2)));
            if slow {
                eps[0].set_slow(&c, Some(3));
            }
            route_with(&mut eps, &mut t, 16_000, |i, dst| i == 1 && dst == 1);
            let fell_back = eps[0].path(&b) == Some(&Path::Via(c));
            assert_eq!((fell_back, eps[0].stats().direct_fallbacks), (!slow, !slow as u64), "c slow: {slow}");
        }
    }
}
