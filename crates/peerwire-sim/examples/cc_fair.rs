//! Two bulk flows from one sender share its 100 Mbps uplink (a tail-drop
//! buffer), each to its own receiver 50 ms away. Prints each flow's goodput
//! over the last two thirds of the run and Jain's fairness index (1 = equal).
//!
//! cargo run --release -p peerwire-sim --example cc_fair -- --buffer 1
//! cargo run --release -p peerwire-sim --example cc_fair -- --buffer 50 --loss 0.01

use peerwire_core::{Event, Lane, SendError};
use peerwire_sim::{NetConfig, Sim};
use std::time::Duration;

fn main() {
    let (mut buffer_ms, mut loss, mut secs, mut seed) = (50u64, 0.0, 60u64, 1u64);
    let v: Vec<String> = std::env::args().skip(1).collect();
    for kv in v.chunks(2) {
        match kv[0].as_str() {
            "--buffer" => buffer_ms = kv[1].parse().unwrap(),
            "--loss" => loss = kv[1].parse().unwrap(),
            "--secs" => secs = kv[1].parse().unwrap(),
            "--seed" => seed = kv[1].parse().unwrap(),
            other => panic!("unknown flag {other}"),
        }
    }
    let net = NetConfig { latency: Duration::from_millis(50), loss, ..NetConfig::default() };
    let mut sim = Sim::new(net, seed);
    let tx = sim.add_node(Default::default());
    sim.nodes[tx].uplink_bps = Some(100_000_000);
    sim.nodes[tx].uplink_buffer = (buffer_ms > 0).then(|| Duration::from_millis(buffer_ms));
    let mut rx = [0; 2];
    for r in &mut rx {
        *r = sim.add_node(Default::default());
        sim.connect(tx, *r).unwrap();
        let n = *r;
        let up = |s: &mut Sim| s.nodes[n].events.iter().any(|e| matches!(e, Event::Connected { .. }));
        assert!(sim.run_until(Duration::from_secs(5), up));
    }
    let chunk = vec![7u8; 1 << 20];
    let start = sim.now;
    let warm = start + Duration::from_secs(secs / 3);
    let end = start + Duration::from_secs(secs);
    let mut got = [0usize; 2];
    while sim.now < end {
        for &r in &rx {
            loop {
                match sim.send(tx, r, Lane::Reliable, &chunk) {
                    Ok(()) => {}
                    Err(SendError::WouldBlock) => break,
                    Err(e) => panic!("{e}"),
                }
            }
        }
        sim.run_until(Duration::from_millis(5), |_| false);
        for f in 0..2 {
            for (at, e) in sim.take_timed_events(rx[f]) {
                if let Event::Message { data, .. } = e {
                    if at >= warm {
                        got[f] += data.len();
                    }
                }
            }
        }
    }
    let span = (end - warm).as_secs_f64();
    let r = got.map(|g| g as f64 / 1e6 / span);
    let jain = (r[0] + r[1]).powi(2) / (2.0 * (r[0] * r[0] + r[1] * r[1]));
    println!(
        "buffer {} ms, loss {}%: {:.2} vs {:.2} MB/s, total {:.2}, jain {:.3}, uplink drops {}",
        buffer_ms,
        loss * 100.0,
        r[0],
        r[1],
        r[0] + r[1],
        jain,
        sim.nodes[tx].uplink_dropped
    );
}
