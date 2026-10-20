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

mod node;

pub use node::{Error, Node, Status};
pub use nobaas_core::{
    Config, ConnectError, DisconnectReason, Identity, Lane, PeerId, PeerStats, SendError, MAX_UNRELIABLE,
};

/// An event from a [`Node`]: a peer connected or left, a message arrived, a link moved.
pub type Event = nobaas_core::Event<std::net::SocketAddr>;

/// How a link reaches its peer: a UDP address, or through a relaying peer.
pub type Path = nobaas_core::Path<std::net::SocketAddr>;
