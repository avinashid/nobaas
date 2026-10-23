# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository. Read this first, then `README.md` and
`docs/ARCHITECTURE.md`.

## What this is

Peerwire is a peer-to-peer networking toolkit in Rust: multiplayer games, whiteboards,
chat and meetings without developer-owned servers or paid relays. It ships as Rust crates, a ~300 KB zero-import
WebAssembly module with a small JS package, and a C ABI. Status: early development (v0.1).

## Layout

| Path | What it is |
|---|---|
| `crates/peerwire-core` | Sans-IO session core: identity, Noise IK handshake, lanes (reliable/unreliable/urgent), NAT traversal, peer relay, STUN codec, invites (`pw:` prefix) |
| `crates/peerwire-sim` | Deterministic network simulator and the multiplayer arena benchmark; integration tests live in `crates/peerwire-sim/tests/` |
| `crates/peerwire-ffi` | C ABI (`pw_*` symbols, `PW_ERR_*`), the one surface for wasm and native bindings |
| `crates/peerwire-game` | Deterministic fixed-point maths, Hilbert-curve islands, desync detection |
| `packages/js` | `@peerwire/js`: `src/room.js` (`joinRoom`), `src/peerwire.js` (`loadPeerwire`, class `Peerwire`), WebRTC transport, Nostr signaling, types in `src/index.d.ts`, demos, tests |
| `examples/` | Live examples (in progress) |
| `docs/` | Architecture, performance model, comparison with other libraries |

## Setup

```sh
source ~/.cargo/env      # Rust; wasm32-unknown-unknown and clippy are installed, rustfmt is not
source ~/.nvm/nvm.sh     # Node
godot --version          # Godot 4.7.2 (user-local in ~/.local/opt/godot); use --headless on this server
```

Do not install toolchains, `cargo install` tools or system packages without asking. Adding crates.io
dependencies is fine; keep them few and justify them in the commit message.

## Gate: who runs what

The full gate runs once per change, by the lead on the merged `main`. Each worktree builds from scratch and parallel
runs share this host's 4 cores, so running it on every branch as well costs more than it catches.

- **Before you hand a branch over** (author):

  ```sh
  scripts/gate.sh --quick            # Rust tests, clippy, wasm build, size and imports, Node tests; no browser tests
  scripts/gate.sh --quick --strict   # the same with clippy -D warnings and the debug fuzz run: use it if you changed Rust
  node packages/js/test/<name>.mjs   # each browser test next to what you changed, e.g. netlab_*.mjs for Net Lab
  ```

  A change comes with its test; run it. A change to docs or comments only needs no run: say so on the card.
  When every file changed since `origin/main` is JS, docs or another non-Rust path, the gate skips the Rust steps
  (`SKIP cargo test (no Rust change since origin/main <sha>)`); `--full` runs them anyway.
- **Reviewing** (security, QA): read the diff against the code. Re-run a test only to reproduce a problem or try a
  mutant, and say which.
- **After merging** (the lead): `scripts/gate.sh --strict` once on the merged `main`, before asking to push. CI runs
  the Rust and Node steps on push but not the browser tests, so this is their only full run.

The gate builds first, then runs the Rust tests (under `cargo-nextest` when it is installed) beside the Node tests,
`GATE_JOBS` (default 4) at a time, so a Node test must not use a fixed port or file. Each step logs to
`target/gate-logs/<step>.log`; a failed step's log is printed in full above the summary.

Browser tests use headless Chromium through Playwright (already installed; do not install it):
`node packages/js/test/browser.mjs [tabs]` and `node packages/js/test/arena.mjs` run on loopback only.
`node packages/js/test/nostr.mjs` touches public Nostr relays and public STUN: run it only when the task needs it.

Report what actually ran and its output. Never claim a test passed without running it.

## Rules for code

- Keep `peerwire-core` **sans-IO and deterministic**: no sockets, threads, clocks or randomness inside it; the host
  passes time and bytes in. This is what makes the simulator and wasm build possible.
- Keep the wasm build **free of imports** (no wasm-bindgen, no std I/O in `peerwire-ffi`).
- Wire-format, crypto, relay or signaling changes need a security review (see `SECURITY.md`) and a test.
- Every performance number in `docs/` must be reproducible from the repo, or labelled **theoretical** with its
  formula. This machine is small (4 cores, aarch64): no heavy load tests; use the simulator or estimates.
- Tests stay local: loopback, LAN, or the simulator. Never point code at third-party production services or run
  anything at scale against public infrastructure. Public STUN (Google, Cloudflare) and the Nostr relays
  `relay.damus.io`, `nos.lol` and `relay.primal.net` are the only external services allowed, and only for small, local-to-local tests.
- Match the surrounding style. rustfmt is not installed, so format by hand to the existing code.

## Git

- Never commit to `main` directly. Work on a branch `<area>/NB-XXX-slug` in your own worktree:
  `git -C ~/projects/personal/peerwire worktree add ../peerwire-wt/<role> -b <role>/NB-XXX-slug`.
- Small, descriptive commits: `git -c core.hooksPath=/dev/null commit -q -m "<what changed>"`.
- **No AI or tool attribution anywhere in git**: no `Co-Authored-By` trailer, no "Generated with" line, no emoji
  signatures. Commit messages describe the change only.
- **Never push, force-push, rewrite history or change remotes.** Only the lead (`@protocol-zero`) merges to `main`
  and pushes.
- Never commit secrets, keys, `.env` files, build output (`target/`, `packages/js/pkg/`) or screenshots.

## Tasks and the team (local only)

- Task files, the roadmap and the team rules live in `~/projects/personal/peerwire/tasks/`.
- That folder is **git-ignored and must never be pushed or force-added**, so it is not in your worktree: read and
  update it at that absolute path. Start with `tasks/TEAM.md` and your task file `tasks/backlog/NB-XXX-*.md`.
- Each task has a card on the Peerwire board.
  - Move it to Doing when you start, comment progress as you go, and move it to Needs review when done.
  - Name the branch and commits on the card, with the gate output.
- Deliverables meant for the repo (test plans, threat model, design notes) go in `docs/`, not `tasks/`.
