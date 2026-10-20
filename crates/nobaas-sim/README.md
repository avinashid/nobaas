# nobaas-sim

A deterministic, single-threaded network simulator for [`nobaas-core`](../nobaas-core).

Every node is a real `Endpoint`; only the network is simulated:
- latency, jitter (which reorders packets unless `in_order` is set), loss and duplication;
- an optional per-node uplink rate;
- NATs (`NatConfig`: mapping and filtering behaviour, port allocation, mapping timeout,
  hairpinning), including NATs behind NATs (carrier-grade NAT);
- STUN servers on the simulated internet.

Time is virtual, so a minute of traffic between thousands of nodes runs as fast as the
endpoints can process it, and the same seed always produces the same run. That makes it the
place for load, loss and NAT tests on a small machine, and for performance numbers that
anyone can reproduce.

## Example

```rust
use nobaas_core::{Config, Event, Lane};
use nobaas_sim::{NetConfig, Sim};
use std::time::Duration;

let net = NetConfig { latency: Duration::from_millis(40), loss: 0.05, ..NetConfig::default() };
let mut sim = Sim::new(net, 42);
let a = sim.add_node(Config::default());
let b = sim.add_node(Config::default());

sim.connect(a, b)?;
sim.run_for(Duration::from_secs(1));
sim.send(a, b, Lane::Reliable, b"hello")?;

let delivered = sim.run_until(Duration::from_secs(10), |sim| {
    sim.take_events(b).iter().any(|e| matches!(e, Event::Message { data, .. } if data == b"hello"))
});
assert!(delivered);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Tests and benchmarks

`cargo test --release -p nobaas-sim` holds the integration tests for the core and the
traversal layer:
- `tests/endpoint.rs`: handshake, loss, reordering, tampering, replay, roaming, restart, a
  300-peer hub, determinism.
- `tests/nat.rs`: hole punching and relaying across NAT types, including an 8×8 matrix
  (`-- --nocapture nat_matrix`).
- `tests/traversal.rs`: the `nobaas` traversal layer driven over simulated NATs.
- `tests/bulk.rs`: throughput, congestion window and urgent-message latency on a long, fast link.
- `tests/arena.rs`: regression runs of the arena benchmark.

Examples:

```sh
cargo run --release -p nobaas-sim --example arena -- --players 512 --supers 8
cargo run --release -p nobaas-sim --example bulk
cargo run --release -p nobaas-sim --example bulk -- --loss 0.01 --cpu 1
```

- `arena`: a super-peer lockstep arena on Hilbert islands. It measures confirmation lag,
  late projectile events, per-role bandwidth and state agreement.
- `bulk`: a bulk reliable transfer with urgent messages alongside, on a 100 ms RTT,
  100 Mbit/s link. It measures throughput, peak congestion window, retransmits and urgent
  latency.

See [docs/PERFORMANCE.md](../../docs/PERFORMANCE.md) for the numbers these produce.

## Status

Early development (v0.1). Used by this workspace's tests and benchmarks.
