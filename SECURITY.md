# Security policy

Peerwire is pre-1.0 research-grade software and has **not** had an external audit. Don't rely on it yet to protect
high-value data.

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's "Report a vulnerability" (Security advisories) on this
repository. Don't open a public issue. We aim to acknowledge within 7 days.

## Scope

- the `peerwire-core` handshake, wire format, relaying and NAT traversal;
- the C ABI (`peerwire-ffi`) and the JavaScript glue;
- the signaling adapters.

## Threat model

See [docs/THREAT-MODEL.md](docs/THREAT-MODEL.md) for what Peerwire defends against,
its trust boundaries, and the standing findings.

## What Peerwire gives you today

- **Authenticated, encrypted links.** A `PeerId` is an Ed25519 public key; links
  run Noise IK keyed by that identity, so no signaling relay, peer relay or NAT
  can impersonate a peer or read link traffic. Signaling payloads (which carry IP
  candidates) are sealed to the recipient PeerId.
- **Replay- and flood-resistant data path.** A sliding nonce window drops replayed
  data packets; half-open handshakes and peer relaying are capped and
  rate-limited.

## What it does not give you yet

- **Signaling metadata privacy.** On the Nostr adapter, anyone who knows a room
  name can see its members' PeerIds, and anyone who knows your PeerId can probe
  for your IP candidates. Treat room names as secrets and don't publish PeerIds
  you want kept private. (THREAT-MODEL.md SEC-2, SEC-3, SEC-5.)
- **Connection-setup DoS resistance** against a source-address spoofer: there is
  no address-validation cookie yet (SEC-4).
- **In-session forward secrecy refresh:** session keys are not rotated until a
  reconnect (SEC-7).

The first review (NB-013, commit `cf9cbb0`) found no issues in the core wire,
invite and STUN parsers, the relay gating, or the handshake authentication. It did
find a memory-safety bug at the JS/FFI boundary (SEC-1) and the signaling-privacy
and DoS items above; all are tracked as NB tasks. SEC-1 is fixed (NB-021), and
room-key proofs (NB-031, NB-037) stop room offers from strangers revealing your
addresses (SEC-3, for rooms). THREAT-MODEL.md lists each finding's status.
