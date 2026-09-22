# Testing

This guide tells how to build rust-bhyve, run its unit tests, and boot
a test guest on an illumos host. Every host name, path and address below
is a placeholder. Replace it with a value from your own environment.

## Build

The VMM binaries run only on illumos. They call the bhyve and viona
ioctls and link `libdladm`. Build release binaries on an illumos host
(SmartOS, OmniOS or an illumos-joyent platform image):

```bash
git clone --recurse-submodules https://github.com/TritonDataCenter/rshyve
cd rust-bhyve
cargo build --workspace --locked --release
```

The build writes `target/release/rshyve`, `target/release/firehyve` and
`target/release/fhrun`.

Prerequisites: a Rust toolchain at or above the `rust-version` in the
root `Cargo.toml`, autoconf, automake, libtool, pkg-config, gmake, gcc
and the OpenSSL development files. The vTPM crate builds the vendored
libtpms with autotools, so these tools are necessary for `rshyve`.

Before you install `rshyve` on a node that runs a stock platform image,
check its shared-library dependencies:

```bash
tools/check-needed.sh target/release/rshyve
```

The script fails if the binary needs a library that a platform image
does not supply. [platform.md](platform.md#checking-a-binary-before-it-ships)
gives the details.

If you edit on a different machine, `tools/remote-build.sh` syncs the
tree to an illumos build host and runs the build or the tests there. Set
`VMM_BUILD_HOST` (`user@host`) and `VMM_BUILD_KEY` (an SSH key) first.
Run `tools/remote-build.sh` with no arguments for its subcommands.

## Unit tests

```bash
cargo test --workspace --locked --no-fail-fast
```

Use `--no-fail-fast`. Without it, the run stops at the first crate that
fails, and the partial output looks like a completed pass.

The workspace compiles and its unit tests run on Linux and macOS as well
as on illumos. CI runs them on Linux. Tests that need a real bhyve
kernel run only on illumos. [CONTRIBUTING.md](../CONTRIBUTING.md#what-runs-where)
tells what CI proves and what it does not.

The `vmm_tpm_sys` and `vmm_tpm` tests build libtpms from the submodule.
On macOS, install the tools first:

```bash
git submodule update --init third_party/libtpms
brew install autoconf automake libtool pkgconf openssl@3
cargo test -p vmm_tpm_sys -p vmm_tpm
```

`pkgconf` is necessary. It supplies `pkg.m4`, which `autoreconf` needs.
Without it, `autoreconf` stops with `'pkgconfig_DATA' is used but
'pkgconfigdir' is undefined`. If `autoreconf` reports `libtoolize:
command not found`, set `LIBTOOLIZE=glibtoolize`.

Two guest-side crates are separate workspaces that build for
`x86_64-unknown-linux-musl`: `tools/fhrun-init` and
`tools/virtiofs-test`. Test each one from its own directory:

```bash
cd tools/fhrun-init
cargo test --locked --target x86_64-unknown-linux-musl
```

## Boot a test guest

You need root in the global zone, or a bhyve-brand zone. You also need a
VNIC that already exists and a disk: a zvol or a raw image file. One
bhyve VM can exist in a non-global zone at a time.

### rshyve (UEFI)

```bash
rshyve -H -c 2 -m 1G \
  -s 0,hostbridge \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/<disk> \
  -s 6,virtio-net-viona,<vnic> \
  -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,stdio \
  --control-socket /var/run/rshyve/<vm>/control.sock \
  --mdata-nics "$(cat nics.json)" \
  --mdata-ssh-keys "$(cat ~/.ssh/<key>.pub)" \
  <vm>
```

COM1 is on the terminal. The built-in metadata agent serves COM2 because
the command gives no `-l com2` backend. A cloud-init guest reads its
network and SSH key from it. [mdata.md](mdata.md#nic-json-format) gives
the `nics.json` format.

Query the VM through the control socket:

```bash
echo '{"command":"status"}' | socat - UNIX-CONNECT:/var/run/rshyve/<vm>/control.sock
echo '{"command":"metrics"}' | socat - UNIX-CONNECT:/var/run/rshyve/<vm>/control.sock
```

SIGTERM stops the VM with `VM_SUSPEND_POWEROFF`. The guest gets no ACPI
power-button event, so it cannot flush first. To stop a guest cleanly,
power it off from inside the guest.

### firehyve (direct boot)

firehyve needs a Linux kernel. `kernel/build.sh` builds a small one in a
Docker container and writes a bzImage (`vmlinuz-fastboot`) and a PVH ELF
(`vmlinux-fastboot`). firehyve boots either.

```bash
firehyve -c 1 -m 512M \
  --kernel kernel/vmlinux-fastboot \
  --initrd <initramfs.cpio> \
  --cmdline "console=ttyS0" \
  -s 4,virtio-rnd \
  -l com1,stdio \
  <vm>
```

When the guest powers off, firehyve exits.

### Clean up

If a VMM process ends abnormally, its kernel VM instance can stay. List
the instances and destroy only your own by name:

```bash
ls /dev/vmm
bhyvectl --vm=<vm> --destroy
```

Give your test VMs a name prefix of your own. On a shared host, do not
use `pkill rshyve` or a pattern that matches other VMs. Do not touch a
`SYSbhyve-*` instance: each one is a zone's running guest.

## Harness scripts

These scripts run a whole test on an illumos node. They run as root in
the global zone. Use them only on a node that you control.

### virtio-fs, virtio-rnd and virtio-vsock

`tools/virtiofs-stage.sh` builds the guest init (musl), a fixture
directory and an initramfs on your machine, and copies them with the
guest kernel to the node. Set `VMM_FIREHYVE_HOST` (`user@host`) and
`VMM_FIREHYVE_KEY` (an SSH key). Set `VMM_FIREHYVE_BIN` to push a
firehyve binary too. Build the guest kernel first:

```bash
cd kernel && KERNEL_EXTRA_CONFIG="virtiofs.config container.config rng.config" \
  KERNEL_OUT_SUFFIX=-container-rng ./build.sh
cd .. && VMM_FIREHYVE_BIN=target/release/firehyve tools/virtiofs-stage.sh
```

Then run the test on the node:

```bash
/var/tmp/virtiofs-test/virtiofs-run.sh [rw|ro|container|entrypoint|all]
```

With no argument, the script boots the VM twice. On the read-write
share, every mutation must succeed. On the `ro` share, every mutation
must fail with EROFS. Each check prints one `VFSTEST:` line, and the run
ends with `VIRTIOFS-TEST: PASS` or `FAIL`. The same boot also tests
virtio-rnd and a vsock echo in both directions.

To boot a container image as the guest root filesystem, stage an image
with `tools/virtiofs-container-stage.sh <image>` and run the `container`
or `entrypoint` mode.

`tools/virtiofs-test/src/checks.rs` and the fixture that
`tools/virtiofs-stage.sh` builds must agree on file names, sizes and
contents. Change them together.

### Hotplug

The staging step above also copies `tools/hotplug-run.sh` and, if it
exists, the hotplug guest kernel. Build that kernel with:

```bash
cd kernel && KERNEL_EXTRA_CONFIG="vsock.config hotplug.config" \
  KERNEL_OUT_SUFFIX=-hotplug ./build.sh
```

On the node:

```bash
cd /var/tmp/virtiofs-test
VMM_HOTPLUG_DISK=<zvol or file> ./hotplug-run.sh [disk|cpu|mem]
```

The script boots a firehyve VM with `--hotplug`, adds a disk, a CPU and
memory through the vsock `CONTROL` verb, and exits non-zero unless the
guest reads the new disk, runs a thread on the new CPU and counts the
new memory. `VMM_HOTPLUG_DISK` has no default. The script reads the disk
and never writes it, and no other VM may use it.

Memory hot-add needs `-S`, so the host VMM reservoir must have free
space. Size it first, for example `/usr/lib/rsrvrctl -c 512 -a 2048`.
[cli.md](cli.md#hotplug) tells why.

### Live migration

Start the destination and send `migrate-source` as
[migration.md](migration.md#control-commands) shows. Run a workload in
the guest during the migration. After it, check that the guest answers
on the network from the destination and that the workload continues.

To test the CPU check, migrate between hosts with different CPUs. From
the older CPU to the newer one, the migration must succeed. From the
newer CPU to the older one, the destination must refuse, unless both
VMs start with a `--cpu-baseline` that the older host supports.

### Device reset matrix

A guest writes the device status register once to reset a device, and
the reset must be complete when that write returns. No unit test drives
a real driver through one, so drive it from inside a Linux guest:

1. Give the guest a virtio-blk data disk. Write a file of known content
   to it, for example 16 MiB, and record its SHA-256.
2. Start a long `dd` against the disk and leave it running. A reset
   with no I/O in flight tests almost nothing.
3. Unbind and rebind the driver in a loop. Each rebind is a real driver
   reset:

   ```bash
   ls /sys/bus/virtio/drivers/virtio_blk    # find <virtioN>
   echo <virtioN> > /sys/bus/virtio/drivers/virtio_blk/unbind
   echo <virtioN> > /sys/bus/virtio/drivers/virtio_blk/bind
   ```

4. After each reset, read the file back and compare its SHA-256. A lost
   or reordered completion corrupts data with no error.
5. Repeat the run with the guest kernel option
   `virtio_pci.force_legacy=1`. The legacy driver reads the status back
   once and does not poll, so a late reset shows there.
6. For multiqueue, give the guest 4 vCPUs, so the disk gets 4 queues.
   For each queue k, write 128 MiB and read it back with
   `taskset -c k`. Then read `/proc/interrupts`. Every virtio-blk queue
   vector must have moved. If only one vector moves, completions go to
   the wrong queue, although the throughput looks normal.
7. Also cover a warm reboot with a dirty page cache and an ACPI S5
   poweroff from inside the guest. After each poweroff, check the host
   for a leaked `rshyve` process and a leaked entry in `/dev/vmm`.

A pass with a Linux guest is not a result for an illumos guest. The
illumos virtio driver writes the status register once and never reads
it back, on either transport.

### Boot profiling

`tools/profile/run.sh` records VM exits, stack samples and off-CPU time
for one VMM boot with DTrace. It needs the global zone.
[tools/profile/README.md](../tools/profile/README.md) tells how to use
it.
