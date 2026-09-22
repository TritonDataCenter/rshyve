# Roadmap

Open work, grouped by theme. What ships today is in
[features.md](features.md). When an item lands, delete its line here and
describe the behavior in features.md.

Status: **planned** is accepted work. **considered** is a real gap with
no priority yet. **blocked** needs an illumos kernel change.

## Windows guests

Headless Windows Server boots today: the Server 2025 WinPE image boots
from an ISO on a read-only NVMe namespace and reaches SAC on COM1.

- **planned**: Validate Secure Boot enrollment and the Windows 11 and Server 2025 hardware checks on an illumos host.
- **planned**: Fill the SMBIOS Type 4 core and thread counts. They are zero now, and Windows Server licensing reads them.
- **planned**: Inject #GP for an unhandled RDMSR, as bhyve does. The VMM returns zero now, so `IA32_FEATURE_CONTROL` reads as a value no real CPU gives.
- **planned**: Migrate vTPM NV state.
- **considered**: Migration state for the framebuffer and the xHCI controller.
- **considered**: One ACPI generator. The fw_cfg table loader and the fixed guest-memory copy use separate functions that can drift.
- **considered**: A VM Generation Counter device, which Windows uses to detect a snapshot rollback.
- **considered**: A configurable RTC basis. The RTC is always UTC, and Windows reads it as local time by default.
- **considered**: A NIC with an inbox Windows driver. A new install has no network until the NetKVM driver is present.
- **considered**: A utility disk image with the EFI Shell and Secure Boot enrollment files for the FreeBSD firmware, which has no shell.

## Platform

- **planned**: virtio-scsi, for named LUNs, UNMAP and many targets.
- **planned**: virtio-balloon, for guest-cooperative memory reclaim.
- **planned**: Per-disk IOPS and bandwidth limits for NVMe and virtio-blk.
- **planned**: Hugepage guest memory, with a measured fallback.
- **planned**: A watchdog device (`i6300esb` or equivalent).
- **planned**: A versioned, introspectable control protocol in place of the fixed command set.
- **planned**: Storage migration that does not depend on ZFS zvols.
- **considered**: qcow2 backing files.

## Blocked on the illumos kernel

- **blocked**: Hyper-V Tier 2 (SynIC, synthetic timers, virtual APIC, TLB-flush and IPI hypercalls).
- **blocked**: CPU hot-remove. illumos has no `vm_deactivate_cpu`.
- **blocked**: Memory hot-remove. illumos has no `VM_FREE_MEMSEG`.
- **blocked**: A `VM_DESTROY_SELF` that can fail. `vmm_lease_block` waits for every vmm_drv lease in an untimed, uninterruptible `cv_wait`, so a lease that does not break keeps the process alive. The kernel must bound that wait or let it return `EINTR`.
- **blocked**: A bounded device reset. Interrupt injection takes the VM read lock, and illumos gives the lock to a waiting writer first. Taking the injection ioctls with `RW_READER_STARVEWRITER` would bound it.

[features.md](features.md#known-caveats) explains the last two.

## Validation

- Validate Hyper-V Tier 1 against an installed Windows Server workload.
- Install Windows Server with writable UEFI variables and `bootindex`, and confirm that the boot entry persists.
- Test migration of Hyper-V, framebuffer, xHCI and vTPM state as each one lands.
- Check the hotplug AML with `iasl`, and boot a `--hotplug` VM with more than one guest kernel configuration.
- Boot an illumos guest. Its virtio driver writes the status register once and never reads it back, which is the weakest contract any driver gives this VMM.
- Hold a vmm_drv lease that never breaks, and run the teardown budget and the shutdown watchdog against it.

## Out of scope

- TCG and architectures other than x86_64.
- SEV, SEV-ES, SEV-SNP and TDX. illumos `vmm.ko` has no hooks for them.
- Nested virtualization.
- Sound, legacy USB, ARM and RISC-V targets, and vhost-user.
