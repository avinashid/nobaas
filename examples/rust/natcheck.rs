//! A field test for NAT traversal on real networks (a home router, a phone hotspot,
//! carrier CGNAT): reports what this network's NAT does, then meets a peer in a
//! Nostr room and says whether the link came up direct, relayed or not at all.
//!
//! ```sh
//! cargo run -p peerwire --features nostr --example natcheck                     # NAT report only
//! cargo run -p peerwire --features nostr --example natcheck -- room <name>      # report, then meet a peer
//! cargo run -p peerwire --features nostr --example natcheck -- room <name> v6   # the same, over IPv6
//! ```
//!
//! `NATCHECK_PORT=<port>` binds the room node to that UDP port instead of a random one,
//! for a peer whose firewall has that one port open.
//!
//! Uses the default public STUN servers (Google, Cloudflare) and Nostr relays only. The
//! router check only asks the LAN gateway what it supports (SSDP search, NAT-PMP
//! external-address query); it never creates a mapping.

use peerwire::{Event, Lane, NatType, Node, NostrSignaling, Path, DEFAULT_STUN_SERVERS};
use peerwire_core::stun;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// STUN targets for the port-pattern probe. Google's hostnames all resolve to
/// one address, so the distinct destinations come from extra ports instead:
/// two server IPs and four IP:port pairs in all, interleaved so consecutive
/// requests go to different IPs. Not port 53: carriers and routers intercept
/// DNS, and in the field it went unanswered or came back from another address.
const PROBE_SERVERS: &[&str] =
    &["stun.l.google.com:19302", "stun.cloudflare.com:3478", "stun.l.google.com:3478", "stun.cloudflare.com:443"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    probe(false)?;
    probe(true)?;
    router()?;
    match args.as_slice() {
        [] => Ok(()),
        ["room", room] => meet(room, false),
        ["room", room, "v6"] => meet(room, true),
        _ => Err("usage: natcheck [room <name> [v6]]".into()),
    }
}

/// Asks each probe server, in turn and from one socket, for its view of our
/// address, and prints the mapped addresses and what they say about the NAT.
fn probe(v6: bool) -> Result<(), Box<dyn std::error::Error>> {
    let family = if v6 { "IPv6" } else { "IPv4" };
    let Some(local_ip) = primary_ip(v6) else {
        println!("== {family} STUN probe ==\n  no {family} route from this machine");
        return Ok(());
    };
    let sock = UdpSocket::bind(SocketAddr::new(unspecified(v6), 0))?;
    sock.set_read_timeout(Some(Duration::from_millis(1500)))?;
    let local = SocketAddr::new(local_ip, sock.local_addr()?.port());
    println!("== {family} STUN probe (local {local}) ==");
    if let IpAddr::V6(ip) = local_ip {
        if !is_global_v6(ip) {
            println!("  {ip} is not a global address: no IPv6 internet here");
            return Ok(());
        }
    }
    // One request per distinct destination: several names can share an address.
    let mut dests: Vec<(&str, SocketAddr)> = Vec::new();
    for server in PROBE_SERVERS {
        let Ok(mut addrs) = server.to_socket_addrs() else { continue };
        match addrs.find(|a| a.is_ipv6() == v6) {
            Some(dest) if !dests.iter().any(|(_, d)| *d == dest) => dests.push((server, dest)),
            _ => {}
        }
    }
    let mut mapped: Vec<(SocketAddr, SocketAddr)> = Vec::new();
    for (i, (server, dest)) in dests.iter().enumerate() {
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
                println!("  {server:<26} {dest:<42} -> {m}");
                mapped.push((*dest, m));
            }
            None => println!("  {server:<26} {dest:<42} -> no answer"),
        }
    }
    println!("  verdict: {}", verdict(local, &mapped));
    Ok(())
}

/// Classifies the NAT from `(destination, mapped address)` pairs, in send order.
fn verdict(local: SocketAddr, mapped: &[(SocketAddr, SocketAddr)]) -> String {
    let Some(&(_, first)) = mapped.first() else {
        return "no STUN answers: UDP is blocked here, or the servers are unreachable".into();
    };
    if mapped.len() == 1 {
        return format!("only one answer ({first}): can't tell");
    }
    if mapped.iter().all(|(_, m)| *m == first) {
        return if first == local {
            "no NAT: the public address is this machine's own (a firewall may still drop unsolicited packets)".into()
        } else {
            format!(
                "cone: the same public address for every destination{}",
                if first.port() == local.port() { ", port preserved" } else { "" }
            )
        };
    }
    let ips: Vec<IpAddr> = mapped.iter().map(|(_, m)| m.ip()).collect();
    let mut distinct = ips.clone();
    distinct.sort();
    distinct.dedup();
    if mapped.iter().all(|(_, m)| m.port() == first.port()) {
        return format!(
            "port kept for every destination{}, but the public IP varies across {} addresses \
             (an address pool without paired pooling)",
            if first.port() == local.port() { " (preserved)" } else { "" },
            distinct.len()
        );
    }
    let multi_ip = if distinct.len() > 1 { "; public IP varies by destination" } else { "" };
    // Same mapping for every port of one server IP means the NAT keys on the address only.
    let per_ip = mapped.iter().all(|(d, m)| mapped.iter().filter(|(d2, _)| d2.ip() == d.ip()).all(|(_, m2)| m2 == m));
    let shared = mapped.iter().any(|(d, _)| mapped.iter().filter(|(d2, _)| d2.ip() == d.ip()).count() > 1);
    let keyed = match (shared, per_ip) {
        (false, _) => "",
        (true, true) => ", address-dependent (one port per server IP)",
        (true, false) => ", address-and-port-dependent (a new port for every destination)",
    };
    // Deltas between successive new ports: small steps mean a predictable allocator.
    let mut ports: Vec<u16> = Vec::new();
    for (_, m) in mapped {
        if !ports.contains(&m.port()) {
            ports.push(m.port());
        }
    }
    let deltas: Vec<i32> = ports.windows(2).map(|w| w[1] as i32 - w[0] as i32).collect();
    let kind = match deltas.as_slice() {
        [] => "one port".to_string(),
        [_] => format!("1 delta {deltas:?}, too few to call the allocator"),
        _ if deltas.iter().all(|d| d.abs() <= 16) => format!("sequential ports, deltas {deltas:?}"),
        _ => format!("random ports, deltas {deltas:?}"),
    };
    format!("symmetric{keyed}; {kind}{multi_ip}")
}

/// Asks the LAN for a port-mapping service, without creating a mapping: an SSDP
/// search for a UPnP Internet Gateway Device, and a NAT-PMP external-address
/// query to the default gateway (a PCP-only router answers it with an error).
fn router() -> Result<(), Box<dyn std::error::Error>> {
    println!("== router port mapping (read-only check) ==");
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_millis(300)))?;
    for st in
        ["urn:schemas-upnp-org:device:InternetGatewayDevice:1", "urn:schemas-upnp-org:device:InternetGatewayDevice:2"]
    {
        let search = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n"
        );
        sock.send_to(search.as_bytes(), "239.255.255.250:1900")?;
    }
    let mut seen: Vec<String> = Vec::new();
    let until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < until {
        let mut buf = [0u8; 1500];
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let text = String::from_utf8_lossy(&buf[..n]);
        let header = |name: &str| {
            text.lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
                .unwrap_or("?")
                .to_string()
        };
        let line = format!("{from}  {}  {}", header("ST"), header("LOCATION"));
        if !seen.contains(&line) {
            println!("  UPnP IGD:  {line}  ({})", header("SERVER"));
            seen.push(line);
        }
    }
    if seen.is_empty() {
        println!("  UPnP IGD:  no answer (UPnP off, or not supported)");
    }
    match gateway() {
        None => println!("  NAT-PMP:   no default gateway found"),
        Some(gw) => {
            let mut answer = None;
            for wait in [250, 500, 1000] {
                sock.set_read_timeout(Some(Duration::from_millis(wait)))?;
                sock.send_to(&[0, 0], (gw, 5351))?;
                let mut buf = [0u8; 64];
                if let Ok((n, from)) = sock.recv_from(&mut buf) {
                    if from.ip() == IpAddr::V4(gw) {
                        answer = Some(buf[..n].to_vec());
                        break;
                    }
                }
            }
            match answer.as_deref() {
                None => println!("  NAT-PMP:   {gw}:5351 no answer (NAT-PMP/PCP off, or not supported)"),
                Some([0, 128, 0, 0, _, _, _, _, a, b, c, d, ..]) => {
                    let ext = Ipv4Addr::new(*a, *b, *c, *d);
                    let behind = ext.is_private() || ext.octets()[0] == 100 && ext.octets()[1] & 0xc0 == 64;
                    let note = if behind { ": NOT public, so the router is itself behind another NAT" } else { "" };
                    println!("  NAT-PMP:   {gw}:5351 supported, router's external IP {ext}{note}");
                }
                Some(other) => println!("  NAT-PMP:   {gw}:5351 answered {other:02x?} (likely PCP-only or refused)"),
            }
        }
    }
    Ok(())
}

/// The IPv4 default gateway: from the routing table on Linux, `route` on macOS,
/// else the conventional .1 of our own subnet.
fn gateway() -> Option<Ipv4Addr> {
    if let Ok(table) = std::fs::read_to_string("/proc/net/route") {
        let gw = table.lines().skip(1).find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.get(1) == Some(&"00000000")).then(|| u32::from_str_radix(f.get(2)?, 16).ok()).flatten()
        });
        if let Some(gw) = gw.filter(|g| *g != 0) {
            return Some(Ipv4Addr::from(gw.swap_bytes()));
        }
    }
    if let Ok(out) = std::process::Command::new("route").args(["-n", "get", "default"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(gw) = text.lines().find_map(|l| l.trim().strip_prefix("gateway:")?.trim().parse().ok()) {
            return Some(gw);
        }
    }
    match primary_ip(false)? {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            Some(Ipv4Addr::new(a, b, c, 1))
        }
        IpAddr::V6(_) => None,
    }
}

fn unspecified(v6: bool) -> IpAddr {
    if v6 {
        Ipv6Addr::UNSPECIFIED.into()
    } else {
        Ipv4Addr::UNSPECIFIED.into()
    }
}

/// The address the default route leaves by (`connect` on UDP sends nothing).
fn primary_ip(v6: bool) -> Option<IpAddr> {
    let probe: SocketAddr = if v6 {
        (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 9).into()
    } else {
        (Ipv4Addr::new(192, 0, 2, 1), 9).into()
    };
    let s = UdpSocket::bind(SocketAddr::new(unspecified(v6), 0)).ok()?;
    s.connect(probe).ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
}

/// 2000::/3, the global unicast range (not link-local, ULA or a documentation prefix).
fn is_global_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] & 0xe000 == 0x2000 && s[0..2] != [0x2001, 0x0db8]
}

/// Joins `room` over Nostr, waits for a peer, exchanges a ping each way and
/// reports the path.
fn meet(room: &str, v6: bool) -> Result<(), Box<dyn std::error::Error>> {
    let port = match std::env::var("NATCHECK_PORT") {
        Ok(p) => p.parse().map_err(|_| format!("NATCHECK_PORT: not a port: {p:?}"))?,
        Err(_) => 0,
    };
    let mut node = Node::builder().signaling(NostrSignaling::default()).bind((unspecified(v6), port))?;
    let start = Instant::now();
    while node.nat_type() == NatType::Unknown && start.elapsed() < Duration::from_secs(5) {
        tick(&mut node);
    }
    println!("== node ==");
    println!("  peer id:  {}", node.peer_id());
    println!("  NAT type: {:?} (from {})", node.nat_type(), DEFAULT_STUN_SERVERS.join(", "));
    println!("  public:   {:?}", node.public_addr());
    println!("  socket:   {} ({})", node.local_addr(), if v6 { "IPv6" } else { "IPv4" });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    const G1: &str = "74.125.250.129:19302";
    const C1: &str = "162.159.207.0:3478";
    const G2: &str = "74.125.250.129:3478";
    const C2: &str = "162.159.207.0:53";
    const LOCAL: &str = "192.168.1.5:40000";

    #[test]
    fn home_wifi_field_result_is_symmetric_with_one_delta() {
        // tsk_vug4qr3b: Google saw :1369, Cloudflare :19000.
        let m = [(a(G1), a("203.145.57.120:1369")), (a(C1), a("203.145.57.120:19000"))];
        let v = verdict(a(LOCAL), &m);
        assert!(v.starts_with("symmetric;"), "{v}");
        assert!(v.contains("too few"), "{v}");
    }

    #[test]
    fn port_dependent_random() {
        let m = [
            (a(G1), a("203.0.113.9:1369")),
            (a(C1), a("203.0.113.9:19000")),
            (a(G2), a("203.0.113.9:40211")),
            (a(C2), a("203.0.113.9:7777")),
        ];
        let v = verdict(a(LOCAL), &m);
        assert!(v.contains("address-and-port-dependent") && v.contains("random"), "{v}");
    }

    #[test]
    fn address_dependent_two_ports() {
        let m = [
            (a(G1), a("203.0.113.9:5000")),
            (a(C1), a("203.0.113.9:5001")),
            (a(G2), a("203.0.113.9:5000")),
            (a(C2), a("203.0.113.9:5001")),
        ];
        let v = verdict(a(LOCAL), &m);
        assert!(v.contains("address-dependent (one port") && v.contains("too few"), "{v}");
    }

    #[test]
    fn field_hotspot_pool_keeps_the_port() {
        // Jio hotspot, ab831e5 run: port kept, one destination saw another pool IP.
        let m = [
            (a(G1), a("152.58.117.228:60241")),
            (a(C1), a("152.58.117.228:60241")),
            (a(C2), a("152.58.117.166:60241")),
        ];
        let v = verdict(a("10.129.132.88:60241"), &m);
        assert!(v.starts_with("port kept for every destination (preserved)") && v.contains("2 addresses"), "{v}");
    }

    #[test]
    fn field_home_wifi_is_port_dependent_random() {
        let m = [
            (a(G1), a("203.145.57.120:32679")),
            (a(C1), a("203.145.57.120:5342")),
            (a(G2), a("203.145.57.120:24211")),
            (a("162.159.207.0:443"), a("203.145.57.120:8499")),
        ];
        let v = verdict(a("192.168.1.35:45973"), &m);
        assert!(v.contains("address-and-port-dependent") && v.contains("random"), "{v}");
    }

    #[test]
    fn cone_open_and_empty() {
        let cone = [(a(G1), a("203.0.113.9:40000")), (a(C1), a("203.0.113.9:40000"))];
        assert!(verdict(a(LOCAL), &cone).ends_with("port preserved"));
        let local = a("[2001:4860::5]:40000");
        let open = [(a(G1), local), (a(C1), local)];
        assert!(verdict(local, &open).starts_with("no NAT"));
        assert!(verdict(a(LOCAL), &[]).starts_with("no STUN answers"));
        assert!(!is_global_v6("fe80::1".parse().unwrap()) && !is_global_v6("fd00::1".parse().unwrap()));
        assert!(is_global_v6("2409:4060::1".parse().unwrap()) && !is_global_v6("2001:db8::1".parse().unwrap()));
    }
}
