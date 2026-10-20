//! `nobaas-core`: sans-IO peer-to-peer session core.
//!
//! The [`Endpoint`] never touches sockets, threads or clocks. The host feeds it
//! datagrams and the current time, then drains outgoing datagrams, events and the
//! next timer deadline. The same code can run under tokio, a browser event loop, a
//! Flutter isolate, a game engine's frame tick, or the deterministic simulator in
//! `nobaas-sim`.
//!
//! ```text
//! host loop:
//!   endpoint.handle_datagram(now, from, bytes)   // for every datagram received
//!   endpoint.handle_timeout(now)                 // when poll_timeout() has passed
//!   while let Some(t) = endpoint.poll_transmit(now) { socket.send_to(t.payload, t.dst) }
//!   while let Some(e) = endpoint.poll_event() { app.on(e) }
//!   sleep until endpoint.poll_timeout() or the next datagram
//! ```
//!
//! Links are authenticated and encrypted with `Noise_IK_25519_ChaChaPoly_BLAKE2s`.
//! The Noise static key is the X25519 form of the peer's Ed25519 identity, so a
//! [`PeerId`] is all a dialer needs, and a completed handshake proves the remote
//! holds that identity's secret key.

mod addr;
mod crypto;
mod endpoint;
mod identity;
mod invite;
mod reliable;
pub mod sealed;
pub mod stun;
mod time;
mod wire;

pub use addr::{Address, Path, MAX_ADDR_LEN};
pub use endpoint::{
    Config, ConnectError, DisconnectReason, Endpoint, Event, Lane, PeerStats, SendError, Stats, Transmit,
};
pub use identity::{Identity, PeerId};
pub use invite::Invite;
pub use time::Instant;
pub use wire::{MAX_DATAGRAM, MAX_UNRELIABLE};

// The README examples run as doc tests (`cargo test --doc`).
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
