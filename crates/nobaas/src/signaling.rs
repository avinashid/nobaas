//! Signaling: how peers that can't reach each other yet swap addresses.
//!
//! A [`Signaling`] carrier is a publish/subscribe channel keyed by [`Topic`]s. Nobaas
//! uses two kinds of topic, named as in `packages/js/nostr.js` so Rust and browser
//! peers can share a carrier:
//! - a room, [`Topic::room`]: presence ("hello"), readable by anyone who knows the room name;
//! - a peer, [`Topic::peer`]: candidate addresses, sealed to that peer's [`PeerId`].
//!
//! Carriers are untrusted: they see topics, timing and the sender's IP, never
//! addresses or other contents of peer messages.

use nobaas_core::PeerId;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

/// A signaling topic: the SHA-256 of a room or peer name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Topic(pub [u8; 32]);

impl Topic {
    /// The presence topic of room `room` in application `app_id`: `sha256("nobaas/room/{app_id}/{room}")`.
    pub fn room(app_id: &str, room: &str) -> Topic {
        Topic::hash(&format!("nobaas/room/{app_id}/{room}"))
    }

    /// The inbox of `peer`: `sha256("nobaas/peer/{hex peer id}")`.
    pub fn peer(peer: &PeerId) -> Topic {
        Topic::hash(&format!("nobaas/peer/{}", hex(peer.as_bytes())))
    }

    fn hash(name: &str) -> Topic {
        Topic(Sha256::digest(name.as_bytes()).into())
    }

    /// Lower-case hex, as in a Nostr `p` tag.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// Parses [`to_hex`](Topic::to_hex) output.
    pub fn from_hex(s: &str) -> Option<Topic> {
        let mut out = [0u8; 32];
        unhex(s, &mut out).then_some(Topic(out))
    }
}

impl fmt::Debug for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Topic({})", &self.to_hex()[..12])
    }
}

/// A publish/subscribe carrier for signaling messages. Implementations never block:
/// [`publish`](Signaling::publish) queues, [`poll`](Signaling::poll) returns what has
/// arrived. Delivery is best effort and need not be ordered; messages are not stored
/// for subscribers that arrive later.
pub trait Signaling: Send {
    /// Starts delivering messages published to `topic`.
    fn subscribe(&mut self, topic: Topic);
    /// Stops delivering messages published to `topic`.
    fn unsubscribe(&mut self, topic: Topic);
    /// Sends `payload` to every other subscriber of `topic`.
    fn publish(&mut self, topic: Topic, payload: &[u8]);
    /// The next message that arrived, if any.
    fn poll(&mut self) -> Option<(Topic, Vec<u8>)>;
}

/// An in-process signaling hub, for tests and for peers in one process.
/// [`connect`](MemoryHub::connect) hands out carriers that reach each other.
#[derive(Clone, Default)]
pub struct MemoryHub(Arc<Mutex<Vec<Member>>>);

#[derive(Default)]
struct Member {
    topics: HashSet<Topic>,
    inbox: VecDeque<(Topic, Vec<u8>)>,
    gone: bool,
}

impl MemoryHub {
    /// An empty hub.
    pub fn new() -> MemoryHub {
        MemoryHub::default()
    }

    /// A new carrier on this hub.
    pub fn connect(&self) -> MemorySignaling {
        let mut m = self.0.lock().unwrap();
        m.push(Member::default());
        MemorySignaling { hub: self.clone(), me: m.len() - 1 }
    }
}

/// A carrier on a [`MemoryHub`].
pub struct MemorySignaling {
    hub: MemoryHub,
    me: usize,
}

impl Signaling for MemorySignaling {
    fn subscribe(&mut self, topic: Topic) {
        self.hub.0.lock().unwrap()[self.me].topics.insert(topic);
    }

    fn unsubscribe(&mut self, topic: Topic) {
        self.hub.0.lock().unwrap()[self.me].topics.remove(&topic);
    }

    fn publish(&mut self, topic: Topic, payload: &[u8]) {
        let mut m = self.hub.0.lock().unwrap();
        for (i, other) in m.iter_mut().enumerate() {
            if i != self.me && !other.gone && other.topics.contains(&topic) {
                other.inbox.push_back((topic, payload.to_vec()));
            }
        }
    }

    fn poll(&mut self) -> Option<(Topic, Vec<u8>)> {
        self.hub.0.lock().unwrap()[self.me].inbox.pop_front()
    }
}

impl Drop for MemorySignaling {
    fn drop(&mut self) {
        if let Ok(mut m) = self.hub.0.lock() {
            let me = &mut m[self.me];
            me.gone = true;
            me.topics.clear();
            me.inbox.clear();
        }
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Decodes exactly `out.len()` bytes of hex.
pub(crate) fn unhex(s: &str, out: &mut [u8]) -> bool {
    let s = s.as_bytes();
    if s.len() != out.len() * 2 {
        return false;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    for (i, o) in out.iter_mut().enumerate() {
        match (nib(s[2 * i]), nib(s[2 * i + 1])) {
            (Some(h), Some(l)) => *o = h << 4 | l,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_match_nostr_js() {
        // sha256("nobaas/room/demo/lobby"), as nostr.js computes for room "demo/lobby".
        assert_eq!(Topic::room("demo", "lobby"), Topic::hash("nobaas/room/demo/lobby"));
        let t = Topic::hash("abc");
        assert_eq!(t.to_hex(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(Topic::from_hex(&t.to_hex()), Some(t));
        assert_eq!(Topic::from_hex("zz"), None);
        let p = PeerId([0xab; 32]);
        assert_eq!(Topic::peer(&p), Topic::hash(&format!("nobaas/peer/{}", "ab".repeat(32))));
    }

    #[test]
    fn memory_hub_delivers_to_other_subscribers() {
        let hub = MemoryHub::new();
        let (mut a, mut b, mut c) = (hub.connect(), hub.connect(), hub.connect());
        let t = Topic::hash("t");
        a.subscribe(t);
        b.subscribe(t);
        a.publish(t, b"from a");
        assert_eq!(a.poll(), None, "not echoed to the sender");
        assert_eq!(b.poll(), Some((t, b"from a".to_vec())));
        assert_eq!(c.poll(), None, "not subscribed");
        b.unsubscribe(t);
        c.publish(t, b"x");
        assert_eq!(b.poll(), None);
        assert_eq!(a.poll(), Some((t, b"x".to_vec())));
        drop(a);
        b.subscribe(t);
        c.publish(t, b"y");
        assert_eq!(b.poll(), Some((t, b"y".to_vec())));
    }
}
