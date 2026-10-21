//! `peerwire-godot`: a Godot 4 GDExtension that runs Godot's high-level multiplayer
//! (`@rpc`, `MultiplayerSynchronizer`, `MultiplayerSpawner`) over Peerwire, with no
//! server. It registers one class, [`PeerwireMultiplayerPeer`](peer::PeerwireMultiplayerPeer).
//!
//! The protocol logic lives in [`session`], free of Godot types, so `cargo test`
//! covers it without a Godot binary. See `docs/GODOT.md`.

use godot::prelude::*;

pub mod peer;
pub mod session;

struct PeerwireExtension;

#[gdextension]
unsafe impl ExtensionLibrary for PeerwireExtension {}
