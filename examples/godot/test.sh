#!/bin/sh
# Runs the demo headless on loopback, three times: one Godot instance hosts and a
# second joins, first with its invite, then by room name through a local Nostr relay
# (examples/rust/nostr_relay.rs), then by room name again with both keeping a link
# to a local relay peer (peerwire-peer). Passes when, in every run, both have seen
# the other's square move and its chat line (and, in the third, both linked the
# relay peer and the link between them is direct, as on loopback it can be). The
# joiner runs public-only in the first, relay-only in the second. No public STUN or
# relays are used.
# Needs a Godot 4.3+ binary: GODOT=/path/to/godot (default: godot on PATH).
set -e
cd "$(dirname "$0")"
GODOT=${GODOT:-godot}
cargo build --release -p peerwire-godot
cargo build --release -p peerwire --features nostr --example nostr_relay
cargo build --release -p peerwire-peer
"$GODOT" --headless --path . --import >/dev/null 2>&1 || true
tmp=$(mktemp -d)
host= relay= relay_peer=
trap 'kill $host $relay $relay_peer 2>/dev/null; rm -rf "$tmp"' EXIT

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
    grep -q "^port mapping: off$" "$tmp/$name-host.log" || { echo "$name: no 'port mapping: off' line"; st=1; }
    grep -q "^route to peer 1: direct to " "$tmp/$name-join.log" || { echo "$name: no direct route line"; st=1; }
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

mkdir "$tmp/peer"
PEERWIRE_DATA="$tmp/peer" PEERWIRE_BIND=127.0.0.1:0 PEERWIRE_STUN=none ../../target/release/peerwire-peer >"$tmp/peer.log" 2>&1 &
relay_peer=$!
wait_for "$tmp/peer/invite" "peerwire-peer never wrote an invite"
via="--relay-peer=$(cat "$tmp/peer/invite")"
run relay-peer --host-room="$room-r" --stun=none --relay="$url" "$via" -- --join-room="$room-r" --stun=none --relay="$url" "$via" || status=1
for side in host join; do
    grep -q "^relay peers: 1 linked$" "$tmp/relay-peer-$side.log" || { echo "relay-peer: $side never linked the relay peer"; status=1; }
done
grep -q "^route to peer 1: direct to " "$tmp/relay-peer-join.log" || { echo "relay-peer: no direct route to the host"; status=1; }

[ $status -eq 0 ] && echo "PASS" || echo "FAIL"
exit $status
