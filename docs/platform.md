# Platform integration

This page tells how to run rshyve for SmartOS bhyve zones, and what the
host must supply.

## Running rshyve in place of the platform bhyve

rshyve takes the command line that the bhyve zone brand builds
(`boot.c`), so it can replace `/usr/sbin/bhyve`. It is not a complete
bhyve: `ahci-hd` is refused, `-D`, `-k` and `--json-config` are refused,
an unknown `-s` driver is logged and skipped, and some device options
are ignored ([features.md](features.md#accepted-and-then-ignored)).

The brand starts `zhyve` inside the zone. In an illumos-joyent tree
whose `usr/src/cmd/zhyve/zhyve.c` has `try_bhyve_override`, zhyve runs
`/bhyve.zone` in place of the platform bhyve when that file exists, with
the same argv. It opens the file once and runs it through the open file
descriptor. A file that is present and not valid is a fatal error. It
does not fall back to the platform bhyve.

To run rshyve for one zone on such a platform:

1. Install `rshyve` on the node.
2. In the zone, make `/bhyve.zone` the `rshyve` binary, or a symbolic
   link to it.
3. Boot the zone.

No change to vmadm, VMAPI or the brand is needed. No prebuilt platform
image is published.

### The boot.c command line

`boot.c` reads the zone configuration and builds a command line of this
form:

```
rshyve -H -A
  -U <uuid>
  -B "1,manufacturer=Joyent,product=SmartDC HVM,version=<ver>,serial=<zone>,sku=001,family=Virtual Machine"
  -c <vcpus> -m <ram>
  -s 0,hostbridge,model=i440fx
  -s <slot>,virtio-blk,<path>[,nodelete][,sectorsize=N]
  -s <slot>,nvme,<path>
  -s <slot>,ahci-cd,<path>
  -s <slot>,virtio-net-viona,<vnic>[,promiscphys]
  -s <slot>,passthru,<path>
  -s 30:0,fbuf,vga=off,unix=<path>[,password=<pw>]
  -s 30:1,xhci,tablet
  -s 31,lpc
  -l bootrom,/usr/share/bhyve/uefi-rom.bin
  -l com1,/dev/zconsole
  -l com2,socket,<path>
  SYSbhyve-<id>
```

rshyve parses all of it and boots from it. `nodelete` and `sectorsize`
work on virtio-blk and are ignored on NVMe. `vga=off` and the `tablet`
on `xhci` are accepted and ignored. `password=` is used as given, with
no base64 decode.

### PCI slots used by boot.c

| Slot | Device |
|------|--------|
| 0 | Host bridge (i440fx) |
| 3 to 5 | CD-ROM devices |
| 4 | Boot disk |
| 6 and up | Other disks |
| 29 | Network (function 0 is the primary NIC) |
| 30:0 | Framebuffer |
| 30:1 | xHCI tablet |
| 31 | LPC bridge (the chipset always puts it at slot 1) |

### Stopping a VM

vmadm stops a VM with SIGTERM. rshyve turns SIGTERM into
`VM_SUSPEND_POWEROFF`: every vCPU stops at once, and the guest gets no
ACPI power-button event and no chance to flush. If the VM does not stop
in time, rshyve halts it. C bhyve sends an ACPI power-button event
instead. rshyve exits 1 on a poweroff, like C bhyve, so the zone does
not restart the guest.

### Metadata

On a SmartOS node, the brand passes `-l com2,socket,<path>`. rshyve
listens on that socket, and the metadata agent in the global zone
connects to it. The built-in metadata agent runs only when there is no
`-l com2` ([mdata.md](mdata.md)).

## Build and dependencies

```bash
cargo build --workspace --locked --release   # on an illumos host
```

rshyve must need only libraries that a stock platform image supplies,
because a platform image has no pkgsrc. `tools/check-needed.sh` holds
the list. It includes `libdladm.so.1` (VNIC lookup) and
`libcrypto-smartos.so.3` (the platform OpenSSL, for the vTPM). A binary
that records `NEEDED libcrypto.so.3` links on the build host and does
not start on a node.

The kernel must supply:

- **bhyve**, API version 18. That is the only version tested. The VMM
  does not enforce a minimum: an older kernel starts and then fails at
  an ioctl, or gives less with no message. The code needs v5 for CPUID
  control, v8 for dirty-page tracking (on unless `--no-track-dirty`),
  and v11 to v13 for live migration. From v16 a suspend reports which
  vCPU had a triple fault.
- **viona**, interface version 6. The VMM does not check it, so a
  mismatched module shows as an ioctl error.

### Checking a binary before it ships

```bash
tools/check-needed.sh target/release/rshyve
```

The script fails if a `NEEDED` entry names a library that a stock
platform image does not supply, or if the recorded platform OpenSSL
version is newer than the oldest supported image.
`tools/remote-build.sh build` runs it on every build.

### OpenSSL and the vTPM

The vTPM uses the vendored libtpms, which needs OpenSSL. A platform
image supplies `libcrypto-smartos.so.3`. Almost all of its exports have
a `sunw_` prefix, so that a zone's pkgsrc OpenSSL and the platform
OpenSSL cannot collide in one process. So `-lcrypto` alone cannot use
it.

`crates/vmm-tpm-sys/build.rs` reads the export list of the platform
library and writes a `#pragma redefine_extname` header, which it
includes when it builds libtpms. A link farm makes `-lcrypto` resolve to
the platform library, so `ld` records its SONAME. Before it builds
anything, the script compiles and runs a known-answer test against the
platform library, so a mismatch fails the build. This path is used only
for a native build on SmartOS. Elsewhere, pkg-config decides.

The OpenSSL headers come from pkgsrc and the library from the platform
image, so they can be different OpenSSL 3 releases. That is supported.
Build on a platform image no newer than the oldest node that you deploy
to: `ld.so.1` refuses a binary that needs a symbol version the node's
library does not define. The reverse is safe.

## Host tunables

`strmsgsz` must be 8192 or more, or 0. Read the value with
`echo 'strmsgsz/E' | mdb -k`, and look for `set strmsgsz=` in
`/etc/system`. The default is 65536.

The VMM writes to some Unix sockets under a lock or on a vCPU thread, so
each write must end. illumos has no send timeout that works on an
AF_UNIX socket, so the VMM waits for `POLLOUT` and sends at most 8 KiB,
which the stream head passes as one message if `strmsgsz` allows it.
Below 8192 the send splits, and the wait between the parts has no
timeout. This applies to the virtio-console client, the vsock host
socket and the firehyve `CONTROL` reply.

Check this on each new host. A test pass on macOS proves nothing:
macOS honours `SO_SNDTIMEO` on an AF_UNIX socket, and illumos accepts
it and ignores it.

## Interrupt injection and the VM lock

A device reset waits for the interrupt injections already in flight,
and both injection ioctls take the bhyve VM read lock. illumos gives the
lock to a waiting writer first. So an operation that holds the VM write
lock delays every reset on that VM: memory hot-add, pause, resume and
VM-wide device-state export.

The delay has no upper bound, and it grows with the size of a hot-add:
233 to 270 ms per GiB added in one test. Do not hot-add memory to a VM whose
guest can reset a device, and do not expect a reset to finish in a
fixed time ([features.md](features.md#known-caveats)).

## Compared with C bhyve

| Feature | C bhyve | rshyve |
|---------|---------|--------|
| virtio-blk | Yes | Yes |
| NVMe | Yes | Yes |
| virtio-net (viona) | Yes | Yes |
| AHCI CD-ROM (`ahci-cd`) | Yes | Yes |
| AHCI disk (`ahci-hd`) | Yes | No, refused at startup |
| Framebuffer and VNC | Yes | Unix socket only: no TCP, no `wait`, no VGA, Raw encoding, one client |
| xHCI tablet | Yes | One slot, absolute pointer, no scroll wheel |
| PCI passthrough | Yes | Intel VT-d only, MSI-X devices refused, not run with an assigned device (OSF-11) |
| Live migration | No | Yes, experimental |
| CPUID masking | No | Yes (`--cpu-baseline`) |
| Built-in metadata agent | No | Yes |
| Control socket | No | Yes (JSON over a Unix socket) |
| SIGTERM | ACPI power button | `VM_SUSPEND_POWEROFF`, no guest shutdown |
