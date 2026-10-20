//! `nobaas`: peer-to-peer nodes over UDP, with no server.
//!
//! A [`Node`] owns one non-blocking UDP socket and one identity, and drives a
//! [`nobaas_core::Endpoint`] from it. It never blocks and never spawns threads:
//! call [`Node::poll`] from your loop (a game's frame tick, a timer, an event
//! loop) and it drains the socket, fires timers, sends what is queued and
//! queues events for [`Node::try_recv_event`].
//!
//! ```no_run
//! use nobaas::{Event, Lane, Node};
//!
//! let mut host = Node::bind("0.0.0.0:0")?;
//! let invite = host.invite(); // hand this "nb:…" string to the other peer
//! # let mut guest = Node::bind("0.0.0.0:0")?;
//! let peer = guest.connect(&invite)?;
//! loop {
//!     guest.poll();
//!     while let Some(e) = guest.try_recv_event() {
//!         if let Event::Connected { peer, .. } = e {
//!             guest.send(&peer, Lane::Reliable, b"hello")?;
//!         }
//!     }
//!     # break;
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Through NATs
//!
//! A node asks STUN servers for its public address ([`Node::public_addr`],
//! [`Node::nat_type`]) and puts it in its invites. Give it a [`Signaling`] carrier
//! ([`Builder::signaling`]) and peers swap addresses over it, so both dial at once
//! and punch through their NATs: [`Node::join_room`] finds and connects the peers
//! of a room, and [`Node::connect`] punches too. Where two symmetric NATs can't
//! punch, a peer connected to both relays. See [`traversal`]. The `nostr` feature
//! adds `NostrSignaling`, over public Nostr relays.
//!
//! # Privacy
//!
//! Directly linked peers see each other's IP address. [`Privacy`] ([`Builder::privacy`],
//! [`Node::set_privacy`]) picks what a node gives out besides: every address, public
//! ones only (no LAN or loopback), or none, linking only through a relay
//! ([`Privacy::RelayOnly`]). Invites carry a key, and a node dials back only peers
//! that hold its invite, share its room, or that it dialled: a PeerId alone gets
//! nothing.
//!
//! With the `tokio` feature, [`AsyncNode`] wraps a `Node` and waits on the socket
//! and timers itself: `node.next_event().await`.

#[cfg(feature = "tokio")]
mod async_node;
mod node;
#[cfg(feature = "nostr")]
pub mod nostr;
mod signaling;
pub mod traversal;

#[cfg(feature = "tokio")]
pub use async_node::AsyncNode;
pub use node::{Builder, Error, Node, Status, DEFAULT_STUN_SERVERS};
#[cfg(feature = "nostr")]
pub use nostr::NostrSignaling;
pub use signaling::{MemoryHub, MemorySignaling, Signaling, Topic};
pub use traversal::{Failure, NatType, Privacy, Role};
pub use nobaas_core::{
    Config, ConnectError, DisconnectReason, Identity, Invite, Lane, PeerId, PeerStats, SendError, MAX_UNRELIABLE,
};

/// An event from a [`Node`]: a peer connected or left, a message arrived, a link moved.
pub type Event = nobaas_core::Event<std::net::SocketAddr>;

/// How a link reaches its peer: a UDP address, or through a relaying peer.
pub type Path = nobaas_core::Path<std::net::SocketAddr>;
