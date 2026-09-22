# CLI reference

```
rshyve   [OPTIONS] <VM_NAME>
firehyve [OPTIONS] --kernel <PATH> <VM_NAME>
```

Both binaries use one flag grammar (`crates/vmm-config`). This page
describes rshyve. [firehyve](#firehyve) lists how firehyve differs.

## bhyve flags

These are the flags that the zone brand's `boot.c` builds.

| Flag | Description |
|------|-------------|
| `-c <N>` | vCPU count, or `cpus=N`. Add `maxcpus=M` for hotplug CPU slots ([Hotplug](#hotplug)). `sockets=`, `cores=` and `threads=` are refused: the VMM programs one socket with one thread per core |
| `-m <SIZE>` | Memory size. A bare number is MiB. Suffixes K, M, G, T |
| `-s <slot,driver[,config]>` | PCI device, repeatable. See [devices.md](devices.md) |
| `-l <device,config>` | LPC device, repeatable: `bootrom`, `com1`, `com2` |
| `-U <UUID>` | VM UUID, for SMBIOS Type 1 and the metadata key `sdc:uuid` |
| `-B <smbios>` | SMBIOS fields: `1,manufacturer=X,product=Y,...` |
| `-H` | Exit the vCPU to the VMM on HLT (`VM_CAP_HALT_EXIT`). The VMM runs the vCPU again at once |
| `-S` | Take guest memory from the VMM reservoir (`VCF_RESERVOIR_MEM`). In C bhyve, `-S` means wired memory, which this VMM does not implement. Guest memory is always wired |
| `-A` | Accepted because `boot.c` passes it. ACPI tables are always built |
| `-o <key=value>` | Config option, repeatable. The VMM reads `hotplug.maxmem`, `hotplug.memslot` and `memory.use_reservoir` (`true` is the same as `-S`). Other keys are ignored. A misspelled `hotplug.` key is an error |

`-D`, `-k` and `--json-config` are C bhyve flags that this VMM refuses.
`-D` would change the lifecycle that the zone brand relies on. `-k` and
`--json-config` name configuration files that nothing here reads.

## Other flags

### Boot

| Flag | Description |
|------|-------------|
| `--kernel <PATH>` | Direct boot of a Linux bzImage or PVH ELF. The VMM reads the format from the file. No UEFI |
| `--initrd <PATH>` | initrd or initramfs |
| `--cmdline <STRING>` | Kernel command line. Default `console=ttyS0 earlyprintk=serial` |
| `--cmdline-base64 <B64>` | The same, in base64, for a zone attribute that cannot hold spaces. Use it or `--cmdline`, not both |

### Migration and control

| Flag | Description |
|------|-------------|
| `--control-socket <PATH>` | Unix socket for [control commands](#control-socket-commands) |
| `--migrate-listen <ADDR>` | Start as a migration destination. `host:port` binds TCP and accepts one connection, with no address check. A path that starts with `/` or ends in `.sock` is a Unix socket that the VMM connects to |
| `--no-track-dirty` | Turn off dirty-page tracking. It is on otherwise, and migration needs it |
| `--cpu-baseline <PROFILE>` | CPU feature mask: `host` (default) or `native`, `avx2` or `no-avx512`, `sse42` or `sse4.2` or `westmere` |

See [migration.md](migration.md).

### Platform

| Flag | Description |
|------|-------------|
| `--vtpm` | Add a TPM 2.0 (CRB at 0xFED40000, TPM2 ACPI table) |
| `--vtpm-state-dir <PATH>` | vTPM NV state. Default `/var/db/rshyve/<vm>/tpm` |
| `--hyperv` | Hyper-V Tier 1 enlightenments |
| `--tsc-freq-hz <HZ>` | Guest TSC frequency for the Hyper-V reference TSC. Default: the host frequency |

### Metadata agent

When the command line gives no `-l com2` backend, rshyve runs a SmartOS
metadata agent on COM2 ([mdata.md](mdata.md)).

| Flag | Description |
|------|-------------|
| `--mdata-nics <JSON>` | Network config, `sdc:nics` format |
| `--mdata-resolvers <JSON>` | DNS resolvers, for example `'["192.0.2.53"]'` |
| `--mdata-ssh-keys <KEYS>` | SSH authorized keys for root |
| `--mdata-root-pw-file <PATH>` | File that holds the root password |
| `--mdata-root-pw <PASS>` | Root password. Other processes can read it from the process arguments. Use `--mdata-root-pw-file` |

### Hotplug

| Flag | Description |
|------|-------------|
| `--hotplug` | Publish the ACPI hotplug interface: the GPE0 block, the PCI, CPU and memory register files, and their AML. Off by default |
| `-c cpus=N,maxcpus=M` | `M` CPU slots, of which `N` boot. CPU hot-add needs `M` more than `N`. `M` is at most 64 |
| `-o hotplug.maxmem=SIZE` | The guest address window for hot-added memory. Needs `--hotplug` and reservoir memory (`-S` or `-o memory.use_reservoir=true`) |
| `-o hotplug.memslot=SIZE` | Size of one memory slot. Default `128M`. It must divide `hotplug.maxmem`, and the window can have at most 8 slots |

Sizes use the `-m` grammar.

A hot-add runs `VM_ALLOC_MEMSEG` on a live VM, and that ioctl stops
every vCPU until it returns. With reservoir memory, one test measured
this stop at 233 to 270 ms per GiB added. Without reservoir memory, the
kernel also creates and zeroes every page while the vCPUs wait. That
work alone measured about 400 ms per GiB at VM create. So memory
hot-add needs reservoir memory. Size the host reservoir first, for
example `/usr/lib/rsrvrctl -c 512 -a <MiB>`.

illumos cannot remove a CPU or memory: it has no `vm_deactivate_cpu` and
no `VM_FREE_MEMSEG`. `cpu-remove` and `mem-remove` are always refused,
with that reason. Device removal is a request: the guest must run
`_EJ0`. A guest that never does leaves the slot in `remove-pending`.

A VM that hot-added anything cannot be a migration source. The
destination is built from the boot command line.

Example, a VM that can grow from 2 to 8 CPUs and add up to 4 GiB:

```bash
rshyve -H -S --hotplug \
  -c cpus=2,maxcpus=8 -m 2G \
  -o hotplug.maxmem=4G -o hotplug.memslot=512M \
  -s 0,hostbridge \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/$UUID/disk0 \
  -s 29:0,virtio-net-viona,net0 \
  -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,stdio \
  --control-socket /var/run/rshyve/growable/control.sock \
  growable
```

That VM claims I/O ports `0xAFE0` (GPE0), `0xAE00` (PCI slots), `0xAF00`
(CPU slots) and `0x0A00` (memory slots). Without `--hotplug` it claims
none of them, and the ACPI tables do not change.

## PCI slot syntax

`[bus:]dev[:func],driver[,config...]`. Only bus 0 exists.

```
-s 4,nvme,/dev/zvol/rdsk/zones/disk0
-s 6,virtio-net-viona,net0,promiscphys
-s 0:5:0,passthru,/dev/ppt0
-s 30:0,fbuf,vga=off,unix=/var/run/rshyve/myvm/vnc.sock
-s 8,virtio-console,/var/run/rshyve/myvm/console.sock
-s 12,virtio-fs,/export/shared,tag=shared,ro
```

Add `bootindex=N` to a disk spec to set the firmware boot order
(rshyve).

## LPC device syntax

```
-l bootrom,/usr/share/bhyve/uefi-rom.bin             # UEFI ROM
-l bootrom,/opt/fw/BHYVE_UEFI_CODE.fd,/path/VARS.fd  # ROM and variable store
-l bootrom,uefi                                      # /usr/share/bhyve/uefi-rom.bin
-l bootrom,bios                                      # /usr/share/bhyve/uefi-csm-rom.bin
-l com1,stdio                                        # serial on stdout and stdin
-l com1,/dev/zconsole                                # serial on a character device
-l com2,socket,/path/to/socket                       # listen on a Unix socket; no mdata agent
```

A varfile cannot be used with `--migrate-listen`.
[firmware-edk2-bhyve.md](firmware-edk2-bhyve.md#use-with-rshyve) tells
how to manage variable stores.

## Control socket commands

With `--control-socket`, send one JSON object per line. Each request
gets one response. It is normally one JSON line with `success` and, on
failure, `error`. `metrics-prometheus` is the exception: it returns
Prometheus text on several lines.

```bash
echo '{"command":"status"}' | socat - UNIX-CONNECT:/var/run/rshyve/myvm/control.sock
```

| Command | Fields | Effect |
|---------|--------|--------|
| `status` | | VM state, name, vCPU count, memory, uptime |
| `pause`, `resume` | | Pause or resume the devices and the vCPUs |
| `shutdown` | | `VM_SUSPEND_POWEROFF` |
| `reset` | | `VM_SUSPEND_RESET` |
| `stop` | | `VM_SUSPEND_HALT` |
| `metrics` | | Counters as JSON |
| `metrics-prometheus` | | Counters in Prometheus text format, on several lines |
| `migrate-source` | `target_addr`, optional `zfs_barrier` | Start a migration to a Unix socket path or a `host:port` |
| `migrate-dest` | `listen_addr`, optional `allow_cpu_feature_mismatch` | Take a migration into this paused VM, over TCP |
| `migrate-status` | | Migration phase and progress |
| `migrate-config` | | The configuration a destination needs |
| `device-list`, `device-add`, `device-remove` | `spec`, `id` | PCI hotplug |
| `cpu-list`, `cpu-add`, `cpu-remove` | `id` | CPU hotplug |
| `mem-list`, `mem-add`, `mem-remove` | `bytes` | Memory hotplug |

`shutdown`, `reset` and `stop` do not ask the guest, and they do not
stop the devices first. `migrate-source` and `migrate-dest` refuse a
loopback address, the wildcard address and port 0.
[migration.md](migration.md#control-commands) has the migration
details.

### Hotplug commands

`device-add` takes the same `slot,driver[,config]` spec as `-s` and
returns the id that `device-remove` needs. `device-remove` asks the
guest. The device goes when the guest runs `_EJ0`, and `device-list`
shows `remove-pending` until then.

```bash
echo '{"command":"device-list"}' | socat - ...
echo '{"command":"device-add","spec":"5,virtio-blk,/dev/zvol/rdsk/zones/d1"}' | socat - ...
echo '{"command":"device-remove","id":"virtio-blk@5"}' | socat - ...

echo '{"command":"cpu-list"}' | socat - ...
echo '{"command":"cpu-add","id":2}' | socat - ...

# bytes, rounded up to a whole hotplug.memslot
echo '{"command":"mem-list"}' | socat - ...
echo '{"command":"mem-add","bytes":536870912}' | socat - ...
```

A request for an engine that the VM was not started with is refused,
with the missing flag. A hot-add needs a running VM: a paused or
migrating VM answers with its state.

## firehyve

firehyve is a microVM over the same crates, with a much smaller device
list. It uses the same flag grammar. A flag that would change the guest
is refused at startup. The items that the bhyve zone brand puts on
every zone are accepted, logged and not used, because a refusal would
stop an unmodified brand from starting the binary.

| Item | firehyve |
|------|----------|
| `--kernel` | Required. There is no UEFI. `-l bootrom,<rom>`, which `boot.c` adds to every zone, is logged and ignored |
| `-l com1` | `stdio`, or an absolute device path such as `/dev/zconsole`, served in both directions so `zlogin -C` works. Another backend falls back to stdio, with a warning |
| `-l com2` | COM2 is always present, transmit only, copied to stdout. A `com2` backend is logged and ignored. There is no metadata agent |
| `-s <slot>,<driver>` | `virtio-blk` (and `virtio-blk-pci`), `virtio-net-viona`, `virtio-rnd`, `virtio-console`, `virtio-fs` and `virtio-vsock` attach. `hostbridge` and `lpc` name what the chipset already made. `fbuf` and `xhci` are logged and not attached. `nvme`, `ahci-*`, `passthru` and unknown drivers are refused |
| `-s ...,bootindex=N` | Refused. A direct boot has no firmware boot order |
| `--migrate-listen`, `--control-socket` | Refused |
| `--vtpm`, `--vtpm-state-dir`, `--hyperv`, `--tsc-freq-hz` | Refused |
| `--mdata-nics`, `--mdata-resolvers`, `--mdata-ssh-keys`, `--mdata-root-pw` | Refused. `--mdata-root-pw-file` is accepted and ignored |
| `--hotplug` | Accepted, with the same `-c maxcpus=` and `-o hotplug.*` options. The commands arrive on the vsock `CONTROL` verb, so the VM also needs a `-s <slot>,virtio-vsock,<path>,cid=<n>` device |

A guest reset ends the firehyve process with exit code 0. A poweroff
gives 1, a halt 2, a triple fault 3 and a guest fault 4.

Example:

```bash
firehyve -c 2 -m 512M \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/$UUID/disk0 \
  -l com1,stdio \
  --kernel /var/tmp/vmlinux --cmdline "console=ttyS0 root=/dev/vda" \
  guest
```

### firehyve CONTROL verb

firehyve has no control socket. It serves the hotplug commands on the
host socket of a `virtio-vsock` device. A peer that sends `CONNECT
<port>` joins a guest port. A peer that sends `CONTROL` talks to the VMM
and reaches no guest. One request per line, and one answer line: `OK
...` or `ERR <reason>`.

```
CONTROL
device-list                     -> OK 2 virtio-blk@4,0.4.0,present ...
device-add 5,virtio-blk,/path   -> OK virtio-blk@5
device-remove virtio-blk@5      -> OK virtio-blk@5,remove-pending
cpu-list                        -> OK 4 0,present,boot 2,absent,hotplug ...
cpu-add 2                       -> OK 2,present
mem-list                        -> OK 1 boot,1073741824,present
mem-add 134217728               -> OK slot0,present
```

A list answer is `OK <count>` and then one field per record. A record is
its values joined by commas. So a client splits on spaces and then on
commas. No value holds a space or a comma. `mem-add` takes bytes on both
transports, not the `-m` grammar.

The socket is the path in the device spec, for example
`-s 9,virtio-vsock,/var/run/firehyve/guest.sock,cid=3`. It is 0600 in a
0700 directory, so the peer is the VMM's own user:

```bash
printf 'CONTROL\ncpu-add 2\n' | socat - UNIX-CONNECT:/var/run/firehyve/guest.sock
```

`virtio-vsock` cannot be hot-added: its CONTROL channel is set up once,
at startup.

## Example: the boot.c command line

This is the command line that `boot.c` builds for a typical SmartOS
bhyve zone:

```bash
rshyve -H -A \
  -U $UUID \
  -B "1,manufacturer=Joyent,product=SmartDC HVM,serial=$ZONENAME" \
  -c $VCPUS -m ${RAM}M \
  -s 0,hostbridge \
  -s 4:0,virtio-blk,/dev/zvol/rdsk/zones/$UUID/disk0 \
  -s 29:0,virtio-net-viona,net0 \
  -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,/dev/zconsole \
  -l com2,socket,/tmp/vm.ttyb \
  SYSbhyve-$ZONENAME
```
