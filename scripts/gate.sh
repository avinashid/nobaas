#!/usr/bin/env bash
# The release gate: every check a change must pass before merge (see docs/TEST-PLAN.md).
# Runs every step even after a failure, prints a summary and exits non-zero if any step failed.
#
#   scripts/gate.sh             Rust tests, clippy, wasm build and checks, Node glue, browser tests (loopback)
#   scripts/gate.sh --quick     skip the browser tests
#   scripts/gate.sh --strict    clippy with -D warnings, and the fuzz tests again in a debug build (overflow checks)
#   scripts/gate.sh --nostr     also run the public-relay Nostr tests, JS and Rust (touch relay.damus.io, nos.lol,
#                               public STUN)
#
# Env: WASM_MAX_BYTES (default 358400 = 350 KB), PLAYWRIGHT (path to playwright's index.mjs).
set -u
cd "$(dirname "$0")/.."

quick=0 strict=0 nostr=0
for a in "$@"; do
  case "$a" in
    --quick) quick=1 ;;
    --strict) strict=1 ;;
    --nostr) nostr=1 ;;
    -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
if ! command -v node >/dev/null && [ -s "$HOME/.nvm/nvm.sh" ]; then . "$HOME/.nvm/nvm.sh" >/dev/null; fi

WASM_MAX_BYTES=${WASM_MAX_BYTES:-358400}
WASM=packages/js/pkg/nobaas_ffi.wasm
results=()
failed=0

step() {
  local name=$1; shift
  echo
  echo "==> $name: $*"
  local t0=$SECONDS
  if "$@"; then
    results+=("PASS  $name ($((SECONDS - t0))s)")
  else
    results+=("FAIL  $name ($((SECONDS - t0))s)")
    failed=1
  fi
}

skip() {
  results+=("SKIP  $1 ($2)")
}

# Size and import count of the browser module: it must stay small and import nothing.
wasm_check() {
  node -e '
    const fs = require("fs");
    const [path, max] = [process.argv[1], Number(process.argv[2])];
    const bytes = fs.readFileSync(path);
    const imports = WebAssembly.Module.imports(new WebAssembly.Module(bytes));
    console.log(`${path}: ${bytes.length} bytes (limit ${max}), ${imports.length} imports`);
    for (const i of imports) console.log(`  import ${i.module}.${i.name} (${i.kind})`);
    process.exit(bytes.length <= max && imports.length === 0 ? 0 : 1);
  ' "$WASM" "$WASM_MAX_BYTES"
}

step "cargo test" cargo test --workspace --release
if [ $strict = 1 ]; then
  step "cargo clippy" cargo clippy --workspace --all-targets -- -D warnings
  # Release builds wrap on integer overflow; the parser fuzzers must also run where it panics.
  step "fuzz (debug)" cargo test --lib -p nobaas-core -p nobaas -p nobaas-ffi -p nobaas-sim fuzz
else
  step "cargo clippy" cargo clippy --workspace --all-targets
fi
if cargo fmt --version >/dev/null 2>&1; then
  step "cargo fmt" cargo fmt --all -- --check
else
  skip "cargo fmt" "rustfmt not installed"
fi
step "wasm build" ./packages/js/build.sh
step "wasm size+imports" wasm_check
step "node glue" node packages/js/test/node_pair.mjs
step "node privacy" node packages/js/test/privacy.mjs
step "node room" node packages/js/test/room_node.mjs
step "node peer ids" node packages/js/test/peer_id.mjs
step "node nostr bounds" node packages/js/test/nostr_bounds.mjs
step "node room proof" node packages/js/test/room_proof.mjs
step "node pump re-arm" node packages/js/test/pump_rearm.mjs
step "node endpoint closed" node packages/js/test/endpoint_closed.mjs
step "node scratch buffers" node packages/js/test/scratch_buffers.mjs
step "node room drain" node packages/js/test/room_drain.mjs
step "node fuzz" node packages/js/test/fuzz.mjs
step "node room leave" node packages/js/test/room_leave.mjs

if [ $quick = 1 ]; then
  skip "browser" "--quick"
  skip "arena" "--quick"
  skip "readme chat" "--quick"
  skip "poll" "--quick"
  skip "webrtc signals" "--quick"
  skip "tab close" "--quick"
else
  step "browser" node packages/js/test/browser.mjs 2
  step "arena" node packages/js/test/arena.mjs 8 10
  step "readme chat" node packages/js/test/readme.mjs
  step "poll" node packages/js/test/poll.mjs 3
  step "webrtc signals" node packages/js/test/webrtc_signals.mjs
  step "tab close" node packages/js/test/leave.mjs --runs=5 --crash=0
fi
if [ $nostr = 1 ]; then
  step "nostr" node packages/js/test/nostr.mjs
  step "nostr (rust)" cargo test -p nobaas --release --test nostr -- --ignored --nocapture
else
  skip "nostr" "public relays; pass --nostr"
  skip "nostr (rust)" "public relays; pass --nostr"
fi

echo
echo "==> gate summary"
printf '  %s\n' "${results[@]}"
[ $failed = 0 ] && echo "GATE PASSED" || echo "GATE FAILED"
exit $failed
