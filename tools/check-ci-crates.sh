#!/bin/sh
# Assert that CI still lints every workspace member, and still tests and
# MSRV-checks every portable crate and the illumos bindings.
#
# A crate that no `-p` list names does not fail the build. It becomes
# unlinted, untested code that CI reports green, so coverage is asserted
# here, not trusted.
#
# The crate list is derived from the workspace members, not kept by
# hand. A new crate therefore fails this check until either CI names it
# or it is declared below, and a declaration that names a crate the
# workspace no longer has fails too, so neither list can rot in silence.
#
# Clippy is checked differently from the rest: EVERY member must appear
# in SOME clippy invocation, because `clippy::correctness` is denied at
# the workspace root and those lints fire under clippy alone. Which job
# lints a crate is a platform question (bindings against the illumos
# target, rshyve on the host), so the union is what matters, not the job.
#
# The other three jobs are checked per job. The illumos cross-check is
# the only one that compiles a `#[cfg(test)]` body behind
# `cfg(target_os = "illumos")`, so a crate missing from ITS list is
# unchecked against the target with every other job green.
set -eu

CI=".github/workflows/ci.yml"
ROOT="Cargo.toml"

# Crates the host MSRV job cannot usefully check. The bhyve, bhyve-sys
# and viona bindings are ioctl and struct-layout code, and the compile
# that means anything for them is the one against the illumos target,
# which the clippy-illumos and cross-check jobs do.
#
# Their TESTS are a separate question, and the answer is the opposite
# one. Each test drives a recorded ioctl seam over /dev/null and a pipe,
# names no illumos symbol, and runs anywhere. Those recordings hold the
# only pins on the command number and on the arguments the kernel is
# given, and a wrong argument answers with the same errno as a right
# one, so nothing else catches it. The test job must name these crates
# too, so the pins do not depend on the build host alone.
TARGET_ONLY="bhyve_api_sys bhyve_api viona_api"

# Crates the portable test and MSRV jobs cannot build on their own.
# vmm-tpm-sys drives an autotools build of the vendored libtpms, and only
# the `binaries` job installs what that build needs, so that job names
# both of them for clippy and for tests.
NO_JOB="vmm_tpm vmm_tpm_sys"

[ -f "$CI" ] || { echo "run from the repository root: no $CI" >&2; exit 1; }

members=$(awk '
    /^members = \[/ { grab = 1; next }
    grab && /^\]/ { exit }
    grab {
        sub(/#.*/, "")
        gsub(/[",]/, "")
        gsub(/^[ \t]+|[ \t]+$/, "")
        if ($0 != "") print
    }
' "$ROOT")

[ -n "$members" ] || { echo "no [workspace] members found in $ROOT" >&2; exit 1; }

status=0

crates=""
known=""
all_members=""
for dir in $members; do
    manifest="$dir/Cargo.toml"
    if [ ! -f "$manifest" ]; then
        echo "$ROOT names $dir, which has no Cargo.toml" >&2
        exit 1
    fi
    name=$(sed -n 's/^name = "\(.*\)"/\1/p' "$manifest" | head -1)
    if [ -z "$name" ]; then
        echo "$manifest has no package name" >&2
        exit 1
    fi
    all_members="$all_members $name"
    # The binaries are not portable, so only the clippy union covers them.
    case "$dir" in
        bin/*) continue ;;
    esac
    known="$known $name"
    case " $TARGET_ONLY $NO_JOB " in
        *" $name "*) ;;
        *) crates="$crates $name" ;;
    esac
done

# An exemption that outlives its crate covers nothing and hides the next
# crate that inherits the name.
for name in $TARGET_ONLY $NO_JOB; do
    case " $known " in
        *" $name "*) ;;
        *)
            echo "no workspace crate is named $name, so this script exempts a crate that is gone" >&2
            status=1
            ;;
    esac
done

# Every `-p` named by any clippy invocation in the workflow, from the
# marker line and the continuation lines that carry the arguments.
clippy_args=" $(awk '
    index($0, "cargo clippy") { seen = 1; grab = 1; print; next }
    grab && $0 ~ /^[ \t]*(-p |--target |-- )/ { print; next }
    grab { grab = 0 }
    END { if (!seen) exit 1 }
' "$CI" | tr '\n' ' ') " || {
    echo "$CI: no cargo clippy invocation at all" >&2
    exit 1
}

for name in $all_members; do
    case "$clippy_args" in
        *" -p $name "*) ;;
        *)
            echo "$CI: no clippy job names $name" >&2
            status=1
            ;;
    esac
done

for job in test msrv cross; do
    # $MSRV is literal text in the workflow, not a shell expansion.
    # shellcheck disable=SC2016
    case "$job" in
        test) marker="cargo test --locked" ;;
        msrv) marker='cargo "+$MSRV" check --locked' ;;
        cross) marker="cargo check --locked --all-targets" ;;
    esac
    # The two jobs that can say something about the bindings must name
    # them: the cross-check compiles them for the target, and the test
    # job runs the recordings that pin their ioctl arguments.
    case "$job" in
        test|cross) required="$crates $TARGET_ONLY" ;;
        *) required="$crates" ;;
    esac
    # Crates this one job may leave out, with the reason recorded at
    # the job itself. Everything else must be named.
    case "$job" in
        # vmm_migrate pulls zstd-sys, which needs an illumos cross-cc
        # the runner does not have. The host clippy and test jobs cover
        # it, and its illumos paths are covered on the build host.
        cross) allowed="vmm_migrate" ;;
        *) allowed="" ;;
    esac
    # The command is a folded YAML scalar: the marker line, then the
    # continuation lines that carry the arguments. A --target line is
    # one of those. Stopping at it would read an empty crate list and
    # report every crate missing.
    block=$(awk -v m="$marker" '
        index($0, m) { seen = 1; grab = 1; print; next }
        grab && $0 ~ /^[ \t]*(-p |--target )/ { print; next }
        grab { exit }
        END { if (!seen) exit 1 }
    ' "$CI") || {
        echo "$CI: no $job job line matching: $marker" >&2
        status=1
        continue
    }
    args=" $(echo "$block" | tr '\n' ' ') "
    case "$args" in
        *" -p "*) ;;
        *)
            echo "$CI: the $job job block carries no -p arguments" >&2
            status=1
            continue
            ;;
    esac
    for crate in $required; do
        skip=no
        for ok in $allowed; do
            if [ "$crate" = "$ok" ]; then
                skip=yes
            fi
        done
        [ "$skip" = yes ] && continue
        case "$args" in
            *" -p $crate "*) ;;
            *)
                echo "$CI: the $job job does not name $crate" >&2
                status=1
                ;;
        esac
    done
done

if [ "$status" -eq 0 ]; then
    echo "check-ci-crates: every member linted, portable crates covered, bindings tested"
fi
exit "$status"
