//! Small bulk-transfer runs as regression tests; `examples/bulk.rs` is the full benchmark.

#[path = "../examples/bulk.rs"]
#[allow(dead_code)]
mod bulk;

use bulk::{run, Args};
use peerwire_core::{Config, Event, Lane};
use peerwire_sim::{NetConfig, Sim};
use std::time::Duration;

fn small() -> Args {
    Args { mib: 4, ..Args::default() }
}

#[test]
fn bulk_transfer_completes_on_a_long_fat_link() {
    let s = run(small());
    assert!(s.done && s.dropped.is_none(), "{s:?}");
    assert_eq!(s.retransmits, 0, "{s:?}");
    // Slow start reaches the link window (1024 fragments) within the transfer;
    // inflight counts window slots, so urgent fragments can add one.
    assert!((1024..=1026).contains(&s.peak_inflight), "{s:?}");
    assert!(s.peak_cwnd >= s.peak_inflight, "{s:?}");
    assert!(s.mb_per_sec > 1.0, "{s:?}");
    assert!(s.urgent_delivered > 0 && s.urgent_p50_ms >= 50.0, "{s:?}");
    assert_eq!(s.sender_cpu, Duration::ZERO, "cpu is only timed with `cpu: true`");
}

#[test]
fn cpu_is_timed_per_endpoint_when_asked() {
    let s = run(Args { mib: 1, cpu: true, ..Args::default() });
    assert!(s.done, "{s:?}");
    assert!(s.sender_cpu > Duration::ZERO && s.receiver_cpu > Duration::ZERO, "{s:?}");
    assert!(s.sender_cpu + s.receiver_cpu <= s.wall, "{s:?}");
}

/// PERF-8: a fragment lost while many are in flight was fast-retransmitted
/// again on every later ACK, passed MAX_TRIES within one round trip, and the
/// link was dropped as timed out.
#[test]
fn bulk_transfer_survives_one_percent_loss() {
    let s = run(Args { loss: 0.01, ..small() });
    assert!(s.done && s.dropped.is_none(), "{s:?}");
    assert!(s.retransmits < 4 * 1024 * 1024 / 1100 / 10, "{s:?}");
}

/// PERF-10: while a hole is repaired, fragments more than 64 above it can't be
/// reported by the ACK's bitmap; their timers ran out by the hundred (309 of
/// 340 timeouts at 1% loss with seed 1) and they were all sent again.
#[test]
fn nothing_beyond_the_sack_bitmaps_reach_times_out() {
    let mut deferred = 0;
    for seed in 1..=3 {
        let s = run(Args { mib: 16, loss: 0.01, seed, ..Args::default() });
        assert!(s.done && s.dropped.is_none(), "{s:?}");
        let r = s.recovery;
        assert_eq!(r.timed_out_beyond_reach, 0, "{s:?}");
        assert!(r.timed_out < 60, "{s:?}");
        deferred += r.deferred;
    }
    // Only when the window outgrew the bitmap (seed 1 reaches 336).
    assert!(deferred > 0);
}

/// Reordering (independent jitter per packet) looked like loss to fast
/// retransmit and stalled the transfer; the reordering window absorbs it.
/// A few timeouts remain where jitter outruns the RTO (up to 38 of 3870
/// fragments over seeds 1-40); a reordering storm would cost thousands.
#[test]
fn bulk_transfer_survives_reordering() {
    let s = run(Args { jitter_ms: 10, ..small() });
    assert!(s.done && s.dropped.is_none(), "{s:?}");
    assert!(s.retransmits < s.fragments_sent / 50, "{s:?}");
}

/// Datagrams between `0` and `1` with a lot of jitter, as the unreliable payloads in arrival order.
fn arrival_order(in_order: bool) -> Vec<u32> {
    let net = NetConfig { jitter: Duration::from_millis(10), in_order, ..NetConfig::default() };
    let mut sim = Sim::new(net, 3);
    sim.add_node(Config::default());
    sim.add_node(Config::default());
    sim.connect(0, 1).unwrap();
    assert!(sim.run_until(Duration::from_secs(5), |s| s.nodes[0].endpoint.is_connected(&s.peer_id(1))));
    for i in 0..200u32 {
        sim.send(0, 1, Lane::Unreliable, &i.to_le_bytes()).unwrap();
        sim.run_for(Duration::from_micros(100));
    }
    sim.run_for(Duration::from_secs(1));
    sim.take_events(1)
        .into_iter()
        .filter_map(|e| match e {
            Event::Message { lane: Lane::Unreliable, data, .. } => {
                Some(u32::from_le_bytes(data[..].try_into().unwrap()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn in_order_jitter_never_reorders() {
    let got = arrival_order(true);
    assert_eq!(got, (0..200).collect::<Vec<_>>());
    // Independent jitter per packet, 10 ms against 0.1 ms spacing, reorders.
    let got = arrival_order(false);
    assert_eq!(got.len(), 200);
    assert!(got.windows(2).any(|w| w[0] > w[1]), "{got:?}");
}
