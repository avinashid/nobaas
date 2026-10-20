#!/bin/sh
# Runs the demo headless on loopback, twice: one Godot instance hosts and a second
# joins, first with its invite, then by room name through a local Nostr relay
# (examples/rust/nostr_relay.rs). Passes when, in both runs, both have seen the
# other's square move and its chat line. The joiner runs public-only in the first,
# relay-only in the second. No public STUN or relays are used.
# Needs a Godot 4.3+ binary: GODOT=/path/to/godot (default: godot on PATH).
set -e
cd "$(dirname "$0")"
GODOT=${GODOT:-godot}
cargo build --release -p nobaas-godot
cargo build --release -p nobaas --features nostr --example nostr_relay
"$GODOT" --headless --path . --import >/dev/null 2>&1 || true
tmp=$(mktemp -d)
host= relay=
trap 'kill $host $relay 2>/dev/null; rm -rf "$tmp"' EXIT

wait_for() { # file, what
    i=0
    while [ ! -s "$1" ]; do
        i=$((i + 1)); [ $i -gt 100 ] && { echo "$2"; exit 1; }
        sleep 0.1
    done
}

# run NAME HOST_ARGS... -- JOIN_ARGS...
run() {
    name=$1; shift
    hargs=; while [ "$1" != "--" ]; do hargs="$hargs $1"; shift; done; shift
    "$GODOT" --headless --path . -- $hargs >"$tmp/$name-host.log" 2>&1 &
    host=$!
    [ "$name" = invite ] && wait_for "$tmp/invite" "host never wrote an invite"
    st=0
    "$GODOT" --headless --path . -- "$@" >"$tmp/$name-join.log" 2>&1 || st=$?
    wait $host || st=$?
    host=
    echo "--- $name: host";   cat "$tmp/$name-host.log"
    echo "--- $name: joiner"; cat "$tmp/$name-join.log"
    [ $st -eq 0 ] && echo "$name: PASS" || echo "$name: FAIL"
    return $st
}

status=0
run invite --host="$tmp/invite" --stun=none --relay=none -- --join="$tmp/invite" --stun=none --relay=none --privacy=public-only || status=1

../../target/release/examples/nostr_relay 127.0.0.1:0 >"$tmp/relay.url" &
relay=$!
wait_for "$tmp/relay.url" "the local relay did not start"
url=$(cat "$tmp/relay.url")
room="test-$$"
run room --host-room="$room" --stun=none --relay="$url" -- --join-room="$room" --stun=none --relay="$url" --privacy=relay-only || status=1

[ $status -eq 0 ] && echo "PASS" || echo "FAIL"
exit $status
