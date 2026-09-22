# Feature reference

This is the reference for what each binary supports, with the limits
and caveats. It is written from the code. [roadmap.md](roadmap.md) lists
the open work.

`rshyve` replaces the illumos `bhyve(8)` command on the command line
that the zone brand builds: UEFI firmware, NVMe, an AHCI CD-ROM, a VNC
framebuffer, PCI passthrough, a control socket and live migration. It
is not all of bhyve. The tables below show what it implements, what it
accepts and ignores, and what it refuses.

`firehyve` is a microVM. It boots a Linux kernel directly and links a
smaller set of crates. It refuses at startup most options that it
cannot honor. It accepts, logs and ignores the options that the bhyve
zone brand puts on every zone, and it ignores `--mdata-root-pw-file`
without a log entry
([What firehyve refuses and ignores](#what-firehyve-refuses-and-ignores)).

Use `firehyve` for a Linux kernel that you supply and a virtio-only
device list. Use `rshyve` for firmware, a graphical console, NVMe,
passthrough or migration.

## At a glance

**yes**: works. **caveat**: works with a condition that the section
below names. **exp**: experimental. **no**: absent. **refused**: a
startup error that names the item. **ignored**: accepted, logged and
not used.

| Capability | rshyve | firehyve |
|---|---|---|
| **Boot** | | |
| Direct Linux bzImage or PVH ELF (`--kernel`) | yes | yes, required |
| `--initrd`, `--cmdline`, `--cmdline-base64` | yes | yes |
| UEFI bootrom (`-l bootrom,...`) | yes | ignored |
| Persistent UEFI variable store | caveat | no |
| `bootindex=N` boot order | yes | refused |
| Guest reboot | re-executes the VMM | exits 0 |
| **Storage** | | |
| `virtio-blk` / `virtio-blk-pci` | yes | yes |
| `nvme` | caveat | refused |
| `ahci-cd` (ATAPI CD-ROM, read-only) | yes | refused |
| `ahci-hd` | refused | refused |
| qcow2 or other image formats, I/O throttling | no | no |
| **Network** | | |
| `virtio-net-viona` | yes | yes |
| Other NIC models (e1000, tap, slirp) | no | refused |
| **Serial and console** | | |
| `-l com1,stdio` | yes | yes |
| `-l com1,socket,<path>` | yes | ignored, falls back to stdio |
| `-l com1,<device path>` (`/dev/zconsole`) | yes | yes |
| COM2 backend | yes | ignored: COM2 is fixed, transmit only, to stdout |
| `virtio-console` | yes | yes |
| `virtio-vsock` | no | yes |
| **Filesystem** | | |
| `virtio-fs` | yes | yes |
| virtio-fs DAX, xattrs, ACLs, locks | no | no |
| **Display and input** | | |
| `fbuf` framebuffer with VNC on a Unix socket | caveat | ignored |
| VNC over TCP, VGA | no | no |
| PS/2 keyboard (always present) | yes | no |
| `xhci` absolute tablet | caveat | ignored |
| USB keyboard, PS/2 mouse motion | no | no |
| **Migration and control** | | |
| Live migration, Unix socket or TCP | exp | refused |
| Control socket (`--control-socket`) | yes | refused |
| Dirty page tracking | on unless `--no-track-dirty` | off |
| **Hotplug (`--hotplug`, off by default)** | | |
| PCI hot-add | caveat | caveat |
| PCI hot-remove (the guest runs `_EJ0`) | caveat | caveat |
| CPU hot-add | caveat | caveat |
| Memory hot-add | caveat | caveat |
| CPU or memory hot-remove | no (illumos) | no (illumos) |
| Hotplug commands | control socket | vsock `CONTROL` verb |
| **Platform** | | |
| vTPM 2.0 (`--vtpm`) | caveat | refused |
| Hyper-V enlightenments (`--hyperv`) | exp | refused |
| `--cpu-baseline` CPUID masking | yes | yes |
| PCI passthrough (`passthru`) | caveat | refused |
| `virtio-rnd` | yes | yes |
| Metadata agent on COM2 | yes | no, `--mdata-*` refused |
| `-D`, `-k`, `--json-config` | refused | refused |

## Tested configurations

These results come from SmartOS x86_64 hosts with bhyve API v18. The
reset, reboot and poweroff rows ran on one host. The cross-host
migrations used a second host with an older CPU (AVX2, no AVX-512). CI
does not repeat any of them. The procedure column links to the steps in
[testing.md](testing.md).

| Scenario | Procedure | Result |
|----------|-----------|--------|
| UEFI boot of Ubuntu 24.04, 2 vCPUs, 1 GiB | [Boot](testing.md#rshyve-uefi) | Works |
| NVMe boot disk on a ZFS zvol | [Boot](testing.md#rshyve-uefi) | Works |
| virtio-net with the cloud-init SmartOS datasource | [Boot](testing.md#rshyve-uefi) | Works |
| Guest address and SSH keys from the metadata agent | [Boot](testing.md#rshyve-uefi) | Works |
| Cross-host migration, older CPU to newer CPU | [Migration](testing.md#live-migration) | Works |
| Cross-host migration, newer CPU to older CPU | [Migration](testing.md#live-migration) | Needs `--cpu-baseline`. Without it the destination refuses |
| Same-host migration with a busy workload | [Migration](testing.md#live-migration) | Works |
| Busybox initramfs guest: 293 virtio-blk resets with `dd` running, 10 of them on the legacy transport, SHA-256 checked after each | [Reset matrix](testing.md#device-reset-matrix) | 0 mismatches |
| Debian 13, 4 vCPUs: 140 virtio-blk resets (100 modern, 40 legacy transport), SHA-256 checked after each | [Reset matrix](testing.md#device-reset-matrix) | 0 mismatches |
| Debian 13, 4 vCPUs, 4-queue virtio-blk: 128 MiB written and read back per queue, each writer pinned to its queue's vCPU | [Reset matrix](testing.md#device-reset-matrix), step 6 | Works, every queue vector fired |
| Busybox initramfs guest: 5 warm reboots with a dirty page cache | [Reset matrix](testing.md#device-reset-matrix), step 7 | Works |
| Busybox initramfs guest: 30 ACPI S5 poweroffs | [Reset matrix](testing.md#device-reset-matrix), step 7 | No leaked process or VM instance |
| firehyve `--hotplug` on Linux 6.12 (`kernel/hotplug.config`) | [Hotplug](testing.md#hotplug) | `cpu-add`, `mem-add`, `device-add` work |

No illumos guest has run through a device reset, and no assigned PCI
device has run with passthrough.

## What firehyve refuses and ignores

`validate_cli` (`bin/firehyve/src/main.rs`) refuses: no `--kernel`,
`--migrate-listen`, `--control-socket`, `--mdata-nics`,
`--mdata-resolvers`, `--mdata-ssh-keys`, `--mdata-root-pw`, `--vtpm`,
`--vtpm-state-dir`, `--hyperv`, `--tsc-freq-hz`, and a `-s` spec with
`bootindex=`. The device catalog (`bin/firehyve/src/devices.rs`) refuses
a PCI driver that it does not implement.

firehyve accepts what the bhyve zone brand puts on every zone, logs a
warning, and does not use it, because a refusal would stop an unmodified
brand from starting the binary:

- `-l bootrom,<rom>`: nothing is loaded.
- A `-l com1` backend other than `stdio` or an absolute device path, and
  any `-l com2` backend: the port uses the inherited stdio, which
  zoneadmd copies to the zone log, not to the zone console.
- The `fbuf` and `xhci` specs: the slots stay empty. A hot-add of either
  is refused.

firehyve accepts `--mdata-root-pw-file` and ignores it without a
warning.

Both binaries use one clap definition (`crates/vmm-config`), so
`firehyve --help` lists the rshyve-only flags too.

## Device specs

Both binaries parse `-s <addr>,<driver>[,<config>]` with the shared
helpers in `crates/vmm-machine/src/parse.rs` and `devspec.rs`. The
address is `dev`, `dev:func` or `bus:dev:func`, in decimal. `dev` is 0
to 31 and `func` is 0 to 7. Only bus 0 exists: a device on another bus
is refused.

The chipset takes slot 0 (host bridge, 0.0.0) and slot 1 (LPC bridge,
0.1.0). `-s 0,hostbridge` and `-s 31,lpc` are accepted and attach
nothing, whatever slot they name. A real driver in an occupied slot is a
startup error. The LPC bridge is at slot 1, not slot 31.

Unknown options:

| Driver | Unknown option |
|---|---|
| `virtio-blk`, `virtio-blk-pci`, `nvme` | ignored |
| `virtio-net-viona` | ignored |
| `passthru`, `xhci`, `virtio-rnd`, `hostbridge`, `lpc` | the config is not read |
| `fbuf`, `virtio-console`, `virtio-fs`, `virtio-vsock`, `ahci-cd` | startup error |

An unknown driver name is a startup error in firehyve. In rshyve it is
a warning, and the VM boots without that device. `ahci-hd` is a startup
error in both.

Devices attach in argv order. The first device that fails stops the
startup.

## Boot

No flag selects the boot protocol. The VMM reads the file
(`crates/vmm-boot/src/image.rs`): ELF magic means PVH, else `HdrS` at
offset 0x202 means bzImage, else startup fails. An ELF file without the
PVH note is not tried as a bzImage.

- **bzImage**: boot protocol 2.12 or later, `XLF_KERNEL_64` set, and a
  `setup_sects` value that matches the file length. The kernel goes to
  its own `pref_address` with no relocation.
- **PVH**: ELF64, little-endian, x86_64, at least one `PT_LOAD` with
  file data, and a `XEN_ELFNOTE_PHYS32_ENTRY` note (`CONFIG_PVH=y`).
  Only `p_filesz` bytes are copied. BSS relies on zeroed guest RAM.

`--cmdline` defaults to `console=ttyS0 earlyprintk=serial` in both
binaries. `--cmdline-base64` takes the same value in base64, for a zone
brand attribute that cannot hold spaces. Use one or the other. The
command line is at most 4095 bytes, and for a bzImage at most the
kernel's own `cmdline_size`. The initrd goes at the highest page-aligned
address below `min(memory size, 3 GiB)`.

In rshyve, `--kernel` turns off all UEFI setup, and a `-l bootrom`
argument is then ignored without a warning. `--initrd` is opened and
checked even without `--kernel`, and then not used.

ACPI tables go to the guest through the fw_cfg table loader and also at
fixed guest addresses. SMBIOS tables go through fw_cfg and at 0xF0000.
The SMBIOS Type 1 defaults are manufacturer `Joyent` and product
`SmartDC HVM`. `-B` changes them.

### UEFI (rshyve)

```
-l bootrom,<rom>[,<varfile>]
-l bootrom,uefi        # /usr/share/bhyve/uefi-rom.bin
-l bootrom,bios        # /usr/share/bhyve/uefi-csm-rom.bin
```

With no `--kernel` and no bootrom argument, rshyve uses
`/usr/share/bhyve/uefi-rom.bin`. A second field that contains `=` is a
bhyve legacy option and is ignored. The first bootrom argument wins, and
rshyve warns about a second one.

The variable store is emulated CFI flash, mapped just below the ROM. The
file is mapped shared and locked, so two VMMs cannot use it. rshyve
checks the firmware-volume GUID and signature, unless the file is all
0xFF (erased). It writes the file back on teardown and on a durable
flush. [firmware-edk2-bhyve.md](firmware-edk2-bhyve.md#use-with-rshyve)
tells how to manage variable stores.

### Example command lines

rshyve, a UEFI guest with a VNC console:

```
rshyve -c 4 -m 8G \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin,/zones/vm0/vars.fd \
  -l com1,socket,/var/run/rshyve/vm0/console.sock \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/vm0/disk0,num-queues=4 \
  -s 5,ahci-cd,/zones/vm0/install.iso,bootindex=1 \
  -s 6,virtio-net-viona,vm0_net0 \
  -s 30:0,fbuf,unix=/var/run/rshyve/vm0/vnc.sock,w=1280,h=1024 \
  -s 30:1,xhci \
  --control-socket /var/run/rshyve/vm0/control.sock \
  vm0
```

firehyve, a direct-boot microVM:

```
firehyve -c 2 -m 512M \
  --kernel /var/lib/firehyve/vmlinux \
  --initrd /var/lib/firehyve/initramfs.cpio \
  --cmdline "console=ttyS0 root=/dev/ram0 panic=-1" \
  -l com1,stdio \
  -s 4,virtio-blk,/var/lib/firehyve/rootfs.img,ro \
  -s 7,virtio-net-viona,fh0 \
  -s 12,virtio-fs,/export/share,tag=share,ro \
  -s 15,virtio-console,/var/run/fh/console0.sock \
  fh0
```

## Storage

### virtio-blk (both)

```
-s <slot>,virtio-blk,<path>[,ro][,nodelete][,sectorsize=N][,num-queues=N]
```

- Defaults: writable, 512-byte logical blocks, and one queue per vCPU,
  at most 4.
- `sectorsize=N`: a power of two from 512 to 65536. It sets the logical
  block size that the guest sees (`VIRTIO_BLK_F_BLK_SIZE`). Capacity is
  always in 512-byte sectors.
- `num-queues=N`: 1 to 8. More than one queue offers `VIRTIO_BLK_F_MQ`.
- An out-of-range or unparsable `sectorsize` or `num-queues` is ignored
  with no message.
- `ro`: the file is opened read-only, `VIRTIO_BLK_F_RO` is set, a write
  gets an I/O error, and neither DISCARD nor WRITE_ZEROES is offered.
- A writable disk offers WRITE_ZEROES, which writes zeros, and DISCARD,
  which the device validates and completes with no I/O. `nodelete`
  removes DISCARD. A DISCARD request then gets `VIRTIO_BLK_S_UNSUPP`.

The backing store is a plain file: a raw image, a zvol device or an ISO.
The capacity comes from `fstat`, so a device whose size reads as 0 is a
zero-size disk. The transport is transitional virtio PCI with 128
entries per ring and one MSI-X vector per queue plus one for config.

8 worker threads serve one queue, or 2 per queue for multiqueue. Every
request that reaches the disk runs on a worker, through a host buffer
([architecture.md](architecture.md#virtio-blk-copies-through-host-memory)).

`VIRTIO_BLK_T_GET_ID` returns `vmm-virtio-blk` for every disk, so a
guest cannot tell two disks apart by serial.

### NVMe (rshyve)

```
-s <slot>,nvme,<path>[,ro]
```

Only `ro` has an effect. `sectorsize=`, `num-queues=` and `nodelete`
parse and are ignored. The logical block size is always 512.

- NVMe 1.0, one namespace (NSID 1), 8 worker threads that read and
  write guest memory directly with `preadv`/`pwritev`.
- MDTS 9 (2 MiB). Up to 15 I/O queue pairs of up to 4096 entries.
- Admin commands: Identify (CNS 0 and 1), Create and Delete I/O SQ and
  CQ, Get and Set Features, Abort (always "not found"), and Get Log Page
  for Error Information (zeros), SMART (a temperature and the spare
  values) and Firmware Slot. Other log pages return Invalid Log Page.
- I/O commands: READ, WRITE, FLUSH. No Dataset Management, Write Zeroes
  or Compare, and `ONCS` is 0.
- A read-only namespace is the tested way to give installation media to
  a UEFI guest. A write to it returns the status code `0x0082` with DNR.
  That value has status code type 0 (generic), so a guest reads it as
  "Namespace Not Ready", not "Attempted Write to Read Only Range",
  which is type 1.
- The serial is `TRITON-NVME-<dev>`, from the PCI device number only.
  Two controllers at different functions of one slot get the same
  serial.

### AHCI CD-ROM (rshyve)

```
-s <slot>,ahci-cd,<iso-path>
```

No options. `bootindex=N` is removed before this parser sees the spec.

- ATAPI on an Intel ICH8 AHCI controller (8086:2821). One port and one
  ISO. The file must be 2048 bytes or larger.
- No media change. INTx only.
- Reads run on the vCPU thread, so a slow ISO read stalls that vCPU.
  Maximum transfer 1 MiB, 512 PRD entries.
- Commands: TEST UNIT READY, REQUEST SENSE, INQUIRY, START STOP UNIT,
  PREVENT ALLOW MEDIUM REMOVAL, READ CAPACITY, READ(10), READ(12),
  READ TOC (formats 0 and 1), GET EVENT STATUS NOTIFICATION, MODE
  SENSE(10) pages 0x01 and 0x2A, REPORT LUNS. Others get ILLEGAL
  REQUEST.

### zvol write cache

For a writable virtio-blk or NVMe backing store, the VMM tries
`DKIOCGETWCE`/`DKIOCSETWCE` (`crates/vmm-devices/src/blkdev.rs`) and puts
the old value back when the device goes away. Regular files are skipped.
`ro` disks and `ahci-cd` do not use it.

## Network

```
-s <slot>,virtio-net-viona,<vnic>[,promiscphys]
```

This is the only network driver. `promiscphys` or `promiscphys=true`
turns on promiscuous mode. Other options, `mtu=` included, are ignored.

The host needs `/dev/viona`, `libdladm` and a VNIC that already exists.
Neither binary creates or configures a VNIC. The MAC address comes from
the VNIC. If the VNIC lookup fails, the device does not attach.

- Two queues (RX and TX) of 256 entries, 3 MSI-X vectors, and one
  `viona-intr-poll` thread per device.
- EVENT_IDX is not offered: viona suppresses notifications in the
  kernel.
- Link status is always up.
- No multiqueue, no MTU setting and no control queue. The viona
  interface version is not checked, so a mismatched kernel module shows
  as an ioctl error.

## Serial and console

### LPC UARTs

COM1 is a 16550 at 0x3F8, IRQ 4. COM2 is at 0x2F8, IRQ 3.

rshyve backends (`bin/rshyve/src/serial.rs`):

| Form | Behavior |
|---|---|
| absent, or `stdio` | TX to stdout, a thread reads stdin |
| `socket,<path>` | rshyve listens on a Unix socket, mode 0600 in a 0700 directory. One client at a time |
| any other value | opened as a character device (`/dev/zconsole` on a zone) |

On the socket backend, guest output goes into an 8 KiB queue, and a
writer thread sends it with a 200 ms budget. A client that stops reading
loses bytes and then its connection. It does not stall a vCPU. Guest
output with no client is dropped.

Only `bootrom`, `com1` and `com2` are read from `-l`. `-l com3,...` or a
misspelled device name is ignored with no message.

rshyve runs its metadata agent on COM2 when no `-l com2` is given
([Metadata agent](#metadata-agent-rshyve)).

firehyve serves COM1 on stdio, or on an absolute device path such as
`/dev/zconsole`. COM2 is always present, transmit only, and copied to
stdout. `fhrun-init` writes its markers there, outside the kernel
console.

### virtio-console (both)

```
-s <slot>,virtio-console,<unix-socket-path>
```

- No options. Any field after the path is an error. A path that holds a
  comma cannot be given.
- One port. No multiport, no size reporting. The guest device is
  `/dev/hvc0`.
- One client at a time. A new connection replaces the old one.
- Guest output with no client is discarded. Host input waits, up to
  64 KiB, for a guest receive buffer. Past that it is dropped with one
  warning.
- A write to the client has 250 ms. A client that stalls longer loses
  the console.

### virtio-vsock (firehyve)

```
-s <slot>,virtio-vsock,<unix-socket-path>,cid=<n>
```

`cid=` is required, 3 or more. Any other option is an error. A host
peer connects to the Unix socket. `CONNECT <port>` joins it to a guest
port. `CONTROL` gives it the hotplug commands
([cli.md](cli.md#firehyve-control-verb)). A guest that connects to host
port `P` reaches the Unix socket `<path>_P`. virtio-vsock cannot be
hot-added.

## Shared filesystem (virtio-fs)

```
-s <slot>,virtio-fs,<host-dir>[,tag=NAME][,ro][,queue-size=N]
```

- `tag` defaults to the last component of the directory and is at most
  36 bytes.
- `queue-size` defaults to 128. It must be 8 to 1024, and it is rounded
  up to a power of two: `queue-size=100` gives 128.
- An unknown option is an error.
- In a Linux guest: `mount -t virtiofs <tag> /mnt`.

One worker thread serves the high-priority queue and the request queue,
so the high-priority queue has no real priority. No DAX: every reply
goes through the request queue.

Opcodes: INIT, DESTROY, LOOKUP, FORGET, BATCH_FORGET, GETATTR, STATFS,
ACCESS, READLINK, OPEN, RELEASE, READ, OPENDIR, RELEASEDIR, READDIR,
READDIRPLUS, FLUSH, FSYNC, FSYNCDIR, WRITE, CREATE, MKDIR, MKNOD, UNLINK,
RMDIR, RENAME, RENAME2, SETATTR, SYMLINK, LINK. GETXATTR and LISTXATTR
return ENOTSUP. Others return ENOSYS. FLUSH and DESTROY do nothing.
Limits: max_write 512 KiB, max_background 64, and 1 MiB for one request
body.

Path handling (`crates/vmm-virtio/src/fs/passthrough/`):

- `validate_name` refuses an empty name, `.`, `..`, a `/` and a NUL
  with EINVAL. Every operation that takes a name calls it.
- The share root is an fd opened with `O_DIRECTORY|O_NOFOLLOW`. Each
  access is an `*at()` call with `O_NOFOLLOW`, except OPEN, which
  reopens an fd that the VMM already holds through `/proc/self/fd`.
  Host symlinks are never followed.
- LOOKUP admits regular files, directories and symlinks only.
- The tables of inodes, handles and directory data have caps. A guest
  past a cap gets ENFILE or EMFILE.

`ro` returns EROFS for CREATE, MKNOD, MKDIR, UNLINK, RMDIR, RENAME,
RENAME2, SYMLINK, LINK, SETATTR, WRITE, and an OPEN for write. OPEN with
`O_TRUNC` is refused too, even with `O_RDONLY`.

**A writable share gives the guest the VMM's credentials.** Every file
operation runs as the VMM's user, which is root in a bhyve zone. There
is no uid or gid mapping. SETATTR passes the guest's uid, gid and mode
through, setuid, setgid and sticky bits included. So a guest can make a
setuid-root file in the share, and a host user who can reach the share
can run it. Export read-write only to a guest that you trust with root
on that tree (OSF-10).

`FUSE_ACCESS` ignores its mask and grants access when the node can be
stat'd.

## Display and input (rshyve)

### fbuf and VNC

```
-s <slot>[:<func>],fbuf[,vga=off][,unix=<path>][,password=<pw>|password-file=<path>][,w=<n>][,h=<n>]
```

- Parser: `parse_fbuf_config` in `bin/rshyve/src/pcidev.rs`. Any other
  option, `vga=on`, `tcp=`, `rfb=` and `wait` included, is an error.
  `vga=off` is accepted and does nothing. There is no VGA emulation.
- Defaults: socket `/var/run/rshyve/<vm>/vnc.sock`, no password,
  1024x768. `w` and `h` must be 1 to 65535.
- `password=` is used as given. Other processes can read it from the
  process arguments, so use `password-file=`.
- PCI ID FB5D:40FB, the same as C bhyve, so the bhyve GOP driver in the
  firmware binds. BAR1 is a 16 MiB XRGB8888 framebuffer in a memory
  segment mapped into the guest. Each fbuf uses one of the VM's memory
  segments.
- The guest can change the resolution through BAR0, up to 2048x2048. A
  resolution from the command line is not checked against the BAR, so
  a size that is too large gives a cut-off picture.
- The RFB server speaks protocol 3.8 and sends XRGB8888 as one
  full-screen Raw rectangle. It sends DesktopSize when the client
  supports it. A client that does not gets updates clipped to its
  first size. `SetPixelFormat` is ignored.
- The frame loop runs every 33 ms and reads one client message per
  pass, so input is limited to about 30 events a second. One client at
  a time.
- Authentication is legacy RFB VncAuth. With no password, only "None"
  is offered. With a password, only VncAuth is offered. DES uses the
  first 8 bytes of the password. No encryption and no rate limit
  (OSF-7). The socket is 0600 in a 0700 directory, with no peer
  credential check.
- Clipboard text is discarded.

### xHCI tablet

```
-s <slot>[:<func>],xhci
```

The config field is not read: `xhci,tablet`, `xhci,keyboard` and `xhci`
all give one absolute-pointer tablet.

- Intel 8086:1E31, BAR0 8 KiB, INTx only.
- `HCSPARAMS1` says 64 slots, but only one is implemented.
- Two ports (USB 2 empty, USB 3 with the tablet), one interrupter, EP0
  and EP1 IN.
- 3 buttons, 16-bit absolute X and Y. No boot protocol. The wheel field
  is always 0.

### PS/2

rshyve adds a PS/2 controller with a keyboard to every VM
(`bin/rshyve/src/input.rs`). Ports 0x60 and 0x64, IRQ 1 and 12. VNC key
events reach the guest only through it: there is no USB keyboard. A
keysym with no scan code is dropped. Scan code set 3 is not
implemented. The PS/2 mouse answers its commands and never reports
motion.

## vTPM, CPUID, Hyper-V

### vTPM (rshyve)

```
rshyve --vtpm [--vtpm-state-dir <PATH>] <vm_name>
```

- TPM 2.0 on the vendored libtpms, in process. The guest sees a TCG PTP
  CRB at 0xFED40000, locality 0 only, with a 3968-byte command buffer.
  A longer command gets `TPM_RC_COMMAND_SIZE` and does not run.
- No interrupts: the guest polls. `TPM2_Cancel` is ignored.
- A command runs in the vCPU's MMIO handler, so a slow command, such as
  an RSA key generation, stops that vCPU.
- ACPI: a TPM2 table (start method 7, CRB) and a DSDT `MSFT0101`
  device. No event log.
- NV state is in `/var/db/rshyve/<vm>/tpm` by default and survives a
  restart. Writes go to a temporary file, fsync, then rename.
- The state directory gets the process umask, not a fixed mode.
- A wrong or empty `--vtpm-state-dir` is not an error: libtpms then
  makes a new TPM, and a BitLocker guest loses its keys with no message.
- One vTPM per process.
- The vendored libtpms is not affected by CVE-2026-6726 or
  CVE-2026-6727. The build fails if libtpms would compile its own RSA
  decrypt code. See [THIRD_PARTY.md](../THIRD_PARTY.md).

### `--cpu-baseline` (both)

Values, case-insensitive (`crates/vmm-core/src/cpuid.rs`): `host` or
`native`, `avx2` or `no-avx512`, `sse42`, `sse4.2` or `westmere`. The
default `host` changes nothing. An unknown value is a startup error.
There is no per-bit override.

- `avx2` clears AVX-512 and AMX in leaf 7, and their leaf 0xD
  components.
- `sse42` also clears AVX, FMA, F16C, RDRAND, BMI1, AVX2 and BMI2.
- Both clear AVX-VNNI, AVX-512_BF16 and AVX-IFMA in leaf 7 subleaf 1.
- `sse42` keeps the SSE4.2 and POPCNT bits and does not change leaf
  0x80000001.

### Hyper-V enlightenments (rshyve)

`--hyperv [--tsc-freq-hz <HZ>]` gives Tier 1, all or nothing.

- CPUID leaves 0x40000000 to 0x40000006, vendor `Microsoft Hv`. Leaf
  0x40000006 is all zeros.
- MSRs: GUEST_OS_ID, HYPERCALL, VP_INDEX, RESET, TIME_REF_COUNT,
  REFERENCE_TSC, CRASH_P0 to P4 and CRASH_CTL. A NOTIFY write logs the
  bugcheck parameters.
- The hypercall page returns `HV_STATUS_INVALID_HYPERCALL_CODE` for
  every call. That is enough for the Windows boot check.
- No SynIC, synthetic timers, virtual APIC, or TLB-flush and IPI
  hypercalls.
- If the host TSC frequency is unknown and `--tsc-freq-hz` is absent,
  the reference TSC is off, and both timing MSRs inject #GP.
- Not tested with an installed Windows workload.

## PCI passthrough (rshyve)

`-s <slot>,passthru,/dev/pptN` gives the guest a real PCI function
through the illumos PPT driver (`crates/vmm-passthru`). Passthrough is
not fully audited (OSF-5) and has not run with an assigned device
(OSF-11).

**Intel VT-d only.** The illumos kernel opens `misc/vmm_vtd` on Intel
and `misc/vmm_amdvi` on AMD, and illumos builds only `vmm_vtd`. On an
AMD host, passthrough does not work.

**A device that advertises MSI-X is refused**, with "device advertises
MSI-X, which this VMM cannot isolate". An MMIO BAR is mapped into the
guest whole, and on an MSI-X device that BAR holds the MSI-X table.
illumos VT-d remaps DMA but not interrupts. So a guest that wrote the
table could send any interrupt vector to the host APIC. Most modern PCIe
devices advertise MSI-X. MSI and INTx devices attach.

**Config-space writes are denied by default.** `classify_cfg_write`
admits the command register, the six BARs, the interrupt line, the
header-type byte and the MSI capability, and drops other writes with a
warning. The illumos PPT driver does not filter them. The MSI
capability is a shadow: guest writes never reach the device, and only
the shadow values go to `VM_PPTDEV_MSI`. Multiple Message Enable is
limited to the kernel's `msi_limit`.

I/O BARs are registered on the PIO bus and relayed to the device. An
MMIO BAR smaller than a page is refused. rshyve does not add `-S` for
passthrough.

## Control socket (rshyve)

`--control-socket <PATH>`: newline-delimited JSON, one response line
per request, except `metrics-prometheus`, which returns several lines.
There are 21 commands (`bin/rshyve/src/control/protocol.rs`); see
[cli.md](cli.md#control-socket-commands).

- Access: the peer's uid must be the VMM's effective uid, and on illumos
  its zone must be the VMM's zone or the global zone. There is no token
  and no per-command check.
- The socket is 0600 in a 0700 directory. rshyve checks the mode and
  owner after bind and removes the socket if they are wrong.
- Limits: 8 connections, 64 KiB per request line, 30 s to read, 10 s to
  write.
- `pause` and `resume` quiesce the devices with a 5 s budget. On
  timeout, the devices resume, the state goes back to running, and the
  error names the stuck devices.
- `shutdown`, `reset` and `stop` are `VM_SUSPEND_POWEROFF`,
  `VM_SUSPEND_RESET` and `VM_SUSPEND_HALT`. They do not stop the devices
  first.

## Live migration (rshyve)

[migration.md](migration.md) describes the protocol, the state that
migrates, what refuses a migration, and the contract for an
orchestrator. This repository has no orchestrator.

## Hotplug (both)

Hotplug is off unless `--hotplug` is given. Without the flag, no hotplug
I/O port is claimed and no hotplug AML is generated.

With `--hotplug`, the VMM claims these port blocks:

| Block | Port | Length | When |
|---|---|---|---|
| GPE0 status and enable | `0xAFE0` | 4 | always |
| PCI slots (`PCIU`, `PCID`, `B0EJ`) | `0xAE00` | 12 | always |
| CPU slots | `0xAF00` | 12 | when `maxcpus` is more than the boot CPU count |
| Memory slots | `0x0A00` | 0x18 | always |

GPE bit 1 runs `_E01` (PCI), bit 2 `_E02` (CPU) and bit 3 `_E03`
(memory). Both binaries use the same engines
(`crates/vmm-machine/src/hotplug.rs`). rshyve takes the commands on
`--control-socket`. firehyve takes them through the vsock `CONTROL`
verb, so a firehyve VM with `--hotplug` and no `virtio-vsock` device has
no way to send them.

### PCI hot-add and hot-remove

`device-add <slot,driver[,config]>` takes the `-s` grammar and builds
the device with the same catalog as the command line, so a control peer
can name no driver and no path that the command line could not.

- Bus 0, function 0, slots 2 to 31. The VMM checks that the slot is free
  before it opens a file or takes an MSI-X vector.
- The answer is the id `driver@slot`, which `device-remove` takes.
- `hostbridge` and `lpc` are refused. In rshyve, an unknown driver is
  refused here, not skipped. firehyve refuses `virtio-vsock`, `fbuf` and
  `xhci`.
- Only `virtio-blk` has been hot-added in a test. The other drivers are
  reachable, not tested.

`device-remove <id>` is a request. It marks the slot `remove-pending`,
raises the eject event and returns. The device goes only when the guest
runs `_EJ0`, which a thread checks every 100 ms. A guest that never runs
`_EJ0` leaves the slot in `remove-pending` for the life of the VM, and
`device-list` shows it. The VMM also keeps the device when:

- the guest ejects it with no operator request (the slot is offered
  back);
- it does not quiesce in 5 s (it resumes);
- it is `passthru`, which can be added and never removed.

### CPU hot-add

Needs `--hotplug` and `-c cpus=N,maxcpus=M` with `M` more than `N`.
`maxcpus` less than the boot count, or more than 64, is a startup error.

`cpu-add <id>` activates the vCPU, starts its thread and raises the CPU
event. The guest brings the CPU online itself. If an add fails after
`VM_ACTIVATE_CPU`, that id is spent for the life of the VM, because
illumos cannot deactivate a vCPU. `cpu-list` shows it as `consumed`.

### Memory hot-add

Needs `--hotplug`, `-o hotplug.maxmem=SIZE`, and reservoir memory (`-S`
or `-o memory.use_reservoir=true`). The VMM checks all three before it
creates the VM. A hot-add runs `VM_ALLOC_MEMSEG` on a live VM, which
stops every vCPU until it returns: 233 to 270 ms per GiB added in one
test, with reservoir memory. Without reservoir memory, the kernel also
creates and zeroes every page while the vCPUs wait. That work alone
measured about 400 ms per GiB at VM create.

- `-o hotplug.memslot=SIZE` sets the slot size. The default is 128 MiB.
  There are 8 slots, the slot size must divide `maxmem`, and a window
  that needs more than 8 slots is refused.
- The window is above all boot RAM, so it is above 4 GiB.
- `mem-add <bytes>` rounds up to a whole slot.
- A VM has 5 kernel memory segments and boot uses some of them, so the
  number of adds can be less than the slot count. A UEFI VM with a
  framebuffer and more than 3 GiB has one add left. A direct-boot VM has
  three. The engine reports `NoSegment`.

### What hotplug cannot do

`cpu-remove` and `mem-remove` are always refused, with the reason:
illumos has no `vm_deactivate_cpu` and no `VM_FREE_MEMSEG`.

A VM that hot-added a CPU, memory or a PCI device, or that has a slot
waiting for removal, cannot be a migration source. The destination is
built from the boot command line. `migrate-config` lists the added
devices in `pci_slots`, but `migrate-source` still refuses.

The hotplug AML is not checked with `iasl`, and no CI job boots a
guest. Windows, illumos and BSD guests have not been tried.

## Metadata agent (rshyve)

A host thread serves the SmartOS metadata protocol on COM2 when no `-l
com2` backend is given. [mdata.md](mdata.md) gives the protocol and the
keys. On a SmartOS node, the brand passes `-l com2,socket,<path>`, and
the metadata agent in the global zone serves the guest instead.

## fhrun

`fhrun <manifest.json>` boots a firehyve microVM around a Linux binary
and exits with the binary's status. [fhrun.md](fhrun.md) gives the
manifest, the emitted command line and the exit codes.

- The kernel command line starts with `console=ttyS0 earlyprintk=ttyS0
  root=/dev/ram0 init=/init rdinit=/init panic=-1 fhrun=1 tsc=reliable`.
  A manifest can append to it only.
- The manifest cannot ask for a disk, a virtio-fs share or an RNG.
- A NIC `mac` is checked for form and not used: the VNIC sets the
  address.
- `fhrun-init` is a separate Cargo workspace for
  `x86_64-unknown-linux-musl`. It forks the payload, reaps it, and
  reports its status on COM2.

## Limits

| Area | Limit |
|---|---|
| PCI addresses | bus 0 only, dev 0 to 31, func 0 to 7 |
| Chipset slots | 0 (host bridge) and 1 (LPC bridge) |
| Kernel command line | 4095 bytes, and the kernel's `cmdline_size` for a bzImage |
| initrd | below `min(memory size, 3 GiB)` |
| E820 map | PVH: 64 entries, then an error. bzImage: 128 entries, then cut |
| Direct-boot page tables | the first 4 GiB, 2 MiB pages |
| Bootrom file | 4 KiB to 16 MiB, a multiple of 4096. ROM plus variable store at most 16 MiB |
| Variable store file | 4 KiB to 16 MiB, page-aligned, a regular file, locked |
| virtio-blk | 128-entry rings, 1 to 8 queues, 8 workers for one queue, 2 per queue for more |
| NVMe | 1 namespace, 512-byte blocks, 15 I/O queue pairs of up to 4096, MDTS 2 MiB, 8 workers |
| ahci-cd | 1 port, 1 ISO, 512 PRD entries, 1 MiB per transfer |
| viona | 2 queues of 256, 3 MSI-X vectors |
| virtio-console | 2 queues of 128, 64 KiB of host input, 250 ms client write, 1 client |
| virtio-fs | `queue-size` 8 to 1024, tag 36 bytes, max_write 512 KiB, max_background 64, 1 MiB per request |
| fbuf | 16 MiB BAR (2048x2048), 33 ms frame tick, 8-byte password, 1 client |
| xHCI | 64 slots advertised, 1 implemented, INTx only |
| vTPM | locality 0, 3968-byte command buffer, 1 per process |
| Control socket | 8 connections, 64 KiB per line, 30 s read, 10 s write |
| PCI hotplug | slots 2 to 31, 5 s quiesce per eject, eject check every 100 ms |
| CPU hotplug | `maxcpus` at most 64. A failed add spends its id |
| Memory hotplug | 8 slots, 128 MiB each by default, above 4 GiB. 5 kernel memory segments per VM |
| Migration | 64 pages per batch sent (256 accepted), 8 MiB per serialized payload, 5 convergence passes 500 ms apart, 1024-page threshold, 5 s pause quiesce, 120 s ZFS barrier |
| mdata | 4096-byte lines, printable ASCII, 256-byte UART FIFOs |
| fhrun | 4 NICs (slots 7 to 10), 2 consoles (slots 15 and 16) |
| Host | one bhyve VM per non-global zone. A second `VM_CREATE` returns EINVAL |
| Host tunable | `strmsgsz` 8192 or more, or 0 (see [Known caveats](#known-caveats)) |

## Not supported

- **`ahci-hd`.** A startup error: "AHCI device type 'ahci-hd' is not
  implemented; use virtio-blk or nvme".
- **Other emulated NICs**: e1000, rtl8139, vmxnet, tap, netgraph, slirp.
- **virtio-scsi, virtio-9p, virtio-balloon, virtio-input, virtio-gpu.**
- **PCI passthrough on AMD**, and passthrough of an MSI-X device.
  illumos builds no AMD IOMMU module, and its VT-d driver does not remap
  interrupts. No option turns the MSI-X refusal off.
- **CPU and memory hot-remove.** illumos has no `vm_deactivate_cpu` and
  no `VM_FREE_MEMSEG`.
- **qcow2 or any image format other than raw.**
- **Block I/O throttling.**
- **VGA, a VGA BIOS and text mode.**
- **VNC over TCP**, and the bhyve `wait` option.
- **A USB keyboard** and **PS/2 mouse motion.**
- **Clipboard** transfer.
- **Other boot formats**: multiboot, a vmlinux without the PVH note,
  real-mode entry, bzImage protocol before 2.12, 32-bit bzImage.
- **Kernel relocation**, an initrd above 4 GiB, and more than one
  initrd.
- **UEFI in firehyve.**
- **Migration of** FPU/XSAVE state, the UEFI variable store, vTPM state,
  the framebuffer, xHCI, AHCI, virtio-fs, virtio-console and the LPC
  UARTs ([migration.md](migration.md#what-prevents-a-migration)).
- **Migration of disk contents.** Disks move outside the protocol.
- **A migration cancel command**, migration IDs and replay protection.
- **vTPM state export, TPM 1.2, localities 1 to 4, TPM interrupts,
  `TPM2_Cancel`, and a TCG event log.**
- **Real Hyper-V hypercalls, SynIC, synthetic timers and a virtual
  APIC.**
- **Per-bit CPUID overrides.** Only the `--cpu-baseline` profiles.
- **A host other than illumos, or an architecture other than x86_64.**
  CI builds and runs the unit tests on Linux. Only illumos runs a VM.
- **A prebuilt platform image, a release or an installer.**

### Accepted and then ignored

rshyve refuses `-D`, `-k` and `--json-config`, so that a flag in
`bhyve_extra_opts` gives an error, not a VM that is different from the
request. The instance is removed when the process exits in any case,
because the VMM sets the kernel autodestruct flag on every VM.

These parse and have no effect:

| Input | What happens |
|---|---|
| `-o <key>`, except `hotplug.maxmem`, `hotplug.memslot` and `memory.use_reservoir` | Ignored. `-o` is a key space that vmadm also uses. A misspelled `hotplug.` key is an error |
| `-A` | Ignored. ACPI tables are always built |
| `nvme,...,sectorsize=N`, `num-queues=N`, `nodelete` | Ignored. NVMe blocks are 512 bytes |
| `virtio-blk,...,<unknown option>` | Ignored |
| `fbuf,...,vga=off` | Ignored. Other `vga=` values are errors |
| `xhci,<anything>` | The config is not read |
| `passthru,<path>,<anything>` | Only the path is used |
| `virtio-rnd,<anything>` | The config is not read |
| `-s 7,hostbridge`, `-s 31,lpc` | The slot is ignored. The chipset uses 0.0.0 and 0.1.0 |
| `-l com3,...`, any `-l` device other than `bootrom`, `com1`, `com2` | Ignored, no message |
| `-l bootrom,...` with `--kernel` (rshyve) | Ignored, no message |
| `--cmdline` with UEFI boot (rshyve) | Only the direct-boot loader reads it |
| A misspelled `-s` driver (rshyve) | A warning. The VM boots without the device |

## Known caveats

**SIGTERM stops the VM without asking the guest.** Both binaries turn
SIGTERM into `VM_SUSPEND_POWEROFF`, which stops every vCPU at once. The
guest gets no ACPI power-button event and cannot flush its caches. If
the VM does not stop in time, the VMM halts it. C bhyve sends an ACPI
power-button event instead. To shut a guest down cleanly, power it off
from inside the guest.

**Live migration is experimental, and its stream has no authentication
and no encryption** (OSF-1). Guest RAM, all vCPU registers and MSRs, and
device state cross the wire in clear text, with no peer identity and no
replay protection. The destination resumes what the first peer sends.
`--migrate-listen` with a TCP address binds any address that it gets,
`0.0.0.0` included, with no check. Migrate only over a channel that is
authenticated by other means, ideally an SSH-forwarded Unix socket so
that the VMM never listens on TCP. Migration also does not carry
FPU/XSAVE state (OSF-2).

**A writable virtio-fs share is root on that tree.** See
[Shared filesystem](#shared-filesystem-virtio-fs).

**The CPU check refuses a feature gap by default.** The destination
compares CPUID leaf 1 ECX and EDX, leaf 7 EBX, ECX and EDX, and XCR0,
after each end's `--cpu-baseline` mask. `migrate-dest` with
`allow_cpu_feature_mismatch` accepts a gap for one migration, and the
guest then takes #UD on the first missing instruction.
`--migrate-listen` always refuses.

**A vTPM state directory and a UEFI `VARS.fd` are one unit of VM
state.** If you lose one, the other is useless. Nothing enforces this.
Back up both together.

**`-S` means reservoir memory, not wired memory.** It sets
`VCF_RESERVOIR_MEM`, as `-o memory.use_reservoir=true` does. Guest
memory is always wired. Size the host reservoir first (`rsrvrctl`), or
VM create with `-S` fails with ENOMEM.

**Both binaries destroy a VM instance with the same name at startup.**
Do not reuse the name of a live VM.

**No minimum bhyve API version is enforced.** The VMM logs the version
and continues. The code needs v5 (CPUID control), v8 (`VCF_TRACK_DIRTY`),
v11 to v13 (the vmm-data reads and writes of a migration) and v16 (the
triple-fault vCPU in the suspend detail). Only v18 has been tested. The
viona interface version is not checked.

**A vmm_drv lease that does not break can keep the process alive.**
Teardown ends in `VM_DESTROY_SELF`, which waits in an untimed
`cv_wait` in `vmm_lease_block` until every vmm_drv lease is released.
viona releases its lease when a ring worker next checks for it. The VMM
runs the destroy on its own thread with a 15 s budget, logs `the VM
destroy did not return inside its budget`, and then the shutdown
watchdog calls `_exit(4)`. That exit usually frees the process: exit
sets `SEXITING`, and the viona workers see it and release the lease. It
does not when a worker cannot reach that check, for example inside the
MAC perimeter or in `viona_tx_wait_outstanding`. That wait is reachable
only with guest-memory loaning on, which illumos turns off by default
(`viona_default_tx_copy`), and the VMM does not change it. In that case
no signal ends the process, and only a kernel change or a node reboot
frees the guest memory ([roadmap.md](roadmap.md#blocked-on-the-illumos-kernel)).

**Bounded socket writes need `strmsgsz` 8192 or more.** virtio-console,
the vsock host socket and the firehyve `CONTROL` reply write with a time
budget. illumos accepts `SO_SNDTIMEO` on an AF_UNIX socket and then
ignores it, so the VMM waits for `POLLOUT` and sends at most 8 KiB
(`crates/vmm-core/src/unixsock.rs`). The stream head sends that as one
message if `strmsgsz` is 8192 or more (default 65536, or 0 for no
limit). Below 8192 the write splits, and the wait between the parts has
no timeout ([platform.md](platform.md#host-tunables)). A macOS or Linux
test pass proves nothing here: macOS honours `SO_SNDTIMEO`.

**A device reset has no upper bound on illumos.** A reset ends when the
interrupt injections already in flight finish (`IntrGate::settle`,
`crates/vmm-virtio/src/pci/intr.rs`). Both injection ioctls take the
bhyve VM read lock, and illumos gives the lock to a waiting writer
first. So an operation that holds the VM write lock delays every reset
on that VM: memory hot-add, `VM_PAUSE`, `VM_RESUME` and VM-wide
device-state export. In one test, `VM_ALLOC_MEMSEG` held the lock
69 ms for a 256 MiB hot-add and 233 to 270 ms per GiB, so a 16 GiB hot-add
delays a reset for seconds. The reset finishes when the lock is
released. The illumos virtio driver resets a device with one status
write and no read-back, so an illumos guest stops inside that write for
the whole delay. A kernel change could bound it
([roadmap.md](roadmap.md#blocked-on-the-illumos-kernel)). Until then,
do not hot-add memory to a VM whose guest can reset a device.

**Some tests do not run on illumos, and some run only there.** The
control socket's peer-credential test (`bin/rshyve/src/peercred.rs`)
runs on Linux and macOS only. CI runs it. The autodestruct test
(`crates/vmm-core/tests/autodestruct.rs`) runs on illumos only, so CI
does not run it. `tools/check-test-parity.py` fails CI when the set of
platform-gated tests changes.

**Dirty page tracking is on by default in rshyve.** `--no-track-dirty`
turns it off. The VMM asks for `VCF_TRACK_DIRTY` at VM create and does
not check first whether the host supports it.

**No part of the VMM starts an async runtime.** A bhyve-brand zone has
no `/dev/poll`. Migration from inside a zone should use a Unix socket
bridged by a global-zone agent. TCP migration from inside a zone is not
tested.

**CI does not boot a guest.** CI checks formatting, clippy on every
crate, the tests in debug and release on Linux (the binaries and the
vTPM crates included), an illumos type-check, the lockfile, the
dependency and public-surface policies, and the repository gates. It
does not run a VM or a platform test. USDT probes do nothing on the
Linux runner. `tools/check-needed.sh` runs on the build host. CI runs
only its self-test, so CI never checks a real illumos binary.

**The MSRV (1.94) is not checked for the binaries.** The MSRV job leaves
out `rshyve`, `firehyve`, `vmm_tpm`, `vmm_tpm_sys`, `bhyve_api`,
`bhyve_api_sys` and `viona_api`. There is no `rust-toolchain.toml`, and
`tools/fhrun-init` has no `rust-version`.

**Build on a platform image no newer than the oldest node you deploy
to.** `tools/check-needed.sh` limits the recorded platform OpenSSL
version (`MAX_OPENSSL_SMARTOS`), because `ld.so.1` refuses a binary
that needs a symbol version the node's library does not have. The
platform OpenSSL link is used only for a native build (host and target
the same) where `/lib/amd64/libcrypto-smartos.so.3` exists. A cross build falls back to
pkg-config, and that gives a binary that no platform image can load
([platform.md](platform.md#openssl-and-the-vtpm)).
