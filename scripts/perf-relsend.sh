#!/usr/bin/env bash
# Share of the bulk benchmark's CPU spent in RelSend's scans (docs/PERFORMANCE.md).
#   scripts/perf-relsend.sh [bulk example flags]     e.g. --up 1000
# Needs perf; set PERF="sudo perf" where perf_event_paranoid forbids user profiling.
set -euo pipefail
cd "$(dirname "$0")/.."
PERF=${PERF:-perf}
target=${TMPDIR:-/tmp}/nobaas-perf-target
data=${TMPDIR:-/tmp}/nobaas-relsend.perf
RUSTFLAGS="-C force-frame-pointers=yes" CARGO_PROFILE_RELEASE_DEBUG=true CARGO_TARGET_DIR="$target" \
  cargo build -q --release -p nobaas-sim --example bulk
args=("$@")
[ ${#args[@]} -gt 0 ] || args=(--up 100)
$PERF record -q -F 2999 --call-graph fp -o "$data" "$target/release/examples/bulk" --cpu 1 "${args[@]}"
$PERF script -i "$data" -F ip,sym --inline 2>/dev/null | python3 -c '
import sys
fns = ["has_sendable", "next_due", "write_frames"]
hits = dict.fromkeys(fns, 0); total = 0
for block in sys.stdin.read().split("\n\n"):
    names = [l.strip().split(" ", 1)[-1].replace(" (inlined)", "") for l in block.strip().split("\n") if l.strip()]
    if not names:
        continue
    total += 1
    for n in names:  # innermost of the three
        if n in hits:
            hits[n] += 1
            break
for n in fns:
    print(f"{n:14} {100 * hits[n] / total:5.1f}% of process samples")
print(f"all three      {100 * sum(hits.values()) / total:5.1f}% of {total} samples")
print("Divide by the sender share of wall time (the cpu line above) for the share of the sender.")
'
