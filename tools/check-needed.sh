#!/bin/sh
# usage: tools/check-needed.sh <elf-binary> [<elf-binary> ...]
# exit 0  every NEEDED entry is a library a stock SmartOS platform
#         image ships, and no recorded OpenSSL version dependency is
#         newer than the oldest supported platform image
# exit 1  otherwise
#
# A binary that records `NEEDED libcrypto.so.3` links and runs on the
# build host, which has pkgsrc, and then fails to exec on a stock node,
# because a platform image ships no such library. Nothing in the build
# reports that: the link succeeds and the tests pass. Run this on every
# binary before it is packaged.
#
# The two checks:
#
#   1. NEEDED must be in ALLOWED below. Every entry there was verified
#      present under /lib/64 or /usr/lib/64 on a stock platform image.
#      Adding one is a deliberate act: check the library really ships
#      in the image, then add it with the rest.
#
#   2. A version dependency on libcrypto-smartos.so.3 must be no newer
#      than MAX_OPENSSL_SMARTOS. The platform OpenSSL tags its exports
#      OPENSSL_SMARTOS_<version>, and ld.so.1 refuses to start a binary
#      whose recorded version is absent from the library it finds. So
#      a binary built on a newer platform image than it will run on
#      dies at exec. Building on an older image is safe: the version
#      dependency stays low and newer libraries still define it.
#
# requires: elfdump and pvs (illumos), or readelf (elsewhere)

set -eu

# Libraries a stock SmartOS platform image provides.
ALLOWED='
libavl.so.1
libc.so.1
libcontract.so.1
libcrypto-smartos.so.3
libdevinfo.so.1
libdl.so.1
libdladm.so.1
libdlpi.so.1
libgcc_s.so.1
libkstat.so.1
libm.so.2
libmd.so.1
libmp.so.2
libnsl.so.1
libnvpair.so.1
libpthread.so.1
libproc.so.1
librt.so.1
libscf.so.1
libsec.so.1
libsocket.so.1
libssl-smartos.so.3
libumem.so.1
libuutil.so.1
libz.so.1
'

# The oldest platform image rshyve is built to run on ships OpenSSL
# 3.0.9. Raising this promises less: it declares that older nodes can
# no longer run the binary. Do not raise it to silence a failure.
MAX_OPENSSL_SMARTOS=3.0.9

if [ $# -eq 0 ]; then
    sed -n '2,3p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
fi

# elfdump and pvs are illumos tools. readelf covers a Linux packaging
# host, which can read an illumos ELF but cannot report version
# dependencies.
if command -v elfdump >/dev/null 2>&1; then
    needed_of() { elfdump -d "$1" | awk '$2 == "NEEDED" { print $NF }'; }
elif command -v readelf >/dev/null 2>&1; then
    needed_of() {
        readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
    }
else
    echo "check-needed: need elfdump or readelf" >&2
    exit 1
fi

# "3.0.9" -> 3000009, so shell string compare cannot get it wrong.
vnum() {
    echo "$1" | awk -F. '{ printf "%d\n", $1 * 1000000 + $2 * 1000 + $3 }'
}

rc=0
for bin in "$@"; do
    if [ ! -f "$bin" ]; then
        echo "check-needed: $bin: no such file" >&2
        rc=1
        continue
    fi

    echo "== $bin"
    # ALLOWED opens and closes with a newline, so every entry in it is
    # newline-delimited on both sides and no name can match a substring
    # of another.
    for lib in $(needed_of "$bin"); do
        mark=ok
        case "$ALLOWED" in
        *"
$lib
"*) ;;
        *)
            mark=NOT_IN_PLATFORM_IMAGE
            rc=1
            ;;
        esac
        printf '   %-26s %s\n' "$lib" "$mark"
    done

    command -v pvs >/dev/null 2>&1 || continue
    pvs -r "$bin" | tr -d '\t;' | while read -r line; do
        printf '   version dep: %s\n' "$line"
    done
    # pvs runs in a subshell above, so redo the scan for the verdict.
    # One line can carry several versions, so split on commas first.
    max=$(pvs -r "$bin" | tr ',' '\n' \
        | sed -n 's/.*OPENSSL_SMARTOS_\([0-9.]*\).*/\1/p' \
        | while read -r v; do vnum "$v"; done \
        | sort -n | tail -1)
    [ -n "${max:-}" ] || continue
    if [ "$max" -gt "$(vnum "$MAX_OPENSSL_SMARTOS")" ]; then
        echo "   FAIL: needs a platform OpenSSL newer than \
$MAX_OPENSSL_SMARTOS" >&2
        echo "   Build on an older platform image, or raise \
MAX_OPENSSL_SMARTOS knowing older nodes lose support." >&2
        rc=1
    fi
done

if [ "$rc" -ne 0 ]; then
    echo "check-needed: FAILED, this binary will not start on a stock \
platform image" >&2
else
    echo "check-needed: every dependency is in the platform image"
fi
exit "$rc"
