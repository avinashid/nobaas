//! Godot's high-level multiplayer model on a Nobaas [`Node`], with no Godot types,
//! so all of it runs under plain `cargo test`.
//!
//! The topology is a star, like Godot's ENet peer: the host is peer 1 and every
//! joiner holds one link, to the host. Joiners reach each other through
//! SceneMultiplayer's server relay. The host gives each joiner its Godot id in a
//! control frame right after the Nobaas handshake.
//!
//! A joiner finds its host by invite ([`Session::join`]) or by room
//! ([`Session::join_room`]): the host announces itself in the room as a
//! [`Role::Host`] and joiners as [`Role::Client`], so joiners link to the host
//! only. Either way, with signaling the two dial each other at once and punch
//! through NATs (see `nobaas::traversal`).
//!
//! Every Godot packet travels in one Nobaas message:
//!
//! ```text
//! data:    0x01 mode(u8) channel(u8) [seq(u32 LE), unreliable-ordered only] payload
//! control: 0x00 op(u8) ...            op 1 = ASSIGN id(i32 LE), host -> joiner
//! ```

use nobaas::{
    Event, Failure, Invite, Lane, NatType, Node, NostrSignaling, PeerId, PeerStats, Privacy, Role, SendError,
    MAX_UNRELIABLE,
};
use std::net::SocketAddr;
use std::collections::{HashMap, VecDeque};
use std::io;

/// The host's Godot peer id.
pub const HOST_ID: i32 = 1;

const TAG_CONTROL: u8 = 0;
const TAG_DATA: u8 = 1;
const OP_ASSIGN: u8 = 1;
/// Largest reliable message: `nobaas::Config::max_message` by default.
const MAX_RELIABLE: usize = 1 << 20;
/// Packets waiting for Godot; past this the oldest are dropped.
const MAX_INBOX: usize = 4096;

/// Godot's `MultiplayerPeer.TransferMode`, in the same order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Unreliable = 0,
    UnreliableOrdered = 1,
    Reliable = 2,
}

impl Mode {
    /// The Nobaas lane a mode travels on. Unreliable-ordered is the unreliable
    /// lane plus a sequence number: the receiver drops anything older than what
    /// it has already delivered.
    pub fn lane(self) -> Lane {
        match self {
            Mode::Reliable => Lane::Reliable,
            Mode::Unreliable | Mode::UnreliableOrdered => Lane::Unreliable,
        }
    }

    fn from_u8(b: u8) -> Option<Mode> {
        match b {
            0 => Some(Mode::Unreliable),
            1 => Some(Mode::UnreliableOrdered),
            2 => Some(Mode::Reliable),
            _ => None,
        }
    }

    fn header_len(self) -> usize {
        if self == Mode::UnreliableOrdered {
            7
        } else {
            3
        }
    }

    /// Largest payload a Godot packet can carry in this mode.
    pub fn max_payload(self) -> usize {
        let lane_max = match self {
            Mode::Reliable => MAX_RELIABLE,
            Mode::Unreliable | Mode::UnreliableOrdered => MAX_UNRELIABLE,
        };
        lane_max - self.header_len()
    }
}

/// Godot's `MultiplayerPeer.ConnectionStatus`, in the same order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Disconnected = 0,
    Connecting = 1,
    Connected = 2,
}

/// A packet waiting for Godot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub from: i32,
    pub channel: u8,
    pub mode: Mode,
    pub data: Vec<u8>,
}

/// What Godot's `peer_connected` / `peer_disconnected` signals report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerEvent {
    Connected(i32),
    Disconnected(i32),
}

/// Why a packet was not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutError {
    /// Not connected, or the target id is unknown.
    Unavailable,
    /// Larger than [`Mode::max_payload`].
    TooLarge,
    /// The reliable send buffer is full; try again after a poll.
    Busy,
}

/// The Godot id for `peer`. Attempt 0 is the stable id; the host moves to
/// attempt 1, 2, ... only when another peer already holds that id. Always in
/// `2..=i32::MAX`.
pub fn derive_id(peer: &PeerId, attempt: u32) -> i32 {
    let b = peer.as_bytes();
    let mut h = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) ^ u64::from(attempt);
    // splitmix64 finaliser: one attempt's id says nothing about the next.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    let id = (h as u32 & 0x7fff_ffff) as i32;
    if id < 2 {
        id + 2
    } else {
        id
    }
}

/// Godot ids and Nobaas peers, one to one.
#[derive(Default)]
pub struct IdMap {
    by_id: HashMap<i32, PeerId>,
    by_peer: HashMap<PeerId, i32>,
}

impl IdMap {
    /// Host side: the peer's id, assigning a free one (stable while it stays free).
    pub fn assign(&mut self, peer: PeerId) -> i32 {
        if let Some(&id) = self.by_peer.get(&peer) {
            return id;
        }
        let id = (0..).map(|a| derive_id(&peer, a)).find(|id| !self.by_id.contains_key(id)).unwrap();
        self.insert(peer, id);
        id
    }

    pub fn insert(&mut self, peer: PeerId, id: i32) {
        self.by_id.insert(id, peer);
        self.by_peer.insert(peer, id);
    }

    pub fn remove(&mut self, peer: &PeerId) -> Option<i32> {
        let id = self.by_peer.remove(peer)?;
        self.by_id.remove(&id);
        Some(id)
    }

    pub fn id(&self, peer: &PeerId) -> Option<i32> {
        self.by_peer.get(peer).copied()
    }

    pub fn peer(&self, id: i32) -> Option<PeerId> {
        self.by_id.get(&id).copied()
    }

    pub fn ids(&self) -> impl Iterator<Item = i32> + '_ {
        self.by_id.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn clear(&mut self) {
        self.by_id.clear();
        self.by_peer.clear();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Frame<'a> {
    Assign(i32),
    Data { mode: Mode, channel: u8, seq: u32, payload: &'a [u8] },
}

fn encode_data(mode: Mode, channel: u8, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(mode.header_len() + payload.len());
    v.extend_from_slice(&[TAG_DATA, mode as u8, channel]);
    if mode == Mode::UnreliableOrdered {
        v.extend_from_slice(&seq.to_le_bytes());
    }
    v.extend_from_slice(payload);
    v
}

fn encode_assign(id: i32) -> Vec<u8> {
    let mut v = vec![TAG_CONTROL, OP_ASSIGN];
    v.extend_from_slice(&id.to_le_bytes());
    v
}

fn decode(b: &[u8]) -> Option<Frame<'_>> {
    match *b {
        [TAG_CONTROL, OP_ASSIGN, a, b2, c, d] => {
            let id = i32::from_le_bytes([a, b2, c, d]);
            (id >= 2).then_some(Frame::Assign(id))
        }
        [TAG_DATA, mode, channel, ref rest @ ..] => {
            let mode = Mode::from_u8(mode)?;
            let (seq, payload) = if mode == Mode::UnreliableOrdered {
                let (s, p) = rest.split_first_chunk::<4>()?;
                (u32::from_le_bytes(*s), p)
            } else {
                (0, rest)
            };
            Some(Frame::Data { mode, channel, seq, payload })
        }
        _ => None,
    }
}

/// Newest sequence number delivered per (sender, channel), for unreliable-ordered.
#[derive(Default)]
struct Ordered(HashMap<(i32, u8), u32>);

impl Ordered {
    /// `true` if `seq` is newer than everything delivered so far (with wrap-around).
    fn accept(&mut self, from: i32, channel: u8, seq: u32) -> bool {
        match self.0.get(&(from, channel)) {
            Some(&last) if (seq.wrapping_sub(last) as i32) <= 0 => false,
            _ => {
                self.0.insert((from, channel), seq);
                true
            }
        }
    }

    fn forget(&mut self, from: i32) {
        self.0.retain(|&(f, _), _| f != from);
    }
}

/// Network settings for the node behind a [`Session`].
#[derive(Clone, Debug, Default)]
pub struct NetOptions {
    /// STUN servers (`host:port`). `None`: Nobaas's defaults; empty: no STUN.
    pub stun: Option<Vec<String>>,
    /// Nostr relays for signaling (`wss://…`, `ws://…`). `None`: the default public
    /// relays; empty: no signaling (rooms can't work, invites don't punch).
    pub relays: Option<Vec<String>>,
    /// Which addresses the peer gives out (see [`Session::set_privacy`]).
    pub privacy: Privacy,
}

impl NetOptions {
    /// Binds a node with these settings.
    pub fn bind(&self, addr: &str) -> io::Result<Node> {
        let mut b = Node::builder().privacy(self.privacy);
        if let Some(stun) = &self.stun {
            b = b.stun_servers(stun.iter().cloned());
        }
        let relays = match &self.relays {
            Some(r) => r.clone(),
            None => nobaas::nostr::DEFAULT_RELAYS.iter().map(|r| r.to_string()).collect(),
        };
        if !relays.is_empty() {
            b = b.signaling(NostrSignaling::new(relays));
        }
        b.bind(addr)
    }
}

/// One Godot multiplayer peer: a host or a joiner.
pub struct Session {
    node: Node,
    host: bool,
    /// The room this session announces in or looks for a host in: (app id, room).
    room: Option<(String, String)>,
    /// 0 until the host assigns one (joiner).
    my_id: i32,
    /// Joiner: the host it dialled.
    host_peer: Option<PeerId>,
    ids: IdMap,
    status: Status,
    refuse: bool,
    inbox: VecDeque<Packet>,
    events: VecDeque<PeerEvent>,
    out_seq: HashMap<(i32, u8), u32>,
    in_seq: Ordered,
    /// Godot's `set_target_peer`: 0 all, `n > 0` that peer, `n < 0` all but `-n`.
    pub target: i32,
    pub mode: Mode,
    pub channel: u8,
}

impl Session {
    fn new(node: Node, host: bool) -> Session {
        Session {
            node,
            host,
            room: None,
            my_id: if host { HOST_ID } else { 0 },
            host_peer: None,
            ids: IdMap::default(),
            status: if host { Status::Connected } else { Status::Connecting },
            refuse: false,
            inbox: VecDeque::new(),
            events: VecDeque::new(),
            out_seq: HashMap::new(),
            in_seq: Ordered::default(),
            target: 0,
            mode: Mode::Reliable,
            channel: 0,
        }
    }

    /// Hosts on `node`. Read the invite to hand out with [`invite`](Session::invite).
    pub fn host(node: Node) -> Session {
        let mut s = Session::new(node, true);
        s.set_privacy(s.node.privacy());
        s
    }

    /// Joins the host named in `invite`. Under [`Privacy::RelayOnly`] the host is
    /// the relay.
    pub fn join(node: Node, invite: &str) -> Result<Session, JoinError> {
        let inv = Invite::<SocketAddr>::parse(invite).ok_or(JoinError::InvalidInvite)?;
        let mut s = Session::new(node, false);
        s.host_peer = Some(inv.peer);
        s.set_privacy(s.node.privacy());
        s.node.connect(invite).map_err(|_| JoinError::InvalidInvite)?;
        Ok(s)
    }

    /// Which addresses this peer gives out, from the next offer and invite on;
    /// links already up are kept. Clients link only with the host, so other
    /// players never see their addresses whatever the mode. Under
    /// [`Privacy::RelayOnly`] a joiner gives only its host its public addresses,
    /// and a host, which every client links to directly, gives out its public
    /// addresses only (as [`Privacy::PublicOnly`]).
    pub fn set_privacy(&mut self, privacy: Privacy) {
        let p = match privacy {
            Privacy::RelayOnly { .. } if self.host => Privacy::PublicOnly,
            Privacy::RelayOnly { via: None } => Privacy::RelayOnly { via: self.host_peer },
            p => p,
        };
        self.node.set_privacy(p);
    }

    /// Hosts on `node` and announces it in room `room` of application `app_id`, for
    /// [`join_room`](Session::join_room). Only peers that know the room name get in. Needs signaling.
    pub fn host_room(node: Node, app_id: &str, room: &str) -> Session {
        let mut s = Session::new(node, true);
        s.set_privacy(s.node.privacy());
        s.room = Some((app_id.to_string(), room.to_string()));
        s.announce();
        s
    }

    /// Joins whichever host announces itself in the room (or only `host`, when its
    /// key is known): connecting until one answers and assigns this peer its id.
    /// Only a peer that proves it knows the room's name counts as a host. Needs
    /// signaling.
    pub fn join_room(mut node: Node, app_id: &str, room: &str, host: Option<PeerId>) -> Session {
        node.join_room_with(app_id, room, Role::Client, host);
        let mut s = Session::new(node, false);
        s.room = Some((app_id.to_string(), room.to_string()));
        s
    }

    fn announce(&mut self) {
        if let Some((app, room)) = &self.room {
            self.node.join_room_as(app, room, Role::Host);
        }
    }

    /// This peer's `nb:` invite.
    pub fn invite(&self) -> String {
        self.node.invite()
    }

    pub fn is_server(&self) -> bool {
        self.host
    }

    /// 1 for the host; a joiner's id once the host has assigned it. Until then,
    /// the stable id derived from this peer's key (what the host assigns unless
    /// it collides), since Godot expects an id in `1..=i32::MAX` at all times.
    pub fn unique_id(&self) -> i32 {
        if self.my_id != 0 {
            self.my_id
        } else {
            derive_id(&self.node.peer_id(), 0)
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// A refusing host also stops announcing itself in its room.
    pub fn set_refuse_new_connections(&mut self, refuse: bool) {
        if self.host && refuse != self.refuse {
            if refuse {
                self.node.leave_room();
            } else {
                self.announce();
            }
        }
        self.refuse = refuse;
    }

    pub fn is_refusing_new_connections(&self) -> bool {
        self.refuse
    }

    /// Godot ids of the connected peers.
    pub fn peer_ids(&self) -> Vec<i32> {
        let mut v: Vec<i32> = self.ids.ids().collect();
        v.sort_unstable();
        v
    }

    /// The authenticated Nobaas identity (Ed25519 public key) behind Godot peer `id`.
    /// Godot ids are short labels; authorize on this key, never on the id.
    pub fn peer_key(&self, id: i32) -> Option<PeerId> {
        self.ids.peer(id)
    }

    /// This peer's own Nobaas identity.
    pub fn public_key(&self) -> PeerId {
        self.node.peer_id()
    }

    /// The public address STUN reported for this peer's socket.
    pub fn public_address(&self) -> Option<SocketAddr> {
        self.node.public_addr()
    }

    /// What STUN says about this peer's NAT.
    pub fn nat_type(&self) -> NatType {
        self.node.nat_type()
    }

    /// Joiner: why the dial to the host failed, once it has.
    pub fn connect_failure(&self) -> Option<Failure> {
        self.node.failure(&self.host_peer?)
    }

    /// RTT and loss towards a connected Godot peer.
    pub fn peer_stats(&self, id: i32) -> Option<PeerStats> {
        self.node.peer_stats(&self.ids.peer(id)?)
    }

    /// Runs the network: reads, timers, sends, and turns Nobaas events into
    /// packets and peer events. Godot calls it once per frame.
    pub fn poll(&mut self) {
        if self.status == Status::Disconnected {
            return;
        }
        self.node.poll();
        while let Some(e) = self.node.try_recv_event() {
            self.on_event(e);
        }
        // An ASSIGN or a refusal queued above goes out now, not a frame later.
        self.node.poll();
    }

    fn on_event(&mut self, e: Event) {
        match e {
            Event::Connected { peer, .. } => {
                if !self.host {
                    if self.host_peer.is_none() && self.room.is_some() && self.node.room_role(&peer) == Some(Role::Host) {
                        // The first peer to link that proved it hosts this room is ours.
                        self.host_peer = Some(peer);
                        self.node.leave_room();
                    } else if Some(peer) != self.host_peer {
                        self.node.disconnect(&peer); // joiners only talk to the host
                    }
                } else if self.refuse {
                    self.node.disconnect(&peer);
                } else {
                    let id = self.ids.assign(peer);
                    let _ = self.node.send(&peer, Lane::Reliable, &encode_assign(id));
                    self.events.push_back(PeerEvent::Connected(id));
                }
            }
            Event::Disconnected { peer, .. } => self.drop_peer(&peer),
            Event::ConnectFailed { peer } => {
                if Some(peer) == self.host_peer {
                    self.status = Status::Disconnected;
                }
            }
            Event::Message { peer, data, .. } => self.on_message(peer, &data),
            Event::PathChanged { .. } | Event::AddrObserved { .. } => {}
        }
    }

    fn on_message(&mut self, peer: PeerId, data: &[u8]) {
        match decode(data) {
            Some(Frame::Assign(id)) => {
                if !self.host && Some(peer) == self.host_peer && self.my_id == 0 {
                    self.my_id = id;
                    self.status = Status::Connected;
                    self.ids.insert(peer, HOST_ID);
                    self.events.push_back(PeerEvent::Connected(HOST_ID));
                }
            }
            Some(Frame::Data { mode, channel, seq, payload }) => {
                let Some(from) = self.ids.id(&peer) else { return };
                if mode == Mode::UnreliableOrdered && !self.in_seq.accept(from, channel, seq) {
                    return;
                }
                if self.inbox.len() >= MAX_INBOX {
                    self.inbox.pop_front();
                }
                self.inbox.push_back(Packet { from, channel, mode, data: payload.to_vec() });
            }
            None => {}
        }
    }

    fn drop_peer(&mut self, peer: &PeerId) {
        if !self.host && Some(*peer) == self.host_peer {
            self.status = Status::Disconnected; // even if the host never assigned an id
        }
        let Some(id) = self.ids.remove(peer) else { return };
        self.in_seq.forget(id);
        self.out_seq.retain(|&(to, _), _| to != id);
        self.events.push_back(PeerEvent::Disconnected(id));
    }

    /// The next `peer_connected` / `peer_disconnected` to signal.
    pub fn next_event(&mut self) -> Option<PeerEvent> {
        self.events.pop_front()
    }

    /// Packets waiting for Godot.
    pub fn available(&self) -> usize {
        self.inbox.len()
    }

    /// The next packet, without taking it (Godot reads its peer, channel and mode first).
    pub fn front(&self) -> Option<&Packet> {
        self.inbox.front()
    }

    pub fn pop(&mut self) -> Option<Packet> {
        self.inbox.pop_front()
    }

    /// Sends `data` to [`target`](Session::target) with the current mode and channel.
    pub fn put(&mut self, data: &[u8]) -> Result<(), PutError> {
        if self.status != Status::Connected {
            return Err(PutError::Unavailable);
        }
        if data.len() > self.mode.max_payload() {
            return Err(PutError::TooLarge);
        }
        let targets: Vec<i32> = match self.target {
            0 => self.peer_ids(),
            t if t > 0 => {
                if self.ids.peer(t).is_none() {
                    return Err(PutError::Unavailable);
                }
                vec![t]
            }
            t => self.peer_ids().into_iter().filter(|&id| id != -t).collect(),
        };
        let mut result = Ok(());
        for id in targets {
            let Some(peer) = self.ids.peer(id) else { continue };
            let seq = if self.mode == Mode::UnreliableOrdered {
                let s = self.out_seq.entry((id, self.channel)).or_insert(0);
                *s = s.wrapping_add(1);
                *s
            } else {
                0
            };
            let msg = encode_data(self.mode, self.channel, seq, data);
            match self.node.send(&peer, self.mode.lane(), &msg) {
                Ok(()) => {}
                Err(SendError::WouldBlock) => result = Err(PutError::Busy),
                Err(SendError::TooLarge) => result = Err(PutError::TooLarge),
                Err(SendError::NotConnected) => {}
            }
        }
        result
    }

    /// Closes the link to Godot peer `id` (host: a joiner; joiner: 1, the host).
    pub fn disconnect_peer(&mut self, id: i32) {
        let Some(peer) = self.ids.peer(id) else { return };
        self.node.disconnect(&peer);
        self.drop_peer(&peer);
    }

    /// Closes every link. No events follow.
    pub fn close(&mut self) {
        let peers: Vec<PeerId> = self.node.peers().copied().collect();
        for p in &peers {
            self.node.disconnect(p);
        }
        self.ids.clear();
        self.inbox.clear();
        self.events.clear();
        self.status = Status::Disconnected;
    }
}

/// Why [`Session::join`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinError {
    /// Not an `nb:` invite, or one with no usable address.
    InvalidInvite,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nobaas::MemoryHub;
    use std::time::{Duration, Instant};

    fn local() -> Node {
        Node::bind("127.0.0.1:0").unwrap()
    }

    fn peer(first: u8, rest: u8) -> PeerId {
        let mut b = [rest; 32];
        b[..8].copy_from_slice(&[first; 8]);
        PeerId(b)
    }

    #[test]
    fn ids_are_stable_and_never_0_or_1() {
        for i in 0..=255u8 {
            let p = peer(i, i);
            let id = derive_id(&p, 0);
            assert!(id >= 2, "{id}");
            assert_eq!(id, derive_id(&p, 0));
        }
        let mut m = IdMap::default();
        let p = peer(3, 3);
        let id = m.assign(p);
        assert_eq!(id, derive_id(&p, 0));
        assert_eq!(m.assign(p), id, "assigning twice keeps the id");
        assert_eq!(m.peer(id), Some(p));
        assert_eq!(m.remove(&p), Some(id));
        assert!(m.is_empty());
    }

    #[test]
    fn colliding_ids_are_moved() {
        // Same first 8 bytes: the same stable id.
        let (a, b) = (peer(9, 1), peer(9, 2));
        assert_eq!(derive_id(&a, 0), derive_id(&b, 0));
        let mut m = IdMap::default();
        let ia = m.assign(a);
        let ib = m.assign(b);
        assert_ne!(ia, ib);
        assert_eq!(ib, derive_id(&b, 1));
        assert_eq!((m.id(&a), m.id(&b)), (Some(ia), Some(ib)));
        // Once a leaves, a newcomer with the same hash gets the stable id back.
        m.remove(&a);
        assert_eq!(m.assign(peer(9, 3)), ia);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn modes_map_to_lanes() {
        assert_eq!(Mode::Reliable.lane(), Lane::Reliable);
        assert_eq!(Mode::Unreliable.lane(), Lane::Unreliable);
        assert_eq!(Mode::UnreliableOrdered.lane(), Lane::Unreliable);
        assert_eq!(Mode::Unreliable.max_payload(), MAX_UNRELIABLE - 3);
        assert_eq!(Mode::UnreliableOrdered.max_payload(), MAX_UNRELIABLE - 7);
        for m in [Mode::Unreliable, Mode::UnreliableOrdered, Mode::Reliable] {
            assert_eq!(Mode::from_u8(m as u8), Some(m));
        }
    }

    #[test]
    fn frames_round_trip_and_garbage_is_dropped() {
        for mode in [Mode::Unreliable, Mode::UnreliableOrdered, Mode::Reliable] {
            let b = encode_data(mode, 7, 42, b"xyz");
            let seq = if mode == Mode::UnreliableOrdered { 42 } else { 0 };
            assert_eq!(decode(&b), Some(Frame::Data { mode, channel: 7, seq, payload: b"xyz" }));
        }
        assert_eq!(decode(&encode_assign(77)), Some(Frame::Assign(77)));
        assert_eq!(decode(&encode_assign(1)), None, "nobody else may be the host");
        assert_eq!(decode(&encode_assign(-5)), None);
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[TAG_DATA, 9, 0]), None);
        assert_eq!(decode(&[TAG_DATA, 1, 0, 1, 2]), None, "ordered frame without a full seq");
        assert_eq!(decode(&[TAG_CONTROL, OP_ASSIGN, 1]), None);
    }

    #[test]
    fn unreliable_ordered_drops_stale_packets() {
        let mut o = Ordered::default();
        assert!(o.accept(5, 0, 1));
        assert!(o.accept(5, 0, 3));
        assert!(!o.accept(5, 0, 2), "older than the last one delivered");
        assert!(!o.accept(5, 0, 3), "duplicate");
        assert!(o.accept(5, 1, 2), "channels are independent");
        assert!(o.accept(6, 0, 1), "senders are independent");
        assert!(o.accept(5, 9, u32::MAX));
        assert!(o.accept(5, 9, 0), "wraps around");
        assert!(!o.accept(5, 9, u32::MAX), "older across the wrap");
        o.forget(5);
        assert!(o.accept(5, 0, 1));
    }

    fn pump(sessions: &mut [&mut Session], mut done: impl FnMut(&mut [&mut Session]) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            for s in sessions.iter_mut() {
                s.poll();
            }
            if done(sessions) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    fn drain_events(s: &mut Session) -> Vec<PeerEvent> {
        std::iter::from_fn(|| s.next_event()).collect()
    }

    #[test]
    fn host_and_two_joiners_over_loopback() {
        let mut host = Session::host(local());
        let invite = host.invite();
        let mut a = Session::join(local(), &invite).unwrap();
        let mut b = Session::join(local(), &invite).unwrap();
        assert_eq!(a.status(), Status::Connecting);
        let tentative = a.unique_id();
        assert!(tentative >= 2);
        assert!(a.put(b"early").is_err(), "nothing goes out before the id arrives");
        assert!(pump(&mut [&mut host, &mut a, &mut b], |s| {
            s[0].peer_ids().len() == 2 && s[1].status() == Status::Connected && s[2].status() == Status::Connected
        }));
        assert!(host.is_server() && !a.is_server());
        let (ia, ib) = (a.unique_id(), b.unique_id());
        assert_eq!(ia, tentative, "no collision: the assigned id is the derived one");
        assert!(ia >= 2 && ib >= 2 && ia != ib);
        let mut want = vec![ia, ib];
        want.sort_unstable();
        assert_eq!(host.peer_ids(), want);
        let mut got: Vec<_> = drain_events(&mut host);
        got.sort_by_key(|e| match e {
            PeerEvent::Connected(i) | PeerEvent::Disconnected(i) => *i,
        });
        assert_eq!(got, want.iter().map(|&i| PeerEvent::Connected(i)).collect::<Vec<_>>());
        assert_eq!(drain_events(&mut a), vec![PeerEvent::Connected(HOST_ID)]);
        assert_eq!(a.peer_ids(), vec![HOST_ID]);

        // Host broadcast, reliable, channel 3.
        host.channel = 3;
        host.put(b"hello all").unwrap();
        // Host to everyone but b, unreliable-ordered.
        host.mode = Mode::UnreliableOrdered;
        host.target = -ib;
        host.put(b"pos").unwrap();
        // Joiner to the host.
        a.target = HOST_ID;
        a.put(b"from a").unwrap();
        assert!(pump(&mut [&mut host, &mut a, &mut b], |s| {
            s[0].available() == 1 && s[1].available() == 2 && s[2].available() == 1
        }));
        let p = host.front().cloned().unwrap();
        assert_eq!(p, Packet { from: ia, channel: 0, mode: Mode::Reliable, data: b"from a".to_vec() });
        assert_eq!(host.pop(), Some(p));
        assert_eq!(host.pop(), None);
        let mut from_a: Vec<Packet> = std::iter::from_fn(|| a.pop()).collect();
        from_a.sort_by_key(|p| p.mode as u8);
        assert_eq!(from_a[0], Packet { from: HOST_ID, channel: 3, mode: Mode::UnreliableOrdered, data: b"pos".to_vec() });
        assert_eq!(from_a[1], Packet { from: HOST_ID, channel: 3, mode: Mode::Reliable, data: b"hello all".to_vec() });
        assert_eq!(b.pop().unwrap().data, b"hello all");

        // Unknown targets and oversize packets are refused.
        host.target = 12345;
        assert_eq!(host.put(b"x"), Err(PutError::Unavailable));
        host.target = 0;
        assert_eq!(host.put(&vec![0; MAX_UNRELIABLE]), Err(PutError::TooLarge));
        assert!(host.peer_stats(ia).is_some());
        assert_eq!(host.peer_key(ia), Some(a.public_key()), "ids resolve to the authenticated key");
        assert_eq!(a.peer_key(HOST_ID), Some(host.public_key()), "1 is bound to the dialled host");
        assert_eq!(a.peer_key(ib), None, "joiners hold no link to each other");

        // The host kicks b: both sides see it.
        host.disconnect_peer(ib);
        assert_eq!(drain_events(&mut host), vec![PeerEvent::Disconnected(ib)]);
        assert!(pump(&mut [&mut host, &mut a, &mut b], |s| s[2].status() == Status::Disconnected));
        assert_eq!(drain_events(&mut b), vec![PeerEvent::Connected(HOST_ID), PeerEvent::Disconnected(HOST_ID)]);
        assert_eq!(host.peer_ids(), vec![ia]);

        // The host closes: a loses the server.
        host.close();
        assert_eq!(host.status(), Status::Disconnected);
        assert!(pump(&mut [&mut host, &mut a], |s| s[1].status() == Status::Disconnected));
        assert_eq!(drain_events(&mut a), vec![PeerEvent::Disconnected(HOST_ID)]);
    }

    #[test]
    fn refusing_host_turns_joiners_away() {
        let mut host = Session::host(local());
        host.set_refuse_new_connections(true);
        assert!(host.is_refusing_new_connections());
        let mut a = Session::join(local(), &host.invite()).unwrap();
        assert!(pump(&mut [&mut host, &mut a], |s| s[1].status() == Status::Disconnected));
        assert!(host.peer_ids().is_empty());
        assert_eq!(drain_events(&mut host), vec![]);
        assert_eq!(a.peer_ids(), Vec::<i32>::new());
    }

    /// NB-034: a host gives out public addresses only (RELAY_ONLY acts as
    /// PUBLIC_ONLY on it); a relay-only joiner takes its host as the relay, links
    /// with it directly, and plays as usual.
    #[test]
    fn privacy_modes_for_hosts_and_joiners() {
        let quiet = |p| Node::builder().stun_servers(Vec::<String>::new()).privacy(p).bind("0.0.0.0:0").unwrap();
        let addrs = |s: &Session| Invite::<SocketAddr>::parse(&s.invite()).unwrap().addrs;
        let mut host = Session::host(quiet(Privacy::RelayOnly { via: None }));
        assert_eq!((host.node.privacy(), addrs(&host)), (Privacy::PublicOnly, vec![]), "no LAN or loopback address");
        host.set_privacy(Privacy::Direct);
        assert!(addrs(&host).iter().any(|a| a.ip().is_loopback()));
        let invite = host.invite();
        let relay_only = Node::builder().privacy(Privacy::RelayOnly { via: None }).bind("127.0.0.1:0").unwrap();
        let mut a = Session::join(relay_only, &invite).unwrap();
        assert_eq!(a.node.privacy(), Privacy::RelayOnly { via: Some(host.public_key()) });
        assert!(pump(&mut [&mut host, &mut a], |s| s[1].status() == Status::Connected));
        assert!(matches!(a.node.path(&host.public_key()), Some(nobaas::Path::Direct(_))));
        assert_eq!(addrs(&a), vec![], "a relay-only joiner's invite names no address");
        a.target = HOST_ID;
        a.put(b"hi").unwrap();
        assert!(pump(&mut [&mut host, &mut a], |s| s[0].available() == 1));
    }

    #[test]
    fn bad_invite_is_refused() {
        assert_eq!(Session::join(local(), "nb:nope").err(), Some(JoinError::InvalidInvite));
    }

    #[test]
    fn room_clients_find_the_host_and_only_the_host() {
        let hub = MemoryHub::new();
        let node = || Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
        let mut host = Session::host_room(node(), "nb-test", "godot-room");
        let mut a = Session::join_room(node(), "nb-test", "godot-room", None);
        let mut b = Session::join_room(node(), "nb-test", "godot-room", None);
        let mut stranger = Session::join_room(node(), "nb-test", "another room", None);
        assert_eq!(a.status(), Status::Connecting);
        assert!(pump(&mut [&mut host, &mut a, &mut b, &mut stranger], |s| {
            s[0].peer_ids().len() == 2 && s[1].status() == Status::Connected && s[2].status() == Status::Connected
        }));
        assert_eq!(a.peer_key(HOST_ID), Some(host.public_key()));
        assert_eq!((a.peer_ids(), b.peer_ids()), (vec![HOST_ID], vec![HOST_ID]));
        assert_eq!(stranger.status(), Status::Connecting);
        a.target = HOST_ID;
        a.put(b"hi").unwrap();
        assert!(pump(&mut [&mut host, &mut a, &mut b], |s| s[0].available() == 1));
        assert_eq!(host.pop().unwrap().from, a.unique_id());

        // A refusing host is not announced: a newcomer finds nobody.
        host.set_refuse_new_connections(true);
        let mut late = Session::join_room(node(), "nb-test", "godot-room", None);
        let found = pump(&mut [&mut host, &mut late], |s| s[1].status() == Status::Connected);
        assert!(!found);
    }

    /// NB-031: a peer that knows the room topic (what relays see) but not its name
    /// can't pose as the host, by hello or by dialling the client straight; with a
    /// host key, the client takes that host only.
    #[test]
    fn room_clients_take_only_a_proven_host() {
        use nobaas::{Signaling, Topic};
        let hub = MemoryHub::new();
        let node = || Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
        let mut a = Session::join_room(node(), "nb-test", "secret-room", None);
        let mut impostor = node();
        let mut side = hub.connect();
        let hex: String = impostor.peer_id().as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let forged = format!(r#"{{"t":"hello","from":"{hex}","meta":{{"nb":1,"role":"host","p":"{}"}}}}"#, "00".repeat(32));
        side.publish(Topic::room("nb-test", "secret-room"), forged.as_bytes());
        impostor.connect(&a.invite()).unwrap();
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end {
            a.poll();
            impostor.poll();
            while impostor.try_recv_event().is_some() {}
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!((a.status(), a.peer_ids()), (Status::Connecting, vec![]));

        // The real host: found. Another host, when the client names one: ignored.
        let mut host = Session::host_room(node(), "nb-test", "secret-room");
        assert!(pump(&mut [&mut host, &mut a], |s| s[1].status() == Status::Connected));
        assert_eq!(a.peer_key(HOST_ID), Some(host.public_key()));
        let mut other = Session::host_room(node(), "nb-test", "secret-room");
        let mut c = Session::join_room(node(), "nb-test", "secret-room", Some(other.public_key()));
        assert!(pump(&mut [&mut host, &mut other, &mut c], |s| s[2].status() == Status::Connected));
        assert_eq!(c.peer_key(HOST_ID), Some(other.public_key()));
        assert_eq!(host.peer_ids().len(), 1, "only a");
    }
}
