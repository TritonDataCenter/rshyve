#!/bin/bash
# Build a minimal fast-boot Linux kernel for rust-bhyve direct boot.
# Runs inside an Alpine Docker container with the Linux build toolchain.
#
# Usage: ./build.sh [kernel_version]
# Output: ./vmlinuz-fastboot  bzImage, gzip self-decompressing
#         ./vmlinux-fastboot  uncompressed ELF with the PVH entry note
#
# KERNEL_EXTRA_CONFIG layers further config fragments on fastboot.config
# and KERNEL_OUT_SUFFIX renames the output, which builds a variant
# without disturbing the kernel the boot benchmarks measure. Several
# fragments can be given, separated by spaces, and are applied in order:
#
#   KERNEL_EXTRA_CONFIG=virtiofs.config KERNEL_OUT_SUFFIX=-virtiofs ./build.sh
#   KERNEL_EXTRA_CONFIG="virtiofs.config msi.config" \
#       KERNEL_OUT_SUFFIX=-virtiofs-msi ./build.sh
#
# Both come from one compile. The PVH image skips the real-mode stub
# and the self-decompression, which the Firecracker NSDI paper measures
# at about 40 ms.

set -euo pipefail

KERNEL_VERSION="${1:-6.12.13}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EXTRA_CONFIG="${KERNEL_EXTRA_CONFIG:-}"
OUT_SUFFIX="${KERNEL_OUT_SUFFIX:-}"

# The guest this kernel boots runs the virtio-fs and hot-plug tests, so a
# MITM or CDN compromise would put a chosen kernel under the VMM being
# tested. The digests are kernel.org's own, from sha256sums.asc for the
# release. Adding a version means adding its line here first.
declare -A KERNEL_SHA256=(
    [6.12.13]=f3ebdeea9e555b4cface44e29670056f4024541e6bd222fbcf776c818974fbba
)
WANT_SHA256="${KERNEL_SHA256[$KERNEL_VERSION]:-}"
if [ -z "$WANT_SHA256" ]; then
    echo "BUILD-FAIL: no recorded SHA-256 for ${KERNEL_VERSION}."
    echo "  Add it from https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/sha256sums.asc"
    exit 1
fi

# The image is pinned by digest, not by the 3.21 tag: the toolchain that
# compiles the kernel must not change under the benchmarks.
ALPINE_IMAGE="alpine@sha256:ce64758a109eb420d874a118f87920e625e12d3634e03b4a5573fd9f6e5d3507" # 3.21

for frag in $EXTRA_CONFIG; do
    if [ ! -f "$SCRIPT_DIR/$frag" ]; then
        echo "BUILD-FAIL: no config fragment at $SCRIPT_DIR/$frag"
        exit 1
    fi
done

echo "=== Building Linux ${KERNEL_VERSION} fast-boot kernel ${EXTRA_CONFIG:+(+ $EXTRA_CONFIG)} ==="

# The fragment list and the digest go in as environment variables rather
# than being spliced into the script text, so a value with a quote or a
# `$(...)` cannot run inside the container.
docker run --rm \
    --platform linux/amd64 \
    -v "${SCRIPT_DIR}:/build" \
    -w /tmp/kernel \
    -e "EXTRA=${EXTRA_CONFIG}" \
    -e "WANT_SHA256=${WANT_SHA256}" \
    "$ALPINE_IMAGE" \
    sh -c "
set -ex

apk add --no-cache \
    build-base bc flex bison elfutils-dev openssl-dev \
    perl linux-headers ncurses-dev diffutils findutils

# Download kernel source, then verify it before it is unpacked. A
# cached tarball is verified too: it may have come from an earlier
# unverified run.
TARBALL=linux-${KERNEL_VERSION}.tar.xz
if [ ! -f /build/\${TARBALL} ]; then
    wget -q https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/\${TARBALL} -O /build/\${TARBALL}.part
    mv /build/\${TARBALL}.part /build/\${TARBALL}
fi
echo \"\${WANT_SHA256}  /build/\${TARBALL}\" | sha256sum -c - || {
    echo 'BUILD-FAIL: the kernel tarball does not match its recorded SHA-256'
    exit 1
}
tar xf /build/\${TARBALL}
cd linux-${KERNEL_VERSION}

# Start from tinyconfig, the smallest config, and add fastboot.config.
make ARCH=x86_64 tinyconfig

# kconfig keeps the last assignment of an option, so a fragment
# appended here overrides fastboot.config.
cat /build/fastboot.config >> .config
for frag in \$EXTRA; do
    cat /build/\$frag >> .config
done
make ARCH=x86_64 olddefconfig

echo '=== Config summary ==='
grep -c '=y' .config | xargs printf 'Static options: %s\n'
grep -c '=m' .config | xargs printf 'Module options: %s (should be 0)\n'

# olddefconfig silently drops an option whose dependencies are unmet.
# Catch that here rather than after a full build and a failed boot.
grep -q '^CONFIG_PVH=y' .config || {
    echo 'BUILD-FAIL: CONFIG_PVH was dropped by olddefconfig'
    exit 1
}

# Same check for every option the fragments ask for.
for frag in \$EXTRA; do
    for opt in \$(grep -o '^CONFIG_[A-Z0-9_]*=y' /build/\$frag); do
        grep -q \"^\$opt\" .config || {
            echo \"BUILD-FAIL: \$opt was dropped by olddefconfig\"
            exit 1
        }
    done
done

make ARCH=x86_64 -j\$(nproc) bzImage 2>&1 | tail -20

# --strip-all keeps allocatable sections, so the PT_NOTE carrying the
# PVH entry survives.
cp arch/x86_64/boot/bzImage /build/vmlinuz-fastboot${OUT_SUFFIX}
objcopy --strip-all vmlinux /build/vmlinux-fastboot${OUT_SUFFIX}

readelf -n /build/vmlinux-fastboot${OUT_SUFFIX} | grep -qi xen || {
    echo 'BUILD-FAIL: vmlinux carries no Xen PVH note'
    exit 1
}

ls -la /build/vmlinuz-fastboot${OUT_SUFFIX} /build/vmlinux-fastboot${OUT_SUFFIX}
echo '=== Kernels built successfully ==='
echo \"bzImage: \$(du -h /build/vmlinuz-fastboot${OUT_SUFFIX} | cut -f1)\"
echo \"PVH ELF: \$(du -h /build/vmlinux-fastboot${OUT_SUFFIX} | cut -f1)\"
"
