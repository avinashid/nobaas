# @nobaas/js

nobaas-core in the browser: the Rust core compiled to a WebAssembly module with no
imports (`crates/nobaas-ffi`), a small JS host, and WebRTC DataChannels as the
datagram transport. No wasm-bindgen, no server of our own.

```
./packages/js/build.sh                         # -> packages/js/pkg/nobaas_ffi.wasm (~300 KB)
python3 -m http.server -d packages/js 8000     # any static server
```

| File | What it is |
|---|---|
| `nobaas.js` | Loads the module; `Endpoint` (connect, send, events, stats, seal/open) and Nostr key helpers. Browser and Node. |
| `webrtc.js` | `WebRtcTransport`: one negotiated, unordered, no-retransmit DataChannel per peer. `BroadcastSignaling` for tabs of one browser. |
| `nostr.js` | `NostrSignaling` over public relays: ephemeral events, a throwaway key per session, payloads sealed to the recipient's PeerId. |
| `demo/index.html` | Mesh chat: every tab links to every other. |
| `demo/arena.html` | Super-peer lockstep arena: `?role=super` in one tab, players in others; arrow keys move. |

Signalling is untrusted by design: whatever carries the offers, the Nobaas Noise
handshake over the DataChannel proves each side holds its PeerId.

## Trying it

- Same browser: open `demo/index.html` or the arena pages in several tabs.
- Different machines: add `?signal=nostr&room=<something only you know>` to every
  page. Peers meet through `wss://relay.damus.io` and `wss://nos.lol` and use
  Google's public STUN. There is no TURN, so two peers behind symmetric NATs won't
  connect directly; the Nobaas peer relay (`connect_via`) is the planned fallback.

## Tests

Run these with Node and the Playwright install on this host:

- `node packages/js/test/node_pair.mjs`: two endpoints in Node, checks the glue.
- `node packages/js/test/browser.mjs [tabs] [shot.png]`: mesh of tabs, RTT and throughput.
- `node packages/js/test/arena.mjs [players] [secs] [shot.png|-]`: the arena. `LOSS=0.1`
  drops datagrams. Checks that every island agrees on its lockstep state.
- `node packages/js/test/nostr.mjs`: two isolated browser profiles meet over public
  Nostr relays and connect.
