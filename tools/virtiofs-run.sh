#!/usr/bin/env bash
#
# Run the virtio-fs test in the global zone of an illumos node.
#
# Boots a microVM whose init mounts the staged share, checks it, and
# powers off. Staged by tools/virtiofs-stage.sh, which puts this script
# next to everything it needs.
#
#   virtiofs-run.sh            both share modes
#   virtiofs-run.sh rw         read-write share only
#   virtiofs-run.sh ro         read-only share only
#   virtiofs-run.sh container  boot an image and run the harness checks
#   virtiofs-run.sh entrypoint boot an image and run its own entrypoint
#   virtiofs-run.sh all        every mode
#
# Exits 0 only when every mode reported result=OK.

# errexit is deliberately off: every status this script cares about it
# checks itself, and a bare `grep` or `kill` that answers non-zero is
# normal here. pipefail is on so a failing left-hand side of a pipe is
# not hidden by a succeeding right-hand side.
set -u
set -o pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
VM="${VMM_FIREHYVE_VM:-vfstest}"
SLOT="${VMM_FIREHYVE_SLOT:-12}"
# virtio-rnd rides along in the check modes: it is independent of the
# share, and sharing a boot is cheaper than a VM of its own.
RNG_SLOT="${VMM_FIREHYVE_RNG_SLOT:-13}"
VSOCK_SLOT="${VMM_FIREHYVE_VSOCK_SLOT:-14}"
VSOCK_CID="${VMM_FIREHYVE_VSOCK_CID:-3}"
VSOCK_SOCK="$DIR/vsock.sock"
# The guest dials this port. The device turns that into a connection to
# <socket>_<port>, which is where the echo server listens.
VSOCK_ECHO_PORT=5555
# The guest listens here and the host dials in, which is the direction a
# CRI shim uses to open a session.
VSOCK_LISTEN_PORT=1234
TAG=testfs
TIMEOUT_S="${VMM_FIREHYVE_TIMEOUT:-90}"
EXTRA_CMDLINE="${VMM_FIREHYVE_CMDLINE:-}"
# Only meaningful against a CONFIG_SMP=y guest. A uniprocessor kernel
# ignores the extra vCPUs.
VCPUS="${VMM_FIREHYVE_VCPUS:-1}"

FIREHYVE="$DIR/firehyve"
KERNEL="${VMM_FIREHYVE_KERNEL:-$DIR/vmlinux-guest}"
INITRD="$DIR/initramfs-virtiofs.cpio"
EXPORT="$DIR/export"
# The share root holds rootfs/ and container.json. The guest mounts the
# whole share and reads the config beside the tree.
SHARE="$DIR/container"
ROOTFS="$SHARE/rootfs"

for f in "$FIREHYVE" "$KERNEL" "$INITRD"; do
    if [ ! -f "$f" ]; then
        echo "RUN-FAIL: missing $f"
        exit 1
    fi
done
if [ ! -d "$EXPORT" ]; then
    echo "RUN-FAIL: no fixture at $EXPORT"
    exit 1
fi

# Only the VM this script started. A `pkill` on the VMM path reaches
# every zone on the node, and these nodes are shared with live guests.
VM_PID=
cleanup_vm() {
    if [ -n "$VM_PID" ]; then
        kill -KILL "$VM_PID" 2>/dev/null
        wait "$VM_PID" 2>/dev/null
        VM_PID=
    fi
    bhyvectl --destroy --vm="$VM" >/dev/null 2>&1
}

# socat echoes whatever the guest sends back to it, so one round trip
# exercises both directions of the device.
start_vsock_echo() {
    stop_vsock_echo
    socat "UNIX-LISTEN:${VSOCK_SOCK}_${VSOCK_ECHO_PORT},fork" EXEC:/bin/cat \
        > "$DIR/vsock-echo.log" 2>&1 &
    VSOCK_ECHO_PID=$!
}

# Dial into the guest the way Firecracker's protocol specifies, and
# leave the transcript for the run to check. Retried: the guest has to
# reach its listen() first, and the socket does not exist until the VMM
# has bound it.
dial_guest() {
    out=$DIR/vsock-h2g.out
    rm -f "$out"
    (
        probe=host-to-guest-probe
        for _ in $(seq 1 100); do
            if [ -S "$VSOCK_SOCK" ]; then
                { printf "CONNECT %s\n%s\n" "$VSOCK_LISTEN_PORT" "$probe"
                  sleep 2
                } | socat - "UNIX-CONNECT:$VSOCK_SOCK" > "$out" 2>&1
                if grep -q "$probe" "$out" 2>/dev/null; then
                    break
                fi
            fi
            sleep 0.2
        done
    ) &
    VSOCK_DIAL_PID=$!
}

stop_vsock_dial() {
    if [ -n "${VSOCK_DIAL_PID:-}" ]; then
        kill -KILL "$VSOCK_DIAL_PID" 2>/dev/null
        wait "$VSOCK_DIAL_PID" 2>/dev/null
        VSOCK_DIAL_PID=
    fi
}

stop_vsock_echo() {
    if [ -n "${VSOCK_ECHO_PID:-}" ]; then
        kill -KILL "$VSOCK_ECHO_PID" 2>/dev/null
        wait "$VSOCK_ECHO_PID" 2>/dev/null
        VSOCK_ECHO_PID=
    fi
    rm -f "${VSOCK_SOCK}_${VSOCK_ECHO_PORT}" "$VSOCK_SOCK"
}

run_mode() {
    mode=$1
    log=$DIR/run-$mode.log
    fsopts="$EXPORT,tag=$TAG"
    if [ "$mode" = ro ]; then
        fsopts="$fsopts,ro"
    fi

    cleanup_vm
    start_vsock_echo
    dial_guest
    sleep 1

    echo "=== mode=$mode ==="
    # console=ttyS0 is explicit: the report lines have to reach com1, and
    # this kernel is built without a default console for boot speed.
    # panic=30 rather than the -1 the zone uses: if init dies, a stall is
    # easier to read than a reboot loop filling the log for the timeout.
    # No -S: it maps to VCF_RESERVOIR_MEM, and a node whose VMM
    # reservoir has no free space then fails the create with ENOMEM.
    # Wired memory is a PCI passthrough requirement, not a virtio-fs one.
    "$FIREHYVE" \
        -c "$VCPUS" -m 1024M \
        --kernel "$KERNEL" \
        --initrd "$INITRD" \
        --cmdline "root=/dev/ram0 init=/init rdinit=/init panic=30 tsc=reliable no_timer_check noapictimer console=ttyS0 $EXTRA_CMDLINE virtiofs.tag=$TAG virtiofs.mode=$mode" \
        -s "$SLOT,virtio-fs,$fsopts" \
        -s "$RNG_SLOT,virtio-rnd" \
        -s "$VSOCK_SLOT,virtio-vsock,$VSOCK_SOCK,cid=$VSOCK_CID" \
        -l com1,stdio \
        "$VM" > "$log" 2>&1 &
    pid=$!
    VM_PID=$pid

    waited=0
    limit=$((TIMEOUT_S * 5))
    while [ $waited -lt $limit ]; do
        grep -q "VFSTEST: DONE" "$log" 2>/dev/null && break
        kill -0 $pid 2>/dev/null || break
        sleep 0.2
        waited=$((waited + 1))
    done

    cleanup_vm
    stop_vsock_dial
    stop_vsock_echo

    grep "VFSTEST:" "$log" 2>/dev/null
    # The guest reports its side. This is the host's own view of the
    # same exchange, so a one-sided pass cannot hide a broken direction.
    if [ -s "$DIR/vsock-h2g.out" ]; then
        echo "host dial transcript: $(tr '\n' ' ' < "$DIR/vsock-h2g.out")"
    fi
    if grep -q "VFSTEST: DONE .*result=OK" "$log" 2>/dev/null; then
        echo "=== mode=$mode: OK ==="
        return 0
    fi
    if ! grep -q "VFSTEST: DONE" "$log" 2>/dev/null; then
        echo "=== mode=$mode: NO RESULT (timeout or boot failure) ==="
        echo "--- last 30 lines of $log ---"
        tail -30 "$log"
        return 1
    fi
    echo "=== mode=$mode: FAILED ==="
    return 1
}

# Boot a container image as the guest root filesystem. The share is
# exported read-only and the guest supplies its own writable layer. Two
# probes below check that the guest's writes stayed in that layer. They
# are two named files, not a comparison of the export against its
# staged state, so they show those two writes did not get through, not
# that nothing did.
# $1 is "checks" to run the harness script inside the image, or
# "entrypoint" to run whatever the image itself declares.
run_container() {
    what=$1
    log=$DIR/run-$what.log
    entry=

    if [ ! -d "$ROOTFS" ]; then
        echo "RUN-FAIL: no image at $ROOTFS"
        echo "  stage one with tools/virtiofs-container-stage.sh"
        return 1
    fi
    if [ "$what" = checks ]; then
        entry=" virtiofs.entry=/container-test.sh"
    elif [ ! -f "$SHARE/container.json" ]; then
        echo "RUN-FAIL: no $SHARE/container.json"
        echo "  restage with tools/virtiofs-container-stage.sh"
        return 1
    fi

    cleanup_vm
    sleep 1

    echo "=== mode=container:$what ==="
    "$FIREHYVE" \
        -c "$VCPUS" -m 1024M \
        --kernel "$KERNEL" \
        --initrd "$INITRD" \
        --cmdline "root=/dev/ram0 init=/init rdinit=/init panic=30 tsc=reliable no_timer_check noapictimer console=ttyS0 $EXTRA_CMDLINE virtiofs.tag=$TAG virtiofs.role=container$entry" \
        -s "$SLOT,virtio-fs,$SHARE,tag=$TAG,ro" \
        -l com1,stdio \
        "$VM" > "$log" 2>&1 &
    pid=$!
    VM_PID=$pid

    waited=0
    limit=$((TIMEOUT_S * 5))
    while [ $waited -lt $limit ]; do
        grep -q "VFSTEST: DONE" "$log" 2>/dev/null && break
        kill -0 $pid 2>/dev/null || break
        sleep 0.2
        waited=$((waited + 1))
    done

    cleanup_vm

    grep -E "VFSTEST:|DOCKERTEST:" "$log" 2>/dev/null

    # VFSTEST result=OK means the init got all the way through and the
    # command it ran exited 0, whichever command that was.
    if ! grep -q "VFSTEST: DONE .*result=OK" "$log" 2>/dev/null; then
        echo "=== mode=container:$what: FAILED ==="
        echo "--- last 30 lines of $log ---"
        tail -30 "$log"
        return 1
    fi
    if [ "$what" = entrypoint ]; then
        echo "=== mode=container:$what: OK ==="
        echo "--- what the image printed ---"
        grep -v -E "^\[|^Sep|VFSTEST:" "$log" | grep -v '^$' | head -20
        return 0
    fi
    if ! grep -q "DOCKERTEST: DONE .*result=OK" "$log" 2>/dev/null; then
        echo "=== mode=container:$what: FAILED (in-image checks) ==="
        return 1
    fi

    # The guest wrote through the overlay. If any of it reached the host
    # the read-only export leaked, which matters more than the checks
    # the guest ran on itself.
    if [ -e "$ROOTFS/tmp/overlay-write-probe.txt" ]; then
        echo "=== mode=container:$what: FAILED (guest write reached the host) ==="
        return 1
    fi
    # The guest also appends to /etc/hostname to force a copy-up. That
    # write belongs in the tmpfs upper layer, never in the export.
    if [ -f "$ROOTFS/etc/hostname" ] &&
        grep -q modified "$ROOTFS/etc/hostname"; then
        echo "=== mode=container:$what: FAILED (copy-up wrote through) ==="
        return 1
    fi
    echo "=== mode=container:$what: OK (both write probes stayed off the host) ==="
    return 0
}

rc=0
case "${1:-both}" in
    rw) run_mode rw || rc=1 ;;
    ro) run_mode ro || rc=1 ;;
    container) run_container checks || rc=1 ;;
    entrypoint) run_container entrypoint || rc=1 ;;
    both)
        run_mode rw || rc=1
        run_mode ro || rc=1
        ;;
    all)
        run_mode rw || rc=1
        run_mode ro || rc=1
        run_container checks || rc=1
        # `entrypoint` is deliberately not here: whether the image's own
        # command terminates is a property of the image, and a server
        # image running until the timeout is correct, not a failure.
        ;;
    *)
        echo "usage: $0 [rw|ro|both|container|entrypoint|all]"
        exit 2
        ;;
esac

if [ $rc -eq 0 ]; then
    echo "VIRTIOFS-TEST: PASS"
else
    echo "VIRTIOFS-TEST: FAIL"
fi
exit $rc
