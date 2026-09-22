#!/usr/bin/env bash
#
# Stage a container image onto the illumos test node as a virtio-fs
# export, so a guest can boot it as its root filesystem.
#
#   tools/virtiofs-container-stage.sh [image]
#
# Defaults to alpine:3.21. The image is flattened with `docker export`,
# which resolves the layers and their whiteouts into one tree, so the
# node needs no OCI tooling. The tar is unpacked on the node rather than
# here: only the node's tar preserves ownership and modes for a rootfs.
#
# The export carries no metadata, so the image's OCI config is fetched
# separately and written beside the tree as container.json. The share
# ends up holding:
#
#   rootfs/          the image tree, mounted as the overlay lower layer
#   container.json   entrypoint, cmd, env, workdir and user
#
# The config sits beside the tree rather than inside it, so the
# container cannot see or alter what it was told to run.
#
#   VMM_FIREHYVE_HOST  user@host of the illumos test node   (required)
#   VMM_FIREHYVE_KEY   SSH key for that node                (required)
#   VMM_FIREHYVE_DIR   staging directory on the node        (/var/tmp/virtiofs-test)

set -euo pipefail

IMAGE="${1:-alpine:3.21}"
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
SSH=(ssh -o ConnectTimeout=15 -i "$KEY" "$HOST")

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --platform matters: the guest is x86_64, and a build host on Apple
# silicon pulls arm64 by default, which produces a rootfs of binaries
# the guest cannot run.
echo "=== pulling $IMAGE (linux/amd64) ==="
docker pull --quiet --platform linux/amd64 "$IMAGE"

echo "=== flattening to a rootfs tar ==="
cid="$(docker create --platform linux/amd64 "$IMAGE" /bin/sh)"
trap 'docker rm -f "$cid" >/dev/null 2>&1; rm -rf "$WORK"' EXIT
docker export "$cid" -o "$WORK/rootfs.tar"
docker rm -f "$cid" > /dev/null
trap 'rm -rf "$WORK"' EXIT
ls -la "$WORK/rootfs.tar"

echo "=== reading the image config ==="
docker image inspect --format '{{json .Config}}' "$IMAGE" > "$WORK/oci-config.json"
# Translated here rather than in the guest, so the guest parser stays
# small and this project controls the field names it reads.
python3 - "$WORK/oci-config.json" "$WORK/container.json" <<'PY'
import json, sys

cfg = json.load(open(sys.argv[1])) or {}
spec = {
    "entrypoint": cfg.get("Entrypoint") or [],
    "cmd": cfg.get("Cmd") or [],
    "env": cfg.get("Env") or [],
    "workdir": cfg.get("WorkingDir") or "/",
    "user": cfg.get("User") or "",
}
if not spec["entrypoint"] and not spec["cmd"]:
    sys.exit("STAGE-FAIL: image declares neither Entrypoint nor Cmd")
json.dump(spec, open(sys.argv[2], "w"), indent=2)
print("  entrypoint:", spec["entrypoint"])
print("  cmd:       ", spec["cmd"])
print("  workdir:   ", spec["workdir"])
print("  user:      ", spec["user"] or "(root)")
PY

echo "=== pushing to $HOST:$DIR/container ==="
"${SSH[@]}" "mkdir -p $DIR/container"
scp -q -i "$KEY" "$WORK/rootfs.tar" "$HOST:$DIR/container/rootfs.tar"
scp -q -i "$KEY" "$WORK/container.json" "$HOST:$DIR/container/container.json"
scp -q -i "$KEY" "$SRC_DIR/tools/virtiofs-container-test.sh" \
    "$HOST:$DIR/container/container-test.sh"

echo "=== unpacking on the node ==="
# gtar, not tar: the platform tar does not understand the GNU long-name
# and sparse entries a docker export can contain.
"${SSH[@]}" "set -e
    rm -rf $DIR/container/rootfs
    mkdir -p $DIR/container/rootfs
    gtar -xpf $DIR/container/rootfs.tar -C $DIR/container/rootfs
    # Only for the harness's own checks. The image's entrypoint runs
    # unless virtiofs.entry names this.
    cp $DIR/container/container-test.sh $DIR/container/rootfs/container-test.sh
    chmod 0755 $DIR/container/rootfs/container-test.sh
    rm -f $DIR/container/rootfs.tar $DIR/container/container-test.sh
    sync
    echo 'rootfs:'
    ls $DIR/container/rootfs | tr '\n' ' '
    echo
    du -sh $DIR/container/rootfs"

echo "STAGED $IMAGE. Now run:  ssh \$VMM_FIREHYVE_HOST $DIR/virtiofs-run.sh container"
