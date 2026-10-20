# NAT traversal

How two `nobaas` nodes behind NATs find and reach each other with no server of their own. The code is
`crates/nobaas/src/traversal.rs` (sans-IO, deterministic), driven by `Node` on its UDP socket and by the
simulator in `crates/nobaas-sim/tests/traversal.rs`. The session core (Noise IK, punching, peer relay) is
unchanged; this layer feeds it addresses.

## Pieces

| Piece | What it does |
|---|---|
| STUN | Binding requests on the session's own socket, so the reflexive address is the session's NAT mapping |
| Signaling | A publish/subscribe carrier keyed by 32-byte topics (`trait Signaling`): `MemoryHub` in-process, `NostrSignaling` over Nostr relays (feature `nostr`) |
| Rooms | Presence on a room topic; peers that hear each other swap sealed offers |
| Offers | Candidate addresses, NAT type and connected peers, sealed to one peer |
| Dialling | Both sides call `Endpoint::connect_with` on every candidate (plus `Path::Via` a mutual peer): a simultaneous open |

## STUN

- Servers: `Builder::stun_servers`, default `stun.l.google.com:19302` and `stun.cloudflare.com:3478`, resolved once
  at bind (sockets bound to loopback skip STUN).
- A round every 25 s (it also keeps the mapping alive); each request is retransmitted at 0.5 s and 1 s, then
  dropped until the next round.
- A response counts only if it comes from the queried server's address and carries the request's random 96-bit
  transaction id.
- NAT type: no answer, or one, gives `Unknown`. A mapped address equal to a local one is `Open`. Two servers
  reporting the same mapping is `Cone`, different ones `Symmetric`.
- Datagrams are told apart by their first byte (RFC 7983): STUN goes to the traversal, everything else to the
  endpoint.

## Topics

Same derivation as `packages/js/nostr.js`, so Rust and browser peers can share relays:

- room: `sha256("nobaas/room/" + app_id + "/" + room)`; a JS peer in room `"<app_id>/<room>"` uses the same topic;
- peer inbox: `sha256("nobaas/peer/" + lowercase hex of the 32-byte PeerId)`.

## Messages

**Hello** (room topic, plaintext, public to anyone who knows the room name). The JSON shape of `nostr.js`, with a
marker for UDP peers:

```json
{"t":"hello","from":"<64 hex PeerId>","meta":{"nb":1,"role":"peer|host|client"}}
```

Parsed strictly (exact prefix, 64 hex digits, known role, exact suffix). Browser hellos lack `meta.nb` and are
ignored. `from` is not authenticated, but nothing trusts it: the hello only triggers an offer sealed to that key,
which only its holder can open, and the dial authenticates it.

**Offer** (peer topic): `sealed::seal(me, to, …)` (`Noise_X_25519_ChaChaPoly_BLAKE2s`, sender authenticated),
around:

```text
0x01  reply(0|1)  role(0 peer,1 host,2 client)  nat(0 unknown,1 open,2 cone,3 symmetric)
ts    u64 LE      sender's Unix time in ms
nonce u64 LE      random
n     u8 (<= 8)   then n x [len u8][Address::encode]
m     u8 (<= 8)   then m x 32-byte PeerId (peers the sender is connected to)
```

The decoder rejects a wrong version, flags other than 0 or 1, unknown roles or NAT types, n or m above 8,
truncation and trailing bytes. Addresses that fail to decode are skipped.

## Rules

- **Roles.** Peer↔Peer, Peer↔Host, Host↔Client. Clients never link to clients, and hosts never to hosts. A node in
  no room (an invite join) accepts any role.
- **Discovery.** On a hello from a peer it isn't linked to (at most one offer per peer per 2 s), a node sends an
  offer with `reply=1`. Hellos repeat at 2, 4, 8, 16, then every 30 s, for peers that missed one.
- **On an offer** (from another key, `|ts - now| <= 60 s`, nonce not seen from that sender in the last 2 min,
  role allowed, within the dial budget): the node dials every candidate except its own addresses, plus `Via(r)`
  for up to 2 listed peers it is itself connected to. If already linked, it calls `add_candidates` instead
  (punching a relayed link). If `reply=1`, it answers with its own offer (`reply=0`).
- **Invites.** `Node::connect` dials the invite's addresses, then sends the host an offer with `reply=1`, so the
  host dials back at the same time.
- **New links.** When a link comes up, peers that offered to this node but are still unlinked get a fresh offer
  listing the new peer, which may be the relay they need.
- **Budget.** Offers acted on and offers sent in answer to hellos share one budget across all senders: a burst of
  30, then one per 2 s. It bounds what forged hellos or offers can make a node dial, seal or publish, and is ample
  for the room sizes this layer targets (no mesh limit yet).
- **Failure.** When a dial ends in `ConnectFailed`, `failure(peer)` is `Symmetric` if one side is symmetric and
  the other isn't open, and `Unreachable` otherwise.

## What others learn or can do

- **Relays and anyone who knows the room name** see the room topic, each peer's inbox topic (a hash of its public
  key), source IPs at the relay and message timing. They cannot read offers: addresses never leave sealed messages.
- **A peer that receives an offer** learns the sender's candidate addresses and up to 8 PeerIds the sender is
  linked to. In a room, members already know each other. A host answering invite joins tells each joiner who else
  is connected.
- **Replay.** A captured offer is dropped once it is older than 60 s or its nonce was seen. Within that window a
  replay to a node that never saw it causes one more dial to the original sender's addresses.
- **Reflection.** Anyone who knows a node's PeerId can make it dial addresses: at most 8 per offer, about 6
  handshake datagrams each (around 100 B) over 30 s, inside the dial budget. That is roughly 4.8 KB of handshakes
  per accepted offer, at most 30 offers in a burst and then one per 2 s.
- **STUN servers** see the node's public address. A forged STUN answer needs the 96-bit transaction id and the
  server's source address.

## Coverage

- `crates/nobaas/src/traversal.rs` unit tests: hello and offer round trips, truncation, browser hellos, roles,
  sealed offers turning into dials, replay and stale drops, the budget under a hello flood, STUN classification
  and retransmits.
- `crates/nobaas-sim/tests/traversal.rs`:
  - NAT types from simulated STUN;
  - the room-of-two matrix over 8 NAT kinds with 150 ms signaling latency;
  - a symmetric pair relayed through a third room peer;
  - an invite join that punches only with signaling.
- `crates/nobaas/tests/nat_loopback.rs` (mock STUN servers, `MemoryHub`) and `crates/nobaas/tests/nostr.rs` (local
  relay; `--ignored`: public relays and STUN, run by `scripts/gate.sh --nostr`).

Room of two, signaling only, from `cargo test -p nobaas-sim --release --test traversal -- --nocapture`. D is
direct, with ms to connect; F S is a failure reported as `Symmetric` on both sides:

```text
         A \ B        public     full-cone    restricted    port-restr       sym-seq    sym-random  cgnat(pr+pr) cgnat(pr+sym)
        public         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360
     full-cone         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360
    restricted         D 510         D 510         D 510         D 860         D 860         D 860         D 510         D 860
    port-restr         D 510         D 510         D 510         D 860           F S           F S         D 860           F S
       sym-seq         D 510         D 530         D 510           F S           F S           F S           F S           F S
    sym-random         D 510         D 530         D 510           F S           F S           F S           F S           F S
  cgnat(pr+pr)         D 860         D 510         D 860         D 860           F S           F S         D 860           F S
 cgnat(pr+sym)         D 510         D 530         D 530           F S           F S           F S           F S           F S
direct: 43/64
```

The simulated NATs follow RFC 4787 behaviours. Real carrier-grade NATs, mobile networks and firewalls that block
UDP have not been tested.
