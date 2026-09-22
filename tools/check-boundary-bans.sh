#!/bin/sh
# usage: tools/check-boundary-bans.sh          (no arguments; run from the repo root)
# exit 0  the config exists, jq is available, and every banned crate
#         is a workspace member
# exit 1  the config is missing, jq is missing, or a ban entry names a
#         crate that is gone
#
# Guards three failure modes cargo-deny cannot report on its own:
#
#   1. An unmatched `deny` entry is silent, so a crate rename disarms
#      that entry without warning.
#   2. A missing --config file makes cargo-deny fall back to the
#      default config, which bans nothing, and still exits 0.
#   3. jq is assumed present on the runner. Check it, do not assume it.
#
# requires: cargo, jq

set -eu

CONFIG=deny-firehyve.toml

if [ ! -f "$CONFIG" ]; then
    echo "$CONFIG is missing" >&2
    echo "  cargo-deny would fall back to the default config and pass" >&2
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required but is not installed" >&2
    exit 1
fi

members=$(cargo metadata --locked --format-version 1 --no-deps \
    | jq -r '.packages[].name' | sort -u)
banned=$(sed -n \
    's/^[[:space:]]*{[[:space:]]*crate[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
    "$CONFIG")

if [ -z "$banned" ]; then
    echo "$CONFIG lists no banned crates" >&2
    echo "  the deny list is empty or its format changed; the gate is disarmed" >&2
    exit 1
fi

rc=0
for crate in $banned; do
    if ! printf '%s\n' "$members" | grep -qxF "$crate"; then
        echo "$CONFIG bans '$crate', which is not a package in this workspace" >&2
        echo "  a rename left that ban entry dead; cargo-deny will not report it" >&2
        rc=1
    fi
done
exit $rc
