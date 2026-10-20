#!/bin/sh
# Runs the demo headless on loopback: one Godot instance hosts, a second joins with
# its invite. Passes when both have seen the other's square move and its chat line.
# Needs a Godot 4.3+ binary: GODOT=/path/to/godot (default: godot on PATH).
set -e
cd "$(dirname "$0")"
GODOT=${GODOT:-godot}
cargo build --release -p nobaas-godot
"$GODOT" --headless --path . --import >/dev/null 2>&1 || true
tmp=$(mktemp -d)
trap 'kill $host 2>/dev/null; rm -rf "$tmp"' EXIT
"$GODOT" --headless --path . -- --host="$tmp/invite" >"$tmp/host.log" 2>&1 &
host=$!
i=0
while [ ! -s "$tmp/invite" ]; do
    i=$((i + 1)); [ $i -gt 100 ] && { cat "$tmp/host.log"; echo "host never wrote an invite"; exit 1; }
    sleep 0.1
done
status=0
"$GODOT" --headless --path . -- --join="$tmp/invite" >"$tmp/join.log" 2>&1 || status=$?
wait $host || status=$?
echo "--- host";  cat "$tmp/host.log"
echo "--- joiner"; cat "$tmp/join.log"
[ $status -eq 0 ] && echo "PASS" || echo "FAIL"
exit $status
