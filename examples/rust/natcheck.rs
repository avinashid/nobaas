//! A field test for NAT traversal on real networks (a home router, a phone hotspot,
//! carrier CGNAT): reports what this network's NAT does, then meets a peer in a
//! Nostr room and says whether the link came up direct, relayed or not at all.
//!
//! ```sh
//! cargo run -p nobaas --example natcheck                  # NAT report only
//! cargo run -p nobaas --example natcheck -- room <name>   # report, then meet whoever runs the same
//! ```
//!
//! Uses the default public STUN servers and Nostr relays only.

use nobaas::{Event, Lane, NatType, Node, NostrSignaling, Path, DEFAULT_STUN_SERVERS};
use nobaas_core::stun;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Extra STUN targets for the port-pattern probe: more destinations than the
/// node's two, so a symmetric NAT's port sequence shows.
const PROBE_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun3.l.google.com:19302",
    "stun4.l.google.com:19302",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    probe()?;
    match (std::env::args().nth(1).as_deref(), std::env::args().nth(2)) {
        (None, _) => Ok(()),
        (Some("room"), Some(room)) => meet(&room),
        _ => Err("usage: natcheck [room <name>]".into()),
    }
}

/// Asks each probe server, in turn and from one socket, for its view of our
/// address, and prints the mapped ports and what they say about the NAT.
fn probe() -> Result<(), Box<dyn std::error::Error>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(1500)))?;
    let local = sock.local_addr()?.port();
    println!("== STUN probe (local port {local}) ==");
    let mut mapped: Vec<SocketAddr> = Vec::new();
    for (i, server) in PROBE_SERVERS.iter().enumerate() {
        let Some(dest) = server.to_socket_addrs()?.find(SocketAddr::is_ipv4) else { continue };
        let mut tx = [0u8; 12];
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64;
        tx[..8].copy_from_slice(&nanos.to_le_bytes());
        tx[8] = i as u8;
        let mut got = None;
        for _ in 0..3 {
            sock.send_to(&stun::binding_request(&tx), dest)?;
            let mut buf = [0u8; 512];
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
                got = stun::parse_binding_response(&buf[..n], &tx);
                if got.is_some() {
                    break;
                }
            }
        }
        match got {
            Some(m) => {
                println!("  {server:<28} {dest:<22} -> {m}");
                mapped.push(m);
            }
            None => println!("  {server:<28} {dest:<22} -> no answer"),
        }
    }
    let ports: Vec<u16> = mapped.iter().map(SocketAddr::port).collect();
    let verdict = match ports.as_slice() {
        [] => "no STUN answers: UDP is blocked, or no network".to_string(),
        [_] => "only one answer: can't tell".to_string(),
        [first, ..] if ports.iter().all(|p| p == first) => format!(
            "cone (same public port for every destination{})",
            if *first == local { ", port preserved" } else { "" }
        ),
        _ => {
            let deltas: Vec<i32> = ports.windows(2).map(|w| w[1] as i32 - w[0] as i32).collect();
            let kind = if deltas.iter().all(|d| d.abs() <= 16) { "sequential" } else { "random" };
            format!("symmetric, {kind} ports (deltas {deltas:?})")
        }
    };
    println!("  verdict: {verdict}");
    Ok(())
}

/// Joins `room` over Nostr, waits for a peer, exchanges a ping each way and
/// reports the path.
fn meet(room: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut node = Node::builder().signaling(NostrSignaling::default()).bind("0.0.0.0:0")?;
    let start = Instant::now();
    while node.nat_type() == NatType::Unknown && start.elapsed() < Duration::from_secs(5) {
        tick(&mut node);
    }
    println!("== node ==");
    println!("  peer id:  {}", node.peer_id());
    println!("  NAT type: {:?} (from {})", node.nat_type(), DEFAULT_STUN_SERVERS.join(", "));
    println!("  public:   {:?}", node.public_addr());
    println!("  joining room {room:?}; run the same command on the other machine (Ctrl-C to stop)");
    node.join_room("natcheck", room);
    let t0 = Instant::now();
    loop {
        tick(&mut node);
        while let Some(event) = node.try_recv_event() {
            let at = t0.elapsed().as_secs_f32();
            match event {
                Event::Connected { peer, path, .. } => {
                    println!("[{at:6.1}s] CONNECTED to {peer}: {}", describe(&path));
                    node.send(&peer, Lane::Reliable, format!("ping from {}", node.peer_id()).as_bytes())?;
                }
                Event::PathChanged { peer, path } => println!("[{at:6.1}s] path to {peer} now {}", describe(&path)),
                Event::Message { peer, data, .. } => {
                    println!("[{at:6.1}s] message from {peer}: {}", String::from_utf8_lossy(&data));
                    if data.starts_with(b"ping") {
                        node.send(&peer, Lane::Reliable, b"pong")?;
                    }
                }
                Event::ConnectFailed { peer } => {
                    println!("[{at:6.1}s] FAILED to reach {peer}: {:?}", node.failure(&peer));
                }
                Event::Disconnected { peer, reason } => println!("[{at:6.1}s] {peer} left: {reason:?}"),
                Event::AddrObserved { by, addr } => println!("[{at:6.1}s] {by} sees us at {addr}"),
            }
        }
    }
}

fn describe(path: &Path) -> String {
    match path {
        Path::Direct(a) => format!("DIRECT via {a}"),
        Path::Via(p) => format!("RELAYED through peer {p}"),
    }
}

fn tick(node: &mut Node) {
    node.poll();
    std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
}
