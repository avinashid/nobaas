use crate::{Error, Event, Node};
use nobaas_core::{Lane, PeerId, SendError};
use std::io::{self, ErrorKind};
use std::net::ToSocketAddrs;
use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};
use tokio::io::Interest;

/// Longest sleep when no timer is armed; readiness wakes the node before that.
const IDLE: Duration = Duration::from_secs(3600);

/// A [`Node`] driven by tokio: [`next_event`](AsyncNode::next_event) waits on the
/// socket and the node's timers instead of being polled.
///
/// It derefs to [`Node`] for everything else (`peer_id`, `invite`, `status`,
/// `peer_stats`, …). [`send`](AsyncNode::send), [`connect`](AsyncNode::connect) and
/// [`disconnect`](AsyncNode::disconnect) flush at once rather than on the next poll.
///
/// ```no_run
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// use nobaas::{AsyncNode, Event, Lane};
///
/// let mut node = AsyncNode::bind("0.0.0.0:0").await?;
/// println!("{}", node.invite());
/// loop {
///     match node.next_event().await {
///         Event::Message { peer, data, .. } => node.send(&peer, Lane::Reliable, &data)?,
///         _ => {}
///     }
/// }
/// # }
/// ```
pub struct AsyncNode {
    node: Node,
    /// A clone of the node's socket, registered with tokio only to learn when it is readable.
    io: tokio::net::UdpSocket,
}

impl AsyncNode {
    /// Binds like [`Node::bind`]. Call it inside a tokio runtime with IO and time enabled.
    pub async fn bind(addr: impl ToSocketAddrs) -> io::Result<AsyncNode> {
        AsyncNode::from_node(Node::bind(addr)?)
    }

    /// Drives an existing node (one made with [`Node::bind_with`], say) from tokio.
    /// Call it inside a tokio runtime.
    pub fn from_node(node: Node) -> io::Result<AsyncNode> {
        let io = tokio::net::UdpSocket::from_std(node.socket().try_clone()?)?;
        Ok(AsyncNode { node, io })
    }

    /// Waits for the next event: reads the socket when it is readable and fires the
    /// node's timers when they are due. Cancel-safe, so it can sit in a `select!`
    /// next to other work; no event is lost when it is dropped.
    pub async fn next_event(&mut self) -> Event {
        loop {
            if let Some(e) = self.node.try_recv_event() {
                return e;
            }
            let wait = self.node.next_timeout().unwrap_or(IDLE);
            tokio::select! {
                r = self.io.readable() => {
                    let node = &mut self.node;
                    // Readiness is cleared only once the socket was read until empty, so a
                    // poll stopped by the read cap gets woken again straight away.
                    let more = self.io.try_io(Interest::READABLE, || {
                        if node.poll_drained(Instant::now()) { Err(ErrorKind::WouldBlock.into()) } else { Ok(()) }
                    });
                    if r.is_err() || more.is_ok() {
                        tokio::task::yield_now().await;
                    }
                }
                _ = tokio::time::sleep(wait) => self.node.poll(),
            }
        }
    }

    /// Dials the peer named in an invite, as [`Node::connect`], and sends the handshake.
    pub fn connect(&mut self, invite: &str) -> Result<PeerId, Error> {
        let peer = self.node.connect(invite)?;
        self.node.poll();
        Ok(peer)
    }

    /// Sends a message to `peer` now, as [`Node::send`]; it never blocks.
    pub fn send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError> {
        self.node.send(peer, lane, data)?;
        self.node.poll();
        Ok(())
    }

    /// Closes the link to `peer`, as [`Node::disconnect`].
    pub fn disconnect(&mut self, peer: &PeerId) {
        self.node.disconnect(peer);
    }

    /// The sync node underneath.
    pub fn into_inner(self) -> Node {
        self.node
    }
}

impl Deref for AsyncNode {
    type Target = Node;
    fn deref(&self) -> &Node {
        &self.node
    }
}

impl DerefMut for AsyncNode {
    fn deref_mut(&mut self) -> &mut Node {
        &mut self.node
    }
}
