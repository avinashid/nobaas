//! Two endpoints on real loopback UDP sockets, driven by a plain std loop.
//!
//! `cargo run --release -p nobaas-core --example udp_pair -- [messages] [lane]`
//! lane is `reliable` (default) or `unreliable`.

use nobaas_core::{Config, Endpoint, Event, Identity, Instant, Lane, PeerId};
use rand_core::{OsRng, RngCore};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::Duration;

fn seed() -> [u8; 32] {
    let mut s = [0u8; 32];
    OsRng.fill_bytes(&mut s);
    s
}

struct Host {
    sock: UdpSocket,
    ep: Endpoint<SocketAddr>,
    start: std::time::Instant,
}

impl Host {
    fn new() -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let identity = Identity::generate(&mut OsRng);
        Host { sock, ep: Endpoint::new(&identity, Config::default(), seed()), start: std::time::Instant::now() }
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.start.elapsed().as_micros() as u64)
    }

    /// One turn of the host loop; returns the events it produced.
    fn flush(&mut self) {
        let now = self.now();
        if self.ep.poll_timeout().is_some_and(|t| t <= now) {
            self.ep.handle_timeout(now);
        }
        while let Some(t) = self.ep.poll_transmit(now) {
            let _ = self.sock.send_to(&t.payload, t.dst);
        }
    }

    /// One turn of the host loop: send what is pending, read what arrived
    /// (blocking until the next timer if `wait`), and return the events.
    fn turn(&mut self, wait: bool) -> Vec<Event<SocketAddr>> {
        self.flush();
        let mut buf = [0u8; 2048];
        if wait {
            let now = self.now();
            let timeout = self
                .ep
                .poll_timeout()
                .map(|t| t.saturating_duration_since(now))
                .unwrap_or(Duration::from_millis(50))
                .clamp(Duration::from_micros(100), Duration::from_millis(50));
            self.sock.set_nonblocking(false).unwrap();
            self.sock.set_read_timeout(Some(timeout)).unwrap();
            if let Ok((n, from)) = self.sock.recv_from(&mut buf) {
                self.ep.handle_datagram(self.now(), from, &buf[..n]);
            }
        }
        self.sock.set_nonblocking(true).unwrap();
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            self.ep.handle_datagram(self.now(), from, &buf[..n]);
        }
        self.flush();
        std::iter::from_fn(|| self.ep.poll_event()).collect()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let total: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let lane = if args.get(2).map(String::as_str) == Some("unreliable") { Lane::Unreliable } else { Lane::Reliable };
    let size = 64;

    let (tx, rx) = mpsc::channel::<(SocketAddr, PeerId)>();
    let receiver = std::thread::spawn(move || {
        let mut h = Host::new();
        tx.send((h.sock.local_addr().unwrap(), h.ep.peer_id())).unwrap();
        let mut got = 0usize;
        let mut first = None;
        let mut idle_since = std::time::Instant::now();
        loop {
            for e in h.turn(true) {
                if let Event::Message { data, .. } = e {
                    first.get_or_insert_with(std::time::Instant::now);
                    got += 1;
                    idle_since = std::time::Instant::now();
                    if data == b"done" {
                        return (got - 1, first.unwrap().elapsed());
                    }
                }
            }
            if let Some(f) = first {
                if idle_since.elapsed() > Duration::from_secs(2) {
                    return (got, f.elapsed());
                }
            }
        }
    });

    let (addr, peer) = rx.recv().unwrap();
    let mut h = Host::new();
    let t0 = std::time::Instant::now();
    h.ep.connect(h.now(), addr, peer).unwrap();
    while !h.ep.is_connected(&peer) {
        h.turn(true);
    }
    println!("connected to {peer} at {addr} in {:?} (Noise IK, 1 RTT)", t0.elapsed());

    let msg = vec![7u8; size];
    let mut sent = 0;
    let t1 = std::time::Instant::now();
    while sent < total {
        match h.ep.send(&peer, lane, &msg) {
            Ok(()) => sent += 1,
            Err(_) => {
                h.turn(true); // reliable buffer full: wait for acks
                continue;
            }
        }
        if sent % 64 == 0 {
            h.turn(false);
        }
    }
    while h.ep.send(&peer, Lane::Reliable, b"done").is_err() {
        h.turn(true);
    }
    let send_time = t1.elapsed();
    let mut spins = 0;
    while !receiver.is_finished() && spins < 100_000 {
        h.turn(true);
        spins += 1;
    }
    let (got, recv_time) = receiver.join().unwrap();
    let st = h.ep.stats();
    println!(
        "{lane:?}: sent {total} x {size} B in {send_time:?}; receiver got {got} in {recv_time:?} = {:.0} msg/s; \
         sender packets {} retransmits {} rtt {:?}",
        got as f64 / recv_time.as_secs_f64(),
        st.tx_packets,
        st.retransmits,
        h.ep.rtt(&peer)
    );
}
