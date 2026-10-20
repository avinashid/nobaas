//! A minimal in-memory Nostr relay, for tests and LAN play without internet: it
//! forwards each event to the live subscriptions whose `#p` filter names its `p`
//! tag. It stores nothing, checks no signatures and serves plain `ws://` only.
//!
//! ```sh
//! cargo run -p nobaas --features nostr --example nostr_relay -- 127.0.0.1:7447
//! ```
//! then give nodes `NostrSignaling::new(["ws://127.0.0.1:7447"])`.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tungstenite::Message;

struct Client {
    out: Sender<String>,
    /// Subscription id -> `#p` tags.
    subs: HashMap<String, Vec<String>>,
}

type Clients = Arc<Mutex<HashMap<u64, Client>>>;

/// Serves a relay on `addr` from background threads; returns the bound address.
pub fn spawn(addr: &str) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    let clients: Clients = Arc::default();
    std::thread::spawn(move || {
        for (id, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { continue };
            let clients = clients.clone();
            std::thread::spawn(move || serve(stream, id as u64, clients));
        }
    });
    Ok(local)
}

fn serve(stream: TcpStream, id: u64, clients: Clients) {
    let Ok(ctl) = stream.try_clone() else { return };
    let Ok(mut ws) = tungstenite::accept(stream) else { return };
    let _ = ctl.set_read_timeout(Some(Duration::from_millis(20)));
    let (out, rx) = channel();
    clients.lock().unwrap().insert(id, Client { out, subs: HashMap::new() });
    'conn: loop {
        while let Ok(m) = rx.try_recv() {
            if ws.send(Message::Text(m)).is_err() {
                break 'conn;
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => handle(id, &t, &clients),
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let _ = ws.flush();
            }
            Err(_) => break,
        }
    }
    clients.lock().unwrap().remove(&id);
}

fn handle(id: u64, text: &str, clients: &Clients) {
    let Ok(Value::Array(m)) = serde_json::from_str::<Value>(text) else { return };
    let mut clients = clients.lock().unwrap();
    match (m.first().and_then(Value::as_str), m.get(1)) {
        (Some("REQ"), Some(Value::String(sub))) => {
            let tags = m.get(2).and_then(|f| f.get("#p")).and_then(Value::as_array);
            let tags =
                tags.map(|t| t.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            if let Some(c) = clients.get_mut(&id) {
                c.subs.insert(sub.clone(), tags);
            }
        }
        (Some("CLOSE"), Some(Value::String(sub))) => {
            if let Some(c) = clients.get_mut(&id) {
                c.subs.remove(sub);
            }
        }
        (Some("EVENT"), Some(ev)) => {
            let tag = ev["tags"].as_array().and_then(|t| t.iter().find(|t| t[0] == "p")).and_then(|t| t[1].as_str());
            if let Some(c) = clients.get(&id) {
                let _ = c.out.send(json!(["OK", ev["id"], tag.is_some(), ""]).to_string());
            }
            let Some(tag) = tag else { return };
            for c in clients.values() {
                for (sub, tags) in &c.subs {
                    if tags.iter().any(|t| t == tag) {
                        let _ = c.out.send(json!(["EVENT", sub, ev]).to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

#[allow(dead_code)]
fn main() -> std::io::Result<()> {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:7447".into());
    println!("ws://{}", spawn(&addr)?);
    loop {
        std::thread::park();
    }
}
