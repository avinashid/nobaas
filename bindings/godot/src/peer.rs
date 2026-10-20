use crate::session::{Mode, PeerEvent, PutError, Session, Status};
use godot::classes::multiplayer_peer::{ConnectionStatus, TransferMode};
use godot::classes::{IMultiplayerPeerExtension, MultiplayerPeerExtension};
use godot::global::Error;
use godot::prelude::*;

/// A `MultiplayerPeer` over Nobaas: Godot's `@rpc`, `MultiplayerSynchronizer` and
/// `MultiplayerSpawner` with no server. One instance hosts (peer 1) and hands out
/// an `nb:` invite; the others join with it.
///
/// ```gdscript
/// var peer := NobaasMultiplayerPeer.new()
/// var invite := peer.host()          # or: peer.join(invite)
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
}

#[godot_api]
impl NobaasMultiplayerPeer {
    /// Starts hosting on UDP `port` (0 picks a free one) and returns the invite to
    /// share, or `""` if the port can't be bound.
    #[func]
    fn host(&mut self, #[opt(default = 0)] port: i32) -> GString {
        self.close();
        match Session::host(&format!("0.0.0.0:{}", port.clamp(0, 65535))) {
            Ok(s) => {
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

    /// Joins the host that made `invite`. The peer is connecting until the host
    /// assigns this peer its id; then `connected_to_server` fires.
    #[func]
    fn join(&mut self, invite: GString) -> Error {
        self.close();
        match Session::join("0.0.0.0:0", &invite.to_string()) {
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
        NobaasMultiplayerPeer { base, session: None, target: 0, mode: Mode::Reliable, channel: 0, refuse: false }
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
