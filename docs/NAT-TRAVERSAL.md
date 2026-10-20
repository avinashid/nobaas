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
- `Cone` covers full, restricted and port-restricted cones, which STUN can't tell apart. So `Cone` doesn't
  promise a punch: a port-restricted cone only lets in packets from ports it has sent to, and fails against a
  symmetric NAT whose ports can't be predicted (see the matrix below).

## Port prediction

A symmetric NAT gives each destination a new public port, so the port it offers (from STUN) is not the one its
packets to the peer come from, and a port-restricted peer drops them. Many symmetric NATs hand ports out in
sequence, though:

- **Prediction.** If both STUN mappings share an IP and the second is 1 to 8 ports above the first, the NAT looks
  sequential with that step, and the next mapping is likely `second + step` (`Traversal::predicted_addr`).
  Random-port NATs give no prediction.
- **Offer.** The prediction rides in the offer's address list as a tagged entry (below), only when the public
  address leads the list. It takes one of the 8 slots.
- **Probing.** A receiver whose NAT is neither open nor symmetric (open and symmetric ones gain nothing, and a
  symmetric one would spend a port per probe) hands the core 32 *guesses*: ports `next - 4·step` to
  `next + 27·step`, leaning forward because the NAT may have given out more since STUN asked
  (`Endpoint::set_guesses`). Guesses are probe-only:
  - while dialling, every handshake retry also goes to each guess;
  - while relayed, every 5th punch round (about two a second) probes them;
  - a guess becomes the path only when the peer answers from it;
  - relay-only links that go through a relay ignore them.
- **Cost.** At most 32 guesses (`MAX_GUESSES` is 64) per offer acted on, each probed by about 6 handshake
  packets while dialling: within the reflection bound below, but to ports of one IP the offer named.
- Datagrams are told apart by their first byte (RFC 7983): STUN goes to the traversal, everything else to the
  endpoint.

## Topics and the room key

Topics use the same derivation as `packages/js/nostr.js`, so Rust and browser peers can share relays:

- room: `sha256("nobaas/room/" + app_id + "/" + room)`; a JS peer in room `"<app_id>/<room>"` uses the same topic;
- peer inbox: `sha256("nobaas/peer/" + lowercase hex of the 32-byte PeerId)`.

Every relay subscriber sees topics, so a topic proves nothing. The **room key**,
`sha256("nobaas/room-key/" + app_id + "/" + room)`, is the same name hashed under another label. It can't be
derived from the topic, so only peers given the name hold it. Hellos and room offers carry HMAC-SHA256 proofs
under it. The room name works as the room's password: a long random name makes a private room, and a name
others can guess makes a public lobby.

## Messages

**Hello** (room topic, plaintext). The JSON shape of `nostr.js`, with a marker for UDP peers and a room proof:

```json
{"t":"hello","from":"<64 hex PeerId>","meta":{"nb":1,"role":"peer|host|client","ts":<Unix ms>,"p":"<64 hex>"}}
```

- `p = HMAC(room key, "nb-hello" || PeerId || role byte || ts u64 LE)`.
- Parsing is strict: exact prefix, 64 hex digits, known role, 1 to 19 decimal digits of `ts`, 64 hex digits of
  proof, exact suffix.
- A hello with a wrong or missing proof, or with `|ts - now| > 60 s`, is dropped before anything else happens.
  Browser hellos lack `meta.nb` and are ignored.
- A hello can be replayed within its minute, but only as itself: it names its sender. The offer it triggers is
  sealed to that sender, and the dial authenticates the sender's key.

**Offer** (peer topic): `sealed::seal(me, to, …)` (`Noise_X_25519_ChaChaPoly_BLAKE2s`, sender authenticated),
around:

```text
ver   0x01 (plain), 0x02 (room) or 0x03 (invite)
      reply(0|1)  role(0 peer,1 host,2 client)  nat(0 unknown,1 open,2 cone,3 symmetric)
ts    u64 LE      sender's Unix time in ms
nonce u64 LE      random
n     u8 (<= 8)   then n x [len u8][Address::encode], or one port prediction entry:
                  [5][0xfe 0x01][step u8][next port u16 LE]
m     u8 (<= 8)   then m x 32-byte PeerId (peers the sender is connected to)
proof 32 B        version 2: HMAC(room key, "nb-offer" || sender || recipient || everything above)
                  version 3: HMAC(recipient's invite key, "nb-invite" || sender || recipient || everything above)
```

The invite key is 32 random bytes per node, carried in its invites (`Node::invite`, invite version 2). A node
sends version 3 to a peer whose invite it dialled, version 2 in a room, and version 1 otherwise (answers to a
peer that dialled it). The address list is filtered by the sender's [privacy mode](#privacy-modes).

The decoder rejects:
- an unknown version, flags other than 0 or 1, unknown roles or NAT types;
- n or m above 8;
- truncation and trailing bytes.

Addresses that fail to decode are skipped. Decoders from before port prediction read the prediction entry as
an address that fails to decode, so old and new nodes still talk; the prediction is just unused. A decoded
prediction with step 0 or above 8 is dropped.

## Rules

- **Roles.** Peer↔Peer, Peer↔Host, Host↔Client. Clients never link to clients, and hosts never to hosts. Outside
  a room (an invite join), any role is accepted.
- **Which offers count.** A version-2 offer only in a room, with its proof checking out; a version-3 offer only
  with a proof under this node's own invite key; a version-1 offer only from a peer this node dialled itself with
  `connect` (its answer). Anything else, or a version 2 or 3 too short to hold a proof, is dropped unread.
- **Membership.** In a room, a node takes only version-2 offers whose proof checks out, plus the answers of peers
  it dialled, so invite dials still punch.
  - A peer whose hello or offer proved the key is a member in the role it proved.
  - `Node` closes, unseen, any link from a peer that is neither a member in a role it links with nor one it
    dialled. Messages and `AddrObserved` from such a peer never surface.
  - A client can pin its host (`Node::join_room_with(.., Some(host))`, Godot `join_room_host`). It then ignores
    every other member.
  - The Godot room client takes as its host only a peer that proved the `host` role.
- **Discovery.** On a proven hello from a peer it isn't linked to, a node sends an offer with `reply=1`. It sends
  at most one offer per peer per 2 s. Hellos repeat at 2, 4, 8, 16, then every 30 s, for peers that missed one.
- **On an offer.** These checks run in order, each one dropping the offer:
  1. the proof, or the dialled-peer check for version 1;
  2. the role;
  3. `|ts - now| <= 60 s`;
  4. a nonce already seen from that sender in the last 2 min;
  5. the sender's limit, and the node's offer budget unless this node dialled the sender.

  The nonce is stored only for an offer that passes all of them, and at most 1024 are kept. The node then dials
  every candidate that is *dialable* and not one of its own addresses, plus `Via(r)` for up to 2 listed peers it
  is itself connected to. If already linked, it calls `add_candidates` instead (punching a relayed link). If
  `reply=1`, it answers with its own offer (`reply=0`). A relay-only node instead dials anyone but its relay as
  `Via(relay)` only (see below).
- **Dialable addresses.** Never port 0, unspecified, multicast or broadcast. Never loopback (including
  `::ffff:127.0.0.1`), unless the node is itself bound to loopback. The same filter applies to addresses peers
  report seeing (`AddrObserved`) and to STUN answers. An offer can't aim a node at services on its own host.
- **Invites.** `Node::connect` dials the invite's addresses, then sends the host a version-3 offer (proved with
  the invite's key) with `reply=1`, so the host dials back at the same time. An invite without a key (version 1)
  still dials, but gets no dial-back. A host that is announcing in a room turns away invite
  joiners: in a room, knowing the name is the admission.
- **New links.** When a link comes up, peers that offered to this node but are still unlinked get a fresh offer
  listing the new peer, which may be the relay they need.
- **Budgets.** Each budget is a burst of 30, then one per 2 s:
  - hellos answered;
  - offers acted on.

  On top of that, each sender's offers get a burst of 8, then one per second, so one key can't spend the node's
  offer budget alone. Proofs are checked before any token is taken, so forged hellos and offers cost nothing.
  Answers from peers this node dialled skip the node's offer budget (the sender's limit still applies), so
  others, even invite holders, can't crowd out the punch of a dial this node made. The
  limits bound what floods can make a node dial, seal or publish, and are ample for the room sizes this layer
  targets (no mesh limit yet).
- **Failure.** When a dial ends in `ConnectFailed`, `failure(peer)` is `Symmetric(side)` if one side is
  symmetric and the other isn't open, and `Unreachable` otherwise. `side` is `Ours`, `Theirs` or `Both`. Its
  `Display` is a sentence for users, such as "the peer's NAT is symmetric (a new public port per destination)
  and this side's router only lets in packets from ports it has sent to, so hole punching can't work, and no
  peer linked to both could relay". Godot shows it as `get_connect_error_message()`.

## Privacy modes

`Privacy` (`Builder::privacy`, `Node::set_privacy`, `Traversal::set_privacy`) decides which addresses a node
gives out. Changing it affects the next invites and offers; links already up keep their path.

- **`Direct`** (default): every candidate. The public one from STUN, ones peers observed, the LAN and loopback.
- **`PublicOnly`**: no loopback, RFC 1918, CGNAT (100.64/10), link-local, IPv6 ULA, unspecified, multicast or
  broadcast address, including private ones peers observed. Two peers on one LAN then link through their NAT
  if it hairpins, or through a relay.
- **`RelayOnly { via }`**: no addresses at all, except to the relay, which gets the public ones (it links
  directly, so it sees the address anyway). The relay is `via`, else the room's pinned host, else the first
  peer that proved the Host role and was dialled. That pick sticks while its dial or link lasts (a later Host, of
  any PeerId, is dialled through it, never directly) and is dropped when the dial fails, the link goes down or the
  node joins another room. Offers name only the relay as a possible relay, never the node's other links.
  - Without a pinned host, the relay is whoever answers first as Host, and anyone who knows the room name can do
    that and so learn the node's IP: pin the host (`join_room_with(.., Some(host))`, Godot `join_room_host`)
    when its key is known. The Godot binding pins `via` to the host a client adopts.
  - The node's invite names the relay with the relay's address: its holders learn the relay's IP.
  - Offers from anyone but the relay are dialled as `Via(relay)` only. If the relay isn't linked yet, the dial
    waits and happens when it links. `Node::connect` dials `Via(relay)` only, and fails with
    `ConnectError::NoRelay` while no relay is linked. It never falls back to a direct path.
  - `Node` closes, unseen, a new direct link from anyone but the relay.
  - The endpoint is relay-only (`Endpoint::set_relay_only`). Its relayed links take no candidates (from offers,
    direct packets or the relay's origin tags), never punch, never answer a punch and never move to a direct
    path. Dials that name a relay drop their direct targets. A direct handshake is answered only from a peer it
    is itself dialling directly or already has a direct link with. It sends no OBSERVED frames or origin tags.
  - It sends its relayed packets in `FWD_QUIET` frames: the relay forwards them in `RELAYED` frames with no
    origin address. (`FWD` asks the relay to say where it sees the sender, so that the far end can punch.) A
    build older than `FWD_QUIET` takes frame type 11 as a protocol violation and drops the link.
  - Cost: latency `rtt(A,R) + rtt(R,B)`, and the relay uploads every relayed byte again (capped by
    `Config::relay_rate`, 256 KiB/s per requesting peer by default).

Per-mode outcome in the simulator: a room with a public host and peers A and B over 36 pairs of NAT kinds (each
unordered pair once) plus two peers behind one non-hairpinning NAT, from `cargo test -p nobaas-sim --release --test
traversal privacy_modes_matrix -- --nocapture`:

```text
                            Direct: direct 29, relayed  8, failed 0
                        PublicOnly: direct 28, relayed  9, failed 0
           RelayOnly { via: None }: direct  0, relayed 37, failed 0
```

## What others learn or can do

- **Relays and anyone watching them** see:
  - room topics and each peer's inbox topic (a hash of its public key);
  - hello JSON, which carries each member's PeerId and role;
  - source IPs at the relay, and message timing.

  They can't read offers, since addresses never leave sealed messages. Without the room name they can't join a
  room or pose as its host. They can replay a member's hello, which only makes members offer to that member again.
- **Knowing a PeerId is not enough** (NB-032, NB-033(b)). PeerIds appear in room hellos and invites, so treat
  them as public. But a node acts on an offer, by dialling back and answering with its own, only if the offer
  proves the room key, proves its invite key, or comes from a peer it dialled. Whoever does get an answer
  learns:
  - the node's candidate addresses, as its privacy mode allows (in `Direct`, **LAN addresses** too);
  - **up to 8 PeerIds** it is linked to (only its relay, under `RelayOnly`).

  In a room, only members (or peers it dialled) get this, and members already know each other. A host answering
  invite joins tells each joiner who else is connected. The invite key is a capability: anyone holding the
  invite has it, and it doesn't rotate yet.
- **Peers linked directly** see each other's source IP:port, whatever the mode; `Event::Connected { path }`,
  `PathChanged` and `Node::path` show it. Only `RelayOnly` avoids direct links. A relay sees both ends.
- **Replay.** A captured offer is dropped once it is older than 60 s or its nonce was seen. Within that window, a
  replay to a node that never saw it (for example, one that restarted) causes one more dial to the original
  sender's addresses.
- **Reflection.** Anyone who can get an offer acted on can make a node dial dialable addresses: at most 8 per
  offer, about 6 handshake datagrams each (around 100 B) over 30 s. That is roughly 4.8 KB per accepted offer,
  within the budgets above. An offer with a port prediction adds up to 32 guessed ports on the IP it offered,
  probed the same way: about 19 KB more per accepted offer, to one IP. While a link stays relayed, its guesses
  are probed again every 5th punch round.
- **Nostr relays** (`NostrSignaling`):
  - Events are checked against their NIP-01 id before deduplication, so a relay can't present one event as many.
    Signatures aren't checked: offers are sealed and authenticate themselves, and hellos carry room proofs.
  - Websocket messages and frames above 128 KiB close the connection.
  - Events waiting to be polled are capped at 1024; past that they are dropped.
  - `wss://` uses rustls with the webpki roots and hostname checks.
  - On `ws://` (a LAN or loopback relay), anyone on the path can drop or inject events. Offers stay sealed and
    hellos stay proven, so that costs availability, not membership.
- **STUN servers** see the node's public address. A forged STUN answer needs the 96-bit transaction id and the
  server's source address.

## Dependencies

Crates added to `nobaas` by NAT traversal:
- `sha2` and `hmac`: RustCrypto, MIT/Apache-2.0.
- Under the `nostr` feature only: `tungstenite`, `rustls` (ring provider), `k256` and `serde_json` (MIT and/or
  Apache-2.0, ISC for ring).
- **`webpki-roots` 1 is CDLA-Permissive-2.0**, because it carries Mozilla's root store as data. It is a
  permissive data licence and needs no notice in binaries. Anyone redistributing the source should keep its
  licence file. Builds without `nostr` don't include it.

## Coverage

- `crates/nobaas/src/traversal.rs` unit tests:
  - hello and offer round trips (versions 1 to 3), hello timestamps, truncation, browser hellos, roles;
  - offer contents in each privacy mode, the public-address classifier, relay-only dials going only through the
    relay (and the room host as the relay), a relay-only client dialling exactly one of two Hosts directly
    (the relay sticks), stale hellos, strangers' plain offers ignored, dialled peers answered
    once the node budget is spent;
  - room proofs: wrong key, topic used as the key, wrong role, pinned host, invited peers;
  - sealed offers turning into dials, and replay and stale drops;
  - separate budgets under hello and offer floods, the per-sender limit, nonces kept only for offers acted on;
  - the dialable-address filter;
  - STUN classification and retransmits, port prediction from the two mappings;
  - predictions round-tripping in offers, and cone (not open or symmetric) receivers probing them;
  - failure messages naming the symmetric side.
- `crates/nobaas/tests/room_security.rs`:
  - the security review's `sec_room_topic_only` repro, which no longer links;
  - links from non-members closed unseen;
  - members linking, and a pinned host.
- `crates/nobaas-sim/tests/traversal.rs`:
  - NAT types from simulated STUN;
  - the room-of-two matrix over 8 NAT kinds with 150 ms signaling latency, each failure naming the right side;
  - sequential symmetric NATs against port-restricted ones, over 10 seeds each way round: 80/80 direct;
  - a symmetric pair relayed through a third room peer;
  - an invite join that punches only with signaling;
  - the per-mode matrix above.
- `crates/nobaas/tests/privacy.rs`: invites per mode and switched at runtime, `NoRelay`, relay-only room peers
  linking `Via(host)` with every event and every sealed offer checked for addresses, relay-only invites.
- `crates/nobaas-sim/tests/nat.rs` `guessed_ports_punch_a_sequential_symmetric_nat`: guesses punch from a
  dial and from a relayed link; a relay-only link ignores them.
- `crates/nobaas-sim/tests/nat.rs` `relay_only_links_stay_relayed`: no packet between the two ends, either side
  dialling, and no punch.
- `crates/nobaas/tests/nat_loopback.rs` (mock STUN servers, `MemoryHub`; an invite without its key gets no
  dial-back) and `crates/nobaas/tests/nostr.rs` (local
  relay; `--ignored`: public relays and STUN, run by `scripts/gate.sh --nostr`).

Room of two, signaling only, from `cargo test -p nobaas-sim --release --test traversal -- --nocapture`. D is
direct, with ms to connect. F S is a failure reported as `Symmetric` on both sides, as A reports it: `S<` A's
NAT is the symmetric one, `S^` B's, `S2` both. Port prediction raised this from 43/64 direct; what still fails
is two symmetric NATs, or a random-port symmetric NAT against a port-restricted one:

```text
         A \ B        public     full-cone    restricted    port-restr       sym-seq    sym-random  cgnat(pr+pr) cgnat(pr+sym)
        public         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360
     full-cone         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360
    restricted         D 510         D 510         D 510         D 860         D 860         D 860         D 510         D 860
    port-restr         D 510         D 510         D 510         D 860         D 510          F S^         D 860         D 860
       sym-seq         D 510         D 530         D 510         D 510          F S2          F S2         D 510          F S2
    sym-random         D 510         D 530         D 510          F S<          F S2          F S2          F S<          F S2
  cgnat(pr+pr)         D 860         D 510         D 860         D 860         D 860          F S^         D 860         D 860
 cgnat(pr+sym)         D 510         D 530         D 530         D 530          F S2          F S2         D 530          F S2
direct: 51/64
```

The simulated NATs follow RFC 4787 behaviours. Real carrier-grade NATs, mobile networks and firewalls that block
UDP have not been tested.
