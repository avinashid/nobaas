//! `nobaas-godot`: a Godot 4 GDExtension that runs Godot's high-level multiplayer
//! (`@rpc`, `MultiplayerSynchronizer`, `MultiplayerSpawner`) over Nobaas, with no
//! server. It registers one class, [`NobaasMultiplayerPeer`](peer::NobaasMultiplayerPeer).
//!
//! The protocol logic lives in [`session`], free of Godot types, so `cargo test`
//! covers it without a Godot binary. See `docs/GODOT.md`.

use godot::prelude::*;

pub mod peer;
pub mod session;

struct NobaasExtension;

#[gdextension]
unsafe impl ExtensionLibrary for NobaasExtension {}
