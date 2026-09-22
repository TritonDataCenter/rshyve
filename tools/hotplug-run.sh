#!/usr/bin/env bash
#
# Run the hot-add test in the global zone of an illumos node.
#
# Boots a microVM with the ACPI hotplug interface published, waits for
# the guest to say it has recorded the hardware it already has, adds a
# disk, a CPU and memory over the vsock CONTROL channel, and reads the
# guest's verdict off the serial log. Staged by tools/virtiofs-stage.sh,
# which puts this script next to everything it needs.
#
#   hotplug-run.sh          disk, cpu and memory
#   hotplug-run.sh disk     one resource only
#   hotplug-run.sh cpu
#   hotplug-run.sh mem
#
# This is the only test that shows a guest CONSUMING a hot-added
# resource: it reads the disk, runs a thread on the new CPU, and counts
# the new memory in MemTotal. The guest snapshots /sys before it says it
# is ready, so nothing that was already there can satisfy a check.
#
# The rendezvous, and why it cannot deadlock:
#
#   guest  snapshot /sys, print "NOTE HOTPLUG-READY", then poll
#   host   wait for that line, then send the CONTROL hot-add commands
#   guest  poll until each resource arrives or its deadline passes,
#          print one line per expectation, power off
#
# A host that adds nothing leaves the guest to time out and report the
# failure, so the run always ends.
#
# Knobs. VMM_HOTPLUG_DISK names a device on the operator's own node and
# has no default. Every other knob has one:
#
#   VMM_HOTPLUG_DISK       block device or file to hot-add    (required)
#   VMM_HOTPLUG_FIREHYVE   VMM to run                         ($DIR/firehyve)
#   VMM_HOTPLUG_KERNEL     hotplug guest kernel               ($DIR/vmlinux-hotplug)
#   VMM_HOTPLUG_INITRD     initramfs holding the guest init   ($DIR/initramfs-virtiofs.cpio)
#   VMM_HOTPLUG_VM         VM name                            (hptest)
#   VMM_HOTPLUG_COLD_DISK  disk attached at boot, so the new one differs
#   VMM_HOTPLUG_TIMEOUT    seconds to wait for the report     (180)
#
# Exits 0 only when the guest reported result=OK and every check this
# run asked for is in the report as a PASS.

# errexit is deliberately off: every status this script cares about it
# checks itself, and a bare `grep` or `kill` that answers non-zero is
# normal here. pipefail is on so a failing left-hand side of a pipe is
# not hidden by a succeeding right-hand side.
set -u
set -o pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"

VM="${VMM_HOTPLUG_VM:-hptest}"
FIREHYVE="${VMM_HOTPLUG_FIREHYVE:-$DIR/firehyve}"
KERNEL="${VMM_HOTPLUG_KERNEL:-$DIR/vmlinux-hotplug}"
INITRD="${VMM_HOTPLUG_INITRD:-$DIR/initramfs-virtiofs.cpio}"
DISK="${VMM_HOTPLUG_DISK:-}"
COLD_DISK="${VMM_HOTPLUG_COLD_DISK:-$DIR/hotplug-cold.img}"

# The boot shape. maxcpus above cpus is what gives the VM its CPU slots,
# and the memory window is what gives it a DIMM to fill.
BOOT_CPUS="${VMM_HOTPLUG_CPUS:-2}"
MAX_CPUS="${VMM_HOTPLUG_MAXCPUS:-8}"
BOOT_MEM="${VMM_HOTPLUG_MEM:-512M}"
MAX_MEM="${VMM_HOTPLUG_MAXMEM:-1G}"

# What to add. The CPU id is the first hotplug slot, above the boot
# CPUs. The memory is one whole 128 MiB slot, which is what the engine
# rounds any smaller request up to.
ADD_CPU="${VMM_HOTPLUG_ADD_CPU:-$BOOT_CPUS}"
ADD_MEM_BYTES="${VMM_HOTPLUG_ADD_MEM:-134217728}"

COLD_SLOT=4
DISK_SLOT=5
VSOCK_SLOT=14
VSOCK_CID="${VMM_HOTPLUG_VSOCK_CID:-3}"
VSOCK_SOCK="$DIR/hotplug-vsock.sock"

# Bytes of the disk the guest reads back and sums. Must match
# DISK_PROBE_LEN in tools/virtiofs-test/src/hotplug_checks.rs.
PROBE_LEN=4096

# Seconds. The guest has its own deadline. The two host waits bound the
# boot and the report. illumos has no timeout(1), so every wait here is
# a polling loop over the log.
READY_TIMEOUT_S="${VMM_HOTPLUG_READY_TIMEOUT:-90}"
TIMEOUT_S="${VMM_HOTPLUG_TIMEOUT:-180}"
GUEST_TIMEOUT_S="${VMM_HOTPLUG_GUEST_TIMEOUT:-60}"
# How long a CONTROL session stays open for its answers.
CONTROL_WAIT_S=3

usage() {
    echo "usage: $0 [all|disk|cpu|mem]"
    exit 2
}

case "${1:-all}" in
    all) EXPECT=disk,cpu,mem ;;
    disk) EXPECT=disk ;;
    cpu) EXPECT=cpu ;;
    mem) EXPECT=mem ;;
    *) usage ;;
esac

# The checks the guest must report a PASS for, per resource. A run whose
# report is missing one of these did not test what it was asked to.
checks_for() {
    case "$1" in
        disk) echo "hotplug_disk_appeared hotplug_disk_read" ;;
        cpu) echo "hotplug_cpu_appeared hotplug_cpu_online hotplug_cpu_runs" ;;
        mem) echo "hotplug_mem_block hotplug_mem_total" ;;
    esac
}

wants() {
    case ",$EXPECT," in
        *",$1,"*) return 0 ;;
        *) return 1 ;;
    esac
}

for f in "$FIREHYVE" "$KERNEL" "$INITRD"; do
    if [ ! -f "$f" ]; then
        echo "RUN-FAIL: missing $f"
        exit 1
    fi
done

# The disk is the operator's, so it is never invented here: a run that
# quietly dropped the disk check would report a pass for work it did not
# do.
if wants disk; then
    if [ -z "$DISK" ]; then
        echo "RUN-FAIL: set VMM_HOTPLUG_DISK to a block device or file to hot-add"
        echo "  it is read, never written, and it must not be in use by another VM"
        exit 1
    fi
    if [ ! -e "$DISK" ]; then
        echo "RUN-FAIL: no disk at $DISK"
        exit 1
    fi
fi

# Bytes of $DISK, when the host can say. A zvol keeps its size in the
# dataset, and no size is better than a wrong one: the guest only checks
# what it is told.
disk_size() {
    case "$1" in
        /dev/zvol/rdsk/*) zfs get -Hpo value volsize "${1#/dev/zvol/rdsk/}" 2>/dev/null ;;
        /dev/zvol/dsk/*) zfs get -Hpo value volsize "${1#/dev/zvol/dsk/}" 2>/dev/null ;;
        *) [ -f "$1" ] && wc -c < "$1" | tr -d ' ' ;;
    esac
}

# Sum of the first $PROBE_LEN bytes, which the guest recomputes off its
# own /dev node. Reading is the whole of the host's part: the disk is
# never written to.
disk_sum() {
    dd if="$1" bs="$PROBE_LEN" count=1 2>/dev/null |
        od -An -v -tu1 |
        awk '{ for (i = 1; i <= NF; i++) s += $i } END { print s + 0 }'
}

cleanup_vm() {
    if [ -n "${VM_PID:-}" ]; then
        kill -KILL "$VM_PID" 2>/dev/null
        wait "$VM_PID" 2>/dev/null
        VM_PID=
    fi
    # Only this VM by name. A pkill on the VMM's path would reach into
    # every zone on the node, and this one is shared.
    bhyvectl --destroy --vm="$VM" >/dev/null 2>&1
    rm -f "$VSOCK_SOCK"
}

# Wait for $1 to appear in $2, giving up after $3 seconds or as soon as
# the VM is gone. illumos has no timeout(1), so this polls.
wait_for_line() {
    pattern=$1
    log=$2
    seconds=$3
    waited=0
    limit=$((seconds * 5))
    while [ "$waited" -lt "$limit" ]; do
        if grep -q "$pattern" "$log" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$VM_PID" 2>/dev/null; then
            # The guest prints its report and powers off in the same
            # breath, so read the log once more before calling the VM
            # gone. Without this a passing run fails whenever the exit
            # lands between the grep above and this test.
            grep -q "$pattern" "$log" 2>/dev/null
            return
        fi
        sleep 0.2
        waited=$((waited + 1))
    done
    return 1
}

# One CONTROL session, one request line per argument. The sleep holds
# the connection open while the answers come back. socat would close it
# as soon as its stdin ended.
control() {
    {
        printf 'CONTROL\n'
        for request in "$@"; do
            printf '%s\n' "$request"
        done
        sleep "$CONTROL_WAIT_S"
    } | socat - "UNIX-CONNECT:$VSOCK_SOCK" 2>&1
}

# Send one hot-add and record what the VMM answered. A refusal is fatal
# to the run: the guest would otherwise time out and report a failure
# that reads as a guest bug.
hot_add() {
    label=$1
    request=$2
    answer="$(control "$request" | tr -d '\r' | grep -v '^OK control$' | tr '\n' ' ')"
    answer="${answer:-<no answer from the control socket>}"
    printf '%s: %s -> %s\n' "$label" "$request" "$answer" | tee -a "$CONTROL_LOG"
    case "$answer" in
        *ERR*) return 1 ;;
        *OK*) return 0 ;;
        *) return 1 ;;
    esac
}

run_hotplug() {
    log=$DIR/hotplug-run.log
    CONTROL_LOG=$DIR/hotplug-control.log
    : > "$CONTROL_LOG"
    VM_PID=

    cleanup_vm

    # A disk at boot, so the hot-added one has to be a device the guest
    # did not already have. Without it the /sys/block diff would start
    # from nothing and prove less.
    cold=
    if [ ! -e "$COLD_DISK" ]; then
        dd if=/dev/zero of="$COLD_DISK" bs=1048576 count=8 >/dev/null 2>&1
    fi
    if [ -e "$COLD_DISK" ]; then
        cold="-s $COLD_SLOT,virtio-blk,$COLD_DISK"
    elif wants disk; then
        # Without a boot disk the guest starts with none, and "a block
        # device appeared" is a weaker claim than this test says it
        # makes. Fail rather than quietly test less.
        echo "RUN-FAIL: cannot make a boot disk at $COLD_DISK"
        return 1
    else
        echo "NOTE: no boot disk at $COLD_DISK; the guest starts with none"
    fi

    guest_disk=
    if wants disk; then
        bytes="$(disk_size "$DISK")"
        sum="$(disk_sum "$DISK")"
        if [ -z "${sum:-}" ]; then
            echo "RUN-FAIL: cannot read $PROBE_LEN bytes from $DISK"
            return 1
        fi
        if [ "$sum" = 0 ]; then
            echo "NOTE: the first $PROBE_LEN bytes of $DISK are all zeros;"
            echo "  the content check cannot tell it from any other zeroed disk"
        fi
        guest_disk="hotplug.disk_sum=$sum"
        if [ -n "${bytes:-}" ]; then
            guest_disk="$guest_disk hotplug.disk_bytes=$bytes"
        else
            echo "NOTE: no size for $DISK; the guest checks its content only"
        fi
        echo "host view of $DISK: bytes=${bytes:-unknown} head_sum=$sum"
    fi

    guest_mem=
    if wants mem; then
        guest_mem="hotplug.mem_bytes=$ADD_MEM_BYTES"
    fi

    echo "=== booting $VM: expect=$EXPECT cpus=$BOOT_CPUS/$MAX_CPUS mem=$BOOT_MEM+$MAX_MEM ==="
    # -S is required by the memory window: without reservoir memory a
    # hot-add zeroes every page inside the ioctl that holds the vCPUs.
    # -l com1,stdio and a redirect, because this VMM serves com1 on
    # stdio only and rejects com1,null.
    # No noapictimer here, unlike the single vCPU virtio-fs run: an SMP
    # guest schedules on the local APIC timer.
    # shellcheck disable=SC2086
    "$FIREHYVE" \
        -c "cpus=$BOOT_CPUS,maxcpus=$MAX_CPUS" -m "$BOOT_MEM" \
        --hotplug -S \
        -o "hotplug.maxmem=$MAX_MEM" \
        --kernel "$KERNEL" \
        --initrd "$INITRD" \
        --cmdline "root=/dev/ram0 init=/init rdinit=/init panic=30 tsc=reliable no_timer_check console=ttyS0 virtiofs.role=hotplug hotplug.expect=$EXPECT hotplug.timeout=$GUEST_TIMEOUT_S $guest_disk $guest_mem" \
        $cold \
        -s "$VSOCK_SLOT,virtio-vsock,$VSOCK_SOCK,cid=$VSOCK_CID" \
        -l com1,stdio \
        "$VM" > "$log" 2>&1 &
    VM_PID=$!

    if ! wait_for_line "HOTPLUG-READY" "$log" "$READY_TIMEOUT_S"; then
        echo "=== FAILED: no HOTPLUG-READY within ${READY_TIMEOUT_S}s ==="
        echo "--- last 30 lines of $log ---"
        tail -30 "$log"
        cleanup_vm
        return 1
    fi
    echo "guest is ready: $(grep 'HOTPLUG-READY' "$log" | head -1)"

    # Only now: everything the guest sees from here on is new to it.
    rc=0
    if wants disk; then
        hot_add disk "device-add $DISK_SLOT,virtio-blk,$DISK" || rc=1
    fi
    if wants cpu; then
        hot_add cpu "cpu-add $ADD_CPU" || rc=1
    fi
    if wants mem; then
        hot_add mem "mem-add $ADD_MEM_BYTES" || rc=1
    fi
    if [ $rc -ne 0 ]; then
        echo "=== FAILED: the VMM refused a hot-add ==="
    fi

    if ! wait_for_line "VFSTEST: DONE" "$log" "$TIMEOUT_S"; then
        echo "=== FAILED: no report within ${TIMEOUT_S}s ==="
        echo "--- last 30 lines of $log ---"
        tail -30 "$log"
        cleanup_vm
        return 1
    fi
    cleanup_vm

    echo "--- guest report ---"
    grep "VFSTEST:" "$log" 2>/dev/null
    echo "--- what the VMM answered ---"
    cat "$CONTROL_LOG" 2>/dev/null

    if ! grep -q "VFSTEST: DONE .*result=OK" "$log" 2>/dev/null; then
        echo "=== FAILED: the guest reported a failure ==="
        return 1
    fi
    # result=OK counts the checks that ran. This counts the checks that
    # had to run, so a report that quietly skipped one cannot pass.
    for resource in disk cpu mem; do
        wants "$resource" || continue
        for name in $(checks_for "$resource"); do
            if ! grep -q "VFSTEST: PASS $name " "$log" 2>/dev/null; then
                echo "=== FAILED: $name is not in the report ==="
                rc=1
            fi
        done
    done
    return $rc
}

trap 'cleanup_vm' EXIT INT TERM

if run_hotplug; then
    echo "HOTPLUG-TEST: PASS"
    exit 0
fi
echo "HOTPLUG-TEST: FAIL"
exit 1
