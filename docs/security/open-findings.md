# Open security findings

This is the register of known security issues that are not resolved.
Each entry gives the boundary it crosses, the evidence, and what closes
it. A resolved entry moves to [Resolved](#resolved) with the change
that closed it, or the reason it does not apply.

## Open

### OSF-1: Live migration has no peer authentication or encryption

**Boundary:** migration peer to VMM. **Severity:** high in any
deployment where the migration path is not already a trusted channel.

`crates/vmm-migrate/src/source.rs` connects and
`crates/vmm-migrate/src/destination.rs` accepts a raw WebSocket over a
TCP or Unix stream with no protocol-level peer identity.
A destination told to listen binds the address it is given and accepts
the first connection.

The destination checks the device set, the queue fields of each device
and the layout of the kernel state before it applies them. The import
is incremental, and the kernel checks the values inside its blobs only
as it applies them. A payload that fails a check stops the migration
with the destination partly programmed but not started, and the source
resumes its guest. That limits the damage. It is not authentication,
and it does not prove that every accepted payload is safe.

Guest RAM, CPU state, and device state therefore have no
confidentiality and no peer authentication unless an external control
supplies them.

**To close:** an authenticated, encrypted channel with destination and
source identity verification, session binding to a single VM, migration
ID and expiry, and negative tests for wrong identity, replay, timeout,
truncation, and extra clients. The recommended shape is an
SSH-forwarded Unix socket with pinned host keys, so the VMM never binds
TCP at all.

**Mitigation today:** migrate only across a trusted network, over a
tunnel established and authenticated by other means.

### OSF-2: Migration does not transfer FPU/XSAVE state

**Boundary:** correctness, guest-visible. **Severity:** high.

`crates/vmm-migrate/src/state.rs` does not export or import FPU/XSAVE
state. A guest using x87, SSE, or AVX state across a migration can
observe corrupted registers.

The kernel interface exists: `VM_GET_FPU`, `VM_SET_FPU` and
`VM_DESC_FPU_AREA` are already bound in `crates/bhyve-api/sys/src/ioctls.rs`.

**To close:** implement `get_fpu`/`set_fpu`/`fpu_area_desc`, reject a
blob whose length does not match the local area size before the ioctl,
and validate across a real cross-host migration with a continuous AVX
workload. A malformed XSAVE blob can set reserved `XCOMP_BV` bits and
fault the guest at its first `XRSTOR`, so this is worse to get wrong
than to leave unimplemented.

### OSF-5: PCI passthrough is not audited

**Boundary:** guest to host. **Severity:** unknown, potentially high.

`crates/vmm-passthru` exposes physical device configuration space
indexed by guest-controlled offsets, and holds the largest
concentration of `unsafe` in the tree.

Part of it is audited. Config-space writes are denied unless the device
model handles them, an MSI-X device is refused because illumos VT-d does
not remap interrupts, and the MSI capability is emulated
(`fix(passthru): deny unhandled config writes and refuse MSI-X devices`).
The BAR mapping and unmapping paths, and the guest-offset arithmetic
behind them, are not audited.

**To close:** audit the BAR mapping paths, and decide whether the
device ships behind a non-default cargo feature for the initial
release. OSF-11 covers the part no reading can settle.

### OSF-6: Operational errors are discarded on non-migration paths

**Boundary:** operational correctness. **Severity:** low to medium.

Around 75 `let _ = ...` statements remain outside the migration page
paths and outside test modules, some on device state and interrupt
delivery. Each needs to be classified as either genuinely idempotent
best-effort cleanup or a real error that should propagate.

**To close:** classify every site, propagate or deliberately log the
rest, and add fault-injection tests for guest-memory, interrupt, queue,
and device-state failures.

### OSF-7: VNC console authentication is legacy and unencrypted

**Boundary:** local. **Severity:** low.

The VNC server uses the RFB DES-based challenge, which is not a
meaningful authentication mechanism by modern standards, and provides no
transport encryption. The socket is Unix-domain and now bound
owner-only, so the exposure is limited to a local user who is already
the VMM's own uid.

The control-socket part of this finding is closed: the control socket
has peer-credential authorization, a bounded read, deadlines, and
concurrent accept.

**To close:** a modern authenticated, encrypted console boundary for any
remote use, and a documented tunnel requirement until then.

### OSF-9: No audit covers every guest-driven log site

**Boundary:** guest to host. **Severity:** low (host disk and log
pipeline, and evidence destruction).

A device path that writes one log record per guest action lets a guest
write to the host log as fast as it can execute. The logger's async
drain is a 128-entry channel with `DropAndReport`, so a flood also
evicts the records that would explain an unrelated failure.

The seam that answers this exists. `crates/vmm-core/src/ratelimit.rs`
is a token bucket, one instance per log site so one noisy source cannot
spend another's budget, and pvpanic, the COM2 serial writer, the mdata
agent and the Hyper-V crash MSRs use it. The two viona paths latch
instead: a refused ring kick logs once per run of refusals, and a
refused ring init quarantines the ring until reset. Several crates
carry a `CountingDrain` test asserting that N guest actions produce a
bounded number of records.

What is open is coverage. The `warn!` and `info!` sites reachable from
a guest action have not been enumerated, and nothing obliges a new
device to use the bucket, so the next one can reintroduce the defect.

**To close:** enumerate the guest-reachable log sites, put each behind
the bucket or a latch, and give each a `CountingDrain` test.

### OSF-10: virtio-fs runs in the VMM process, not a sandbox

**Boundary:** guest to host filesystem. **Severity:** medium.

`crates/vmm-virtio/src/fs/passthrough.rs` serves FUSE requests from the
VMM process itself. The isolation is the shared root fd, `*at()` calls
with `O_NOFOLLOW`, name validation at LOOKUP, and refusal of device,
FIFO and socket nodes. The two tables it keeps are capped, so a guest
cannot pin host descriptors or host memory without bound:
`crates/vmm-virtio/src/fs/passthrough/tables.rs` bounds cached inodes,
open file and directory handles, and the bytes of directory snapshots,
and a guest past a cap sees ENFILE or EMFILE the way it would on a full
system (`Cap the host fds and memory one guest pins in virtio-fs`).

Those caps are a mitigation, not the boundary. virtiofsd runs the same
work in a separate process under a namespace or `pivot_root`, so a defect
in path handling costs an attacker a sandbox rather than the VMM's whole
address space and every fd in it. illumos has no seccomp and no mount
namespaces, so the equivalent needs its own design.

**To close:** decide whether virtio-fs ships in the VMM process at all.
If it does, the argument for it needs writing down, together with a
negative test set for every path-handling rule the module claims. If it
does not, a helper process with a reduced privilege set and its own
`root_fd`, talking to the device over a socket.

**Mitigation today:** export only directories whose contents the guest
may read, and never a read-write share of anything the host trusts.

### OSF-11: The PCI passthrough audit has not run on real hardware

**Boundary:** guest to host. **Severity:** unknown.

OSF-5 records how far reading the source has got. What no reading can
supply is a device: every conclusion about passthrough in this tree has
been reached from the code and the PPT ioctl contract, never by
assigning a physical function to a guest.

Specifically unproven: that the config-space windows the guest sees match
what reading the code concluded, that BAR mapping and unmapping survive a guest
reset with DMA in flight, that the MSI-X refusal covers every device that
advertises it, and that illumos VT-d remapping behaves as the module
header assumes. There is no AMD IOMMU backend at all.

**To close:** run a real assigned device: a NIC and a storage controller,
each through guest boot, reset, reboot and VM destroy, with DMA active,
on an Intel VT-d host. Record in this entry what was assigned and what
was seen. Until then, treat `passthru` as experimental.

**Mitigation today:** do not offer `-s N,passthru` to an untrusted guest.

## Resolved

| Finding | Closed by |
|---|---|
| OSF-3: CPU feature mismatch was warned about, not rejected, and the two implementations compared different leaves against unmasked host features | `Migrate device state by PCI identity over one driver` |
| OSF-4: vendored libtpms and CVE-2026-6726, CVE-2026-6727 | Not affected. See [OSF-4](#osf-4-not-affected) below |
| OSF-8: Hyper-V enlightenment state was not migrated; `TIME_REF_COUNT` restarted from a process-local clock and ran backwards for the guest | `Migrate the Hyper-V enlightenment` |
| PS/2 controller RAM out-of-bounds index reachable from guest port I/O | `fix(ps2): guest-reachable out-of-bounds index in controller RAM` |
| Virtio indirect descriptor table unbounded against queue size | `fix(virtio): bound indirect descriptor tables to the queue size` |
| Migration `PageBatch` slice index abort and discarded guest-memory errors | `fix(migrate): validate and bound peer-controlled page transfers` |
| Wide guest access to an 8-bit port aborts the VMM; unbounded MMIO and PIO widths | `fix: wide guest accesses to narrow ports and buses abort the VMM` |
| virtio-blk accumulated requests without bound across the EVENT_IDX retry loop | `fix(virtio-blk): bound requests collected per notification` |
| virtio EVENT_IDX re-check loop spins forever holding the device lock when the available-ring entry is unmapped | `fix(virtio): guest-triggerable hang in the EVENT_IDX re-check loop` |
| NVMe queue-size arithmetic overflowed on guest input | `fix(nvme): compute guest-supplied queue sizes in u32` |
| Control socket unbounded requests, no deadlines, no peer authorization, serial accept | `fix: restrict and authorize every Unix socket the VMM exposes` |
| VNC and mdata sockets bound with no permission protection | same commit |

### OSF-4: not affected

CERT VU#431093 lists two libtpms CVEs. Neither applies to this build:

- **CVE-2026-6726:** the pinned libtpms has the upstream fix.
  `FindEmptyObjectSlot()` zeroes the whole OBJECT before reuse.
- **CVE-2026-6727:** the affected `OaepDecode()` is compiled only
  without OpenSSL RSA functions. This build uses OpenSSL for RSA.
- **What keeps it true:** a vendored patch adds an `#error` to
  `CryptRsa.c`, so libtpms does not compile unless
  `USE_OPENSSL_FUNCTIONS_RSA` is 1.

[THIRD_PARTY.md](../../THIRD_PARTY.md) has the source references.
