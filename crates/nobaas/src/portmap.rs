//! Router port mapping: asks the LAN gateway to forward a public UDP port to the
//! node's socket, with PCP (RFC 6887), NAT-PMP (RFC 6886) or UPnP-IGD, tried in
//! that order. The mapped address is offered like a public one, so a peer can
//! reach a node behind a symmetric NAT that punching can't get through.
//!
//! [`Mapper`] is sans-IO, like the traversal: the host passes time, datagrams from
//! the gateway and HTTP responses in, and sends the datagrams and HTTP requests it
//! queues. [`Worker`] is that host for [`Node`](crate::Node): one thread with its
//! own UDP socket, started only by [`Builder::port_mapping`](crate::Builder::port_mapping)
//! (UPnP needs TCP, and std has no non-blocking connect).

use crate::traversal::is_public;
use nobaas_core::Instant;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// The port PCP and NAT-PMP servers listen on, at the default gateway.
pub(crate) const GATEWAY_PORT: u16 = 5351;
/// Where SSDP discovery is sent.
pub(crate) const SSDP: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900));
/// First PCP and NAT-PMP retransmit; doubles, `TRIES` sends in all.
const RTO: Duration = Duration::from_millis(250);
const TRIES: u32 = 3;
/// How long SSDP answers are waited for (the search asks for them within 2 s).
const SSDP_WAIT: Duration = Duration::from_secs(3);
/// Lease asked for, in seconds; renewed at half of what the router grants.
const LIFETIME: u32 = 7200;
const MIN_RENEW: Duration = Duration::from_secs(1);
/// After every protocol failed (or the router's own address wasn't public), ask again this often.
const RETRY: Duration = Duration::from_secs(300);
/// UPnP ports tried when the node's own port is taken on the router.
const IGD_PORT_TRIES: u32 = 3;
const MAX_HTTP: usize = 64 * 1024;
/// Most time one HTTP exchange (connect, send, read the answer) may take, however
/// slowly the gateway sends its bytes.
const HTTP_DEADLINE: Duration = Duration::from_secs(3);
/// How long dropping a node waits for its mapping's delete to go out: an HTTP
/// exchange already under way, then the delete's own.
const STOP_WAIT: Duration = Duration::from_secs(7);
const PCP_VERSION: u8 = 2;
const PCP_MAP: u8 = 1;
const UDP: u8 = 17;
const PCP_UNSUPP_VERSION: u8 = 1;

/// Which protocol made a router port mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapProtocol {
    /// Port Control Protocol (RFC 6887).
    Pcp,
    /// NAT-PMP (RFC 6886), PCP's predecessor.
    NatPmp,
    /// UPnP Internet Gateway Device.
    Upnp,
}

/// Where router port mapping stands ([`Node::port_mapping`](crate::Node::port_mapping)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortMapping {
    /// Not asked for: [`Builder::port_mapping`](crate::Builder::port_mapping) is off.
    Off,
    /// Asking the gateway.
    Trying,
    /// The router forwards `addr` to this node, which offers it to peers as a public address.
    Mapped { addr: SocketAddr, protocol: MapProtocol },
    /// The router's own address (`addr`'s IP) is not public: carrier-grade NAT
    /// (100.64/10) or a private range, so another NAT sits beyond it and the
    /// mapping would not be reachable. Not offered; the mapping is removed.
    NotPublic { addr: SocketAddr, protocol: MapProtocol },
    /// No gateway answered, or each protocol was refused. Asked again every 5 minutes.
    Unavailable,
    /// Asked for, but not run while the node is under
    /// [`Privacy::RelayOnly`](crate::Privacy::RelayOnly): it gives out no address,
    /// so a mapped port would only be open for nothing. Any mapping is removed;
    /// leaving relay-only asks the router again.
    RelayOnly,
}

impl PortMapping {
    /// The mapped public address, when there is one.
    pub fn addr(&self) -> Option<SocketAddr> {
        match self {
            PortMapping::Mapped { addr, .. } => Some(*addr),
            _ => None,
        }
    }
}

/// An HTTP/1.1 request to a UPnP gateway; the host answers with
/// [`Mapper::handle_http`] before polling again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HttpRequest {
    pub to: SocketAddr,
    pub bytes: Vec<u8>,
}

/// A UPnP gateway's port mapping service.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Igd {
    at: SocketAddr,
    control: String,
    service: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Lease {
    protocol: MapProtocol,
    addr: SocketAddrV4,
    igd: Option<Igd>,
    /// Seconds; 0 for a permanent UPnP mapping (deleted on stop, never renewed).
    lifetime: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Pcp,
    NatPmpAddr,
    NatPmpMap(Ipv4Addr),
    Search,
    /// The steps below wait on an HTTP answer.
    Describe(SocketAddr),
    IgdAddr(Igd),
    IgdAdd {
        igd: Igd,
        ip: Ipv4Addr,
        port: u16,
        tries: u32,
        lifetime: u32,
    },
    /// Mapped: renews when the timer fires.
    Held,
    /// Failed or not public: starts over when the timer fires.
    Idle,
    Stopped,
}

/// One port mapping for one UDP port, sans-IO. See the module docs.
pub(crate) struct Mapper {
    gateway: SocketAddr,
    ssdp: SocketAddr,
    local: Ipv4Addr,
    port: u16,
    nonce: [u8; 12],
    rng: u64,
    step: Step,
    tries: u32,
    timer: Option<Instant>,
    state: PortMapping,
    lease: Option<Lease>,
    transmits: VecDeque<(SocketAddr, Vec<u8>)>,
    http: VecDeque<HttpRequest>,
}

impl Mapper {
    /// Maps UDP `port` at `local` (this host's address towards the gateway).
    /// `gateway`: the PCP and NAT-PMP server (the default gateway, port 5351).
    /// `ssdp`: where UPnP discovery goes ([`SSDP`]). `seed`: fresh random bytes.
    pub fn new(gateway: SocketAddr, ssdp: SocketAddr, local: Ipv4Addr, port: u16, seed: [u8; 20]) -> Self {
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&seed[..12]);
        let rng = u64::from_le_bytes(seed[12..].try_into().expect("8 bytes")) | 1;
        Mapper {
            gateway,
            ssdp,
            local,
            port,
            nonce,
            rng,
            step: Step::Idle,
            tries: 0,
            timer: None,
            state: PortMapping::Trying,
            lease: None,
            transmits: VecDeque::new(),
            http: VecDeque::new(),
        }
    }

    pub fn state(&self) -> PortMapping {
        self.state
    }

    /// Starts (or restarts) asking, from PCP down.
    pub fn start(&mut self, now: Instant) {
        self.lease = None;
        self.state = PortMapping::Trying;
        self.send(now, Step::Pcp);
    }

    /// Removes the mapping (if any) and stops: queues its delete. A PCP or
    /// NAT-PMP map request still unanswered may have made one the answer to
    /// which won't be read, so that gets a delete too, sent once.
    pub fn stop(&mut self) {
        let unanswered = match self.step {
            _ if self.lease.is_some() => None,
            Step::Pcp => Some(pcp_request(self.local, self.port, &self.nonce, 0, None)),
            Step::NatPmpMap(_) => Some(natpmp_request(self.port, 0, 0)),
            _ => None,
        };
        self.transmits.extend(unanswered.map(|d| (self.gateway, d)));
        self.release();
        (self.step, self.timer) = (Step::Stopped, None);
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.timer
    }

    pub fn poll_transmit(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        self.transmits.pop_front()
    }

    pub fn poll_http(&mut self) -> Option<HttpRequest> {
        self.http.pop_front()
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        if self.timer.is_none_or(|t| now < t) {
            return;
        }
        self.timer = None;
        match self.step.clone() {
            Step::Pcp | Step::NatPmpAddr | Step::NatPmpMap(_) if self.tries < TRIES => self.resend(now),
            Step::Pcp | Step::NatPmpAddr | Step::NatPmpMap(_) | Step::Search => self.fail(now),
            Step::Held => self.renew(now),
            Step::Idle => self.start(now),
            _ => {}
        }
    }

    /// A datagram on the mapping socket. Only the gateway's answers (and, while
    /// searching, SSDP answers) count.
    pub fn handle_datagram(&mut self, now: Instant, from: SocketAddr, data: &[u8]) {
        if from != self.gateway && self.step != Step::Search {
            return;
        }
        match self.step.clone() {
            Step::Pcp => match pcp_response(data, &self.nonce, self.port) {
                Some(Pcp::Mapped { addr, lifetime }) => self.mapped(now, MapProtocol::Pcp, addr, lifetime, None),
                // A NAT-PMP server answers PCP with its own version: ask it in that.
                Some(Pcp::NatPmpOnly) => self.send(now, Step::NatPmpAddr),
                Some(Pcp::Refused) => self.fail(now),
                None => {}
            },
            Step::NatPmpAddr => match natpmp_address(data) {
                Some(Ok(ip)) if public(ip) => self.send(now, Step::NatPmpMap(ip)),
                Some(Ok(ip)) => self.not_public(now, MapProtocol::NatPmp, SocketAddrV4::new(ip, self.port)),
                Some(Err(())) => self.fail(now),
                None => {}
            },
            Step::NatPmpMap(ip) => match natpmp_mapping(data, self.port) {
                Some(Ok((port, lifetime))) => {
                    self.mapped(now, MapProtocol::NatPmp, SocketAddrV4::new(ip, port), lifetime, None)
                }
                Some(Err(())) => self.fail(now),
                None => {}
            },
            Step::Search => {
                if let Some((at, path)) = ssdp_location(data, from) {
                    (self.step, self.timer) = (Step::Describe(at), None);
                    self.http.push_back(HttpRequest { to: at, bytes: http_get(at, &path) });
                }
            }
            _ => {}
        }
    }

    /// The answer to the last [`HttpRequest`]: the raw response, or `None` when
    /// the gateway could not be reached or didn't answer in time.
    pub fn handle_http(&mut self, now: Instant, response: Option<&[u8]>) {
        let res = response.and_then(parse_http);
        match (self.step.clone(), res) {
            (Step::Describe(at), Some((200, body))) => match find_igd(&body, at) {
                Some(igd) => {
                    self.http.push_back(soap(&igd, "GetExternalIPAddress", &[]));
                    self.step = Step::IgdAddr(igd);
                }
                None => self.fail(now),
            },
            (Step::IgdAddr(igd), Some((200, body))) => {
                match tag(&body, "NewExternalIPAddress").and_then(|s| s.parse::<Ipv4Addr>().ok()) {
                    Some(ip) if public(ip) => self.add(igd, ip, self.port, 1, LIFETIME),
                    Some(ip) => self.not_public(now, MapProtocol::Upnp, SocketAddrV4::new(ip, self.port)),
                    None => self.fail(now),
                }
            }
            (Step::IgdAdd { igd, ip, port, lifetime, .. }, Some((200, _))) => {
                self.mapped(now, MapProtocol::Upnp, SocketAddrV4::new(ip, port), lifetime, Some(igd))
            }
            (Step::IgdAdd { igd, ip, port, tries, lifetime }, Some((_, body))) => {
                match tag(&body, "errorCode").and_then(|c| c.parse::<u16>().ok()) {
                    // ConflictInMappingEntry: the port is taken on the router; try another.
                    Some(718) if tries < IGD_PORT_TRIES => {
                        let other = 49152 + (self.next_random() % 16384) as u16;
                        self.add(igd, ip, other, tries + 1, lifetime)
                    }
                    // OnlyPermanentLeasesSupported: ask for one, deleted on stop.
                    Some(725) if lifetime != 0 => self.add(igd, ip, port, tries, 0),
                    _ => self.fail(now),
                }
            }
            (Step::Describe(_) | Step::IgdAddr(_) | Step::IgdAdd { .. }, _) => self.fail(now),
            _ => {}
        }
    }

    fn add(&mut self, igd: Igd, ip: Ipv4Addr, port: u16, tries: u32, lifetime: u32) {
        let args = [
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", port.to_string()),
            ("NewProtocol", "UDP".into()),
            ("NewInternalPort", self.port.to_string()),
            ("NewInternalClient", self.local.to_string()),
            ("NewEnabled", "1".into()),
            ("NewPortMappingDescription", "nobaas".into()),
            ("NewLeaseDuration", lifetime.to_string()),
        ];
        self.http.push_back(soap(&igd, "AddPortMapping", &args));
        (self.step, self.timer) = (Step::IgdAdd { igd, ip, port, tries, lifetime }, None);
    }

    /// Sends the request for `step` (its first try).
    fn send(&mut self, now: Instant, step: Step) {
        (self.step, self.tries) = (step, 0);
        self.resend(now);
    }

    fn resend(&mut self, now: Instant) {
        self.tries += 1;
        let renew = self.lease.as_ref().map(|l| l.addr);
        let data = match self.step {
            Step::Pcp => pcp_request(self.local, self.port, &self.nonce, LIFETIME, renew),
            Step::NatPmpAddr => vec![0, 0],
            Step::NatPmpMap(_) => natpmp_request(self.port, renew.map_or(self.port, |a| a.port()), LIFETIME),
            Step::Search => {
                self.transmits.push_back((self.ssdp, SSDP_SEARCH.to_vec()));
                self.timer = Some(now + SSDP_WAIT);
                return;
            }
            _ => return,
        };
        self.transmits.push_back((self.gateway, data));
        self.timer = Some(now + RTO * (1 << (self.tries - 1)));
    }

    /// The current protocol gave up: the next one, or wait and start over.
    fn fail(&mut self, now: Instant) {
        if self.lease.take().is_some() {
            return self.start(now); // a renewal failed: the mapping may be gone
        }
        match self.step {
            Step::Pcp => self.send(now, Step::NatPmpAddr),
            Step::NatPmpAddr | Step::NatPmpMap(_) => self.send(now, Step::Search),
            _ => {
                self.state = PortMapping::Unavailable;
                (self.step, self.timer) = (Step::Idle, Some(now + RETRY));
            }
        }
    }

    fn mapped(&mut self, now: Instant, protocol: MapProtocol, addr: SocketAddrV4, lifetime: u32, igd: Option<Igd>) {
        self.lease = Some(Lease { protocol, addr, igd, lifetime });
        if !public(*addr.ip()) {
            return self.not_public(now, protocol, addr);
        }
        self.state = PortMapping::Mapped { addr: addr.into(), protocol };
        let renew = Duration::from_secs(u64::from(lifetime) / 2).max(MIN_RENEW);
        (self.step, self.timer) = (Step::Held, (lifetime != 0).then(|| now + renew));
    }

    fn not_public(&mut self, now: Instant, protocol: MapProtocol, addr: SocketAddrV4) {
        self.release();
        self.state = PortMapping::NotPublic { addr: addr.into(), protocol };
        (self.step, self.timer) = (Step::Idle, Some(now + RETRY));
    }

    /// Asks for the held mapping again, before its lease runs out.
    fn renew(&mut self, now: Instant) {
        let Some(lease) = self.lease.clone() else { return self.start(now) };
        match (lease.protocol, lease.igd) {
            (MapProtocol::Pcp, _) => self.send(now, Step::Pcp),
            (MapProtocol::NatPmp, _) => self.send(now, Step::NatPmpMap(*lease.addr.ip())),
            (MapProtocol::Upnp, Some(igd)) => self.add(igd, *lease.addr.ip(), lease.addr.port(), 1, lease.lifetime),
            (MapProtocol::Upnp, None) => self.start(now),
        }
    }

    /// Queues the delete of the mapping held, if any.
    fn release(&mut self) {
        let Some(lease) = self.lease.take() else { return };
        match (lease.protocol, lease.igd) {
            (MapProtocol::Pcp, _) => {
                let data = pcp_request(self.local, self.port, &self.nonce, 0, Some(lease.addr));
                self.transmits.push_back((self.gateway, data));
            }
            (MapProtocol::NatPmp, _) => self.transmits.push_back((self.gateway, natpmp_request(self.port, 0, 0))),
            (MapProtocol::Upnp, Some(igd)) => {
                let args = [
                    ("NewRemoteHost", String::new()),
                    ("NewExternalPort", lease.addr.port().to_string()),
                    ("NewProtocol", "UDP".into()),
                ];
                self.http.push_back(soap(&igd, "DeletePortMapping", &args));
            }
            (MapProtocol::Upnp, None) => {}
        }
    }

    /// xorshift64: only picks alternative UPnP ports.
    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
}

fn public(ip: Ipv4Addr) -> bool {
    is_public(&SocketAddr::from((ip, 1)))
}

/// A PCP MAP request for UDP `port` (RFC 6887 §11.1); lifetime 0 deletes.
fn pcp_request(local: Ipv4Addr, port: u16, nonce: &[u8; 12], lifetime: u32, suggest: Option<SocketAddrV4>) -> Vec<u8> {
    let mut b = vec![PCP_VERSION, PCP_MAP, 0, 0];
    b.extend_from_slice(&lifetime.to_be_bytes());
    b.extend_from_slice(&local.to_ipv6_mapped().octets());
    b.extend_from_slice(nonce);
    b.extend_from_slice(&[UDP, 0, 0, 0]);
    b.extend_from_slice(&port.to_be_bytes());
    b.extend_from_slice(&suggest.map_or(0, |a| a.port()).to_be_bytes());
    b.extend_from_slice(&suggest.map_or(Ipv4Addr::UNSPECIFIED, |a| *a.ip()).to_ipv6_mapped().octets());
    b
}

#[derive(Debug, PartialEq, Eq)]
enum Pcp {
    Mapped {
        addr: SocketAddrV4,
        lifetime: u32,
    },
    /// The server speaks NAT-PMP only.
    NatPmpOnly,
    Refused,
}

/// A PCP MAP response to our request (same nonce, protocol and internal port).
fn pcp_response(d: &[u8], nonce: &[u8; 12], port: u16) -> Option<Pcp> {
    match d {
        [0, ..] if d.len() >= 4 => return Some(Pcp::NatPmpOnly),
        [PCP_VERSION, 0x81, _, PCP_UNSUPP_VERSION, ..] => return Some(Pcp::NatPmpOnly),
        [PCP_VERSION, 0x81, ..] if d.len() >= 60 => {}
        _ => return None,
    }
    if d[24..36] != nonce[..] || d[36] != UDP || u16::from_be_bytes([d[40], d[41]]) != port {
        return None;
    }
    let lifetime = u32::from_be_bytes(d[4..8].try_into().ok()?);
    // Lifetime 0 means there is no mapping (RFC 6887 §15): never one to keep.
    if d[3] != 0 || lifetime == 0 {
        return Some(Pcp::Refused);
    }
    let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&d[44..60]).ok()?).to_ipv4_mapped();
    let ext = u16::from_be_bytes([d[42], d[43]]);
    match ip {
        Some(ip) if ext != 0 => Some(Pcp::Mapped { addr: SocketAddrV4::new(ip, ext), lifetime }),
        _ => Some(Pcp::Refused),
    }
}

/// A NAT-PMP UDP mapping request (RFC 6886 §3.3); lifetime 0 deletes.
fn natpmp_request(port: u16, suggest: u16, lifetime: u32) -> Vec<u8> {
    let mut b = vec![0, 1, 0, 0];
    b.extend_from_slice(&port.to_be_bytes());
    b.extend_from_slice(&suggest.to_be_bytes());
    b.extend_from_slice(&lifetime.to_be_bytes());
    b
}

/// A NAT-PMP external address answer: the address, or `Err` for a refusal.
fn natpmp_address(d: &[u8]) -> Option<Result<Ipv4Addr, ()>> {
    match d {
        [0, 128, 0, 0, _, _, _, _, a, b, c, e, ..] => Some(Ok(Ipv4Addr::new(*a, *b, *c, *e))),
        [0, 128, _, _, ..] => Some(Err(())),
        _ => None,
    }
}

/// A NAT-PMP UDP mapping answer for `port`: the external port and lifetime, or
/// `Err` (lifetime 0 included: that answers a delete, RFC 6886 §3.4).
fn natpmp_mapping(d: &[u8], port: u16) -> Option<Result<(u16, u32), ()>> {
    if d.len() < 16 || d[..2] != [0, 129] || u16::from_be_bytes([d[8], d[9]]) != port {
        return (d.len() >= 4 && d[..2] == [0, 129] && d[2..4] != [0, 0]).then_some(Err(()));
    }
    let (result, ext) = (u16::from_be_bytes([d[2], d[3]]), u16::from_be_bytes([d[10], d[11]]));
    let lifetime = u32::from_be_bytes([d[12], d[13], d[14], d[15]]);
    Some(if result == 0 && ext != 0 && lifetime != 0 { Ok((ext, lifetime)) } else { Err(()) })
}

const SSDP_SEARCH: &[u8] = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\
ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n";

/// The description URL in an SSDP answer, if it is on the host that answered and
/// that host is not public: a LAN responder can't send us to anyone else.
fn ssdp_location(d: &[u8], from: SocketAddr) -> Option<(SocketAddr, String)> {
    let text = std::str::from_utf8(d).ok()?;
    let mut lines = text.split("\r\n");
    if !lines.next()?.starts_with("HTTP/1.1 200") {
        return None;
    }
    let url = lines.find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("location")))?.1;
    let (at, path) = parse_url(url.trim())?;
    (at.ip() == from.ip() && !is_public(&at)).then_some((at, path))
}

/// `http://a.b.c.d[:port]/path` (an IPv4 literal: no names are looked up).
fn parse_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.get(..7).filter(|p| p.eq_ignore_ascii_case("http://")).map(|_| &url[7..])?;
    let (host, path) = rest.split_once('/').map_or((rest, String::from("/")), |(h, p)| (h, format!("/{p}")));
    let at = match host.parse::<SocketAddrV4>() {
        Ok(a) => a,
        Err(_) => SocketAddrV4::new(host.parse().ok()?, 80),
    };
    safe(&path).then_some((at.into(), path))
}

/// Printable ASCII with no spaces: nothing that could break out of a request line or header.
fn safe(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic())
}

/// The WANIPConnection (or else WANPPPConnection) service in an IGD description, on `at`.
fn find_igd(xml: &str, at: SocketAddr) -> Option<Igd> {
    let services: Vec<&str> = xml.split("<service>").skip(1).collect();
    let pick = |kind: &str| {
        services.iter().find_map(|s| {
            let service = tag(s, "serviceType")?;
            if !service.contains(kind) || !service.bytes().all(|b| b.is_ascii_alphanumeric() || b":.-".contains(&b)) {
                return None;
            }
            let url = tag(s, "controlURL")?;
            let control = match parse_url(url) {
                Some((a, path)) if a.ip() == at.ip() => path,
                Some(_) => return None,
                None if url.starts_with('/') => url.to_string(),
                None => format!("/{url}"),
            };
            safe(&control).then(|| Igd { at, control, service: service.to_string() })
        })
    };
    pick(":service:WANIPConnection:").or_else(|| pick(":service:WANPPPConnection:"))
}

/// The trimmed text of the first `<name>…</name>`.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{name}>"))? + name.len() + 2;
    let len = xml[start..].find(&format!("</{name}>"))?;
    Some(xml[start..start + len].trim())
}

fn http_get(at: SocketAddr, path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: {at}\r\nConnection: close\r\n\r\n").into_bytes()
}

fn soap(igd: &Igd, action: &str, args: &[(&str, String)]) -> HttpRequest {
    let args: String = args.iter().map(|(k, v)| format!("<{k}>{v}</{k}>")).collect();
    let body = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{}\">{args}\
         </u:{action}></s:Body></s:Envelope>",
        igd.service
    );
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{}#{action}\"\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        igd.control,
        igd.at,
        igd.service,
        body.len()
    );
    HttpRequest { to: igd.at, bytes: (head + &body).into_bytes() }
}

/// Splits a response into its status code and body, undoing chunked encoding.
fn parse_http(d: &[u8]) -> Option<(u16, String)> {
    let end = d.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&d[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.strip_prefix("HTTP/1.")?.get(2..5)?.parse().ok()?;
    let chunked = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding") && v.trim().eq_ignore_ascii_case("chunked")
        })
    });
    let mut body = &d[end + 4..];
    if !chunked {
        return Some((status, String::from_utf8_lossy(body).into_owned()));
    }
    let mut out = Vec::new();
    loop {
        let line = body.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&body[..line]).ok()?.split(';').next()?.trim();
        let size = usize::from_str_radix(size, 16).ok()?;
        body = &body[line + 2..];
        if size == 0 {
            return Some((status, String::from_utf8_lossy(&out).into_owned()));
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

/// Whether a response read so far is whole (by its Content-Length, or its last chunk).
fn http_complete(d: &[u8]) -> bool {
    let Some(end) = d.windows(4).position(|w| w == b"\r\n\r\n") else { return false };
    let head = String::from_utf8_lossy(&d[..end]).to_ascii_lowercase();
    let len = head.split("\r\n").find_map(|l| l.strip_prefix("content-length:")?.trim().parse::<usize>().ok());
    match len {
        // The body read so far against the length, which a gateway may set to anything.
        Some(n) => d.len() - (end + 4) >= n,
        None => head.contains("transfer-encoding: chunked") && d.ends_with(b"0\r\n\r\n"),
    }
}

/// Sends a request and reads the answer, all within `deadline` ([`HTTP_DEADLINE`]):
/// a gateway that drips its answer is cut off, not waited for.
fn http(req: &HttpRequest, deadline: Duration) -> Option<Vec<u8>> {
    let end = std::time::Instant::now() + deadline;
    // The time left, or `None` once it's up (a zero timeout would mean none at all).
    let left = || end.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero());
    let mut s = TcpStream::connect_timeout(&req.to, left()?).ok()?;
    s.set_write_timeout(Some(left()?)).ok()?;
    s.write_all(&req.bytes).ok()?;
    let (mut out, mut buf) = (Vec::new(), [0u8; 4096]);
    while out.len() <= MAX_HTTP && !http_complete(&out) {
        s.set_read_timeout(Some(left()?)).ok()?;
        match s.read(&mut buf) {
            // The gateway closed: that ends an answer without a length.
            Ok(0) => break,
            // Cut off (or timed out) mid-answer: no answer.
            Err(_) => return None,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    (!out.is_empty() && out.len() <= MAX_HTTP).then_some(out)
}

/// The IPv4 default gateway, from the routing table on Linux; elsewhere (or
/// without one), `.1` in `local`'s /24, the usual home router address.
pub(crate) fn default_gateway(local: Ipv4Addr) -> Ipv4Addr {
    let table = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    let from_table = table.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        let flags = u16::from_str_radix(f.get(3)?, 16).ok()?;
        // Destination 0 (the default route), flag RTF_GATEWAY; the kernel prints
        // the address as a number in host byte order.
        let gw = u32::from_str_radix(f.get(2)?, 16).ok()?;
        (*f.get(1)? == "00000000" && flags & 2 != 0 && gw != 0).then(|| Ipv4Addr::from(gw.to_ne_bytes()))
    });
    from_table.unwrap_or_else(|| {
        let [a, b, c, _] = local.octets();
        Ipv4Addr::new(a, b, c, 1)
    })
}

/// Runs a [`Mapper`] on its own thread and UDP socket, for a [`Node`](crate::Node).
/// Dropping it deletes the mapping, waiting up to [`STOP_WAIT`] for that to go out.
pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    /// The thread's socket, on loopback: a datagram there wakes it to stop.
    wake: SocketAddr,
    states: mpsc::Receiver<PortMapping>,
    done: mpsc::Receiver<()>,
}

impl Worker {
    /// Maps UDP `port` through `gateway` (PCP and NAT-PMP) and `ssdp` (UPnP).
    pub fn spawn(port: u16, gateway: SocketAddr, ssdp: SocketAddr, seed: [u8; 20]) -> io::Result<Worker> {
        let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        probe.connect(gateway)?; // no packet: only finds the address the gateway knows us by
        let IpAddr::V4(local) = probe.local_addr()?.ip() else { return Err(ErrorKind::Unsupported.into()) };
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let _ = sock.set_multicast_ttl_v4(2);
        let wake = SocketAddr::from((Ipv4Addr::LOCALHOST, sock.local_addr()?.port()));
        let stop = Arc::new(AtomicBool::new(false));
        let (state_tx, states) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let mapper = Mapper::new(gateway, ssdp, local, port, seed);
        let flag = stop.clone();
        std::thread::Builder::new().name("nobaas-portmap".into()).spawn(move || {
            run(mapper, sock, &flag, &state_tx);
            let _ = done_tx.send(());
        })?;
        Ok(Worker { stop, wake, states, done })
    }

    /// The latest state, if it changed since the last call.
    pub fn poll(&self) -> Option<PortMapping> {
        self.states.try_iter().last()
    }
}

impl Worker {
    /// Tells the thread to delete the mapping and end, without waiting for it.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(s) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            let _ = s.send_to(&[0], self.wake);
        }
    }

    /// Stops the thread, which deletes the mapping first. Whether it finished
    /// within [`STOP_WAIT`].
    fn shut_down(&mut self) -> bool {
        self.stop();
        self.done.recv_timeout(STOP_WAIT).is_ok()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shut_down();
    }
}

fn run(mut m: Mapper, sock: UdpSocket, stop: &AtomicBool, states: &mpsc::Sender<PortMapping>) {
    let epoch = std::time::Instant::now();
    let now = || Instant::from_micros(epoch.elapsed().as_micros() as u64);
    let mut buf = [0u8; 1500];
    let mut last = None;
    m.start(now());
    loop {
        let mut stopping = stop.load(Ordering::Acquire);
        if stopping {
            m.stop();
        }
        loop {
            while let Some((to, data)) = m.poll_transmit() {
                let _ = sock.send_to(&data, to);
            }
            let Some(req) = m.poll_http() else { break };
            // Stopped mid-chain (UPnP: description, address, Add retries): drop the
            // rest, so at most one exchange runs before the delete.
            if !stopping && stop.load(Ordering::Acquire) {
                stopping = true;
                m.stop();
                continue;
            }
            let res = http(&req, HTTP_DEADLINE);
            m.handle_http(now(), res.as_deref());
        }
        if last != Some(m.state()) {
            last = Some(m.state());
            if states.send(m.state()).is_err() {
                return;
            }
        }
        if stopping {
            return;
        }
        let wait = m.poll_timeout().map(|t| t.saturating_duration_since(now()).max(Duration::from_millis(1)));
        let _ = sock.set_read_timeout(wait);
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            m.handle_datagram(now(), from, &buf[..n]);
        }
        m.handle_timeout(now());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Mutex;

    const PORT: u16 = 40000;
    const LOCAL: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);
    const GW: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), GATEWAY_PORT));
    const EXT: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);
    const CGNAT: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 9);

    fn t(ms: u64) -> Instant {
        Instant::from_millis(ms)
    }

    fn mapper() -> Mapper {
        let mut m = Mapper::new(GW, SSDP, LOCAL, PORT, [7; 20]);
        m.start(t(0));
        m
    }

    fn sent(m: &mut Mapper) -> Vec<(SocketAddr, Vec<u8>)> {
        std::iter::from_fn(|| m.poll_transmit()).collect()
    }

    /// The one datagram `m` sent, to the gateway.
    fn one(m: &mut Mapper) -> Vec<u8> {
        let s = sent(m);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].0, GW);
        s[0].1.clone()
    }

    /// A PCP server's answer to `req`.
    fn pcp_answer(req: &[u8], result: u8, lifetime: u32, ext: SocketAddrV4) -> Vec<u8> {
        let mut b = vec![2, 0x81, 0, result];
        b.extend_from_slice(&lifetime.to_be_bytes());
        b.extend_from_slice(&[0; 16]); // epoch, reserved
        b.extend_from_slice(&req[24..42]); // nonce, protocol, internal port
        b.extend_from_slice(&ext.port().to_be_bytes());
        b.extend_from_slice(&ext.ip().to_ipv6_mapped().octets());
        b
    }

    fn natpmp_addr_answer(result: u16, ip: Ipv4Addr) -> Vec<u8> {
        let mut b = vec![0, 128];
        b.extend_from_slice(&result.to_be_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&ip.octets());
        b
    }

    fn natpmp_map_answer(req: &[u8], result: u16, ext: u16, lifetime: u32) -> Vec<u8> {
        let mut b = vec![0, 129];
        b.extend_from_slice(&result.to_be_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&req[4..6]);
        b.extend_from_slice(&ext.to_be_bytes());
        b.extend_from_slice(&lifetime.to_be_bytes());
        b
    }

    pub(crate) fn lifetime_of(pcp: &[u8]) -> u32 {
        u32::from_be_bytes(pcp[4..8].try_into().unwrap())
    }

    const DESC: &str = "<root><device><serviceList>\
        <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
        <controlURL>/ctl/L3F</controlURL></service>\
        <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
        <controlURL>/ctl/IPConn</controlURL></service></serviceList></device></root>";

    fn http_answer(status: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    fn soap_error(code: u16) -> Vec<u8> {
        http_answer(
            "500 Internal Server Error",
            &format!("<s:Envelope><UPnPError><errorCode>{code}</errorCode></UPnPError></s:Envelope>"),
        )
    }

    fn ssdp_answer(location: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLocation: {location}\r\n\r\n").into_bytes()
    }

    /// Drives `m` through PCP and NAT-PMP refusals to its SSDP search.
    fn to_search(m: &mut Mapper) {
        let req = one(m);
        m.handle_datagram(t(1), GW, &pcp_answer(&req, 2, 0, SocketAddrV4::new(EXT, 0))); // NOT_AUTHORIZED
        assert_eq!(one(m), vec![0, 0], "NAT-PMP next");
        m.handle_datagram(t(2), GW, &natpmp_addr_answer(3, EXT)); // network failure
        assert_eq!(sent(m), vec![(SSDP, SSDP_SEARCH.to_vec())], "then UPnP, without waiting");
    }

    /// Answers the description and external address requests of a UPnP gateway at `at`.
    fn to_add(m: &mut Mapper, at: SocketAddr, ip: Ipv4Addr) -> Option<HttpRequest> {
        to_search(m);
        m.handle_datagram(t(3), at, &ssdp_answer(&format!("http://{at}/rootDesc.xml")));
        assert_eq!(m.poll_http(), Some(HttpRequest { to: at, bytes: http_get(at, "/rootDesc.xml") }));
        m.handle_http(t(4), Some(&http_answer("200 OK", DESC)));
        let req = m.poll_http().unwrap();
        assert!(String::from_utf8_lossy(&req.bytes).contains("WANIPConnection:1#GetExternalIPAddress"));
        let body = format!("<NewExternalIPAddress>{ip}</NewExternalIPAddress>");
        m.handle_http(t(5), Some(&http_answer("200 OK", &body)));
        m.poll_http()
    }

    #[test]
    fn pcp_maps_renews_at_half_the_lease_and_deletes_on_stop() {
        let mut m = mapper();
        assert_eq!(m.state(), PortMapping::Trying);
        let req = one(&mut m);
        assert_eq!(req.len(), 60);
        assert_eq!((req[0], req[1], lifetime_of(&req)), (2, 1, LIFETIME));
        assert_eq!(&req[8..24], &LOCAL.to_ipv6_mapped().octets(), "client address");
        assert_eq!((req[36], u16::from_be_bytes([req[40], req[41]])), (UDP, PORT));
        let ext = SocketAddrV4::new(EXT, 41000);
        // Answers from elsewhere, or for another nonce, don't count.
        m.handle_datagram(t(5), "192.168.1.66:5351".parse().unwrap(), &pcp_answer(&req, 0, 120, ext));
        let mut other = req.clone();
        other[24] ^= 1;
        m.handle_datagram(t(5), GW, &pcp_answer(&other, 0, 120, ext));
        assert_eq!(m.state(), PortMapping::Trying);
        m.handle_datagram(t(5), GW, &pcp_answer(&req, 0, 120, ext));
        let mapped = PortMapping::Mapped { addr: ext.into(), protocol: MapProtocol::Pcp };
        assert_eq!((m.state(), m.state().addr()), (mapped, Some(ext.into())));
        assert_eq!(m.poll_timeout(), Some(t(60_005)), "renewed at half the lease");
        m.handle_timeout(t(60_005));
        let renew = one(&mut m);
        assert_eq!(
            (&renew[24..36], &renew[42..44]),
            (&req[24..36], &41000u16.to_be_bytes()[..]),
            "same nonce, same port"
        );
        m.handle_datagram(t(60_010), GW, &pcp_answer(&renew, 0, 120, ext));
        assert_eq!((m.state(), m.poll_timeout()), (mapped, Some(t(120_010))));
        m.stop();
        let del = one(&mut m);
        assert_eq!((lifetime_of(&del), &del[24..36]), (0, &req[24..36]), "deleted, same nonce");
        assert_eq!(m.poll_timeout(), None);
    }

    /// NPM-3: a map request sent but not answered may still have made a mapping,
    /// so stopping then deletes it, once, by the same nonce (PCP) or port (NAT-PMP).
    #[test]
    fn stop_deletes_a_map_request_still_unanswered() {
        let mut m = mapper();
        let req = one(&mut m);
        m.handle_timeout(t(250));
        assert_eq!(one(&mut m), req, "retransmitted");
        m.stop();
        let del = one(&mut m);
        assert_eq!((lifetime_of(&del), &del[24..42]), (0, &req[24..42]), "deleted, same nonce and port");
        assert_eq!(m.poll_timeout(), None, "sent once");
        m.stop();
        assert!(sent(&mut m).is_empty(), "nothing more");

        let mut m = mapper();
        one(&mut m);
        m.handle_datagram(t(1), GW, &[0, 129, 0, 1, 0, 0, 0, 0]);
        assert_eq!(one(&mut m), vec![0, 0]);
        let mut asking_address = mapper();
        one(&mut asking_address);
        asking_address.handle_datagram(t(1), GW, &[0, 129, 0, 1, 0, 0, 0, 0]);
        one(&mut asking_address);
        asking_address.stop();
        assert!(sent(&mut asking_address).is_empty(), "no mapping asked for yet");
        m.handle_datagram(t(2), GW, &natpmp_addr_answer(0, EXT));
        assert_eq!(one(&mut m), natpmp_request(PORT, PORT, LIFETIME));
        m.stop();
        assert_eq!(one(&mut m), natpmp_request(PORT, 0, 0), "deleted");
        assert_eq!(m.poll_timeout(), None);
    }

    #[test]
    fn natpmp_after_pcp_unsupported_version() {
        let mut m = mapper();
        let req = one(&mut m);
        m.handle_datagram(t(1), GW, &[0, 129, 0, 1, 0, 0, 0, 0]); // NAT-PMP server: unsupported version
        assert_eq!(one(&mut m), vec![0, 0]);
        m.handle_datagram(t(2), GW, &natpmp_addr_answer(0, EXT));
        let map = one(&mut m);
        assert_eq!(map, natpmp_request(PORT, PORT, LIFETIME));
        m.handle_datagram(t(3), GW, &natpmp_map_answer(&map, 0, 40001, 60));
        let addr = SocketAddr::from((EXT, 40001));
        assert_eq!(m.state(), PortMapping::Mapped { addr, protocol: MapProtocol::NatPmp });
        assert_eq!(m.poll_timeout(), Some(t(30_003)));
        m.handle_timeout(t(30_003));
        assert_eq!(one(&mut m), natpmp_request(PORT, 40001, LIFETIME), "renewal asks for the same port");
        m.stop();
        assert_eq!(one(&mut m), natpmp_request(PORT, 0, 0), "delete");
        // A PCP version-2 server that says UNSUPP_VERSION is treated the same way.
        let mut m = mapper();
        let req2 = one(&mut m);
        let mut unsupp = pcp_answer(&req2, 1, 0, SocketAddrV4::new(EXT, 0));
        unsupp.truncate(24);
        m.handle_datagram(t(1), GW, &unsupp);
        assert_eq!(one(&mut m), vec![0, 0]);
        let _ = req;
    }

    #[test]
    fn upnp_after_refusals_maps_and_deletes() {
        let at: SocketAddr = "192.168.1.1:5000".parse().unwrap();
        let mut m = mapper();
        let add = to_add(&mut m, at, EXT).unwrap();
        let text = String::from_utf8(add.bytes.clone()).unwrap();
        assert!(text.starts_with("POST /ctl/IPConn HTTP/1.1\r\nHost: 192.168.1.1:5000\r\n"), "{text}");
        for part in [
            "SOAPAction: \"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\"",
            "<NewExternalPort>40000</NewExternalPort><NewProtocol>UDP</NewProtocol>",
            "<NewInternalPort>40000</NewInternalPort><NewInternalClient>192.168.1.20</NewInternalClient>",
            "<NewLeaseDuration>7200</NewLeaseDuration>",
        ] {
            assert!(text.contains(part), "{part} in {text}");
        }
        let len = text.split("\r\n\r\n").nth(1).unwrap().len();
        assert!(text.contains(&format!("Content-Length: {len}\r\n")));
        m.handle_http(t(6), Some(&http_answer("200 OK", "")));
        let addr = SocketAddr::from((EXT, PORT));
        assert_eq!(m.state(), PortMapping::Mapped { addr, protocol: MapProtocol::Upnp });
        assert_eq!(m.poll_timeout(), Some(t(3_600_006)));
        m.handle_timeout(t(3_600_006));
        assert!(String::from_utf8(m.poll_http().unwrap().bytes).unwrap().contains("#AddPortMapping"), "renewal");
        m.handle_http(t(3_600_010), Some(&http_answer("200 OK", "")));
        m.stop();
        let del = String::from_utf8(m.poll_http().unwrap().bytes).unwrap();
        assert!(
            del.contains("#DeletePortMapping") && del.contains("<NewExternalPort>40000</NewExternalPort>"),
            "{del}"
        );
    }

    #[test]
    fn upnp_conflict_tries_other_ports_and_permanent_leases() {
        let at: SocketAddr = "192.168.1.1:5000".parse().unwrap();
        let mut m = mapper();
        to_add(&mut m, at, EXT).unwrap();
        m.handle_http(t(6), Some(&soap_error(718)));
        let retry = String::from_utf8(m.poll_http().unwrap().bytes).unwrap();
        let port: u16 = tag(&retry, "NewExternalPort").unwrap().parse().unwrap();
        assert!(port >= 49152 && port != PORT, "{port}");
        m.handle_http(t(7), Some(&soap_error(725)));
        let permanent = String::from_utf8(m.poll_http().unwrap().bytes).unwrap();
        assert!(permanent.contains(&format!("<NewExternalPort>{port}</NewExternalPort>")));
        assert!(permanent.contains("<NewLeaseDuration>0</NewLeaseDuration>"));
        m.handle_http(t(8), Some(&http_answer("200 OK", "")));
        let addr = SocketAddr::from((EXT, port));
        assert_eq!((m.state(), m.poll_timeout()), (PortMapping::Mapped { addr, protocol: MapProtocol::Upnp }, None));
        m.stop();
        assert!(String::from_utf8(m.poll_http().unwrap().bytes).unwrap().contains("#DeletePortMapping"));
        // Conflicts on every port: gives up after IGD_PORT_TRIES.
        let mut m = mapper();
        to_add(&mut m, at, EXT).unwrap();
        for _ in 1..IGD_PORT_TRIES {
            m.handle_http(t(6), Some(&soap_error(718)));
            assert!(m.poll_http().is_some());
        }
        m.handle_http(t(6), Some(&soap_error(718)));
        assert_eq!((m.poll_http(), m.state()), (None, PortMapping::Unavailable));
    }

    #[test]
    fn timeouts_fall_through_every_protocol_then_retry_later() {
        let mut m = mapper();
        let (mut log, mut now) = (Vec::new(), t(0));
        while m.state() == PortMapping::Trying {
            log.extend(sent(&mut m).into_iter().map(|(to, d)| (now, to, d.len())));
            now = m.poll_timeout().unwrap();
            m.handle_timeout(now);
        }
        let ms = |i: usize| log[i].0.as_micros() / 1000;
        assert_eq!(log.len(), 7, "{log:?}");
        assert_eq!((0..7).map(ms).collect::<Vec<_>>(), [0, 250, 750, 1750, 2000, 2500, 3500]);
        assert!(log[..3].iter().all(|l| l.1 == GW && l.2 == 60), "3 PCP tries");
        assert!(log[3..6].iter().all(|l| l.1 == GW && l.2 == 2), "3 NAT-PMP tries");
        assert_eq!((log[6].1, log[6].2), (SSDP, SSDP_SEARCH.len()));
        assert_eq!(m.state(), PortMapping::Unavailable);
        let again = m.poll_timeout().unwrap();
        assert_eq!(again, t(3500) + SSDP_WAIT + RETRY);
        m.handle_timeout(again);
        assert_eq!((m.state(), one(&mut m).len()), (PortMapping::Trying, 60), "starts over at PCP");
    }

    #[test]
    fn a_failed_renewal_starts_over() {
        let mut m = mapper();
        let req = one(&mut m);
        m.handle_datagram(t(1), GW, &pcp_answer(&req, 0, 10, SocketAddrV4::new(EXT, PORT)));
        let mut now = m.poll_timeout().unwrap();
        for _ in 0..TRIES {
            m.handle_timeout(now);
            assert_eq!(one(&mut m).len(), 60);
            now = m.poll_timeout().unwrap();
        }
        assert!(m.state().addr().is_some(), "kept while the renewal is tried");
        m.handle_timeout(now);
        assert_eq!(m.state(), PortMapping::Trying, "dropped once it fails");
        let fresh = one(&mut m);
        assert_eq!(&fresh[42..44], &[0, 0], "a fresh request, no suggestion");
    }

    #[test]
    fn a_router_behind_another_nat_is_not_advertised() {
        // PCP: the mapping is deleted at once.
        let mut m = mapper();
        let req = one(&mut m);
        m.handle_datagram(t(1), GW, &pcp_answer(&req, 0, 120, SocketAddrV4::new(CGNAT, 41000)));
        let addr = SocketAddr::from((CGNAT, 41000));
        assert_eq!((m.state(), m.state().addr()), (PortMapping::NotPublic { addr, protocol: MapProtocol::Pcp }, None));
        assert_eq!(lifetime_of(&one(&mut m)), 0, "deleted");
        assert_eq!(m.poll_timeout(), Some(t(1) + RETRY));
        // NAT-PMP: no mapping is asked for.
        let mut m = mapper();
        one(&mut m);
        m.handle_datagram(t(1), GW, &[0, 129, 0, 1]);
        one(&mut m);
        m.handle_datagram(t(2), GW, &natpmp_addr_answer(0, Ipv4Addr::new(10, 0, 0, 2)));
        assert!(matches!(m.state(), PortMapping::NotPublic { protocol: MapProtocol::NatPmp, .. }));
        assert_eq!(sent(&mut m), vec![]);
        // UPnP: neither.
        let mut m = mapper();
        assert_eq!(to_add(&mut m, "192.168.1.1:5000".parse().unwrap(), CGNAT), None);
        assert!(matches!(m.state(), PortMapping::NotPublic { protocol: MapProtocol::Upnp, .. }));
    }

    #[test]
    fn a_zero_lifetime_answer_is_no_mapping() {
        // Lifetime 0 means deleted: PCP counts it as a refusal, so NAT-PMP is asked next...
        let ext = SocketAddrV4::new(EXT, 41000);
        let mut m = mapper();
        let req = one(&mut m);
        m.handle_datagram(t(1), GW, &pcp_answer(&req, 0, 0, ext));
        assert_eq!((m.state(), one(&mut m)), (PortMapping::Trying, vec![0, 0]), "NAT-PMP next");
        // ...and so does NAT-PMP, so UPnP is.
        m.handle_datagram(t(2), GW, &natpmp_addr_answer(0, EXT));
        let map = one(&mut m);
        m.handle_datagram(t(3), GW, &natpmp_map_answer(&map, 0, 40001, 0));
        assert_eq!(m.state(), PortMapping::Trying);
        assert_eq!(sent(&mut m), vec![(SSDP, SSDP_SEARCH.to_vec())], "then UPnP");
        // A renewal answered with lifetime 0 drops the address and starts over.
        let mut m = mapper();
        let req = one(&mut m);
        m.handle_datagram(t(1), GW, &pcp_answer(&req, 0, 120, ext));
        m.handle_timeout(t(60_001));
        let renew = one(&mut m);
        m.handle_datagram(t(60_002), GW, &pcp_answer(&renew, 0, 0, ext));
        assert_eq!(m.state(), PortMapping::Trying);
        assert_eq!(&one(&mut m)[42..44], &[0, 0], "a fresh request");
    }

    #[test]
    fn ssdp_answers_must_point_at_their_lan_sender() {
        let lan: SocketAddr = "192.168.1.1:1900".parse().unwrap();
        let ok = ssdp_answer("http://192.168.1.1:5000/rootDesc.xml");
        assert_eq!(ssdp_location(&ok, lan), Some(("192.168.1.1:5000".parse().unwrap(), "/rootDesc.xml".into())));
        let elsewhere = ssdp_answer("http://192.168.1.99:5000/rootDesc.xml");
        assert_eq!(ssdp_location(&elsewhere, lan), None, "another host");
        let public = ssdp_answer("http://198.51.100.1/d.xml");
        assert_eq!(ssdp_location(&public, "198.51.100.1:1900".parse().unwrap()), None, "a public host");
        for bad in ["http://router.lan/d.xml", "https://192.168.1.1/d.xml", "http://192.168.1.1/a b", "http://[::1]/x"]
        {
            assert_eq!(ssdp_location(&ssdp_answer(bad), lan), None, "{bad}");
        }
        assert_eq!(ssdp_location(b"NOTIFY * HTTP/1.1\r\nLocation: http://192.168.1.1/x\r\n\r\n", lan), None);
        assert_eq!(parse_url("http://192.168.1.1"), Some(("192.168.1.1:80".parse().unwrap(), "/".into())));
        // The search waits for SSDP answers only, from anyone, but a datagram that isn't one changes nothing.
        let mut m = mapper();
        to_search(&mut m);
        m.handle_datagram(t(3), "127.0.0.1:9".parse().unwrap(), &[0]);
        assert_eq!((m.poll_http(), m.poll_timeout()), (None, Some(t(2) + SSDP_WAIT)));
    }

    #[test]
    fn igd_descriptions_and_http_answers() {
        let at: SocketAddr = "192.168.1.1:5000".parse().unwrap();
        let igd = find_igd(DESC, at).unwrap();
        assert_eq!(
            (igd.control.as_str(), igd.service.as_str()),
            ("/ctl/IPConn", "urn:schemas-upnp-org:service:WANIPConnection:1")
        );
        let ppp = "<service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>\
                   <controlURL>http://192.168.1.1:5000/ppp</controlURL></service>";
        assert_eq!(find_igd(ppp, at).unwrap().control, "/ppp");
        assert_eq!(find_igd(&ppp.replace("192.168.1.1", "192.168.1.9"), at), None, "control on another host");
        assert_eq!(find_igd(&ppp.replace("/ppp", "/p\r\nX: y"), at), None, "header injection");
        assert_eq!(find_igd(&ppp.replace("WANPPPConnection:1", "WANPPPConnection:1\"><x"), at), None);
        assert_eq!(find_igd(&ppp.replace("http://192.168.1.1:5000/ppp", "ppp"), at).unwrap().control, "/ppp");
        assert_eq!(find_igd("<root/>", at), None);
        assert_eq!(parse_http(&http_answer("200 OK", "hi")), Some((200, "hi".into())));
        let chunked =
            b"HTTP/1.1 500 Error\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nab\r\n\r\n3;x=1\r\ncde\r\n0\r\n\r\n";
        assert_eq!(parse_http(chunked), Some((500, "ab\r\ncde".into())));
        assert!(http_complete(chunked) && http_complete(&http_answer("200 OK", "hi")));
        assert!(!http_complete(&http_answer("200 OK", "hi")[..39]));
        for cut in 0..chunked.len() - 1 {
            let _ = parse_http(&chunked[..cut]); // never panics
        }
        assert_eq!(parse_http(b"garbage"), None);
    }

    /// NB-014 (found by fuzz_gateway_answers): a Content-Length near usize::MAX
    /// overflowed the completeness check, a panic in debug builds on the mapping
    /// thread. Such an answer is never complete: reading goes on to MAX_HTTP.
    #[test]
    fn huge_content_length_is_not_complete() {
        for n in [u64::MAX, u64::MAX - 3, u64::MAX / 2] {
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n");
            assert!(!http_complete(head.as_bytes()));
            assert!(!http_complete(format!("{head}{DESC}").as_bytes()));
        }
        assert!(http_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"));
    }

    // Mock gateways on loopback, driving the real worker thread.

    /// A UDP gateway on loopback answering with `answer` (`None`: silence); keeps the requests.
    fn mock_gateway(
        answer: impl Fn(&[u8]) -> Option<Vec<u8>> + Send + 'static,
    ) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let at = sock.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                seen.lock().unwrap().push(buf[..n].to_vec());
                if let Some(a) = answer(&buf[..n]) {
                    let _ = sock.send_to(&a, from);
                }
            }
        });
        (at, log)
    }

    /// Waits up to 5 s for `f`.
    pub(crate) fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// A PCP gateway on loopback mapping port 41000 at `ip`.
    pub(crate) fn pcp_gateway(ip: Ipv4Addr, lifetime: u32) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
        mock_gateway(move |r| (r[0] == 2).then(|| pcp_answer(r, 0, lifetime, SocketAddrV4::new(ip, 41000))))
    }

    /// Waits for `w` to reach `want`.
    fn reaches(w: &Worker, want: PortMapping) -> bool {
        let mut state = PortMapping::Trying;
        eventually(|| {
            state = w.poll().unwrap_or(state);
            state == want
        })
    }

    #[test]
    fn worker_maps_with_pcp_renews_and_deletes_on_drop() {
        let (gw, log) = pcp_gateway(EXT, 2);
        let w = Worker::spawn(PORT, gw, "127.0.0.1:9".parse().unwrap(), [9; 20]).unwrap();
        assert!(reaches(&w, PortMapping::Mapped { addr: SocketAddr::from((EXT, 41000)), protocol: MapProtocol::Pcp }));
        // A 2 s lease is renewed after 1 s.
        assert!(eventually(|| log.lock().unwrap().len() >= 2), "renewed");
        assert_eq!(lifetime_of(&log.lock().unwrap()[1]), LIFETIME);
        drop(w);
        assert!(eventually(|| log.lock().unwrap().last().is_some_and(|r| lifetime_of(r) == 0)), "deleted on drop");
    }

    /// NPM-3: a worker stopped while its PCP request is unanswered (a router
    /// answering in 200 ms) still deletes whatever that request mapped.
    #[test]
    fn worker_stopped_before_the_answer_deletes_the_request() {
        let (gw, log) = mock_gateway(|r| {
            (r[0] == 2 && lifetime_of(r) != 0).then(|| {
                std::thread::sleep(Duration::from_millis(200));
                pcp_answer(r, 0, lifetime_of(r), SocketAddrV4::new(EXT, 41000))
            })
        });
        let w = Worker::spawn(PORT, gw, "127.0.0.1:9".parse().unwrap(), [9; 20]).unwrap();
        assert!(eventually(|| !log.lock().unwrap().is_empty()), "asked");
        drop(w);
        std::thread::sleep(Duration::from_secs(1));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2, "the request and one delete");
        assert_eq!(lifetime_of(&log[0]), LIFETIME);
        assert_eq!((lifetime_of(&log[1]), &log[1][24..42]), (0, &log[0][24..42]), "deleted, same nonce");
    }

    #[test]
    fn worker_maps_with_natpmp() {
        let (gw, log) = mock_gateway(|r| match r {
            [2, ..] => Some(vec![0, 129, 0, 1, 0, 0, 0, 0]), // unsupported version
            [0, 0] => Some(natpmp_addr_answer(0, EXT)),
            [0, 1, ..] => Some(natpmp_map_answer(r, 0, 40002, 3600)),
            _ => None,
        });
        let w = Worker::spawn(PORT, gw, "127.0.0.1:9".parse().unwrap(), [9; 20]).unwrap();
        assert!(reaches(
            &w,
            PortMapping::Mapped { addr: SocketAddr::from((EXT, 40002)), protocol: MapProtocol::NatPmp }
        ));
        drop(w);
        assert!(eventually(|| log.lock().unwrap().last() == Some(&natpmp_request(PORT, 0, 0))), "deleted on drop");
    }

    #[test]
    fn worker_reports_a_cgnat_router() {
        let (gw, log) = pcp_gateway(CGNAT, 120);
        let w = Worker::spawn(PORT, gw, "127.0.0.1:9".parse().unwrap(), [9; 20]).unwrap();
        let addr = SocketAddr::from((CGNAT, 41000));
        assert!(reaches(&w, PortMapping::NotPublic { addr, protocol: MapProtocol::Pcp }));
        assert!(eventually(|| log.lock().unwrap().len() == 2));
        assert_eq!(lifetime_of(&log.lock().unwrap()[1]), 0, "removed at once");
    }

    /// A gateway on loopback that refuses PCP and NAT-PMP.
    fn refusing_gateway() -> SocketAddr {
        let (gw, _) = mock_gateway(|r| match r {
            [2, ..] => Some(pcp_answer(r, 2, 0, SocketAddrV4::new(EXT, 0))),
            [0, 0] => Some(natpmp_addr_answer(2, EXT)),
            _ => None,
        });
        gw
    }

    /// An SSDP responder on loopback pointing at a description at `at`.
    fn ssdp_responder(at: SocketAddr) -> SocketAddr {
        let location = format!("http://{at}/rootDesc.xml");
        mock_gateway(move |_| Some(ssdp_answer(&location))).0
    }

    /// A UPnP gateway on loopback: SSDP on UDP, description and SOAP on HTTP. PCP
    /// and NAT-PMP are refused. Keeps the SOAP actions it was asked.
    fn upnp_gateway() -> (SocketAddr, SocketAddr, Arc<Mutex<Vec<String>>>) {
        let gw = refusing_gateway();
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = http.local_addr().unwrap();
        let ssdp = ssdp_responder(at);
        let actions = Arc::new(Mutex::new(Vec::new()));
        let seen = actions.clone();
        std::thread::spawn(move || {
            for s in http.incoming() {
                let mut s = s.unwrap();
                let (mut req, mut buf) = (Vec::new(), [0u8; 4096]);
                while !http_complete(&req) && !(req.starts_with(b"GET") && req.ends_with(b"\r\n\r\n")) {
                    let n = s.read(&mut buf).unwrap();
                    req.extend_from_slice(&buf[..n]);
                }
                let req = String::from_utf8(req).unwrap();
                let action = req.split("#").nth(1).and_then(|a| a.split('"').next()).unwrap_or("GET").to_string();
                let body = match action.as_str() {
                    "GET" => DESC.to_string(),
                    "GetExternalIPAddress" => format!("<NewExternalIPAddress>{EXT}</NewExternalIPAddress>"),
                    _ => String::new(),
                };
                seen.lock().unwrap().push(action);
                // Chunked, as some routers answer.
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(answer.as_bytes());
            }
        });
        (gw, ssdp, actions)
    }

    #[test]
    fn worker_maps_with_upnp_after_refusals_and_deletes_on_drop() {
        let (gw, ssdp, actions) = upnp_gateway();
        let w = Worker::spawn(PORT, gw, ssdp, [9; 20]).unwrap();
        assert!(reaches(&w, PortMapping::Mapped { addr: SocketAddr::from((EXT, PORT)), protocol: MapProtocol::Upnp }));
        drop(w);
        let want = ["GET", "GetExternalIPAddress", "AddPortMapping", "DeletePortMapping"];
        assert!(eventually(|| *actions.lock().unwrap() == want), "{:?}", actions.lock().unwrap());
    }

    /// An HTTP server on loopback that answers anything with `Content-Length: 1000`,
    /// then sends one byte every 200 ms. Counts the connections it took.
    fn dripping_server() -> (SocketAddr, Arc<Mutex<usize>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        let taken = Arc::new(Mutex::new(0));
        let count = taken.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let mut s = s.unwrap();
                *count.lock().unwrap() += 1;
                std::thread::spawn(move || {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
                    while s.write_all(b"x").is_ok() {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                });
            }
        });
        (at, taken)
    }

    #[test]
    fn http_gives_up_on_a_dripping_gateway_at_its_deadline() {
        let (at, _) = dripping_server();
        let start = std::time::Instant::now();
        assert_eq!(http(&HttpRequest { to: at, bytes: http_get(at, "/") }, HTTP_DEADLINE), None);
        let took = start.elapsed();
        assert!(took >= HTTP_DEADLINE && took < HTTP_DEADLINE + Duration::from_secs(1), "{took:?}");
        // One that sends half an answer and goes quiet: the read times out, and half an answer is none.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut s = l.accept().unwrap().0;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhalf").unwrap();
            std::thread::sleep(Duration::from_secs(2));
        });
        assert_eq!(http(&HttpRequest { to: at, bytes: http_get(at, "/") }, Duration::from_millis(300)), None);
    }

    #[test]
    fn worker_stuck_on_a_dripping_gateway_still_stops() {
        let (at, taken) = dripping_server();
        let mut w = Worker::spawn(PORT, refusing_gateway(), ssdp_responder(at), [9; 20]).unwrap();
        assert!(eventually(|| *taken.lock().unwrap() == 1), "fetching the description");
        let start = std::time::Instant::now();
        assert!(w.shut_down(), "the thread finished");
        assert!(start.elapsed() < HTTP_DEADLINE + Duration::from_secs(1), "{:?}", start.elapsed());
    }

    type Log = Arc<Mutex<Vec<String>>>;

    /// A UPnP gateway on loopback whose `slow` actions answer after 2.8 s, and whose
    /// AddPortMappings get `adds` in turn (0: OK, else that UPnP error). Keeps the
    /// actions it was asked and the ports it holds mapped.
    fn slow_upnp_gateway(slow: &'static [&'static str], adds: Vec<u16>) -> (SocketAddr, SocketAddr, Log, Log) {
        let gw = refusing_gateway();
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let ssdp = ssdp_responder(http.local_addr().unwrap());
        let (actions, held) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        let (seen, table) = (actions.clone(), held.clone());
        std::thread::spawn(move || {
            let mut adds = adds.into_iter();
            for s in http.incoming() {
                let mut s = s.unwrap();
                let (mut req, mut buf) = (Vec::new(), [0u8; 4096]);
                while !http_complete(&req) && !(req.starts_with(b"GET") && req.ends_with(b"\r\n\r\n")) {
                    let n = s.read(&mut buf).unwrap();
                    req.extend_from_slice(&buf[..n]);
                }
                let req = String::from_utf8(req).unwrap();
                let action = req.split("#").nth(1).and_then(|a| a.split('"').next()).unwrap_or("GET").to_string();
                seen.lock().unwrap().push(action.clone());
                if slow.contains(&action.as_str()) {
                    std::thread::sleep(Duration::from_millis(2800));
                }
                let port = tag(&req, "NewExternalPort").unwrap_or_default().to_string();
                let answer = match action.as_str() {
                    "GET" => http_answer("200 OK", DESC),
                    "GetExternalIPAddress" => {
                        http_answer("200 OK", &format!("<NewExternalIPAddress>{EXT}</NewExternalIPAddress>"))
                    }
                    "AddPortMapping" => match adds.next().unwrap_or(0) {
                        0 => {
                            table.lock().unwrap().push(port);
                            http_answer("200 OK", "")
                        }
                        code => http_answer("500 Error", &format!("<errorCode>{code}</errorCode>")),
                    },
                    _ => {
                        table.lock().unwrap().retain(|p| *p != port);
                        http_answer("200 OK", "")
                    }
                };
                let _ = s.write_all(&answer);
            }
        });
        (gw, ssdp, actions, held)
    }

    #[test]
    fn worker_stopped_mid_upnp_chain_stops_in_time_and_leaves_nothing_mapped() {
        // Stopped while fetching the description: the rest of the chain
        // (address, Add, 718, 718, 725, OK) is dropped, so nothing gets mapped.
        let (gw, ssdp, actions, held) =
            slow_upnp_gateway(&["GET", "GetExternalIPAddress", "AddPortMapping"], vec![718, 718, 725, 0]);
        let mut w = Worker::spawn(PORT, gw, ssdp, [9; 20]).unwrap();
        assert!(eventually(|| actions.lock().unwrap().len() == 1), "fetching the description");
        assert!(w.shut_down(), "the thread finished within STOP_WAIT");
        assert_eq!(*actions.lock().unwrap(), ["GET"]);
        assert!(held.lock().unwrap().is_empty());
        // Stopped while a slow Add is under way that succeeds: it finishes, then the delete goes out.
        let (gw, ssdp, actions, held) = slow_upnp_gateway(&["AddPortMapping", "DeletePortMapping"], vec![0]);
        let mut w = Worker::spawn(PORT, gw, ssdp, [9; 20]).unwrap();
        assert!(eventually(|| actions.lock().unwrap().len() == 3), "adding");
        let start = std::time::Instant::now();
        assert!(w.shut_down(), "the thread finished within STOP_WAIT");
        assert!(start.elapsed() < STOP_WAIT, "{:?}", start.elapsed());
        assert_eq!(actions.lock().unwrap().last().map(String::as_str), Some("DeletePortMapping"));
        assert!(held.lock().unwrap().is_empty(), "deleted on the router");
    }
}
