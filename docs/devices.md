# Devices

This page lists the devices and their options.
[features.md](features.md) gives the defaults, the limits and the
caveats of each one.

## PCI devices

Attach a PCI device with `-s <slot>[:<func>],<driver>[,<config>]`.

| Driver | rshyve | firehyve | Type | Description |
|--------|:---:|:---:|------|-------------|
| `hostbridge` | yes | yes | Chipset | i440fx host bridge. The chipset always puts it at 0.0.0 |
| `lpc` | yes | yes | Chipset | ISA/LPC bridge. The chipset always puts it at slot 1 |
| `virtio-blk` | yes | yes | Storage | virtio block device, multiqueue |
| `virtio-blk-pci` | yes | yes | Storage | Another name for `virtio-blk` |
| `virtio-net-viona` | yes | yes | Network | virtio network through the viona kernel driver |
| `virtio-rnd` | yes | yes | Misc | virtio entropy source |
| `virtio-console` | yes | yes | Serial | virtio console on a Unix socket |
| `virtio-fs` | yes | yes | Filesystem | FUSE passthrough of a host directory |
| `virtio-vsock` | no | yes | Socket | vsock streams to a host Unix socket |
| `nvme` | yes | refused | Storage | NVMe 1.0 controller |
| `ahci-cd` | yes | refused | Storage | ATAPI CD-ROM, read-only |
| `ahci-hd` | refused | refused | Storage | Not implemented. A startup error |
| `fbuf` | yes | ignored | Display | Framebuffer with a VNC server on a Unix socket |
| `xhci` | yes | ignored | USB | xHCI controller with an absolute-pointer tablet |
| `passthru` | yes | refused | Passthrough | PCI device passthrough through the PPT driver |

**refused** means a startup error that names the driver. **ignored**
means firehyve accepts the driver at startup, because the bhyve zone
brand adds it to every zone, logs a warning and attaches nothing.

An unknown driver name is different in each binary. firehyve refuses to
start. rshyve logs a warning, skips the device and boots without it, so
a typo in `-s` gives a VM without that disk and no error.

## LPC devices

| Device | Description |
|--------|-------------|
| `bootrom` | UEFI firmware ROM and an optional variable store (rshyve) |
| `com1` | 16550 UART at 0x3F8, IRQ 4 |
| `com2` | 16550 UART at 0x2F8, IRQ 3. rshyve runs its metadata agent here when no backend is given |

## Kernel-emulated devices

The bhyve kernel module emulates these devices. rshyve reads and writes
their state for live migration.

| Device | Description |
|--------|-------------|
| LAPIC | Local APIC, one per vCPU |
| IOAPIC | I/O APIC |
| ATPIC | 8259 PIC pair |
| ATPIT | 8254 PIT |
| HPET | High Precision Event Timer |
| RTC | MC146818 real-time clock |
| PM timer | ACPI power-management timer |

## Storage

### virtio-blk

```
-s 4,virtio-blk,/dev/zvol/rdsk/zones/disk0              # read-write
-s 4,virtio-blk,/dev/zvol/rdsk/zones/disk0,ro           # read-only
-s 4,virtio-blk,/dev/zvol/rdsk/zones/disk0,nodelete     # no DISCARD
-s 4,virtio-blk,/path/to/disk.img,sectorsize=4096       # 4 KiB logical blocks
-s 4,virtio-blk,/dev/zvol/rdsk/zones/disk0,num-queues=4 # four queues
```

- Transitional virtio PCI transport: legacy and modern.
- Options: `ro`, `nodelete`, `sectorsize=N` (a power of two, 512 to
  65536), `num-queues=N` (1 to 8). The default queue count is the vCPU
  count, at most 4. A `sectorsize` or `num-queues` value that is out of
  range is ignored and the default stays. Other options are ignored.
- 128 descriptors per queue.
- A writable disk offers WRITE_ZEROES, which writes zeros to the file,
  and DISCARD, which completes without I/O. `nodelete` removes DISCARD.
  A read-only disk offers neither.
- Each request that reaches the disk goes to a worker thread and through
  a host buffer. [architecture.md](architecture.md#virtio-blk-copies-through-host-memory)
  tells why.

### NVMe (rshyve)

```
-s 4,nvme,/dev/zvol/rdsk/zones/disk0
-s 5,nvme,/path/to/install.iso,ro
```

- NVMe 1.0, one namespace, 512-byte logical blocks.
- 8 worker threads that read and write guest memory directly.
- MSI-X interrupts. Maximum transfer 2 MiB.
- Only `ro` has an effect. `sectorsize=`, `num-queues=` and `nodelete`
  parse and are ignored.

### AHCI CD-ROM (rshyve)

```
-s 5,ahci-cd,/path/to/install.iso
```

One ATAPI port with one ISO, read-only. The device takes no options.

## Network

### virtio-net-viona

The illumos viona driver moves the packets in the kernel. The VMM opens
`/dev/viona`, binds it to an existing VNIC, programs the ring addresses,
forwards queue notifications, and polls for interrupts.

```
-s 6,virtio-net-viona,net0               # basic
-s 6,virtio-net-viona,net0,promiscphys   # promiscuous mode
```

## Serial

### virtio-console

A single-port virtio console. The host end is a Unix socket, mode 0600
in a 0700 directory. The guest sees `/dev/hvc0`.

```
-s 8,virtio-console,/var/run/rshyve/<vm>/console.sock
socat - UNIX-CONNECT:/var/run/rshyve/<vm>/console.sock
```

The config is a socket path and nothing else. One client at a time: a
new connection replaces the old one. Guest output with no client is
discarded. Host input is held, up to 64 KiB, until the guest posts a
receive buffer. There is no multiport and no terminal-size reporting.

### virtio-vsock (firehyve)

```
-s 9,virtio-vsock,/var/run/firehyve/guest.sock,cid=3
```

`cid=` is required and must be 3 or more. A host peer that connects to
the socket and sends `CONNECT <port>` joins a guest port. A peer that
sends `CONTROL` reaches the firehyve hotplug commands
([cli.md](cli.md#firehyve-control-verb)).

## Display (rshyve)

### Framebuffer (fbuf)

A framebuffer with a VNC server on a Unix socket.

```
-s 30:0,fbuf,vga=off,unix=/var/run/rshyve/<vm>/vnc.sock
-s 30:0,fbuf,vga=off,unix=/var/run/rshyve/<vm>/vnc.sock,password-file=/path
-s 30:0,fbuf,vga=off,unix=/var/run/rshyve/<vm>/vnc.sock,w=1920,h=1080
-s 30:1,xhci
```

Options: `unix=`, `password=`, `password-file=`, `w=`, `h=`, and the
literal `vga=off`, which is accepted for bhyve compatibility and does
nothing. Any other option is a startup error. There is no VGA
emulation. Prefer `password-file=`: other processes can read a
`password=` value from the process arguments.

### xHCI tablet

`-s <slot>,xhci` adds a USB controller with an absolute-pointer tablet
for the VNC pointer. The config field is not read, so `xhci,tablet` and
`xhci` are the same.

## PCI passthrough (rshyve)

```
-s 5,passthru,/dev/ppt0
```

Passthrough needs the illumos PPT driver and an Intel VT-d host. illumos
builds no AMD IOMMU module. rshyve refuses a device that advertises
MSI-X, because illumos VT-d does not remap interrupts. It filters guest
writes to config space. Passthrough is not fully audited (OSF-5,
OSF-11). [features.md](features.md#pci-passthrough-rshyve) has the
details.

## Filesystem

### virtio-fs

Exports a host directory to the guest as a FUSE filesystem.

```
-s 12,virtio-fs,/export/shared                   # tag "shared", read-write
-s 12,virtio-fs,/export/data,tag=data,ro         # read-only
-s 12,virtio-fs,/export/data,queue-size=256      # deeper queue
```

Options: `tag=<name>` (default: the last path component, at most 36
bytes), `ro`, and `queue-size=<n>` (8 to 1024, rounded up to a power of
two). An unknown option is an error. In a Linux guest:

```
mount -t virtiofs <tag> /mnt
```

- No DAX and no shared-memory window.
- One worker serves both queues, so the high-priority queue gives no
  priority.
- A writable share gives the guest the VMM's credentials on the
  exported tree (OSF-10). Use `ro` unless the guest is trusted.
- No migration state: a VM with virtio-fs cannot migrate.
