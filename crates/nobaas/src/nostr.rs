//! Signaling over public Nostr relays (NIP-01), wire-compatible with
//! `packages/js/src/nostr.js`: ephemeral events of kind 25059 (relays forward them to
//! live subscribers and don't store them), one `p` tag holding the [`Topic`] in
//! hex, the payload in base64 as `content`, and a fresh secp256k1 key per carrier
//! so nothing links events to the Nobaas identity or to other sessions.
//!
//! Each relay gets a small thread that does the blocking websocket I/O; the
//! [`Signaling`] side never blocks. `wss://` uses rustls with the webpki roots;
//! `ws://` is plain TCP, for a relay on the LAN or loopback.

use crate::signaling::{hex, Signaling, Topic};
use k256::schnorr::SigningKey;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::Message;

/// Ephemeral event kind used by Nobaas signaling.
pub const KIND: u64 = 25059;
/// Relays [`NostrSignaling::default`] connects to.
pub const DEFAULT_RELAYS: &[&str] = &["wss://relay.damus.io", "wss://nos.lol"];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a relay thread blocks reading before it looks at its queue again.
const READ_SLICE: Duration = Duration::from_millis(20);
/// Events kept while a relay is down; older ones are dropped.
const MAX_QUEUED: usize = 64;
/// Event ids remembered to drop the copy each relay delivers.
const MAX_SEEN: usize = 4096;
/// Events received but not yet polled; past this, relays' events are dropped.
const MAX_INBOX: usize = 1024;
/// Largest websocket message or frame taken from a relay (tungstenite's default is 64 MiB).
const MAX_MESSAGE: usize = 128 << 10;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Counters for a [`NostrSignaling`] carrier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NostrStats {
    /// Relays with an open connection now.
    pub relays_open: u64,
    pub published: u64,
    pub received: u64,
    /// Events a relay refused (`OK false`).
    pub rejected: u64,
}

#[derive(Default)]
struct Shared {
    open: AtomicU64,
    received: AtomicU64,
    rejected: AtomicU64,
}

enum Cmd {
    /// The subscription to keep open (`None`: none).
    Req(Option<String>),
    Event(String),
}

struct Incoming {
    id: String,
    pubkey: String,
    tag: String,
    content: String,
}

/// A [`Signaling`] carrier over Nostr relays. See the module docs.
pub struct NostrSignaling {
    key: SigningKey,
    pubkey: String,
    sub: String,
    topics: BTreeSet<Topic>,
    relays: Vec<Sender<Cmd>>,
    inbox: Receiver<Incoming>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    shared: Arc<Shared>,
    published: u64,
}

impl Default for NostrSignaling {
    /// A carrier on the [`DEFAULT_RELAYS`].
    fn default() -> Self {
        NostrSignaling::new(DEFAULT_RELAYS.iter().copied())
    }
}

impl NostrSignaling {
    /// Connects to `relays` (`wss://host[:port][/path]` or `ws://…`) in the background,
    /// reconnecting with backoff when one drops.
    pub fn new<S: AsRef<str>>(relays: impl IntoIterator<Item = S>) -> NostrSignaling {
        let key = loop {
            let mut b = [0u8; 32];
            OsRng.fill_bytes(&mut b);
            if let Ok(k) = SigningKey::from_bytes(&b) {
                break k;
            }
        };
        let pubkey = hex(&key.verifying_key().to_bytes());
        let mut sub = [0u8; 8];
        OsRng.fill_bytes(&mut sub);
        let (tx, inbox) = sync_channel(MAX_INBOX);
        let shared = Arc::new(Shared::default());
        let relays = relays
            .into_iter()
            .map(|url| {
                let (cmd, rx) = channel();
                let (url, tx, shared) = (url.as_ref().to_string(), tx.clone(), shared.clone());
                std::thread::Builder::new()
                    .name("nobaas-nostr".into())
                    .spawn(move || relay_loop(url, rx, tx, shared))
                    .expect("spawn relay thread");
                cmd
            })
            .collect();
        NostrSignaling {
            key,
            pubkey,
            sub: hex(&sub),
            topics: BTreeSet::new(),
            relays,
            inbox,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            shared,
            published: 0,
        }
    }

    /// Counters: open relays, events published, received and refused.
    pub fn stats(&self) -> NostrStats {
        NostrStats {
            relays_open: self.shared.open.load(Ordering::Relaxed),
            published: self.published,
            received: self.shared.received.load(Ordering::Relaxed),
            rejected: self.shared.rejected.load(Ordering::Relaxed),
        }
    }

    fn resubscribe(&mut self) {
        let req = (!self.topics.is_empty()).then(|| {
            let tags: Vec<String> = self.topics.iter().map(Topic::to_hex).collect();
            let since = unix_secs().saturating_sub(30);
            json!(["REQ", self.sub, {"kinds": [KIND], "#p": tags, "since": since}]).to_string()
        });
        for r in &self.relays {
            let _ = r.send(Cmd::Req(req.clone()));
        }
    }
}

impl Signaling for NostrSignaling {
    fn subscribe(&mut self, topic: Topic) {
        if self.topics.insert(topic) {
            self.resubscribe();
        }
    }

    fn unsubscribe(&mut self, topic: Topic) {
        if self.topics.remove(&topic) {
            self.resubscribe();
        }
    }

    fn publish(&mut self, topic: Topic, payload: &[u8]) {
        let event = sign_event(&self.key, &self.pubkey, unix_secs(), &topic.to_hex(), &b64(payload));
        let msg = json!(["EVENT", event]).to_string();
        for r in &self.relays {
            let _ = r.send(Cmd::Event(msg.clone()));
        }
        self.published += 1;
    }

    fn poll(&mut self) -> Option<(Topic, Vec<u8>)> {
        loop {
            let m = self.inbox.try_recv().ok()?;
            if m.pubkey == self.pubkey || !self.seen.insert(m.id.clone()) {
                continue; // our own, or a copy from another relay
            }
            self.seen_order.push_back(m.id);
            if self.seen_order.len() > MAX_SEEN {
                let old = self.seen_order.pop_front().unwrap();
                self.seen.remove(&old);
            }
            let Some(topic) = Topic::from_hex(&m.tag).filter(|t| self.topics.contains(t)) else { continue };
            if let Some(payload) = unb64(&m.content) {
                return Some((topic, payload));
            }
        }
    }
}

impl Drop for NostrSignaling {
    fn drop(&mut self) {
        // Relay threads see their command channel close and exit.
        self.relays.clear();
    }
}

/// A NIP-01 event: id = sha256 of `[0, pubkey, created_at, kind, tags, content]`,
/// signed with BIP-340 Schnorr.
fn sign_event(key: &SigningKey, pubkey: &str, created_at: u64, tag: &str, content: &str) -> Value {
    let tags = json!([["p", tag]]);
    let id: [u8; 32] = Sha256::digest(json!([0, pubkey, created_at, KIND, tags, content]).to_string()).into();
    let mut aux = [0u8; 32];
    OsRng.fill_bytes(&mut aux);
    let sig = key.sign_raw(&id, &aux).expect("schnorr signs a 32-byte digest");
    json!({
        "id": hex(&id), "pubkey": pubkey, "created_at": created_at, "kind": KIND,
        "tags": tags, "content": content, "sig": hex(&sig.to_bytes()),
    })
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}
type Ws = tungstenite::WebSocket<Box<dyn Stream>>;

/// One relay: connect, keep the subscription, forward events both ways, reconnect.
fn relay_loop(url: String, cmds: Receiver<Cmd>, out: SyncSender<Incoming>, shared: Arc<Shared>) {
    let mut req: Option<String> = None;
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        if let Ok(mut ws) = connect(&url) {
            shared.open.fetch_add(1, Ordering::Relaxed);
            backoff = Duration::from_secs(1);
            let alive = session(&mut ws, &cmds, &out, &shared, &mut req, &mut queue);
            shared.open.fetch_sub(1, Ordering::Relaxed);
            if !alive {
                let _ = ws.close(None);
                let _ = ws.flush();
                return;
            }
        }
        // Wait out the backoff, still taking commands (and leaving if the carrier is gone).
        let mut waited = Duration::ZERO;
        while waited < backoff {
            match cmds.recv_timeout(Duration::from_millis(100)) {
                Ok(Cmd::Req(r)) => req = r,
                Ok(Cmd::Event(e)) => {
                    if queue.len() >= MAX_QUEUED {
                        queue.pop_front();
                    }
                    queue.push_back(e);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => waited += Duration::from_millis(100),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Runs one connection until it drops (`true`) or the carrier is dropped (`false`).
fn session(
    ws: &mut Ws,
    cmds: &Receiver<Cmd>,
    out: &SyncSender<Incoming>,
    shared: &Shared,
    req: &mut Option<String>,
    queue: &mut VecDeque<String>,
) -> bool {
    let mut sent_req = false;
    loop {
        let mut frames = Vec::new();
        loop {
            match cmds.try_recv() {
                Ok(Cmd::Req(r)) => {
                    if r.is_none() && sent_req {
                        frames.push(json!(["CLOSE", req_sub(req.as_deref())]).to_string());
                    }
                    *req = r;
                    sent_req = false;
                }
                Ok(Cmd::Event(e)) => queue.push_back(e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        if !sent_req {
            if let Some(r) = req.as_ref() {
                frames.push(r.clone());
            }
            sent_req = true;
        }
        frames.extend(queue.drain(..));
        for f in frames {
            if ws.send(Message::Text(f)).is_err() {
                return true;
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => on_relay_message(&t, out, shared),
            Ok(Message::Close(_)) => return true,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if ws.flush().is_err() {
                    return true;
                }
            }
            Err(_) => return true,
        }
    }
}

fn req_sub(req: Option<&str>) -> String {
    req.and_then(|r| serde_json::from_str::<Value>(r).ok())
        .and_then(|v| v.get(1).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn on_relay_message(text: &str, out: &SyncSender<Incoming>, shared: &Shared) {
    let Ok(Value::Array(m)) = serde_json::from_str::<Value>(text) else { return };
    match m.first().and_then(Value::as_str) {
        Some("EVENT") => {
            let Some(ev) = m.get(2) else { return };
            if ev.get("kind").and_then(Value::as_u64) != Some(KIND) {
                return;
            }
            let field = |k: &str| ev.get(k).and_then(Value::as_str).map(str::to_string);
            let tags = ev.get("tags").and_then(Value::as_array);
            let tag = tags.and_then(|tags| {
                tags.iter().find_map(|t| match t.as_array()?.as_slice() {
                    [k, v, ..] if k.as_str() == Some("p") => v.as_str().map(str::to_string),
                    _ => None,
                })
            });
            let created_at = ev.get("created_at").and_then(Value::as_u64);
            let (Some(id), Some(pubkey), Some(tag), Some(content), Some(created_at)) =
                (field("id"), field("pubkey"), tag, field("content"), created_at)
            else {
                return;
            };
            // The id must be the NIP-01 hash of the event, so a relay can't dress one
            // event up as many (copies are dropped by id). Signatures aren't checked:
            // offers are sealed and authenticate themselves.
            let ser = json!([0, pubkey, created_at, KIND, tags, content]).to_string();
            if hex(&Sha256::digest(ser)) != id {
                return;
            }
            shared.received.fetch_add(1, Ordering::Relaxed);
            let _ = out.try_send(Incoming { id, pubkey, tag, content }); // full: dropped, as a relay may drop
        }
        Some("OK") if m.get(2) == Some(&Value::Bool(false)) => {
            shared.rejected.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn connect(url: &str) -> io::Result<Ws> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(io::Error::new(ErrorKind::InvalidInput, "relay URL must start with ws:// or wss://"));
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') || h.ends_with(']') => {
            (h, p.parse().map_err(|_| io::Error::new(ErrorKind::InvalidInput, "bad port"))?)
        }
        _ => (authority, if tls { 443 } else { 80 }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
    let mut last = io::Error::new(ErrorKind::NotFound, "relay host has no address");
    let mut tcp = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let tcp = tcp.ok_or(last)?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    let stream: Box<dyn Stream> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "bad relay host name"))?;
        let conn = rustls::ClientConnection::new(tls_config(), name).map_err(io::Error::other)?;
        Box::new(rustls::StreamOwned::new(conn, tcp.try_clone()?))
    } else {
        Box::new(tcp.try_clone()?)
    };
    let config = WebSocketConfig { max_message_size: Some(MAX_MESSAGE), max_frame_size: Some(MAX_MESSAGE), ..Default::default() };
    let (ws, _) = tungstenite::client::client_with_config(url, stream, Some(config)).map_err(|e| io::Error::other(e.to_string()))?;
    tcp.set_read_timeout(Some(READ_SLICE))?;
    Ok(ws)
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, as `btoa` writes it.
fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = u32::from(c[0]) << 16 | u32::from(*c.get(1).unwrap_or(&0)) << 8 | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let val = |c: u8| B64.iter().position(|&b| b == c).map(|v| v as u32);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (k, q) in s.chunks(4).enumerate() {
        let last = k == s.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = n << 6 | val(c)?;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_btoa() {
        for (raw, enc) in [(&b""[..], ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"), (b"foob", "Zm9vYg==")] {
            assert_eq!(b64(raw), enc);
            assert_eq!(unb64(enc).as_deref(), Some(raw));
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(unb64(&b64(&all)), Some(all));
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9v!A==", "===="] {
            assert_eq!(unb64(bad), None, "{bad}");
        }
    }

    #[test]
    fn events_are_nip01() {
        let key = SigningKey::from_bytes(&[7u8; 32]).unwrap();
        let pubkey = hex(&key.verifying_key().to_bytes());
        let ev = sign_event(&key, &pubkey, 1_700_000_000, &"ab".repeat(32), "aGk=");
        let ser = json!([0, pubkey, 1_700_000_000u64, KIND, [["p", "ab".repeat(32)]], "aGk="]).to_string();
        let id: [u8; 32] = Sha256::digest(ser).into();
        assert_eq!(ev["id"], hex(&id));
        let mut sig = [0u8; 64];
        crate::signaling::unhex(ev["sig"].as_str().unwrap(), &mut sig);
        let vk = k256::schnorr::VerifyingKey::from_bytes(&key.verifying_key().to_bytes()).unwrap();
        let sig = k256::schnorr::Signature::try_from(&sig[..]).unwrap();
        assert!(vk.verify_raw(&id, &sig).is_ok());
        assert_eq!(ev["kind"], KIND);
    }

    #[test]
    fn relay_messages_are_parsed_defensively() {
        let (tx, rx) = sync_channel(4);
        let shared = Shared::default();
        for junk in ["", "{}", "[]", r#"["EVENT"]"#, r#"["EVENT","s",{"kind":1}]"#, r#"["EVENT","s",{"kind":25059,"tags":[["e","x"]]}]"#] {
            on_relay_message(junk, &tx, &shared);
        }
        // An id that isn't the event's hash: dropped.
        on_relay_message(r#"["EVENT","s",{"id":"i","pubkey":"p","created_at":1,"kind":25059,"tags":[["p","t"]],"content":"c"}]"#, &tx, &shared);
        assert!(rx.try_recv().is_err());
        let key = SigningKey::from_bytes(&[7u8; 32]).unwrap();
        let pubkey = hex(&key.verifying_key().to_bytes());
        let ev = sign_event(&key, &pubkey, 1_700_000_000, "t", "c");
        on_relay_message(&json!(["EVENT", "s", ev]).to_string(), &tx, &shared);
        let m = rx.try_recv().unwrap();
        assert_eq!((m.id, m.tag.as_str(), m.content.as_str()), (ev["id"].as_str().unwrap().to_string(), "t", "c"));
        // The same id with other content (a relay replaying it as new): dropped.
        let mut forged = ev.clone();
        forged["content"] = json!("d");
        on_relay_message(&json!(["EVENT", "s", forged]).to_string(), &tx, &shared);
        assert!(rx.try_recv().is_err());
        // A full inbox drops events rather than growing or blocking the relay thread.
        for i in 0..10 {
            let ev = sign_event(&key, &pubkey, 1_700_000_000 + i, "t", "c");
            on_relay_message(&json!(["EVENT", "s", ev]).to_string(), &tx, &shared);
        }
        assert_eq!(rx.try_iter().count(), 4);
        on_relay_message(r#"["OK","i",false,"blocked"]"#, &tx, &shared);
        assert_eq!(shared.rejected.load(Ordering::Relaxed), 1);
    }

    /// A local server sends one 200 KiB message: the client drops the connection
    /// instead of buffering it (NB-032(d)).
    #[test]
    fn oversized_relay_messages_close_the_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            let _ = ws.send(Message::Text("x".repeat(200 << 10)));
            let _ = ws.read(); // until the client goes
        });
        let mut ws = connect(&url).unwrap();
        let err = loop {
            match ws.read() {
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
                other => break other,
            }
        };
        assert!(matches!(err, Err(tungstenite::Error::Capacity(_))), "{err:?}");
        drop(ws);
        server.join().unwrap();
    }
}
