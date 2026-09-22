# firehyve: direct-boot microVMs on illumos bhyve

**Draft measurement report.** All numbers in this document come from
one host, in one series of runs. Section 7 lists what was not measured.
firehyve is experimental software and is not production-supported. See
[../README.md](../README.md) and [../SECURITY.md](../SECURITY.md).

## Abstract

firehyve is a virtual machine monitor for short-lived Linux guests on
illumos bhyve. It boots a kernel directly, with no firmware and no
bootloader, and presents only the devices a transient workload needs.
On a SmartOS compute node with an Intel Xeon Gold 6240, a 1 vCPU,
128 MiB guest reaches its workload 64.9 ms after the host forks the
VMM process, measured at the median of 20 boots.

Two results account for most of that figure. Entering the kernel
through the PVH boot ABI rather than the compressed bzImage entry point
removes about 40 ms, because the guest no longer decompresses itself.
Reading boot images from the file descriptor straight into guest memory,
rather than staging them in the monitor's address space first, removes
a further 15 ms from the load phase. Neither result is novel; both are
reported here because the magnitudes are large enough to dominate every
other choice we examined.

## 1. Introduction

A microVM monitor for transient workloads is judged mostly on how fast
it can get out of the way. The workload is often measured in
milliseconds of useful compute, so a boot path costing hundreds of
milliseconds sets the floor on what the platform can economically run,
and determines how large a pool of pre-booted instances must be kept to
hide that latency.

Firecracker established the reference points for this class of monitor
on Linux and KVM [1, 2]. firehyve targets the same shape of workload on
illumos and bhyve, reusing the kernel interfaces that Triton and SmartOS
already provide, in particular the VMM memory reservoir.

This document reports what firehyve currently costs to boot, where that
cost sits, and which two changes moved it most. It is a measurement
report, not a claim of production readiness.

## 2. Platform envelope and design trades

### 2.1 What illumos bhyve permits

firehyve is a userspace monitor over the illumos bhyve kernel module, so the
guest it can describe is bounded first by that module. These are compile-time
constants in the illumos source, not tunables.

| Limit | Value | Constant |
|---|---:|---|
| vCPUs per guest | 64 | `VM_MAXCPU`, vmm.h:148 |
| Memory segments per guest | 5 | `VM_MAX_MEMSEGS`, vmm.c:179 |
| Address-space mappings | 8 | `VM_MAX_MEMMAPS`, vmm.c:189 |
| MMIO hooks per guest | 64 | `mmiohook_entry_limit`, vmm.c |
| CPUID entries | 256 | `VMM_MAX_CPUID_ENTRIES`, vmm.h:480 |
| Guest name length | 128 | `VM_MAX_NAMELEN`, vmm.h:144 |

Guest memory has no fixed ceiling. Reservoir-backed memory is capped by
`vmmr_total_limit`, computed at module load as all of `physmem` less 120% of
`pages_pp_maximum`, so it scales with the host rather than with a constant. On
the host used here that ceiling was 186,006 MiB.

**Guests per host is not a kernel constant.** It is bounded by host memory, the
reservoir ceiling above, and per-process resources, and the concurrency results
in section 5.4 should be read as one host's behaviour rather than a platform
limit.

The configuration measured throughout this document, 1 vCPU and 128 MiB, sits
far inside every one of these bounds. None of them is load bearing for the
results reported here.

### 2.2 What the design optimizes for

firehyve makes the following trades deliberately.

**Direct kernel boot only.** There is no UEFI path and no bootloader.
The kernel image is entered directly, which removes firmware
initialization from the boot path entirely. The cost is that firehyve
cannot boot a general-purpose guest image; it boots kernels built for
it.

**A minimal device model.** When these measurements ran, firehyve had
virtio block, network, RNG, filesystem and console, two 16550 UARTs,
an i440fx chipset, and the ACPI tables a Linux guest expects. No PCI
passthrough, no vTPM, no live migration. Current firehyve also has
virtio-vsock, the vsock `CONTROL` verb, and hotplug behind `--hotplug`
([features.md](features.md)).

**Guest memory from the reservoir.** illumos can pre-populate a
host-wide pool of wired pages, and a VM created with
`VCF_RESERVOIR_MEM` draws from it rather than faulting pages in from
the general pool as the guest touches them. This moves page allocation
and zeroing off the boot path and onto an operator-controlled
provisioning step.

**A known guest contract.** The guest runs `fhrun-init` as PID 1, a
statically linked musl binary that mounts the essential filesystems,
configures declared NICs, and `execve`s the workload. Because the
monitor knows what PID 1 will do, it can be instrumented precisely.

## 3. Boot path

```
host                                 guest
────                                 ─────
fork VMM
  create VM (reservoir memory)
  attach chipset, timers, PM
  attach COM1, COM2
  build fw_cfg, ACPI, SMBIOS
  load kernel + initramfs
  start vCPU  ─────────────────────► kernel entry
                                       (PVH: 32-bit protected mode,
                                        no decompression)
                                     kernel init
                                     /init = fhrun-init
                                       mount devtmpfs
                                       marker: init-start
                                       mounts, spec, net
                                       marker: ready
                                       execve workload
```

### 3.1 Boot protocols

firehyve accepts two kernel image formats and classifies the file
itself, so there is no flag for an operator to get wrong:

- **bzImage**, identified by the `HdrS` magic at offset 0x202. Entered
  through the standard 64-bit entry point. The image is compressed and
  decompresses itself during boot.
- **PVH ELF**, identified by an ELF header carrying an
  `XEN_ELFNOTE_PHYS32_ENTRY` note, which requires `CONFIG_PVH=y`.
  Entered in 32-bit protected mode per the x86/HVM direct boot ABI.
  The image is uncompressed, so nothing decompresses at boot.

### 3.2 Timing markers

`fhrun-init` writes two markers to COM2 (`/dev/ttyS1`) as raw writes to
a device it opens itself, deliberately not through the kernel's printk
path:

| Marker | Meaning |
|---|---|
| `init-start` | first instruction of PID 1 |
| `ready` | immediately before `execve` of the workload |

Keeping the markers off the console path is what allows `console=` to
be dropped from the guest command line, which the Firecracker paper
measures as worth up to 70 ms of boot time [1]. The monitor mirrors
COM2 to its stdout, and the host harness timestamps the markers.

## 4. Methodology

Boot time is the interval from the host forking the VMM process to the
guest marker, measured on the host with a polling loop at 2 ms
granularity.

This follows the definition the Firecracker paper uses, which is "the
time between when VMM process is forked and the guest kernel forks its
`init` process", using "a minimal `init` implementation, which just
writes to a pre-configured IO port" [1]. firehyve's marker transport is
a serial port rather than a bare I/O port, and `ready` fires slightly
later than Firecracker's marker, immediately before the workload is
executed rather than at init entry. Where the two are compared below,
firehyve's number therefore covers marginally more work.

Unless stated otherwise, every measurement below used:

- 1 vCPU, 128 MiB, reservoir enabled
- Linux 6.12.13, configured from `tinyconfig` with `CONFIG_SMP=n`,
  `NR_CPUS=1`, no EFI, no modules
- an initramfs holding `fhrun-init` and the workload
- command line `root=/dev/ram0 init=/init rdinit=/init panic=-1
  fhrun=1 tsc=reliable no_timer_check noapictimer`, with no `console=`

`fhrun` cannot reproduce this command line. Its launcher always starts
the command line with `console=ttyS0 earlyprintk=ttyS0`, and a manifest
can only append to it. The measurements started the VMM directly.
- host: SmartOS, Intel Xeon Gold 6240 at 2.60 GHz
- 20 boots per configuration, VM destroyed between boots

The host shows roughly 10 ms of run to run drift across repeated
20-boot batches. Medians below should be read with that in mind;
per-phase numbers taken from the monitor's own logs are tighter and are
the stronger evidence where the two disagree.

## 5. Results

### 5.1 Boot to workload

Spawn to `ready`, 20 boots per configuration, no timeouts:

| Kernel image | min | p50 | max |
|---|---|---|---|
| bzImage, compressed | 91.5 ms | 105.5 ms | 119.8 ms |
| PVH ELF, uncompressed | 61.4 ms | **64.9 ms** | 78.3 ms |

Entering through PVH removes about 40 ms at the median. The Firecracker
paper attributes approximately 40 ms to kernel self-decompression on
its own hardware [1], and the agreement is close enough that we treat
decompression as the explanation rather than looking further.

Both images came from the same compile. Enabling `CONFIG_PVH` changes
the bzImage as well, so comparing the new ELF against a previously
built bzImage would have confounded the boot protocol with a kernel
rebuild.

### 5.2 Where the remaining time goes

For the PVH configuration, from the monitor's phase log:

| Phase | Duration |
|---|---|
| guest memory allocation | 6.2 ms |
| chipset and kernel devices | 0.04 ms |
| PCI devices | 0.02 ms |
| ACPI tables | 0.02 ms |
| kernel and initramfs load | 7.4 ms |
| **host setup total** | **16 ms** |
| guest kernel to `ready` | remainder, about 49 ms |

Host-side setup is roughly a quarter of the total. Two thirds of it is
guest memory allocation and image loading.

### 5.3 Image loading

Loading a 9.34 MiB PVH image into guest memory was measured across four
implementations. The load phase is instrumented to time the read and
the copy separately, because the two have different fixes and a single
number hides which one is moving.

| Implementation | read | copy | total |
|---|---|---|---|
| `read(2)` whole file, byte-at-a-time volatile store loop | 17.1 ms | 5.3 ms | 22.4 ms |
| `read(2)` whole file, `memcpy` | 17.1 ms | 5.3 ms | 22.4 ms |
| `mmap`, `memcpy` | 0.1 ms | 14.6 ms | 14.8 ms |
| `mmap` for headers, `pread` into guest memory | 0.06 ms | 7.3 ms | **7.4 ms** |

Three things in this table were not obvious in advance.

Replacing the byte-at-a-time volatile store loop with a `memcpy` did
not help at all. Both variants are dominated by first-touch faults on
the destination: illumos installs guest mappings on demand through
`seg_vmm`, 4 KiB at a time and without large pages, so the cost is
per-page fault handling rather than per-byte copying.

Mapping the file did not remove work so much as relocate it. The read
phase collapsed from 17.1 ms to 0.1 ms, but the page cache faults
reappeared inside the copy, which grew from 5.3 ms to 14.6 ms. The net
saving was real but roughly half of what the read-phase number alone
suggested.

The change that worked was eliminating the staging buffer entirely.
Reading each `PT_LOAD` segment from the file descriptor directly into
the validated guest range means the image is never materialized in the
monitor's address space, so only one set of faults is paid instead of
two. The mapping is retained for header parsing, which touches a
handful of pages.

Because this writes file data directly into guest memory, the
implementation validates the destination range as mapped and writable
before the first byte is read, resumes short reads, retries `EINTR`,
and treats end of file before the segment length as an error rather
than a short fill. A segment claiming more bytes than its file holds
must not leave the tail of the range holding whatever the guest page
held previously.

### 5.4 Concurrent boots

All N guests launched as near simultaneously as the harness allows,
measuring until the last one reaches `ready`. Every guest is 128 MiB
with the reservoir sized to cover the batch. This used the bzImage
configuration, so the per-guest floor is the 105 ms figure, not 65 ms.
Results are reported to N=600, the largest batch this host sustains without
hitting a separate limit.

| N | time for all N to reach ready | marginal cost per guest | succeeded |
|---:|---:|---:|---:|
| 10 | 0.22 s | -- | 10/10 |
| 50 | 0.57 s | -- | 50/50 |
| 100 | 1.10 s | -- | 100/100 |
| 200 | 2.20 s | 11.0 ms | 200/200 |
| 400 | 5.00 s | 14.0 ms | 400/400 |
| 600 | 9.29 s | 21.4 ms | 600/600 |

No guest failed to reach `ready` at any batch size tested. Growth is
superlinear: the marginal column is the increment over the previous row divided
by the guests added, and it roughly doubles between N=200 and N=600. That says
the launch path contends with itself well before any capacity ceiling is
reached. Where that contention sits has not been established.

Batch sizes above 600 were measured but are excluded here. They exercise a
separate failure mode on a host of this size, and reporting them alongside
these numbers would conflate two different limits.

### 5.5 Comparison to Firecracker

The Firecracker specification claims 125 ms or less from the
`InstanceStart` API call to the start of `/sbin/init`, with the serial
console disabled and a minimal kernel and root filesystem [2]. The NSDI
paper measures approximately 100 ms for pre-configured Firecracker
booting serially, and approximately 150 ms end to end through the API,
on an EC2 m5d.metal instance with a 1 vCPU, 256 MiB guest [1].

firehyve's 64.9 ms median is against a later marker, on different
hardware, with a smaller guest and a newer kernel. It is not a
controlled comparison and should not be read as one. What it supports
is the weaker claim that firehyve is in the same class, and that the
gap does not run the wrong way.

The two efforts agree on the levers. Firecracker reports up to 70 ms
from disabling console logging, approximately 40 ms from booting an
uncompressed kernel, approximately 20 ms from static network
configuration, and 900 ms from a general-purpose distribution kernel
rather than a minimal one [1]. firehyve's configuration already takes
the first two, and the PVH result in section 5.1 independently
reproduces the second at nearly the same magnitude.

## 6. Related work

Firecracker [1, 2] is the direct antecedent and the source of the
measurement definition used here. Propolis, Oxide Computer Company's
bhyve VMM, is the basis for this project's kernel ioctl bindings,
address space manager, and several device models; see
[../THIRD_PARTY.md](../THIRD_PARTY.md) for the derivation, which
`tools/propolis-derivation.py` regenerates so the claim can be checked.
Cloud Hypervisor is the other Rust VMM in this space and is reported in
the NSDI evaluation as marginally faster than pre-configured
Firecracker to boot [1]. rust-vmm's `linux-loader` implements the
segment loading approach that section 5.3 arrives at.

## 7. What this document does not measure

These are gaps, not results. Each would need work before firehyve could
be compared to Firecracker on the axes the NSDI paper covers.

- **Memory overhead per microVM.** Not measured. Firecracker reports
  approximately 3 MiB, Cloud Hypervisor approximately 13 MiB, QEMU
  approximately 131 MiB [1].
- **Block and network throughput and latency.** Not measured.
- **Density under load.** Section 5.4 measures boot concurrency with
  idle guests. It says nothing about steady-state density with guests
  doing work.
- **Snapshot and restore.** Not implemented in firehyve. This is the
  mechanism by which production systems reach the 10 ms range, and its
  absence is the largest functional gap against the state of the art.
- **Security properties.** No isolation testing, no side-channel
  analysis, no fuzzing of the device models is reported here.
- **Sustained or multi-host results.** Every number above comes from
  one host in one session.

## 8. Status

firehyve boots a minimal Linux guest to its workload in about 65 ms at
the median on the hardware described, and boots 600 such guests
concurrently in 9.3 s with none failing. The boot path is understood
well enough that the two largest remaining costs, guest memory
allocation and the guest kernel's own initialization, are identified
and measured.

It remains experimental software with no published release, and the
gaps in section 7 are substantial.

## References

[1] A. Agache et al. "Firecracker: Lightweight Virtualization for
Serverless Applications." NSDI 2020.
https://www.usenix.org/system/files/nsdi20-paper-agache.pdf

[2] Firecracker specification.
https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md

[3] D. Venhoek. "Minimizing Linux boot times."
https://blog.davidv.dev/posts/minimizing-linux-boot-times/
