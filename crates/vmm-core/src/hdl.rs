// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Safe wrapper over a bhyve VMM instance file descriptor.
//!
//! All kernel ioctls are exposed as typed, safe methods. The raw ioctl
//! interface stays private. Add a new operation here as a typed method,
//! not through a generic ioctl escape hatch.

use std::io::{Error, ErrorKind, Result};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::Duration;

use bhyve_api::{
    ioctls, vcpu_cpuid_entry, vm_cap_type, vm_capability, vm_create_req,
    vm_isa_irq, vm_lapic_msi, vm_memmap, vm_memseg, vm_npt_operation,
    vm_pptdev, vm_pptdev_limits, vm_pptdev_mmio, vm_pptdev_msi, vm_rtc_data,
    vm_suspend, vm_suspend_how, vm_vcpu_cpuid_config, vmm_dirty_tracker,
    ApiVersion, VmmCtlFd, VmmFd, VCF_RESERVOIR_MEM, VCF_TRACK_DIRTY,
    VM_MAX_SEG_NAMELEN, VM_MEMMAP_F_WIRED, VNO_FLAG_BITMAP_IN,
    VNO_OP_SET_DIRTY,
};

use crate::common::PAGE_SIZE;
use crate::mem::Prot;

mod vcpu;

/// How a VM is asked to stop. The kernel latches the first request and
/// answers `EALREADY` to the rest.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SuspendHow {
    Reset,
    PowerOff,
    Halt,
}

impl SuspendHow {
    fn raw(self) -> u32 {
        let how = match self {
            SuspendHow::Reset => vm_suspend_how::VM_SUSPEND_RESET,
            SuspendHow::PowerOff => vm_suspend_how::VM_SUSPEND_POWEROFF,
            SuspendHow::Halt => vm_suspend_how::VM_SUSPEND_HALT,
        };
        how as u32
    }
}

impl std::fmt::Display for SuspendHow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SuspendHow::Reset => "reset",
            SuspendHow::PowerOff => "poweroff",
            SuspendHow::Halt => "halt",
        })
    }
}

/// What a suspend request did. The kernel latches the first request
/// for a VM and refuses the rest, and a refusal is not a failure: the
/// VM is stopping either way.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SuspendOutcome {
    Requested,
    AlreadyLatched,
}

/// MSI and MSI-X vector counts a passthrough device may use.
#[derive(Copy, Clone, Debug)]
pub struct PptdevLimits {
    pub msi: i32,
    pub msix: i32,
}

/// Options for creating a new VM instance.
#[derive(Debug, Clone, Default)]
pub struct CreateOpts {
    /// Force creation even if a VM with this name already exists.
    pub force: bool,
    /// Allocate memory from the VMM reservoir.
    pub use_reservoir: bool,
    /// Enable dirty page tracking (required for live migration).
    pub track_dirty: bool,
}

/// Handle to a bhyve VMM instance.
///
/// Wraps the kernel `/dev/vmm/{name}` file descriptor and provides safe
/// typed methods for all VM operations. The raw ioctl interface is
/// private, so all operations go through the methods on this type.
pub struct VmmHdl {
    inner: VmmFd,
    name: String,
}

impl VmmHdl {
    /// Create a new VM instance and open a handle to it.
    pub fn create(name: &str, opts: &CreateOpts) -> Result<Self> {
        let ctl = VmmCtlFd::open()?;

        if opts.force {
            // Best-effort destroy of any VM with this name. It fails when
            // no such VM exists.
            let _ = ctl.vm_destroy(name.as_bytes());
        }

        let mut req = vm_create_req::new(name.as_bytes())?;
        if opts.use_reservoir {
            req.flags |= VCF_RESERVOIR_MEM;
        }
        if opts.track_dirty {
            req.flags |= VCF_TRACK_DIRTY;
        }

        let mut retries = 0;
        loop {
            // Safety: vm_create_req is the correct struct for VMM_CREATE_VM.
            match unsafe { ctl.ioctl(ioctls::VMM_CREATE_VM, &mut req) } {
                Ok(_) => break,
                Err(err)
                    if opts.force
                        && matches!(
                            err.raw_os_error(),
                            Some(libc::EEXIST) | Some(libc::EBUSY)
                        )
                        && retries < 20 =>
                {
                    // A destroyed VM can remain visible until kernel teardown
                    // releases its final reference.
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(err),
            }
        }

        // VMM_CREATE_VM has succeeded, so a failed open leaves an instance
        // that no fd owns and no autodestruct can reclaim. Destroy it here.
        let fd = match VmmFd::open(name) {
            Ok(fd) => fd,
            Err(err) => {
                let _ = ctl.vm_destroy(name.as_bytes());
                return Err(err);
            }
        };
        Ok(Self {
            inner: fd,
            name: name.to_string(),
        })
    }

    /// Open an existing VM instance.
    pub fn open(name: &str) -> Result<Self> {
        let fd = VmmFd::open(name)?;
        Ok(Self {
            inner: fd,
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the bhyve API version from the kernel.
    pub fn api_version(&self) -> Result<u32> {
        self.inner.api_version()
    }

    // ---------------------------------------------------------------
    // Memory segment management
    // ---------------------------------------------------------------

    /// Allocate a guest memory segment in the kernel.
    ///
    /// Returns an error if the segment name exceeds the kernel's
    /// maximum length.
    pub fn create_memseg(
        &self,
        segid: i32,
        len: usize,
        name: &str,
    ) -> Result<()> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() >= VM_MAX_SEG_NAMELEN {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "segment name '{}' exceeds max length {}",
                    name,
                    VM_MAX_SEG_NAMELEN - 1,
                ),
            ));
        }

        let mut seg = vm_memseg {
            segid,
            len,
            name: [0u8; VM_MAX_SEG_NAMELEN],
        };
        seg.name[..name_bytes.len()].copy_from_slice(name_bytes);

        // Safety: vm_memseg is the correct struct for VM_ALLOC_MEMSEG.
        unsafe { self.inner.ioctl(ioctls::VM_ALLOC_MEMSEG, &mut seg) }?;
        Ok(())
    }

    /// Map a memory segment into the guest physical address space.
    pub fn map_memseg(
        &self,
        segid: i32,
        gpa: u64,
        len: usize,
        segoff: i64,
        prot: Prot,
    ) -> Result<()> {
        let mut map = vm_memmap {
            gpa,
            segid,
            segoff,
            len,
            prot: prot.bits(),
            flags: VM_MEMMAP_F_WIRED,
        };
        // Safety: vm_memmap is the correct struct for VM_MMAP_MEMSEG.
        unsafe { self.inner.ioctl(ioctls::VM_MMAP_MEMSEG, &mut map) }?;
        Ok(())
    }

    /// Get the mmap offset for a device memory segment.
    pub fn devmem_offset(&self, segid: i32) -> Result<i64> {
        let mut dmo = bhyve_api::vm_devmem_offset { segid, offset: 0 };
        // Safety: vm_devmem_offset is the correct struct.
        unsafe { self.inner.ioctl(ioctls::VM_DEVMEM_GETOFFSET, &mut dmo) }?;
        Ok(dmo.offset)
    }

    /// Remove a memory segment mapping from the guest physical address space.
    ///
    /// The kernel matches an exact `(gpa, len)` pair, so the caller must pass
    /// the same values used for [`Self::map_memseg`].
    pub fn munmap_memseg(&self, gpa: u64, len: usize) -> Result<()> {
        let mut arg = bhyve_api::vm_munmap { gpa, len };
        // Safety: vm_munmap is the correct struct for VM_MUNMAP_MEMSEG.
        unsafe { self.inner.ioctl(ioctls::VM_MUNMAP_MEMSEG, &mut arg) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // RTC
    // ---------------------------------------------------------------

    /// Set the time reported by the virtual RTC.
    pub fn rtc_settime(&self, time: Duration) -> Result<()> {
        self.inner.rtc_settime(time)
    }

    /// Write a byte to the virtual RTC at the given offset.
    pub fn rtc_write(&self, offset: i32, value: u8) -> Result<()> {
        let mut data = vm_rtc_data { offset, value };
        // Safety: vm_rtc_data is the correct struct for VM_RTC_WRITE.
        unsafe { self.inner.ioctl(ioctls::VM_RTC_WRITE, &mut data) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Interrupts
    // ---------------------------------------------------------------

    /// Assert an ISA IRQ (edge: assert + deassert).
    pub fn isa_assert_irq(
        &self,
        atpic_irq: i32,
        ioapic_irq: i32,
    ) -> Result<()> {
        self.isa_irq_op(ioctls::VM_ISA_ASSERT_IRQ, atpic_irq, ioapic_irq)
    }

    /// Deassert an ISA IRQ.
    pub fn isa_deassert_irq(
        &self,
        atpic_irq: i32,
        ioapic_irq: i32,
    ) -> Result<()> {
        self.isa_irq_op(ioctls::VM_ISA_DEASSERT_IRQ, atpic_irq, ioapic_irq)
    }

    /// Set an ISA IRQ trigger mode (edge or level).
    pub fn isa_set_irq_trigger(
        &self,
        irq: i32,
        level_trigger: bool,
    ) -> Result<()> {
        let mut trigger = bhyve_api::vm_isa_irq_trigger {
            atpic_irq: irq,
            trigger: if level_trigger { 1 } else { 0 },
        };
        // Safety: vm_isa_irq_trigger is the correct struct.
        unsafe {
            self.inner
                .ioctl(ioctls::VM_ISA_SET_IRQ_TRIGGER, &mut trigger)
        }?;
        Ok(())
    }

    /// Pulse an ISA IRQ (assert then immediately deassert).
    pub fn isa_pulse_irq(&self, atpic_irq: i32, ioapic_irq: i32) -> Result<()> {
        self.isa_irq_op(ioctls::VM_ISA_PULSE_IRQ, atpic_irq, ioapic_irq)
    }

    fn isa_irq_op(
        &self,
        cmd: i32,
        atpic_irq: i32,
        ioapic_irq: i32,
    ) -> Result<()> {
        let mut irq = vm_isa_irq {
            atpic_irq,
            ioapic_irq,
        };
        // Safety: vm_isa_irq is the correct struct for ISA IRQ ioctls.
        unsafe { self.inner.ioctl(cmd, &mut irq) }?;
        Ok(())
    }

    /// Assert an IOAPIC IRQ (for PCI device GSI delivery).
    pub fn ioapic_assert_irq(&self, irq: i32) -> Result<()> {
        let mut req = bhyve_api::vm_ioapic_irq { irq };
        unsafe { self.inner.ioctl(ioctls::VM_IOAPIC_ASSERT_IRQ, &mut req) }?;
        Ok(())
    }

    /// Deassert an IOAPIC IRQ.
    pub fn ioapic_deassert_irq(&self, irq: i32) -> Result<()> {
        let mut req = bhyve_api::vm_ioapic_irq { irq };
        unsafe { self.inner.ioctl(ioctls::VM_IOAPIC_DEASSERT_IRQ, &mut req) }?;
        Ok(())
    }

    /// Deliver a LAPIC MSI (message signaled interrupt).
    pub fn lapic_msi(&self, addr: u64, msg: u64) -> Result<()> {
        let mut msi = vm_lapic_msi { addr, msg };
        // Safety: vm_lapic_msi is the correct struct for VM_LAPIC_MSI.
        unsafe { self.inner.ioctl(ioctls::VM_LAPIC_MSI, &mut msi) }?;
        Ok(())
    }

    /// Inject an interrupt directly into a vCPU's LAPIC.
    ///
    /// Sets the IRR bit for the given vector and kicks the vCPU
    /// out of HLT via vcpu_notify_event(). More direct than
    /// lapic_msi() for targeted vCPU interrupt delivery.
    pub fn lapic_irq(&self, cpuid: i32, vector: i32) -> Result<()> {
        let mut irq = bhyve_api::vm_lapic_irq { cpuid, vector };
        unsafe { self.inner.ioctl(ioctls::VM_LAPIC_IRQ, &mut irq) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // VM capabilities and lifecycle
    // ---------------------------------------------------------------

    /// Set the CPU topology (sockets, cores, threads per core).
    pub fn set_topology(
        &self,
        sockets: u16,
        cores: u16,
        threads: u16,
        maxcpus: u16,
    ) -> Result<()> {
        // struct vm_cpu_topology { u16 sockets, cores, threads, maxcpus }
        let mut topo: [u16; 4] = [sockets, cores, threads, maxcpus];
        // Safety: passing a 4×u16 struct matching vm_cpu_topology.
        unsafe {
            self.inner.ioctl(ioctls::VM_SET_TOPOLOGY, topo.as_mut_ptr())
        }?;
        Ok(())
    }

    /// Make one vCPU exit to userspace on HLT.
    pub fn set_halt_exit(&self, cpuid: i32, enable: bool) -> Result<()> {
        let mut cap = vm_capability {
            cpuid,
            captype: vm_cap_type::VM_CAP_HALT_EXIT as i32,
            capval: i32::from(enable),
            allcpus: 0,
        };
        // Safety: vm_capability is the correct struct.
        unsafe { self.inner.ioctl(ioctls::VM_SET_CAPABILITY, &mut cap) }?;
        Ok(())
    }

    /// Pause the VM (all vCPUs and kernel timers).
    pub fn pause(&self) -> Result<()> {
        self.inner.ioctl_usize(ioctls::VM_PAUSE, 0)?;
        Ok(())
    }

    /// Resume the VM after pause.
    pub fn resume(&self) -> Result<()> {
        self.inner.ioctl_usize(ioctls::VM_RESUME, 0)?;
        Ok(())
    }

    /// Request VM destruction.
    pub fn destroy(&self) -> Result<()> {
        self.inner.ioctl_usize(ioctls::VM_DESTROY_SELF, 0)?;
        Ok(())
    }

    /// Arm or disarm destruction of the instance when the last handle
    /// closes.
    ///
    /// The kernel sets the VMM_AUTODESTROY flag for a non-zero argument
    /// and clears it for zero (`VM_SET_AUTODESTRUCT` in `vmm_sol_dev.c`).
    /// A zero disarms the only reclaimer that survives a crash or a
    /// SIGKILL.
    pub fn set_autodestruct(&self, enable: bool) -> Result<()> {
        self.inner
            .ioctl_usize(ioctls::VM_SET_AUTODESTRUCT, usize::from(enable))?;
        Ok(())
    }

    /// Suspend the VM. `source` is the requesting vCPU, or -1 for a
    /// device or operator request.
    pub fn suspend(
        &self,
        how: SuspendHow,
        source: i32,
    ) -> Result<SuspendOutcome> {
        let mut req = vm_suspend {
            how: how.raw(),
            source,
        };
        // Safety: vm_suspend is the correct struct for VM_SUSPEND.
        match unsafe { self.inner.ioctl(ioctls::VM_SUSPEND, &mut req) } {
            Ok(_) => Ok(SuspendOutcome::Requested),
            Err(e) if e.raw_os_error() == Some(libc::EALREADY) => {
                Ok(SuspendOutcome::AlreadyLatched)
            }
            Err(e) => Err(e),
        }
    }

    /// Tell the kernel which PIO port serves the ACPI PM timer.
    pub fn pmtmr_locate(&self, port: u16) -> Result<()> {
        self.inner
            .ioctl_usize(ioctls::VM_PMTMR_LOCATE, usize::from(port))?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // PCI passthrough
    // ---------------------------------------------------------------

    /// Bind an open `/dev/pptN` descriptor to this VM.
    pub fn bind_pptdev(&self, pptfd: BorrowedFd<'_>) -> Result<()> {
        let mut req = vm_pptdev {
            pptfd: pptfd.as_raw_fd(),
        };
        // Safety: vm_pptdev is the correct struct for VM_BIND_PPTDEV.
        unsafe { self.inner.ioctl(ioctls::VM_BIND_PPTDEV, &mut req) }?;
        Ok(())
    }

    /// Give a bound passthrough device back to the host.
    pub fn unbind_pptdev(&self, pptfd: BorrowedFd<'_>) -> Result<()> {
        let mut req = vm_pptdev {
            pptfd: pptfd.as_raw_fd(),
        };
        // Safety: vm_pptdev is the correct struct for VM_UNBIND_PPTDEV.
        unsafe { self.inner.ioctl(ioctls::VM_UNBIND_PPTDEV, &mut req) }?;
        Ok(())
    }

    /// Ask how many MSI and MSI-X vectors a bound device may use.
    pub fn pptdev_limits(&self, pptfd: BorrowedFd<'_>) -> Result<PptdevLimits> {
        let mut req = vm_pptdev_limits {
            pptfd: pptfd.as_raw_fd(),
            msi_limit: 0,
            msix_limit: 0,
        };
        // Safety: vm_pptdev_limits is correct for VM_GET_PPTDEV_LIMITS.
        unsafe { self.inner.ioctl(ioctls::VM_GET_PPTDEV_LIMITS, &mut req) }?;
        Ok(PptdevLimits {
            msi: req.msi_limit,
            msix: req.msix_limit,
        })
    }

    /// Map `len` bytes of a device BAR at host physical `hpa` into the
    /// guest at `gpa`.
    pub fn map_pptdev_mmio(
        &self,
        pptfd: BorrowedFd<'_>,
        gpa: u64,
        hpa: u64,
        len: usize,
    ) -> Result<()> {
        let mut req = vm_pptdev_mmio {
            pptfd: pptfd.as_raw_fd(),
            gpa,
            hpa,
            len,
        };
        // Safety: vm_pptdev_mmio is correct for VM_MAP_PPTDEV_MMIO.
        unsafe { self.inner.ioctl(ioctls::VM_MAP_PPTDEV_MMIO, &mut req) }?;
        Ok(())
    }

    /// Undo [`map_pptdev_mmio`](Self::map_pptdev_mmio) for `gpa`.
    pub fn unmap_pptdev_mmio(
        &self,
        pptfd: BorrowedFd<'_>,
        gpa: u64,
        len: usize,
    ) -> Result<()> {
        let mut req = vm_pptdev_mmio {
            pptfd: pptfd.as_raw_fd(),
            gpa,
            hpa: 0,
            len,
        };
        // Safety: vm_pptdev_mmio is correct for VM_UNMAP_PPTDEV_MMIO.
        unsafe { self.inner.ioctl(ioctls::VM_UNMAP_PPTDEV_MMIO, &mut req) }?;
        Ok(())
    }

    /// Point a bound device's MSI at `addr`/`msg`. `numvec` of 0 turns
    /// MSI off.
    pub fn pptdev_msi(
        &self,
        pptfd: BorrowedFd<'_>,
        addr: u64,
        msg: u64,
        numvec: i32,
    ) -> Result<()> {
        let mut req = vm_pptdev_msi {
            vcpu: 0,
            pptfd: pptfd.as_raw_fd(),
            numvec,
            msg,
            addr,
        };
        // Safety: vm_pptdev_msi is correct for VM_PPTDEV_MSI.
        unsafe { self.inner.ioctl(ioctls::VM_PPTDEV_MSI, &mut req) }?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Dirty page tracking (for live migration)
    // ---------------------------------------------------------------

    /// Read and clear the dirty bits of a memory region.
    ///
    /// Each bit of `bitmap` stands for one 4 KiB page, so it needs
    /// `ceil(len / 4096 / 8)` bytes. The region is walked in
    /// [`DIRTY_CHUNK`] pieces because the kernel refuses more per call.
    pub fn track_dirty_pages(
        &self,
        start_gpa: u64,
        len: usize,
        bitmap: &mut [u8],
    ) -> Result<()> {
        for chunk in dirty_chunks(start_gpa, len, bitmap.len())? {
            let mut tracker = vmm_dirty_tracker {
                vdt_start_gpa: chunk.gpa,
                vdt_len: chunk.len,
                // Safety: the slice outlives this ioctl, which blocks
                // until the kernel has copied the bits out.
                vdt_pfns: bitmap[chunk.bits.clone()].as_mut_ptr() as *mut _,
            };
            // Safety: vmm_dirty_tracker is the correct struct.
            unsafe {
                self.inner.ioctl(ioctls::VM_TRACK_DIRTY_PAGES, &mut tracker)
            }?;
        }
        Ok(())
    }

    /// Mark pages dirty in the NPT, for a migration that has to send
    /// them again.
    ///
    /// `bitmap` has the same shape as for [`Self::track_dirty_pages`].
    /// Requires bhyve API version 17 or later.
    pub fn set_dirty_pages(
        &self,
        gpa: u64,
        len: usize,
        bitmap: &[u8],
    ) -> Result<()> {
        let api_ver = self.api_version()?;
        if api_ver < ApiVersion::V17 as u32 {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("set_dirty_pages requires API v17+, have v{}", api_ver,),
            ));
        }

        for chunk in dirty_chunks(gpa, len, bitmap.len())? {
            let mut op = vm_npt_operation {
                vno_gpa: chunk.gpa,
                vno_len: chunk.len as u64,
                // Safety: the slice outlives this ioctl. The kernel only
                // reads through the pointer with VNO_FLAG_BITMAP_IN.
                vno_bitmap: bitmap[chunk.bits.clone()].as_ptr() as *mut u8,
                vno_operation: VNO_OP_SET_DIRTY | VNO_FLAG_BITMAP_IN,
            };
            // Safety: vm_npt_operation is the correct struct.
            unsafe { self.inner.ioctl(ioctls::VM_NPT_OPERATION, &mut op) }?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------
    // VMM data interface (state export/import for migration)
    // ---------------------------------------------------------------

    /// Build a vmm-data read/write operation.
    pub fn data_op(
        &self,
        class: u16,
        version: u16,
    ) -> bhyve_api::VmmDataOp<'_> {
        self.inner.data_op(class, version)
    }

    // ---------------------------------------------------------------
    // CPUID control
    // ---------------------------------------------------------------

    /// Set custom CPUID entries for a vCPU.
    ///
    /// The entries must be sorted by function/index (use
    /// `vcpu_cpuid_entry::eval_sort`). The kernel uses these entries to
    /// emulate CPUID for the guest instead of passing through host
    /// values.
    ///
    /// Requires bhyve API version >= 5.
    pub fn set_cpuid(
        &self,
        vcpuid: i32,
        entries: &mut [vcpu_cpuid_entry],
        flags: u32,
    ) -> Result<()> {
        let mut config = vm_vcpu_cpuid_config {
            vvcc_vcpuid: vcpuid,
            vvcc_flags: flags,
            vvcc_nent: entries.len() as u32,
            _pad: 0,
            vvcc_entries: entries.as_mut_ptr() as *mut std::ffi::c_void,
        };
        unsafe { self.inner.ioctl(ioctls::VM_SET_CPUID, &mut config) }?;
        Ok(())
    }
}

/// Most guest memory one dirty-bitmap ioctl may cover.
///
/// The kernel caps `VM_TRACK_DIRTY_PAGES` at a bitmap of eight pages,
/// which is 1 GiB of guest memory, and `VM_NPT_OPERATION` at the same
/// bitmap size (`vmm_sol_dev.c`, `max_track_region_len` and
/// `max_bitmap_size`).
pub const DIRTY_CHUNK: usize = 8 * PAGE_SIZE * 8 * PAGE_SIZE;

/// One ioctl's worth of a dirty-bitmap walk.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DirtyChunk {
    gpa: u64,
    len: usize,
    /// The bytes of the caller's bitmap this piece fills.
    bits: std::ops::Range<usize>,
}

/// Split `[start_gpa, start_gpa + len)` into pieces the kernel accepts.
///
/// Checks what the kernel would refuse with a bare `EINVAL`, and that
/// `bitmap_len` bytes hold a bit for every page, so no ioctl ever
/// copies past the caller's buffer.
fn dirty_chunks(
    start_gpa: u64,
    len: usize,
    bitmap_len: usize,
) -> Result<Vec<DirtyChunk>> {
    let page = PAGE_SIZE as u64;
    if !start_gpa.is_multiple_of(page) || !len.is_multiple_of(PAGE_SIZE) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "dirty range [{start_gpa:#x}, +{len:#x}) is not page aligned"
            ),
        ));
    }
    let end = start_gpa.checked_add(len as u64).ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, "dirty range overflows")
    })?;
    let pages = len / PAGE_SIZE;
    let required = pages.div_ceil(8);
    if bitmap_len < required {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "bitmap too small: need {required} bytes for {pages} pages, \
                 got {bitmap_len}"
            ),
        ));
    }

    let bits_per_chunk = DIRTY_CHUNK / PAGE_SIZE / 8;
    let mut chunks = Vec::with_capacity(len.div_ceil(DIRTY_CHUNK));
    let mut gpa = start_gpa;
    let mut bit_off = 0;
    while gpa < end {
        let chunk_len = DIRTY_CHUNK.min((end - gpa) as usize);
        let bit_len = (chunk_len / PAGE_SIZE).div_ceil(8);
        chunks.push(DirtyChunk {
            gpa,
            len: chunk_len,
            bits: bit_off..bit_off + bit_len,
        });
        gpa += chunk_len as u64;
        bit_off += bits_per_chunk;
    }
    Ok(chunks)
}

impl AsRawFd for VmmHdl {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsFd for VmmHdl {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
    }
}

#[cfg(test)]
mod dirty_chunk_tests {
    use super::{dirty_chunks, DIRTY_CHUNK};
    use crate::common::PAGE_SIZE;

    #[test]
    fn a_region_over_one_gib_is_split_at_the_kernel_cap() {
        // A 4 GiB guest has a 3 GiB low region. One ioctl for it gets
        // EINVAL, so the walk has to hand the kernel three pieces whose
        // bitmap slices abut.
        let len = 3 * DIRTY_CHUNK;
        let chunks = dirty_chunks(0, len, len / PAGE_SIZE / 8).unwrap();
        assert_eq!(chunks.len(), 3);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.gpa, (i * DIRTY_CHUNK) as u64);
            assert_eq!(c.len, DIRTY_CHUNK);
            assert_eq!(c.bits, i * 0x8000..(i + 1) * 0x8000);
        }
    }

    #[test]
    fn the_last_piece_keeps_the_remainder() {
        let len = DIRTY_CHUNK + 3 * PAGE_SIZE;
        let chunks =
            dirty_chunks(0x1_0000_0000, len, (len / PAGE_SIZE).div_ceil(8))
                .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].gpa, 0x1_0000_0000 + DIRTY_CHUNK as u64);
        assert_eq!(chunks[1].len, 3 * PAGE_SIZE);
        assert_eq!(chunks[1].bits, 0x8000..0x8001);
    }

    #[test]
    fn a_short_bitmap_is_refused_before_any_ioctl() {
        let len = 16 * PAGE_SIZE;
        assert!(dirty_chunks(0, len, 1).is_err());
        assert!(dirty_chunks(0, len, 2).is_ok());
    }

    #[test]
    fn an_unaligned_range_is_refused_with_a_reason() {
        assert!(dirty_chunks(1, PAGE_SIZE, 1).is_err());
        assert!(dirty_chunks(0, PAGE_SIZE + 1, 1).is_err());
        assert!(dirty_chunks(u64::MAX - 0xFFF, PAGE_SIZE * 2, 1).is_err());
    }

    #[test]
    fn an_empty_range_needs_no_ioctl() {
        assert!(dirty_chunks(0, 0, 0).unwrap().is_empty());
    }
}
