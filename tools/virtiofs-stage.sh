#!/usr/bin/env bash
#
# Stage the virtio-fs test onto an illumos test node.
#
# Builds the guest init, the fixture the guest checks against, and an
# initramfs holding just that init, then pushes them with the VMM and
# the FUSE-capable kernel. Run tools/virtiofs-run.sh on the node after
# this.
#
# The same init also runs the hot-add checks, so the hotplug kernel and
# tools/hotplug-run.sh go with it. One initramfs serves both.
#
#   tools/virtiofs-stage.sh
#
# The node is site-specific and must not be baked in:
#
#   VMM_FIREHYVE_HOST  user@host of the illumos test node   (required)
#   VMM_FIREHYVE_KEY   SSH key for that node                (required)
#   VMM_FIREHYVE_DIR   staging directory on the node        (/var/tmp/virtiofs-test)
#   VMM_FIREHYVE_BIN   firehyve to push  (kernel/../ build host copy by default)

set -euo pipefail

HOST="${VMM_FIREHYVE_HOST:?set VMM_FIREHYVE_HOST to user@host of your illumos test node}"
KEY="${VMM_FIREHYVE_KEY:?set VMM_FIREHYVE_KEY to the SSH key for that node}"
DIR="${VMM_FIREHYVE_DIR:-/var/tmp/virtiofs-test}"

# The value is spliced into a command that a remote root shell parses, so
# it is constrained to an absolute path made of path characters. A space,
# a quote or a `$(...)` would otherwise run as root on the node.
case "$DIR" in
    /*) ;;
    *) echo "STAGE-FAIL: VMM_FIREHYVE_DIR must be an absolute path" >&2; exit 1 ;;
esac
if printf '%s' "$DIR" | grep -qv '^[A-Za-z0-9_./-]*$'; then
    echo "STAGE-FAIL: VMM_FIREHYVE_DIR may hold only letters, digits, _ . / -" >&2
    exit 1
fi

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The harness exercises virtio-fs and virtio-rnd, so its kernel needs
# the fragments for both. container.config is included because the
# platform targets that kernel.
KERNEL="${VMM_FIREHYVE_KERNEL_SRC:-$SRC_DIR/kernel/vmlinux-fastboot-container-rng}"
# The hot-add checks need ACPI CPU, memory and PCI hotplug, which only
# this kernel carries.
HOTPLUG_KERNEL="${VMM_FIREHYVE_HOTPLUG_KERNEL_SRC:-$SRC_DIR/kernel/vmlinux-fastboot-hotplug}"
GUEST_SRC="$SRC_DIR/tools/virtiofs-test"
GUEST_BIN="$GUEST_SRC/target/x86_64-unknown-linux-musl/release/virtiofs-test"

# Must match tools/virtiofs-test/src/checks.rs.
BIG_LEN=$((4 * 1024 * 1024))

SSH=(ssh -o ConnectTimeout=15 -i "$KEY" "$HOST")

if [ ! -f "$KERNEL" ]; then
    echo "STAGE-FAIL: no $KERNEL"
    echo "  build it with:"
    echo "    cd kernel && KERNEL_EXTRA_CONFIG=\"virtiofs.config container.config rng.config\" \\"
    echo "        KERNEL_OUT_SUFFIX=-container-rng ./build.sh"
    exit 1
fi

echo "=== building the guest init (musl) ==="
(cd "$GUEST_SRC" && cargo build --release --target x86_64-unknown-linux-musl)

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "=== building the fixture ==="
EXPORT="$WORK/export"
mkdir -p "$EXPORT/sub/nested" "$EXPORT/empty"
printf 'hello from the host\n' > "$EXPORT/hello.txt"
printf 'deep\n' > "$EXPORT/sub/nested/deep.txt"
ln -s hello.txt "$EXPORT/link-to-hello"
cp "$GUEST_BIN" "$EXPORT/exec-probe"
chmod 0755 "$EXPORT/exec-probe"

# byte i = i % 251, the pattern checks.rs::pat recomputes. 251 is prime,
# so the pattern does not align with any power of two block size and a
# chunk read back from the wrong offset cannot go unnoticed.
python3 - "$EXPORT/big.bin" "$BIG_LEN" <<'PY'
import sys
path, length = sys.argv[1], int(sys.argv[2])
# period[j] == j, so tiling it puts i % 251 at every offset i.
period = bytes(range(251))
data = (period * (length // 251 + 1))[:length]
open(path, 'wb').write(data)
PY
actual=$(wc -c < "$EXPORT/big.bin" | tr -d ' ')
if [ "$actual" != "$BIG_LEN" ]; then
    echo "STAGE-FAIL: big.bin is $actual bytes, want $BIG_LEN"
    exit 1
fi

echo "=== building the initramfs ==="
ROOTFS="$WORK/rootfs"
mkdir -p "$ROOTFS"/{dev,proc,sys,mnt}
cp "$GUEST_BIN" "$ROOTFS/init"
chmod 0755 "$ROOTFS/init"
# -R 0:0 because the kernel takes ownership from the archive, and the
# build host's uid is not root on the node.
(cd "$ROOTFS" && find . | cpio -o -H newc -R 0:0 2>/dev/null) > "$WORK/initramfs-virtiofs.cpio"

cp "$KERNEL" "$WORK/vmlinux-guest"
cp "$SRC_DIR/tools/virtiofs-run.sh" "$WORK/virtiofs-run.sh"
cp "$SRC_DIR/tools/hotplug-run.sh" "$WORK/hotplug-run.sh"
chmod 0755 "$WORK/virtiofs-run.sh" "$WORK/hotplug-run.sh"

PUSH=("$WORK/initramfs-virtiofs.cpio" "$WORK/vmlinux-guest"
      "$WORK/virtiofs-run.sh" "$WORK/hotplug-run.sh")
if [ -f "$HOTPLUG_KERNEL" ]; then
    cp "$HOTPLUG_KERNEL" "$WORK/vmlinux-hotplug"
    PUSH+=("$WORK/vmlinux-hotplug")
else
    # Not fatal: the virtio-fs runs do not need it. hotplug-run.sh says
    # the same thing again on the node, and refuses to boot without it.
    echo "STAGE-WARN: no hotplug kernel at $HOTPLUG_KERNEL"
    echo "  hotplug-run.sh will refuse to run. Build it with:"
    echo "    cd kernel && KERNEL_EXTRA_CONFIG=\"vsock.config hotplug.config\" \\"
    echo "        KERNEL_OUT_SUFFIX=-hotplug ./build.sh"
fi

echo "=== pushing to $HOST:$DIR ==="
"${SSH[@]}" "mkdir -p $DIR"
# --delete only on the fixture, where a file left over from an older
# layout would be checked against the current checks.rs. The rest of the
# directory holds a firehyve pushed by a separate step, which a wider
# delete would remove.
rsync -a --delete -e "ssh -i $KEY" "$WORK/export/" "$HOST:$DIR/export/"
rsync -a -e "ssh -i $KEY" "${PUSH[@]}" "$HOST:$DIR/"

if [ -n "${VMM_FIREHYVE_BIN:-}" ]; then
    echo "=== pushing $VMM_FIREHYVE_BIN ==="
    rsync -a -e "ssh -i $KEY" "$VMM_FIREHYVE_BIN" "$HOST:$DIR/firehyve"
    "${SSH[@]}" "chmod 0755 $DIR/firehyve"
fi

"${SSH[@]}" "ls -la $DIR"
echo "STAGED. Now run:  ssh \$VMM_FIREHYVE_HOST $DIR/virtiofs-run.sh"
echo "  or, with VMM_HOTPLUG_DISK set on the node: $DIR/hotplug-run.sh"
