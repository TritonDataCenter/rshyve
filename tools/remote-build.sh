#!/usr/bin/env bash
#
# Build and test rust-bhyve on the illumos build host.
#
# The rshyve binary builds only on illumos: vmm-tpm-sys drives an
# autotools build of vendored libtpms, bhyve-api binds SunOS ioctls, and
# viona-api needs libdladm. The library crates other than vmm-tpm-sys
# and vmm-tpm build and test on macOS, which covers most unit work. The
# full workspace build and every platform test run on the build host,
# and this script makes each of them one command.
#
#   tools/remote-build.sh build              # cargo build --release
#   tools/remote-build.sh test               # cargo test --workspace
#   tools/remote-build.sh test -p vmm_tpm    # extra args pass through
#   tools/remote-build.sh clippy
#   tools/remote-build.sh check
#   tools/remote-build.sh needed             # stock-PI dependency gate
#   tools/remote-build.sh sync               # push sources, build nothing
#   tools/remote-build.sh run -- -H -c 2 ... # run the built rshyve
#   tools/remote-build.sh sh 'arbitrary command'
#
# Override the target with VMM_BUILD_HOST / VMM_BUILD_KEY / VMM_BUILD_DIR.

set -euo pipefail

# No defaults: the build host is site-specific and must not be baked in.
HOST="${VMM_BUILD_HOST:?set VMM_BUILD_HOST to user@host of your illumos build host}"
KEY="${VMM_BUILD_KEY:?set VMM_BUILD_KEY to the SSH key for that host}"
REMOTE_DIR="${VMM_BUILD_DIR:-/opt/rust-bhyve}"

# The value is spliced into a command that a remote root shell parses, so
# it is constrained to an absolute path made of path characters. A space,
# a quote or a `$(...)` would otherwise run as root on the node.
case "$REMOTE_DIR" in
    /*) ;;
    *) echo "BUILD-FAIL: VMM_BUILD_DIR must be an absolute path" >&2; exit 1 ;;
esac
if printf '%s' "$REMOTE_DIR" | grep -qv '^[A-Za-z0-9_./-]*$'; then
    echo "BUILD-FAIL: VMM_BUILD_DIR may hold only letters, digits, _ . / -" >&2
    exit 1
fi
PROFILE="${VMM_BUILD_PROFILE:---release}"

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Host key verification stays on. Add the build host to known_hosts once,
# after checking its fingerprint out of band.
SSH=(ssh -o ConnectTimeout=15 -i "$KEY" "$HOST")

# /opt/tools/bin and /opt/local/bin hold the pkgsrc toolchain (cargo, gcc,
# autoconf, gmake, pkg-config). Neither is on root's default PATH.
REMOTE_ENV="export PATH=/opt/tools/bin:/opt/local/bin:/usr/bin:/usr/sbin:\$PATH; cd $REMOTE_DIR"

usage() { sed -n '3,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }

sync_sources() {
    # --checksum, because rsync-over-ssh timestamps do not reliably
    # invalidate cargo's fingerprints. Without it, an edited file can fail
    # to rebuild with no error.
    rsync -az --checksum --delete \
        --exclude target --exclude .git --exclude 'kernel/linux-*' \
        -e "ssh -i $KEY" \
        "$SRC_DIR/" "$HOST:$REMOTE_DIR/"
}

remote() { "${SSH[@]}" "$REMOTE_ENV; $*"; }

cmd="${1:-build}"
shift || true

case "$cmd" in
    sync)
        sync_sources
        echo "synced $SRC_DIR -> $HOST:$REMOTE_DIR"
        ;;
    build)
        sync_sources
        remote "cargo build $PROFILE $*" 2>&1
        # A binary that needs a library the platform image does not
        # ship links fine here and then fails to exec on a node, so
        # gate every build rather than only the packaging step.
        remote "set -- ; for b in target/release/rshyve target/release/firehyve; do \
                    if [ -f \$b ]; then set -- \"\$@\" \$b; fi; \
                done; \
                if [ \$# -gt 0 ]; then tools/check-needed.sh \"\$@\"; \
                else echo 'no release binary to check'; fi" 2>&1
        ;;
    check|clippy)
        sync_sources
        remote "cargo $cmd $PROFILE $*" 2>&1
        ;;
    needed)
        sync_sources
        remote "tools/check-needed.sh ${*:-target/release/rshyve target/release/firehyve}" 2>&1
        ;;
    test)
        sync_sources
        # Tests run in the dev profile: release strips the debug assertions
        # several device models rely on to catch ring-index mistakes.
        remote "cargo test --workspace $*" 2>&1
        ;;
    fmt)
        sync_sources
        remote "cargo fmt --all $*" 2>&1
        ;;
    run)
        sync_sources
        remote "cargo build $PROFILE" 2>&1
        remote "./target/release/rshyve $*" 2>&1
        ;;
    sh)
        remote "$*" 2>&1
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        echo "unknown command: $cmd" >&2
        usage
        ;;
esac
