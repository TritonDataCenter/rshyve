#!/usr/bin/env bash
# run.sh: profile a VMM boot with DTrace, on an illumos host.
#
# Usage: run.sh [-d duration_sec] [-o outdir] -- <vmm-argv...>
#
# Env:
#   VMM_PROFILE_BIN   VMM to run (default: target/release/firehyve)
#
# Produces, in <outdir>:
#   profile.out    raw DTrace output
#   stacks.raw     the USER STACKS block
#   stacks.folded  collapsed user stacks (flamegraph input)
#   top.txt        top stacks report
#   guest.log      guest console output
#   vmm.log        VMM stderr
#   wall.txt       the sampling window, as a sanity check on -d

set -euo pipefail

DUR=10
OUT=""

while [ $# -gt 0 ]; do
    case "$1" in
        -d) DUR="$2"; shift 2 ;;
        -o) OUT="$2"; shift 2 ;;
        --) shift; break ;;
        *)  echo "usage: run.sh [-d sec] [-o dir] -- <vmm-argv...>" >&2
            exit 2 ;;
    esac
done

if [ $# -eq 0 ]; then
    echo "error: no VMM arguments given after --" >&2
    exit 2
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="${VMM_PROFILE_BIN:-target/release/firehyve}"

if [ ! -x "$BIN" ]; then
    echo "error: VMM binary not found at $BIN" >&2
    echo "hint: cargo build --release, or set VMM_PROFILE_BIN" >&2
    exit 1
fi

# The kernel probe only exists in the global zone. Say so now, instead
# of handing back an empty exit count that reads like a fast boot.
if ! dtrace -l -n 'sdt:vmm::vmm-vexit' 2>/dev/null | grep -q vmm-vexit; then
    echo "error: sdt:vmm::vmm-vexit is not available here" >&2
    echo "hint: kernel probes need the global zone and dtrace_kernel" >&2
    exit 1
fi

# mktemp, not a fixed path: a fixed one invites `rm -f /tmp/<fixed>/*`.
if [ -z "$OUT" ]; then
    OUT="$(mktemp -d -t vmm-profile)"
else
    mkdir -p "$OUT"
fi

echo "[profile] running: $BIN $*"
echo "[profile] output dir: $OUT"

now_ns() { dtrace -qn 'BEGIN { printf("%d\n", walltimestamp); exit(0); }'; }
START_NS=$(now_ns)

"$BIN" "$@" > "$OUT/guest.log" 2> "$OUT/vmm.log" &
PID=$!
echo "[profile] vmm pid=$PID"

# Small settle delay so the vCPU threads exist before dtrace attaches.
sleep 0.05

dtrace -q -p "$PID" -o "$OUT/profile.out" -s "$HERE/profile.d" &
D1=$!

echo "[profile] dtrace pid=$D1; sampling ${DUR}s..."
sleep "$DUR"

kill "$D1" 2>/dev/null || true
wait "$D1" 2>/dev/null || true

if kill -0 "$PID" 2>/dev/null; then
    kill -TERM "$PID" 2>/dev/null || true
    sleep 0.5
    kill -KILL "$PID" 2>/dev/null || true
fi
wait "$PID" 2>/dev/null || true

END_NS=$(now_ns)
echo "[profile] wall window: $(( (END_NS - START_NS) / 1000000 )) ms" \
    | tee "$OUT/wall.txt"

awk '/^# USER STACKS BEGIN/,/^# USER STACKS END/' "$OUT/profile.out" \
    > "$OUT/stacks.raw"
awk -f "$HERE/collapse.awk" "$OUT/stacks.raw" > "$OUT/stacks.folded"
awk -f "$HERE/top-stacks.awk" -v N=30 "$OUT/stacks.folded" > "$OUT/top.txt"

echo
echo "==== profile.out (summary section) ===="
awk '/^=== elapsed/,/# USER STACKS BEGIN/{
    if ($0 !~ /# USER STACKS BEGIN/) print
}' "$OUT/profile.out"
echo
echo "==== top.txt ===="
cat "$OUT/top.txt"
echo
echo "==== guest.log head (first 40 lines) ===="
head -40 "$OUT/guest.log"
echo
echo "==== vmm.log ===="
cat "$OUT/vmm.log"
echo
echo "[profile] done. artifacts in $OUT"
