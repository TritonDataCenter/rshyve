# Live migration

rshyve can move a running VM to another host. firehyve cannot. Live
migration is **experimental**.

> **Security.** The migration stream has no peer authentication and no
> encryption (OSF-1 in [security/open-findings.md](security/open-findings.md)).
> Guest RAM, vCPU registers, MSRs and device state cross the wire in
> clear text, and a destination accepts the first peer that connects.
> Migrate only through a channel that you authenticate and encrypt by
> other means, for example an SSH-forwarded Unix socket, so that the VMM
> never listens on TCP.

This repository supplies the VMM side: the protocol, the state export
and import, and the control-socket commands. It does not supply an
orchestrator. Disk replication, the start of the destination VM, the
transport between the hosts and the clean-up of the source are the work
of the program that drives the migration.
[The orchestrator contract](#the-orchestrator-contract) tells what that
program must do.

## Transport and wire format

The source and the destination exchange WebSocket binary frames
(`tungstenite`) over one byte stream. The stream is a Unix socket or a
TCP socket. Neither end starts an async runtime, because a bhyve-brand
zone has no `/dev/poll`. Inside a zone, use a Unix socket and bridge it
to the peer from the global zone.

The protocol identifier is `vmm-migrate-ron/2`. The two ends must offer
and select exactly this string. There is no compatibility window: a peer
that reads a field differently would resume a guest on state that is not
its own.

Each frame is the payload followed by one tag byte:

| Tag | Message | Payload |
|---:|---|---|
| 0 | `Okay` | empty |
| 1 | `Error` | RON `MigrateError` |
| 2 | `Serialized` | RON structure: protocol offer or selection, preamble, time data, device state |
| 7 | `MemFetch` | RON list of GPAs for the next `PageBatch`, sent when its pages are not contiguous |
| 9 | `MemEnd` | empty |
| 10 | `MemDone` | empty |
| 11 | `PageBatch` | `base_gpa: u64`, `page_count: u32`, `flags: u32`, all little-endian, then page data |
| 12 | `PauseSignal` | empty |

`PageBatch` flag bit 0 means the data is zstd-compressed (level 1). The
source compresses a batch only when the result is smaller. Limits
(`crates/vmm-migrate/src/limits.rs`):

| Limit | Value |
|---|---|
| Pages per batch that the source sends | 64 |
| Pages per batch that the destination accepts | 256 |
| GPAs in one `MemFetch` | 256 |
| One `Serialized` payload | 8 MiB |

## Phases

```
Source                                   Destination
  |---- ProtocolOffer --------------------->|
  |<--- ProtocolSelect ---------------------|
  |---- Preamble -------------------------->|  CPU count, memory size,
  |<--- Okay -------------------------------|  device set, CPU features
  |                                         |
  |---- PageBatch* ... MemEnd ------------->|  full RAM pass
  |<--- MemDone ----------------------------|
  |---- PageBatch* ... MemEnd ------------->|  up to 5 dirty passes
  |<--- MemDone ----------------------------|
  |---- PauseSignal ----------------------->|
  |                                         |
  |  pause devices, pause vCPUs,            |
  |  flush backing stores, ZFS barrier      |
  |                                         |
  |---- PageBatch* ... MemEnd ------------->|  final dirty pass
  |<--- MemDone ----------------------------|  destination pauses its VM
  |---- TimeData -------------------------->|
  |---- DeviceState ----------------------->|  identity check, import
  |---- MemEnd ---------------------------->|
  |<--- Okay -------------------------------|  "I hold everything"
  |---- Okay ------------------------------>|  the source commits
  |                                         |  devices start, vCPUs resume
  |<--- Okay -------------------------------|  "the guest runs here"
```

1. **Sync.** The source offers the protocol and sends the preamble. The
   destination checks it (see [Preamble checks](#preamble-checks)) and
   answers `Okay`, or sends `Error` and stops.
2. **RAM before the pause.** The source sends every RAM page, then runs
   up to 5 dirty-page passes, 500 ms apart. It stops early when a pass
   finds 1024 or fewer dirty pages. Dirty tracking must work: the first
   pass fails at once if the kernel refuses `VM_TRACK_DIRTY_PAGES`.
3. **Pause.** The source parks every device worker, pauses the kernel
   (viona) rings and waits for them to quiesce. Then it pauses the
   vCPUs, flushes every backing store, and waits for the ZFS barrier if
   the command named one. For a VM with Hyper-V enlightenments, it
   removes the hypercall and reference-TSC overlay pages, so the RAM
   image holds the guest's own bytes.
4. **Final RAM pass.** Every writer is stopped, so this pass is the last
   word on guest memory.
5. **Time data and device state.** See [State that migrates](#state-that-migrates).
6. **Finish.** The destination says that it holds everything. The source
   then commits: from that point it never runs the guest again. Only
   after the commit does the destination start its device workers and
   resume the vCPUs. Its last `Okay` releases the source.

A destination that resumed before the commit would run a second copy of
the guest against the same disk each time the source rolled back.

### Read deadlines

Every read has a deadline. It is 300 s before the pause and 30 s after
it, because after the pause each second is guest downtime on both hosts.
The ZFS barrier has 120 s. There are no write deadlines: illumos accepts
`SO_SNDTIMEO` on a socket and then ignores it.

### Preamble checks

The destination refuses the migration before it accepts a page when:

- the vCPU count or the memory size differs from its own;
- the device set differs. Each device that carries state is named by
  its kind and its PCI bus, device and function;
- the source guest sees a CPU feature that this host does not supply.

The CPU comparison covers CPUID leaf 1 ECX and EDX, leaf 7 subleaf 0
EBX, ECX and EDX, and the XCR0 component mask. Each end reports what it
shows a guest after its own `--cpu-baseline` mask. A guest that already
used a missing instruction takes #UD on the next one, in kernel context,
so the default is to refuse. To accept that risk for one migration, send
`migrate-dest` with `"allow_cpu_feature_mismatch": true`. The
destination then logs the missing bits. A destination started with
`--migrate-listen` always refuses.

`--cpu-baseline` sets the mask (see [cli.md](cli.md)). To move a guest
from a newer CPU to an older one, start the source with a baseline that
the older CPU supports.

## State that migrates

### Kernel state

Through the vmm-data interface:

| Device | Class | Version |
|---|---|---:|
| I/O APIC | `VDC_IOAPIC` | 1 |
| 8254 PIT | `VDC_ATPIT` | 1 |
| 8259 PIC | `VDC_ATPIC` | 1 |
| HPET | `VDC_HPET` | 1 |
| ACPI PM timer | `VDC_PM_TIMER` | 1 |
| RTC | `VDC_RTC` | 2 |

Per vCPU: the general registers and segment descriptors, the MSRs, the
local APIC, `VDC_VMM_ARCH` (pending events), the run state and the SIPI
vector. The destination imports the system devices first (the I/O APIC
before the local APICs), then per vCPU the MSRs, the local APIC, the
registers, the run state and `VMM_ARCH`. Every write is fatal on
failure.

### Time

The destination rebases the guest clock on its own host time:

```
guest_uptime    = src.hrtime - src.boot_hrtime
migrate_delta   = max(0, dst.wall_clock - src.wall_clock)
new_boot_hrtime = dst.hrtime - (guest_uptime + migrate_delta)
```

If the TSC frequencies differ, the source TSC is scaled to the
destination frequency and the destination frequency is kept. The
arithmetic is checked, in `i128`. A wall-clock difference of more than
one hour is refused as clock disagreement.

### Emulated devices

The device payload has one entry for each PCI address in the device
set, including devices with no state. The destination restores each
entry by address only. It refuses a payload with an entry for an address
where it has no device, two entries for one address, or no entry for one
of its devices.

- **virtio PCI transport** (each virtio device): the status byte, the
  negotiated features, the config MSI-X vector, the PCI command register
  and BAR bases, the MSI-X table with masks and pending bits, and each
  configured queue with its ring addresses, size, cursors and vector. A
  queue programmed through the legacy PFN register is live, although it
  has no enable bit. MSI-X is restored in the state that the source had.
- **viona**: the kernel's own ring cursors from `VNA_IOC_RING_GET_STATE`,
  not the guest's available index. The destination sets them with
  `VNA_IOC_RING_SET_STATE`.
- **NVMe**: CC, CSTS, AQA, the admin queue bases, each queue pair with
  its size, cursors, phase, vector and interrupt-enable bit, the PCI
  command register and BAR bases, and the MSI-X table.
- **Hyper-V** (a VM started with `--hyperv`): `GUEST_OS_ID`,
  `HYPERCALL`, `REFERENCE_TSC`, the five crash MSRs and the reference
  counter. The destination reinstalls both overlay pages from the MSR
  values. It refuses this state if it was started without `--hyperv`.

The destination checks the payload in stages. It does not apply the
payload as one transaction.

- Before it accepts a page: the vCPU count, the memory size, the device
  set and the CPU features.
- Before it restores a device: each device entry matches one local
  device, with no duplicate and no gap. Hyper-V state goes only to a VM
  started with `--hyperv`.
- Before it applies the state of one device: that device's queue
  fields. These are queue sizes, cursors inside their ring, ring
  addresses mapped and aligned, vectors inside the table, and a
  completion queue for each NVMe submission queue. The MSI-X table is
  checked when it is imported, after the queues.
- Before it applies kernel state: one state per vCPU, each class and
  version known to the local kernel, and each blob length. The kernel
  checks the values inside the blobs as it applies them.

The import is incremental: time data first, then each device in turn,
then kernel state. If a step fails, the destination can be partly
programmed. It reports the error to the source and does not start its
vCPUs. The source resumes its guest if it paused it.
`rshyve --migrate-listen` exits with an error. After a failed
`migrate-dest`, the process keeps running and reports the run state it
had before the command.

After the restore, the destination raises an interrupt on each live
queue, and once more 100 ms after the vCPU threads start. The second one
reaches a running driver, which then processes its used ring and posts
new receive buffers. virtio-blk also forces an interrupt for its first
32 completions, whatever EVENT_IDX says.

## What prevents a migration

`migrate-source` refuses a VM that has:

- a vTPM or a UEFI variable store. Their state is in host files that the
  protocol does not carry;
- an `ahci-cd`, `virtio-console`, `virtio-fs`, `virtio-vsock` or `xhci`
  device. No state is carried for these, so the guest would find the
  device reset on the destination (`UNMIGRATABLE` in
  `bin/rshyve/src/control/migrate.rs`). rshyve cannot attach
  `virtio-vsock`, so in practice that entry never applies;
- a CPU, memory or PCI device that was hot-added, or a slot that waits
  for removal. The destination is built from the boot command line.

rshyve refuses to start with `--migrate-listen` together with `--vtpm`,
a variable store, `fbuf` or `ahci-cd`.

This state is not carried and does not block a migration, because the
guest recovers from its loss:

- the LPC UART registers and FIFO contents;
- the i8042 (PS/2) controller and keyboard state;
- the `fw_cfg` selector;
- the ACPI PM1 and GPE registers.

**FPU and XSAVE state is not carried** (OSF-2). A guest that uses x87,
SSE or AVX registers across a migration can see corrupt values.

## Control commands

Send each command as one JSON line to the `--control-socket`.

| Command | Fields | Where |
|---|---|---|
| `migrate-source` | `target_addr`, optional `zfs_barrier` | source, VM running |
| `migrate-dest` | `listen_addr`, optional `allow_cpu_feature_mismatch` | destination, VM paused |
| `migrate-status` | none | either |
| `migrate-config` | none | source |

`target_addr` is a Unix socket path (it starts with `/` or ends in
`.sock`) or a `host:port`. A `host:port` must not be loopback, the
wildcard address or port 0. `migrate-dest` takes a TCP `host:port` only,
with the same rules.

`migrate-status` returns `state` and, during a migration,
`migrate_phase` (`sync`, `ram-push-pre-pause`, `pause`,
`ram-push-post-pause`, `time-data`, `device-state`, `ram-pull`,
`finish`, `committed`, `error`), `migrate_bytes` and `migrate_pages`.

`migrate-config` returns `vm_name`, `num_cpus`, `memory_mb`, `pci_slots`
and `lpc_devices`: what the destination command line needs.

The usual destination is a new rshyve process started with the same
command line as the source plus `--migrate-listen <addr>`. It creates
the VM and its devices and does not start the vCPUs. With a TCP address,
it binds and accepts one connection and does not validate the address.
With a Unix socket path, it connects to that path and tries for 15 s.

```bash
# destination host
rshyve <same options as the source> --migrate-listen 192.0.2.20:4567 <vm>

# source host
echo '{"command":"migrate-source","target_addr":"192.0.2.20:4567"}' \
  | socat - UNIX-CONNECT:/var/run/rshyve/<vm>/control.sock
```

## The orchestrator contract

The program that drives a migration must do these things.

1. **Start the destination** from the source's boot configuration
   (`migrate-config` gives it) plus `--migrate-listen`. The vCPU count,
   the memory size and the device set must match.
2. **Supply the stream.** Inside a bhyve zone, both VMMs connect to
   Unix sockets. The orchestrator listens on those sockets and bridges
   them between the hosts.
3. **Replicate the disk.** The protocol carries no block data. Copy the
   disk while the guest runs, for example with incremental `zfs send`.
4. **Serve the ZFS barrier.** Pass a Unix socket path as `zfs_barrier`
   in `migrate-source`, and listen on it. After the vCPUs pause and every
   backing store is flushed, the source connects to the socket. That
   connection means "the guest is paused". Send the last incremental,
   then write one byte: `0` if the destination disk now holds every
   write the guest was told was complete, any other value if not. The
   source treats a non-zero byte, a timeout (120 s), a failed connect
   and a closed socket as a failed migration, and resumes its guest.
5. **Finish the source.** After a successful migration the source VM
   state is `stopped` and the source waits to be reaped. If nobody stops
   it within 300 s, it halts itself.
6. **Handle a commit failure.** If the source loses the destination
   after its commit, it stays paused and in `migrating` state, and it is
   not released. The destination may be running the guest. Find out
   where the guest runs before you resume or stop either side.
7. **Clean up a failed destination.** A failure before the commit rolls
   the source back: device workers resume, each kernel ring that the
   pause stopped is reset and rewritten from its saved state, and the
   vCPUs run again. The destination VM is not usable. Destroy it.

A failed `migrate-dest` puts that VM back in the state it had before
the command.

## Known limits

- No peer authentication or encryption (OSF-1).
- No FPU or XSAVE state (OSF-2).
- No cancel command, no migration ID and no replay protection.
- Disk contents move outside the protocol. The barrier assumes ZFS.
- A VM that hot-added anything cannot migrate.
- Hyper-V state migration has unit tests and has not run with a Windows
  guest.
- VMs larger than 32 GiB have not been migrated in a test.
