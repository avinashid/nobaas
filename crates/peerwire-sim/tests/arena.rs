//! Small arena runs as regression tests; `examples/arena.rs` is the full harness.

#[path = "../examples/arena.rs"]
#[allow(dead_code)]
mod arena;

use arena::{run, Args};

#[test]
fn lockstep_islands_agree_over_super_peers() {
    let s = run(Args { players: 96, supers: 2, secs: 3, ..Args::default() });
    assert_eq!(s.agree, s.islands, "{s:?}");
    assert_eq!(s.stalled, 0, "{s:?}");
    assert!(s.lag_p99_ms < 120.0, "{s:?}");
}

#[test]
fn bundles_survive_loss_and_jitter() {
    // Resending each member's latest input in every bundle keeps islands
    // confirming at 5% loss with jitter longer than a tick.
    let s = run(Args { players: 96, supers: 2, secs: 3, loss: 0.05, jitter_ms: 30, ..Args::default() });
    assert_eq!((s.agree, s.stalled), (s.islands, 0), "{s:?}");
}

#[test]
fn islands_survive_a_super_peer_dying() {
    // Super-peer 1 goes silent at 2 s: its islands move to their next-ranked
    // super-peers (hot standby links) and lockstep carries on.
    let s = run(Args { players: 144, supers: 3, secs: 4, kill: Some(1), kill_at: 2, ..Args::default() });
    assert_eq!((s.agree, s.stalled), (s.islands, 0), "{s:?}");
}

#[test]
fn overloaded_super_peer_degrades_without_stalling() {
    // 128 players per super-peer on a 10 Mbit/s uplink with a 250 ms buffer:
    // overload control and NACK repair keep every island confirming.
    let s = run(Args { players: 256, supers: 2, secs: 4, super_up_mbps: 10, ..Args::default() });
    assert_eq!((s.agree, s.stalled), (s.islands, 0), "{s:?}");
    assert!(s.lag_p50_ms < 150.0, "{s:?}");
}
