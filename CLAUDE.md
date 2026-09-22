Write virtio devices against the VIRTIO 1.3 specification:
https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html

Propolis is the upstream reference: https://github.com/oxidecomputer/propolis

## Coding standards

### Virtio devices
- The `VirtioDevice` trait uses **u64** feature bits. The legacy transport truncates them to u32.
- **Transitional PCI transport**: BAR0 (PIO) holds the legacy registers. BAR2 (MMIO, 16 KiB) holds the modern structures. PCI capabilities at 0x40 and above point to them. BAR4 (MMIO) holds the MSI-X table and PBA.
- Devices include **EVENT_IDX** (bit 29) and **INDIRECT_DESC** (bit 28) in `device_features()`. The legacy transport (`pci/legacy.rs`) strips both. Modern (VERSION_1) guests get them. Viona strips EVENT_IDX because the kernel handles its rings.
- EVENT_IDX locations: the device reads `used_event` from the **avail ring**, and writes `avail_event` to the **used ring**. Do not swap them.
- `collect_chain()` collects a descriptor chain, direct or indirect.
- Asynchronous completions go through `VirtioCompletion`, one `Mutex<u16>` used-ring writer per queue (the Propolis pattern). virtio-blk creates one per queue on the first notification of that queue.

### virtio-blk
- `VirtioBlock::new()` takes `num_queues`. `VIRTIO_BLK_F_MQ` is offered when `num_queues > 1`, with the queue count at config offset 0x22.
- Worker threads: 8 for one queue, 2 per queue for multiqueue.
- Every request that reaches the backing store runs on a worker and bounces through a per-worker host buffer. Only requests that touch guest memory alone, such as GET_ID, run inline on the vCPU.
- A worker holds guest-memory access across memory copies and ring writes only, never across a backing-store call. So a device reset waits for bounded work and never for the disk. Keep that invariant.

### Guest memory
- All guest memory access goes through `PhysMap::lookup()` and `SubMapping`.
- `SubMapping::read<T>`/`write<T>` check bounds and alignment and use `read_volatile`/`write_volatile`.
- `SubMapping::read_bytes`/`write_bytes` copy one byte at a time with volatile access. `copy_out`/`copy_in` are plain memcpy: use them only where a torn copy is acceptable (see their doc comments).
- Use `checked_add`/`checked_mul` for all arithmetic on guest-supplied values.
- Do not pass raw pointers across threads. Use `Arc`.

### Errors and logging
- Use `slog` structured logging. No `eprintln!` in production paths.
- No `let _ =` on a path that changes state or is security-critical.
- If a migration thread does not spawn, put the VM back in its previous state and return an error.
- The control socket is mode 0600 in a 0700 directory.

### PCI and MSI-X
- A virtio transport holds `msix: Option<Arc<MsixTable>>`. `None` means INTx only.
- In the legacy BAR0 layout, device config starts at 0x14, and at 0x18 when the driver has enabled MSI-X.
- A backend raises a queue interrupt with `raise_queue_interrupt_in(session, queue_idx)`. It uses the queue's MSI-X vector, or INTx when MSI-X is off, and does nothing if a reset ended that session.

## Testing

[docs/testing.md](docs/testing.md) tells how to build, run the unit
tests, and boot a test guest. Put site-specific settings in
`CLAUDE.local.md`, which is gitignored, not in a tracked file.
