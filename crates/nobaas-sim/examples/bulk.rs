//! A bulk reliable transfer on a long, fast link, with small urgent messages
//! sent during it, on real nobaas-core endpoints in the simulator.
//!
//! Two hosts 50 ms apart (100 ms RTT) with 100 Mbps uplinks. The sender queues
//! the transfer as 1 MiB `Lane::Reliable` messages, keeping the send buffer
//! full, and sends a 64-byte `Lane::Urgent` message every 20 ms until the last
//! byte arrives.
//!
//! Measured: throughput, peak congestion window and fragments in flight, delay
//! backoffs and retransmits, and how long the urgent messages took (they share
//! the reliable queue, so they wait behind the bulk data already queued). With
//! `--cpu`, the wall-clock time spent in each endpoint (not deterministic).
//!
//! cargo run --release -p nobaas-sim --example bulk                  # the three variants
//! cargo run --release -p nobaas-sim --example bulk -- --loss 0.01 --cpu 1

use nobaas_core::{Config, Event, Lane, PeerStats, SendError};
use nobaas_sim::{NetConfig, Sim};
use std::time::Duration;

const CHUNK: usize = 1 << 20;
const URGENT_BYTES: usize = 64;

#[derive(Clone, Debug)]
pub struct Args {
    pub mib: usize,
    pub latency_ms: u64,
    pub jitter_ms: u64,
    pub loss: f64,
    /// Jitter without reordering (`NetConfig::in_order`).
    pub in_order: bool,
    pub up_mbps: u64,
    pub urgent_hz: u64,
    /// Give up after this much virtual time.
    pub limit_secs: u64,
    pub cpu: bool,
    /// Print the window, RTT and delivery rate every 500 ms of virtual time.
    pub trace: bool,
    pub seed: u64,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            mib: 64,
            latency_ms: 50,
            jitter_ms: 0,
            loss: 0.0,
            in_order: false,
            up_mbps: 100,
            urgent_hz: 50,
            limit_secs: 600,
            cpu: false,
            trace: false,
            seed: 1,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub done: bool,
    /// Virtual time from the first byte queued to the last delivered.
    pub secs: f64,
    pub mb_per_sec: f64,
    pub peak_cwnd: u32,
    pub peak_inflight: u32,
    pub delay_backoffs: u64,
    pub retransmits: u64,
    pub fragments_sent: u64,
    pub urgent_sent: usize,
    /// Urgent messages delivered before the transfer ended (the ones timed).
    pub urgent_delivered: usize,
    pub urgent_p50_ms: f64,
    pub urgent_p99_ms: f64,
    pub urgent_max_ms: f64,
    /// Urgent send attempts refused with `WouldBlock`; each is retried 1 ms later.
    pub urgent_refused: u64,
    /// Wall-clock CPU inside each endpoint, with `--cpu`.
    pub sender_cpu: Duration,
    pub receiver_cpu: Duration,
    pub wall: Duration,
    /// The sender's link dropped: reason and time.
    pub dropped: Option<String>,
}

fn args() -> Vec<Args> {
    let v: Vec<String> = std::env::args().skip(1).collect();
    if v.is_empty() {
        let base = Args::default();
        return vec![
            base.clone(),
            Args { loss: 0.01, ..base.clone() },
            Args { jitter_ms: 10, ..base.clone() },
            Args { jitter_ms: 10, in_order: true, ..base },
        ];
    }
    let mut a = Args::default();
    for kv in v.chunks(2) {
        let val = kv.get(1).map(String::as_str).unwrap_or("");
        match kv[0].as_str() {
            "--mib" => a.mib = val.parse().unwrap(),
            "--latency" => a.latency_ms = val.parse().unwrap(),
            "--jitter" => a.jitter_ms = val.parse().unwrap(),
            "--loss" => a.loss = val.parse().unwrap(),
            "--in-order" => a.in_order = val == "1",
            "--up" => a.up_mbps = val.parse().unwrap(),
            "--urgent-hz" => a.urgent_hz = val.parse().unwrap(),
            "--limit" => a.limit_secs = val.parse().unwrap(),
            "--cpu" => a.cpu = val == "1",
            "--trace" => a.trace = val == "1",
            "--seed" => a.seed = val.parse().unwrap(),
            other => panic!("unknown flag {other}"),
        }
    }
    vec![a]
}

fn main() {
    for a in args() {
        let s = run(a.clone());
        println!(
            "{} MiB, rtt {} ms, jitter {} ms{}, loss {}%, {} Mbps",
            a.mib,
            2 * a.latency_ms,
            a.jitter_ms,
            if a.in_order { " in order" } else { "" },
            a.loss * 100.0,
            a.up_mbps
        );
        println!(
            "  {}: {:.2} s, {:.2} MB/s; peak cwnd {}, peak inflight {}; delay backoffs {}, retransmits {} of {} fragments",
            match (&s.dropped, s.done) {
                (Some(d), _) => format!("LINK DROPPED ({d})"),
                (None, true) => "done".into(),
                (None, false) => "INCOMPLETE".into(),
            },
            s.secs,
            s.mb_per_sec,
            s.peak_cwnd,
            s.peak_inflight,
            s.delay_backoffs,
            s.retransmits,
            s.fragments_sent
        );
        println!(
            "  urgent: {} sent, {} timed, p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms, refused {}",
            s.urgent_sent, s.urgent_delivered, s.urgent_p50_ms, s.urgent_p99_ms, s.urgent_max_ms, s.urgent_refused
        );
        if a.cpu {
            println!(
                "  cpu: sender {:.3} s ({:.1} µs per fragment), receiver {:.3} s, wall {:.3} s",
                s.sender_cpu.as_secs_f64(),
                s.sender_cpu.as_secs_f64() * 1e6 / (s.fragments_sent + s.retransmits).max(1) as f64,
                s.receiver_cpu.as_secs_f64(),
                s.wall.as_secs_f64()
            );
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

pub fn run(a: Args) -> Summary {
    let wall = std::time::Instant::now();
    let net = NetConfig {
        latency: Duration::from_millis(a.latency_ms),
        jitter: Duration::from_millis(a.jitter_ms),
        loss: a.loss,
        duplicate: 0.0,
        in_order: a.in_order,
    };
    let mut sim = Sim::new(net, a.seed);
    let (tx, rx) = (sim.add_node(Config::default()), sim.add_node(Config::default()));
    for n in [tx, rx] {
        sim.nodes[n].uplink_bps = Some(a.up_mbps * 1_000_000);
    }
    sim.connect(tx, rx).unwrap();
    let connected = |s: &Sim, n: usize| s.nodes[n].events.iter().any(|e| matches!(e, Event::Connected { .. }));
    assert!(sim.run_until(Duration::from_secs(5), |s| connected(s, tx) && connected(s, rx)));
    sim.take_events(tx);
    sim.take_events(rx);
    sim.measure_cpu = a.cpu;

    let start = sim.now;
    let period = Duration::from_micros(1_000_000 / a.urgent_hz);
    let total = a.mib * CHUNK;
    let chunk: Vec<u8> = (0..CHUNK).map(|i| i as u8).collect();
    let (mut queued, mut received) = (0usize, 0usize);
    let (mut next_urgent, mut urgent_seq, mut sent_at) = (start, 0u32, Vec::new());
    let mut latencies = Vec::new();
    let mut s = Summary::default();
    let limit = start + Duration::from_secs(a.limit_secs);
    let peer = sim.peer_id(rx);
    let (mut next_trace, mut traced) = (start, 0usize);
    let mut last = PeerStats::default();
    while received < total && sim.now < limit {
        if a.trace && sim.now >= next_trace {
            let p = sim.nodes[tx].endpoint.peer_stats(&peer).unwrap_or(last);
            println!(
                "  t {:5.1} s: {:5.2} MB/s, cwnd {:4}, inflight {:4}, rtt {:3} ms, retransmits {}, delay backoffs {}",
                (sim.now - start).as_secs_f64(),
                (received - traced) as f64 / 1e6 / 0.5,
                p.cwnd,
                p.inflight,
                p.rtt.unwrap_or_default().as_millis(),
                p.retransmits,
                p.delay_backoffs
            );
            (next_trace, traced) = (next_trace + Duration::from_millis(500), received);
        }
        while queued < total {
            match sim.send(tx, rx, Lane::Reliable, &chunk[..CHUNK.min(total - queued)]) {
                Ok(()) => queued += CHUNK,
                Err(SendError::WouldBlock) => break,
                Err(SendError::NotConnected) => break,
                Err(e) => panic!("{e}"),
            }
        }
        if sim.now >= next_urgent {
            let mut m = vec![0u8; URGENT_BYTES];
            m[..4].copy_from_slice(&urgent_seq.to_le_bytes());
            match sim.send(tx, rx, Lane::Urgent, &m) {
                Ok(()) => {
                    // Timed from when it was due, so a refused send's wait counts.
                    sent_at.push(next_urgent);
                    urgent_seq += 1;
                    next_urgent = next_urgent + period;
                }
                Err(SendError::WouldBlock) => s.urgent_refused += 1,
                Err(SendError::NotConnected) => {} // the Disconnected event ends the run
                Err(e) => panic!("{e}"),
            }
        }
        // Step until the next urgent tick, sampling the window after every event.
        let until = next_urgent.max(sim.now + Duration::from_millis(1));
        sim.run_until(until - sim.now, |sim| {
            if let Some(p) = sim.nodes[tx].endpoint.peer_stats(&peer) {
                s.peak_cwnd = s.peak_cwnd.max(p.cwnd);
                s.peak_inflight = s.peak_inflight.max(p.inflight);
                last = p;
            }
            false
        });
        for (at, e) in sim.take_timed_events(tx) {
            if let Event::Disconnected { reason, .. } = e {
                s.dropped = Some(format!("{reason:?} at {:.2} s", (at - start).as_secs_f64()));
            }
        }
        if s.dropped.is_some() {
            break;
        }
        for (at, e) in sim.take_timed_events(rx) {
            match e {
                Event::Message { data, .. } if data.len() == URGENT_BYTES => {
                    let seq = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
                    latencies.push((at - sent_at[seq]).as_secs_f64() * 1e3);
                }
                Event::Message { data, .. } => {
                    received += data.len();
                    if received == total {
                        s.secs = (at - start).as_secs_f64();
                    }
                }
                _ => {}
            }
        }
    }
    let p = sim.nodes[tx].endpoint.peer_stats(&peer).unwrap_or(last);
    latencies.sort_by(f64::total_cmp);
    s.done = received == total;
    s.mb_per_sec = if s.done { total as f64 / 1e6 / s.secs } else { 0.0 };
    s.delay_backoffs = p.delay_backoffs;
    s.retransmits = p.retransmits;
    s.fragments_sent = p.fragments_sent;
    s.urgent_sent = sent_at.len();
    s.urgent_delivered = latencies.len();
    s.urgent_p50_ms = percentile(&latencies, 0.5);
    s.urgent_p99_ms = percentile(&latencies, 0.99);
    s.urgent_max_ms = latencies.last().copied().unwrap_or(0.0);
    s.sender_cpu = sim.nodes[tx].cpu;
    s.receiver_cpu = sim.nodes[rx].cpu;
    s.wall = wall.elapsed();
    s
}
