use crate::session::{Mode, NetOptions, PeerEvent, PutError, Session, Status};
use godot::classes::multiplayer_peer::{ConnectionStatus, TransferMode};
use godot::classes::{IMultiplayerPeerExtension, MultiplayerPeerExtension};
use godot::global::Error;
use godot::prelude::*;
use nobaas::{Failure, NatType, Node, PeerId, Privacy};

/// A `MultiplayerPeer` over Nobaas: Godot's `@rpc`, `MultiplayerSynchronizer` and
/// `MultiplayerSpawner` with no server. One instance hosts (peer 1); the others
/// join with its `nb:` invite, or find it by room name. Peers swap addresses over
/// Nostr relays and punch through NATs.
///
/// ```gdscript
/// var peer := NobaasMultiplayerPeer.new()
/// peer.host_room("my-game", "lobby-42")   # or: peer.join_room("my-game", "lobby-42")
/// multiplayer.multiplayer_peer = peer
/// ```
#[derive(GodotClass)]
#[class(base = MultiplayerPeerExtension, tool)]
pub struct NobaasMultiplayerPeer {
    base: Base<MultiplayerPeerExtension>,
    session: Option<Session>,
    target: i32,
    mode: Mode,
    channel: u8,
    refuse: bool,
    net: NetOptions,
}

#[godot_api]
impl NobaasMultiplayerPeer {
    /// STUN servers (`host:port`) asked for the public address, from the next
    /// `host` / `join`. Default: Google's and Cloudflare's. Empty: no STUN.
    #[func]
    fn set_stun_servers(&mut self, servers: PackedStringArray) {
        self.net.stun = Some(strings(&servers));
    }

    /// Nostr relays (`wss://…`, or `ws://…` on a LAN) peers swap addresses through,
    /// from the next `host` / `join`. Default: relay.damus.io and nos.lol. Empty: no
    /// signaling, so no rooms, and invites work only where the host is reachable.
    #[func]
    fn set_signaling_relays(&mut self, relays: PackedStringArray) {
        self.net.relays = Some(strings(&relays));
    }

    /// Every address: the lowest latency, and LAN play.
    #[constant]
    const PRIVACY_DIRECT: i32 = 0;
    /// Public addresses only: no LAN or loopback address in invites and offers.
    #[constant]
    const PRIVACY_PUBLIC_ONLY: i32 = 1;
    /// A joiner gives its public addresses to the host only, and nothing to anyone
    /// else; on a host, the same as `PRIVACY_PUBLIC_ONLY`.
    #[constant]
    const PRIVACY_RELAY_ONLY: i32 = 2;

    /// Which addresses this peer gives out: `PRIVACY_DIRECT` (default),
    /// `PRIVACY_PUBLIC_ONLY` or `PRIVACY_RELAY_ONLY`. Applies to the next `host` /
    /// `join` and to the running session's next invites and offers; links already
    /// up are kept. Players link only with the host, so they never see each other's
    /// addresses in any mode; the host sees every player's public address.
    #[func]
    fn set_privacy_mode(&mut self, mode: i32) {
        self.net.privacy = match mode {
            Self::PRIVACY_PUBLIC_ONLY => Privacy::PublicOnly,
            Self::PRIVACY_RELAY_ONLY => Privacy::RelayOnly { via: None },
            Self::PRIVACY_DIRECT => Privacy::Direct,
            _ => {
                godot_error!("NobaasMultiplayerPeer.set_privacy_mode: unknown mode {mode}");
                return;
            }
        };
        if let Some(s) = self.session.as_mut() {
            s.set_privacy(self.net.privacy);
        }
    }

    /// The mode set with `set_privacy_mode`.
    #[func]
    fn get_privacy_mode(&self) -> i32 {
        match self.net.privacy {
            Privacy::Direct => Self::PRIVACY_DIRECT,
            Privacy::PublicOnly => Self::PRIVACY_PUBLIC_ONLY,
            Privacy::RelayOnly { .. } => Self::PRIVACY_RELAY_ONLY,
        }
    }

    /// Hosts on UDP `port` (0: a free one) and announces this game in room `room` of
    /// application `app_id`, for `join_room`. Only peers that know the room name get
    /// in: while announcing, the host turns away invite joins. Returns the invite, or
    /// `""` if the port can't be bound.
    #[func]
    fn host_room(&mut self, app_id: GString, room: GString, #[opt(default = 0)] port: i32) -> GString {
        let (app, room) = (app_id.to_string(), room.to_string());
        self.start_host(port, |node| Session::host_room(node, &app, &room))
    }

    /// Joins the host announced in room `room` of `app_id`. The peer is connecting
    /// until a host answers and assigns this peer its id; then
    /// `connected_to_server` fires. It waits for a host as long as it takes.
    ///
    /// The room name is the password: only a peer that knows it can be taken for
    /// the host (relays see a hash). Use a long random name for a private game; a
    /// guessable one is a public lobby.
    #[func]
    fn join_room(&mut self, app_id: GString, room: GString) -> Error {
        self.join_room_of(app_id, room, None)
    }

    /// `join_room`, taking only the host whose `get_public_key()` is `host_key`
    /// (32 bytes): another peer in the room can't pose as it.
    #[func]
    fn join_room_host(&mut self, app_id: GString, room: GString, host_key: PackedByteArray) -> Error {
        let Ok(key) = <[u8; 32]>::try_from(host_key.as_slice()) else {
            godot_error!("NobaasMultiplayerPeer.join_room_host: host_key must be 32 bytes");
            return Error::ERR_INVALID_PARAMETER;
        };
        self.join_room_of(app_id, room, Some(PeerId(key)))
    }

    fn join_room_of(&mut self, app_id: GString, room: GString, host: Option<PeerId>) -> Error {
        self.close();
        match self.net.bind("0.0.0.0:0") {
            Ok(node) => {
                self.install(Session::join_room(node, &app_id.to_string(), &room.to_string(), host));
                Error::OK
            }
            Err(e) => {
                godot_error!("NobaasMultiplayerPeer.join_room: {e}");
                Error::ERR_CANT_CREATE
            }
        }
    }

    /// The public `ip:port` STUN reported for this peer, or `""` until a server answers.
    #[func]
    fn get_public_address(&self) -> GString {
        match self.session.as_ref().and_then(Session::public_address) {
            Some(a) => GString::from(a.to_string().as_str()),
            None => GString::new(),
        }
    }

    /// `"open"` (no NAT), `"cone"` (one public port for every peer: punching usually
    /// works, but not against a symmetric NAT if this router filters by port, which
    /// STUN can't see), `"symmetric"` (a new port per peer: punching works towards
    /// open and most cone peers, never towards another symmetric one) or `"unknown"`
    /// (no STUN answer from two servers yet).
    #[func]
    fn get_nat_type(&self) -> GString {
        let nat = self.session.as_ref().map_or(NatType::Unknown, Session::nat_type);
        GString::from(match nat {
            NatType::Unknown => "unknown",
            NatType::Open => "open",
            NatType::Cone => "cone",
            NatType::Symmetric => "symmetric",
        })
    }

    /// After `connection_failed`: `"symmetric"` if one side's NAT is symmetric and
    /// the other's router filters by port, so punching can't work (no TURN relay,
    /// by design), `"unreachable"` if the host never answered, or `""`.
    /// `get_connect_error_message()` says which side.
    #[func]
    fn get_connect_error(&self) -> GString {
        GString::from(match self.session.as_ref().and_then(Session::connect_failure) {
            Some(Failure::Symmetric(_)) => "symmetric",
            Some(Failure::Unreachable) => "unreachable",
            None => "",
        })
    }

    /// After `connection_failed`: why, as a sentence to show the player, e.g. "the
    /// peer's NAT is symmetric (...) and this side's router only lets in packets
    /// from ports it has sent to, ...". `""` if nothing failed.
    #[func]
    fn get_connect_error_message(&self) -> GString {
        match self.session.as_ref().and_then(Session::connect_failure) {
            Some(f) => GString::from(f.to_string().as_str()),
            None => GString::new(),
        }
    }

    /// Starts hosting on UDP `port` (0 picks a free one) and returns the invite to
    /// share, or `""` if the port can't be bound.
    #[func]
    fn host(&mut self, #[opt(default = 0)] port: i32) -> GString {
        self.start_host(port, Session::host)
    }

    /// Joins the host that made `invite`. The peer is connecting until the host
    /// assigns this peer its id; then `connected_to_server` fires.
    #[func]
    fn join(&mut self, invite: GString) -> Error {
        self.close();
        let node = match self.net.bind("0.0.0.0:0") {
            Ok(n) => n,
            Err(e) => {
                godot_error!("NobaasMultiplayerPeer.join: {e}");
                return Error::ERR_CANT_CREATE;
            }
        };
        match Session::join(node, &invite.to_string()) {
            Ok(s) => {
                self.install(s);
                Error::OK
            }
            Err(e) => {
                godot_error!("NobaasMultiplayerPeer.join: {e:?}");
                Error::ERR_INVALID_PARAMETER
            }
        }
    }

    /// This peer's invite: the host's is what joiners need.
    #[func]
    fn get_invite(&self) -> GString {
        self.session.as_ref().map(|s| GString::from(s.invite().as_str())).unwrap_or_default()
    }

    /// The 32-byte Ed25519 public key that authenticated peer `id`'s link, or an
    /// empty array if `id` is not directly connected (on a joiner, only the host
    /// is). Godot ids are short labels anyone can aim at; authorize on this key.
    #[func]
    fn get_peer_key(&self, id: i32) -> PackedByteArray {
        match self.session.as_ref().and_then(|s| s.peer_key(id)) {
            Some(k) => PackedByteArray::from(&k.as_bytes()[..]),
            None => PackedByteArray::new(),
        }
    }

    /// This peer's own 32-byte public key (empty before `host()` / `join()`).
    #[func]
    fn get_public_key(&self) -> PackedByteArray {
        match self.session.as_ref() {
            Some(s) => PackedByteArray::from(&s.public_key().as_bytes()[..]),
            None => PackedByteArray::new(),
        }
    }

    /// Smoothed round-trip time to peer `id` in milliseconds, or -1 if unknown.
    #[func]
    fn get_peer_rtt_ms(&self, id: i32) -> f64 {
        self.stats(id).and_then(|s| s.rtt).map_or(-1.0, |d| d.as_secs_f64() * 1000.0)
    }

    /// Estimated packet loss to peer `id`, 0 to 1 (reliable traffic only), or -1 if unknown.
    #[func]
    fn get_peer_loss(&self, id: i32) -> f64 {
        self.stats(id).map_or(-1.0, |s| f64::from(s.loss()))
    }
}

impl NobaasMultiplayerPeer {
    fn start_host(&mut self, port: i32, make: impl FnOnce(Node) -> Session) -> GString {
        self.close();
        match self.net.bind(&format!("0.0.0.0:{}", port.clamp(0, 65535))) {
            Ok(node) => {
                let s = make(node);
                let invite = s.invite();
                self.install(s);
                GString::from(invite.as_str())
            }
            Err(e) => {
                godot_error!("NobaasMultiplayerPeer.host: {e}");
                GString::new()
            }
        }
    }

    fn install(&mut self, mut s: Session) {
        s.target = self.target;
        s.mode = self.mode;
        s.channel = self.channel;
        s.set_refuse_new_connections(self.refuse);
        self.session = Some(s);
    }

    fn stats(&self, id: i32) -> Option<nobaas::PeerStats> {
        self.session.as_ref()?.peer_stats(id)
    }
}

fn strings(a: &PackedStringArray) -> Vec<String> {
    a.as_slice().iter().map(GString::to_string).collect()
}

fn to_godot(mode: Mode) -> TransferMode {
    match mode {
        Mode::Unreliable => TransferMode::UNRELIABLE,
        Mode::UnreliableOrdered => TransferMode::UNRELIABLE_ORDERED,
        Mode::Reliable => TransferMode::RELIABLE,
    }
}

fn from_godot(mode: TransferMode) -> Mode {
    if mode == TransferMode::UNRELIABLE {
        Mode::Unreliable
    } else if mode == TransferMode::UNRELIABLE_ORDERED {
        Mode::UnreliableOrdered
    } else {
        Mode::Reliable
    }
}

#[godot_api]
impl IMultiplayerPeerExtension for NobaasMultiplayerPeer {
    fn init(base: Base<MultiplayerPeerExtension>) -> Self {
        NobaasMultiplayerPeer {
            base,
            session: None,
            target: 0,
            mode: Mode::Reliable,
            channel: 0,
            refuse: false,
            net: NetOptions::default(),
        }
    }

    fn get_available_packet_count(&self) -> i32 {
        self.session.as_ref().map_or(0, |s| s.available() as i32)
    }

    fn get_max_packet_size(&self) -> i32 {
        self.mode.max_payload() as i32
    }

    fn get_packet_script(&mut self) -> PackedByteArray {
        match self.session.as_mut().and_then(Session::pop) {
            Some(p) => PackedByteArray::from(p.data.as_slice()),
            None => PackedByteArray::new(),
        }
    }

    fn put_packet_script(&mut self, buffer: PackedByteArray) -> Error {
        let Some(s) = self.session.as_mut() else { return Error::ERR_UNCONFIGURED };
        match s.put(buffer.as_slice()) {
            Ok(()) => Error::OK,
            Err(PutError::Unavailable) => Error::ERR_UNAVAILABLE,
            Err(PutError::TooLarge) => Error::ERR_OUT_OF_MEMORY,
            Err(PutError::Busy) => Error::ERR_BUSY,
        }
    }

    fn get_packet_channel(&self) -> i32 {
        self.session.as_ref().and_then(Session::front).map_or(0, |p| i32::from(p.channel))
    }

    fn get_packet_mode(&self) -> TransferMode {
        to_godot(self.session.as_ref().and_then(Session::front).map_or(Mode::Reliable, |p| p.mode))
    }

    fn set_transfer_channel(&mut self, channel: i32) {
        self.channel = channel.clamp(0, 255) as u8;
        if let Some(s) = self.session.as_mut() {
            s.channel = self.channel;
        }
    }

    fn get_transfer_channel(&self) -> i32 {
        i32::from(self.channel)
    }

    fn set_transfer_mode(&mut self, mode: TransferMode) {
        self.mode = from_godot(mode);
        if let Some(s) = self.session.as_mut() {
            s.mode = self.mode;
        }
    }

    fn get_transfer_mode(&self) -> TransferMode {
        to_godot(self.mode)
    }

    fn set_target_peer(&mut self, peer: i32) {
        self.target = peer;
        if let Some(s) = self.session.as_mut() {
            s.target = peer;
        }
    }

    fn get_packet_peer(&self) -> i32 {
        self.session.as_ref().and_then(Session::front).map_or(0, |p| p.from)
    }

    fn is_server(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_server)
    }

    fn poll(&mut self) {
        let Some(s) = self.session.as_mut() else { return };
        s.poll();
        let events: Vec<PeerEvent> = std::iter::from_fn(|| s.next_event()).collect();
        // Signal handlers call back into this peer; base_mut() allows that.
        for e in events {
            let (signal, id) = match e {
                PeerEvent::Connected(id) => ("peer_connected", id),
                PeerEvent::Disconnected(id) => ("peer_disconnected", id),
            };
            self.base_mut().emit_signal(signal, &[i64::from(id).to_variant()]);
        }
    }

    fn close(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.close();
        }
    }

    fn disconnect_peer(&mut self, peer: i32, _force: bool) {
        let Some(s) = self.session.as_mut() else { return };
        s.disconnect_peer(peer);
        let events: Vec<PeerEvent> = std::iter::from_fn(|| s.next_event()).collect();
        for e in events {
            if let PeerEvent::Disconnected(id) = e {
                self.base_mut().emit_signal("peer_disconnected", &[i64::from(id).to_variant()]);
            }
        }
    }

    fn get_unique_id(&self) -> i32 {
        self.session.as_ref().map_or(0, Session::unique_id)
    }

    fn set_refuse_new_connections(&mut self, enable: bool) {
        self.refuse = enable;
        if let Some(s) = self.session.as_mut() {
            s.set_refuse_new_connections(enable);
        }
    }

    fn is_refusing_new_connections(&self) -> bool {
        self.refuse
    }

    fn is_server_relay_supported(&self) -> bool {
        true
    }

    fn get_connection_status(&self) -> ConnectionStatus {
        match self.session.as_ref().map_or(Status::Disconnected, Session::status) {
            Status::Disconnected => ConnectionStatus::DISCONNECTED,
            Status::Connecting => ConnectionStatus::CONNECTING,
            Status::Connected => ConnectionStatus::CONNECTED,
        }
    }
}
