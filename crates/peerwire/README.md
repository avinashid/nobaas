# peerwire

Peer-to-peer nodes over UDP, with no server. This is the Rust crate most applications
want: it drives the sans-IO [`peerwire-core`](../peerwire-core) `Endpoint` from a real UDP
socket and adds STUN, rooms, signaling and NAT traversal.
Design: [docs/ARCHITECTURE.md](../../docs/ARCHITECTURE.md), NAT traversal:
[docs/NAT-TRAVERSAL.md](../../docs/NAT-TRAVERSAL.md).

A `Node` owns one non-blocking UDP socket and one identity. It never blocks and never
spawns threads: call `Node::poll` from your loop (a game's frame tick, a timer, an event
loop) and read events with `Node::try_recv_event`.

## Example

```rust,no_run
use peerwire::{Event, Lane, Node};

let mut host = Node::bind("[::]:0")?;
let invite = host.invite(); // hand this "pw:…" string to the other peer

let mut guest = Node::bind("[::]:0")?;
guest.connect(&invite)?;
loop {
    host.poll();
    guest.poll();
    while let Some(e) = guest.try_recv_event() {
        if let Event::Connected { peer, .. } = e {
            guest.send(&peer, Lane::Reliable, b"hello")?;
        }
    }
    while let Some(e) = host.try_recv_event() {
        if let Event::Message { data, .. } = e {
            println!("{}", String::from_utf8_lossy(&data));
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Meeting by name instead of passing an invite needs a signaling carrier:

```rust,no_run
use peerwire::{Node, NostrSignaling};

let mut node = Node::builder()
    .signaling(NostrSignaling::new(["wss://relay.damus.io", "wss://nos.lol"]))
    .bind("[::]:0")?;
node.join_room("my-app", "lobby"); // finds and connects the room's peers
# Ok::<(), Box<dyn std::error::Error>>(())
```

Runnable versions live in [`examples/rust`](../../examples/rust):

```sh
cargo run -p peerwire --example echo                                  # prints an pw: invite and echoes
cargo run -p peerwire --example echo -- pw:…                          # joins, sends "hello"
cargo run -p peerwire --features tokio --example echo_async           # the same on tokio
cargo run -p peerwire --features nostr --example natcheck             # what does this network's NAT do?
cargo run -p peerwire --features nostr --example nostr_relay -- 127.0.0.1:7447   # LAN/test Nostr relay
```

## What it adds to the core

- **STUN:** a node asks STUN servers (`DEFAULT_STUN_SERVERS`, or `Builder::stun_servers`)
  for its public address (`Node::public_addr`, `Node::nat_type`) and puts it in its invites.
- **Signaling:** the `Signaling` trait carries addresses between peers. `MemoryHub` is an
  in-process carrier for tests; `NostrSignaling` (feature `nostr`) uses public Nostr relays,
  with offers sealed to the recipient's `PeerId`.
- **Rooms:** `join_room(app_id, room)` finds and connects every peer in a room (a full mesh).
  `join_room_as` / `join_room_with` pick a `Role` (`Peer`, `Host` or `Client`) for star
  topologies. Rooms are closed by their name: hellos and offers carry an HMAC under a key
  derived from it, so a relay that sees the room's topic still can't join. A room whose name
  can be guessed is a public lobby.
- **NAT traversal:** both sides dial at once and punch through their NATs, with port
  prediction for symmetric NATs. Where punching can't work, a peer linked to both relays,
  and `Node::failure` says which side was symmetric. See the `traversal` module.
- **Port mapping:** `Builder::port_mapping(true)` asks the router (PCP, NAT-PMP or UPnP-IGD)
  to forward a public port, and offers that address too.
- **Privacy:** `Privacy::Direct` (every address), `PublicOnly` (no LAN or loopback addresses)
  or `RelayOnly` (no address at all, every link through a relay). A node dials back only
  peers that hold its invite, share its room, or that it dialled: a `PeerId` alone gets nothing.
- **Async:** with feature `tokio`, `AsyncNode` waits on the socket and timers itself:
  `node.next_event().await`.

## Features

| Feature | Adds |
|---|---|
| `tokio` | `AsyncNode` |
| `nostr` | `NostrSignaling` (websocket client with rustls, BIP-340 signatures) |
| `testing` | test hooks for bindings (`Builder::port_mapping_at`); not for apps |

## Tests

`cargo test --release -p peerwire` runs on loopback only: two nodes linking, async nodes,
NAT traversal over loopback, room security, privacy modes, port mapping against a fake
gateway, and Nostr signaling against the in-memory relay. The test against public relays
and STUN is `#[ignore]`d and runs only through `scripts/gate.sh --nostr`.

## Status

Early development (v0.1). The API may still change.
