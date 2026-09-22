#!/bin/sh
#
# Runs inside a container image whose rootfs came over virtio-fs.
# Staged into the image by tools/virtiofs-container-stage.sh and run by
# the guest init after switch_root.
#
# POSIX shell only: the image may provide busybox ash or dash, and
# neither awk nor bash can be assumed, so there is no `pipefail` to set.
# `errexit` is deliberately off: a failed check has to be recorded as a
# FAIL and the remaining checks still have to run.

pass=0
fail=0

ok() { pass=$((pass + 1)); echo "DOCKERTEST: PASS $1 $2"; }
bad() { fail=$((fail + 1)); echo "DOCKERTEST: FAIL $1 $2"; }

# Identify the image, so the log says what actually booted.
if [ -r /etc/os-release ]; then
    . /etc/os-release
    echo "DOCKERTEST: NOTE image ${ID:-?} ${VERSION_ID:-?}"
    ok os_release "${PRETTY_NAME:-unknown}"
else
    bad os_release "/etc/os-release is missing"
fi

# / must be the overlay, and the image must still be served by virtio-fs
# underneath it.
# Only / is checked here. The virtio-fs lower layer is not reachable
# from this root after switch_root, so the guest init asserts that one
# before it hands over.
root_fs=
while read -r _dev mnt fstype _rest; do
    [ "$mnt" = "/" ] && [ -z "$root_fs" ] && root_fs=$fstype
done < /proc/mounts

if [ "$root_fs" = overlay ]; then
    ok root_is_overlay "/ is $root_fs"
else
    bad root_is_overlay "/ is ${root_fs:-unknown}, want overlay"
fi

# The shell running this script came off the image, so a dynamic loader
# and its libc have already been paged in over FUSE. Run a second,
# separate binary to show it was not a one-off.
if /bin/ls /etc > /dev/null 2>&1; then
    ok exec_image_binary "/bin/ls ran from the image"
else
    bad exec_image_binary "/bin/ls did not run"
fi

# A pipeline is two more execs plus a pipe between them.
n=$(ls /bin | wc -l)
if [ "${n:-0}" -gt 5 ]; then
    ok pipeline "/bin has $n entries"
else
    bad pipeline "/bin has ${n:-0} entries"
fi

# Reports which account the image config asked for. Not pass/fail: a
# non-root image is a valid configuration, and its writes below are
# expected to behave differently.
echo "DOCKERTEST: NOTE running as uid=$(id -u) gid=$(id -g) pwd=$(pwd)"

if [ -s /etc/passwd ]; then
    ok read_image_file "/etc/passwd is readable"
else
    bad read_image_file "/etc/passwd is empty or missing"
fi

# The writable layer is tmpfs. This file must NOT appear in the host
# export: the run script checks that afterwards. Written under /tmp,
# which every base image makes world-writable, so the check means the
# same thing for an image that runs as a non-root user.
probe=/tmp/overlay-write-probe.txt
if echo overlay-write-probe > "$probe" 2> /dev/null &&
    [ "$(cat "$probe")" = overlay-write-probe ]; then
    ok write_upper "wrote through the overlay"
else
    bad write_upper "could not write to /tmp"
fi

# Overwriting a file that exists only in the read-only lower layer is a
# copy-up, the operation a container does most often. Only root may
# write /etc/hostname, and an image that asks to run as another user is
# correctly refused, so this is not a failure for one.
if [ "$(id -u)" = 0 ]; then
    if echo modified >> /etc/hostname 2> /dev/null; then
        ok copy_up "modified a file from the read-only layer"
    else
        bad copy_up "copy-up failed"
    fi
else
    echo "DOCKERTEST: NOTE copy_up skipped: image runs as uid $(id -u)"
fi

echo "DOCKERTEST: DONE pass=$pass fail=$fail result=$([ "$fail" -eq 0 ] && echo OK || echo FAILED)"
[ "$fail" -eq 0 ]
