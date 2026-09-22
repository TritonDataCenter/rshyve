// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The modern (VirtIO 1.0+) face of the transitional device.
//!
//! A modern driver finds the register file through the PCI capability
//! chain. Each capability names a BAR and an offset in it. This module
//! owns both ends of that map.
//! [`cap_chain_read`](VirtioPciDevice::cap_chain_read) publishes the
//! offsets and lengths, and `modern_bar_read` and `modern_bar_write`
//! decode BAR2 at them. The two must describe the same layout. A
//! capability that points at a page the decoder does not serve gives
//! the driver a register file of zeroes: a device with no features and
//! no queues. Linux `map_capability()` refuses a length the BAR cannot
//! hold, and the probe fails.
//!
//! The write side takes the reset latch through
//! [`lock_for_write`](VirtioPciDevice::lock_for_write), as the legacy
//! side does, so a register write cannot land on a half-reset device.

use super::bits;
use super::{VirtioDevice, VirtioPciDevice};

/// Offsets of virtio PCI capabilities in config space.
pub(super) struct CapOffsets {
    /// First capability offset (either MSI-X or first virtio cap).
    pub(super) first: u8,
    /// MSI-X capability offset (0 if no MSI-X).
    msix: u8,
    /// Common config capability offset.
    common: u8,
    /// Notify capability offset.
    notify: u8,
    /// ISR capability offset.
    isr: u8,
    /// Device config capability offset.
    device: u8,
}

impl CapOffsets {
    pub(super) fn new(has_msix: bool) -> Self {
        if has_msix {
            // MSI-X cap (12 bytes) at 0x40, then virtio caps after
            Self {
                first: 0x40,
                msix: 0x40,
                common: 0x4C,
                notify: 0x5C, // 0x4C + 16
                isr: 0x70,    // 0x5C + 20
                device: 0x80, // 0x70 + 16
            }
        } else {
            Self {
                first: 0x40,
                msix: 0,
                common: 0x40,
                notify: 0x50, // 0x40 + 16
                isr: 0x64,    // 0x50 + 20
                device: 0x74, // 0x64 + 16
            }
        }
    }
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// Features advertised through the modern transport path.
    ///
    /// Device features include EVENT_IDX and INDIRECT_DESC for devices
    /// that support them (block, rng). Viona strips EVENT_IDX because
    /// it handles rings in the kernel.
    fn modern_features(&self) -> u64 {
        self.device.device_features() | bits::VIRTIO_F_VERSION_1
    }

    /// Whether this device presents a VIRTIO_PCI_CAP_DEVICE_CFG.
    ///
    /// VIRTIO 1.3 4.1.4.6 requires the capability only when the device
    /// has device-specific configuration. virtio-rng has none. Linux
    /// `map_capability()` rejects a length-0 capability (`length <=
    /// start`) and fails the probe with -EINVAL, so the device never
    /// binds.
    fn has_device_cfg_cap(&self) -> bool {
        self.dev_config_size != 0
    }

    /// Read a dword from a virtio PCI capability structure.
    ///
    /// Returns `Some(val)` if `offset` falls within a virtio or MSI-X
    /// capability, `None` otherwise.
    pub(super) fn cap_chain_read(&self, offset: u8) -> Option<u32> {
        let co = &self.cap_offsets;

        // MSI-X capability (12 bytes)
        if co.msix != 0 && offset >= co.msix && offset < co.msix + 12 {
            if let Some(ref msix) = self.msix {
                let mut val = msix.cap_read(offset, co.msix)?;
                // Patch next-pointer: MSI-X byte 1 → first virtio cap
                if offset == co.msix {
                    val = (val & !0x0000_FF00) | (u32::from(co.common) << 8);
                }
                return Some(val);
            }
        }

        // Virtio common config cap (16 bytes)
        if offset >= co.common && offset < co.common + 16 {
            return Some(self.read_virtio_cap(
                offset - co.common,
                bits::VIRTIO_PCI_CAP_COMMON_CFG,
                co.notify,
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_COMMON_OFFSET,
                bits::COMMON_CFG_SIZE as u32,
            ));
        }

        // Virtio notify cap (20 bytes)
        if offset >= co.notify && offset < co.notify + 20 {
            let rel = offset - co.notify;
            if rel >= 16 {
                // Extra dword: notify_off_multiplier = 0
                return Some(0);
            }
            return Some(self.read_virtio_cap(
                rel,
                bits::VIRTIO_PCI_CAP_NOTIFY_CFG,
                co.isr,
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_NOTIFY_OFFSET,
                4,
            ));
        }

        // Virtio ISR cap (16 bytes)
        if offset >= co.isr && offset < co.isr + 16 {
            return Some(self.read_virtio_cap(
                offset - co.isr,
                bits::VIRTIO_PCI_CAP_ISR_CFG,
                // With no device-specific config there is no device-config
                // capability, so ISR ends the chain.
                if self.has_device_cfg_cap() {
                    co.device
                } else {
                    0
                },
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_ISR_OFFSET,
                4,
            ));
        }

        // Virtio device config cap (16 bytes)
        if self.has_device_cfg_cap()
            && offset >= co.device
            && offset < co.device + 16
        {
            return Some(self.read_virtio_cap(
                offset - co.device,
                bits::VIRTIO_PCI_CAP_DEVICE_CFG,
                0, // last in chain
                bits::MODERN_BAR_IDX,
                bits::MODERN_BAR_DEVICE_OFFSET,
                u32::from(self.dev_config_size),
            ));
        }

        None
    }

    /// Build a dword from a virtio_pci_cap structure at relative offset.
    fn read_virtio_cap(
        &self,
        rel: u8,
        cfg_type: u8,
        next: u8,
        bar: u8,
        offset: u32,
        length: u32,
    ) -> u32 {
        let dword_off = rel & 0xFC;
        match dword_off {
            // Byte 0: cap_vndr=0x09, Byte 1: cap_next, Byte 2: cap_len,
            // Byte 3: cfg_type
            0 => {
                let cap_len = if cfg_type == bits::VIRTIO_PCI_CAP_NOTIFY_CFG {
                    bits::VIRTIO_PCI_NOTIFY_CAP_SIZE
                } else {
                    bits::VIRTIO_PCI_CAP_SIZE
                };
                u32::from(bits::PCI_CAP_ID_VNDR)
                    | (u32::from(next) << 8)
                    | (u32::from(cap_len) << 16)
                    | (u32::from(cfg_type) << 24)
            }
            // Byte 4: bar, Byte 5: id(0), Bytes 6-7: padding
            4 => u32::from(bar),
            // Bytes 8-11: offset within BAR
            8 => offset,
            // Bytes 12-15: length
            12 => length,
            _ => 0,
        }
    }

    /// Handle writes to virtio PCI capabilities (mostly read-only).
    pub(super) fn cap_chain_write(&self, offset: u8, val: u32) {
        let co = &self.cap_offsets;
        // Only MSI-X has writable fields
        if co.msix != 0 && offset >= co.msix && offset < co.msix + 12 {
            if let Some(ref msix) = self.msix {
                // Sample before the write removes the messages from
                // the pending array, so any reset from here on comes
                // after this sample.
                let session = self.intr.session();
                let released = msix.cap_write_deferred(offset, co.msix, val);
                self.deliver_released(session, msix, released);
            }
        }
        // All virtio cap fields are read-only
    }

    /// Handle a read from the modern config BAR (BAR2).
    pub(super) fn modern_bar_read(&self, offset: u16, len: usize) -> u32 {
        let page = offset & 0xF000;
        let reg = offset & 0x0FFF;

        match page {
            0x0000 => self.modern_common_read(reg, len),
            0x1000 => self.device.cfg_read(reg, len as u8),
            0x2000 => 0, // notify is write-only
            0x3000 => u32::from(self.read_isr()),
            _ => 0,
        }
    }
    /// Handle a write to the modern config BAR (BAR2).
    pub(super) fn modern_bar_write(&self, offset: u16, val: u32, len: usize) {
        let page = offset & 0xF000;
        let reg = offset & 0x0FFF;

        match page {
            0x0000 => self.modern_common_write(reg, val, len),
            0x1000 => {
                // Device config runs under the transport lock, as
                // common config does, so the write cannot land during
                // a reset. The named guard holds the lock across the
                // call.
                let Some(_vs) = self.lock_for_write() else {
                    return;
                };
                self.device.cfg_write(reg, val, len as u8);
            }
            0x2000 => {
                // Notification register: guest writes queue index
                let queue_idx = val as u16;
                let Some(mut vs) = self.lock_for_write() else {
                    return;
                };
                if !Self::can_notify(&vs, queue_idx) {
                    return;
                }
                let did_work = self.device.notify_queue(
                    queue_idx,
                    &mut vs.queues,
                    &self.physmap,
                );
                if Self::queue_needs_reset(&vs, queue_idx) {
                    // The queue logged its own reason when it refused.
                    self.refuse_queue(vs, queue_idx as usize, None);
                    return;
                }
                if did_work {
                    // Sample under the guard, so a reset that ends this
                    // session comes after this sample.
                    let session = self.intr.session();
                    drop(vs);
                    self.raise_queue_interrupt_in(session, queue_idx);
                }
            }
            0x3000 => {} // ISR is read-only
            _ => {}
        }
    }

    /// Read from the modern common configuration structure.
    fn modern_common_read(&self, offset: u16, _len: usize) -> u32 {
        let vs = self.virtio_state.lock().expect("virtio lock");

        match offset {
            bits::COMMON_CFG_DEVICE_FEATURE_SELECT => vs.device_feature_select,
            bits::COMMON_CFG_DEVICE_FEATURE => {
                let feats = self.modern_features();
                match vs.device_feature_select {
                    0 => feats as u32,
                    1 => (feats >> 32) as u32,
                    _ => 0,
                }
            }
            bits::COMMON_CFG_DRIVER_FEATURE_SELECT => vs.driver_feature_select,
            bits::COMMON_CFG_DRIVER_FEATURE => match vs.driver_feature_select {
                0 => vs.guest_features as u32,
                1 => (vs.guest_features >> 32) as u32,
                _ => 0,
            },
            bits::COMMON_CFG_MSIX_CONFIG => u32::from(vs.config_msix_vector),
            bits::COMMON_CFG_NUM_QUEUES => u32::from(self.num_queues),
            bits::COMMON_CFG_DEVICE_STATUS => u32::from(vs.readable_status()),
            bits::COMMON_CFG_CONFIG_GENERATION => {
                u32::from(vs.config_generation)
            }
            bits::COMMON_CFG_QUEUE_SELECT => u32::from(vs.queue_select),
            bits::COMMON_CFG_QUEUE_SIZE => vs
                .selected_queue()
                .map(|q| u32::from(q.size()))
                .unwrap_or(0),
            bits::COMMON_CFG_QUEUE_MSIX_VECTOR => {
                let idx = vs.queue_select as usize;
                u32::from(
                    vs.queue_msix_vectors
                        .get(idx)
                        .copied()
                        .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR),
                )
            }
            bits::COMMON_CFG_QUEUE_ENABLE => {
                let idx = vs.queue_select as usize;
                u32::from(
                    vs.queue_enabled.get(idx).copied().unwrap_or(false) as u16
                )
            }
            bits::COMMON_CFG_QUEUE_NOTIFY_OFF => {
                // With multiplier=0, all queues notify at the same offset
                u32::from(vs.queue_select)
            }
            bits::COMMON_CFG_QUEUE_DESC_LO => {
                let idx = vs.queue_select as usize;
                vs.queue_desc.get(idx).copied().unwrap_or(0) as u32
            }
            bits::COMMON_CFG_QUEUE_DESC_HI => {
                let idx = vs.queue_select as usize;
                (vs.queue_desc.get(idx).copied().unwrap_or(0) >> 32) as u32
            }
            bits::COMMON_CFG_QUEUE_AVAIL_LO => {
                let idx = vs.queue_select as usize;
                vs.queue_avail.get(idx).copied().unwrap_or(0) as u32
            }
            bits::COMMON_CFG_QUEUE_AVAIL_HI => {
                let idx = vs.queue_select as usize;
                (vs.queue_avail.get(idx).copied().unwrap_or(0) >> 32) as u32
            }
            bits::COMMON_CFG_QUEUE_USED_LO => {
                let idx = vs.queue_select as usize;
                vs.queue_used.get(idx).copied().unwrap_or(0) as u32
            }
            bits::COMMON_CFG_QUEUE_USED_HI => {
                let idx = vs.queue_select as usize;
                (vs.queue_used.get(idx).copied().unwrap_or(0) >> 32) as u32
            }
            _ => 0,
        }
    }

    /// Write to the modern common configuration structure.
    fn modern_common_write(&self, offset: u16, val: u32, _len: usize) {
        let Some(mut vs) = self.lock_for_write() else {
            return;
        };

        match offset {
            bits::COMMON_CFG_DEVICE_FEATURE_SELECT => {
                vs.device_feature_select = val;
            }
            bits::COMMON_CFG_DRIVER_FEATURE_SELECT => {
                vs.driver_feature_select = val;
            }
            bits::COMMON_CFG_DRIVER_FEATURE => {
                // Negotiation ends at FEATURES_OK (VirtIO 1.3 sec
                // 2.2.1). Only a reset allows renegotiation. A feature
                // half written after FEATURES_OK would reach the
                // backend and the rings while the driver uses them.
                if vs.status & bits::STATUS_FEATURES_OK != 0 {
                    return;
                }
                let offered = self.modern_features();
                match vs.driver_feature_select {
                    0 => {
                        let accepted = val & (offered as u32);
                        vs.guest_features = (vs.guest_features
                            & 0xFFFF_FFFF_0000_0000)
                            | u64::from(accepted);
                    }
                    1 => {
                        let accepted = val & ((offered >> 32) as u32);
                        vs.guest_features = (vs.guest_features
                            & 0x0000_0000_FFFF_FFFF)
                            | (u64::from(accepted) << 32);
                    }
                    _ => {}
                }
            }
            bits::COMMON_CFG_MSIX_CONFIG => {
                vs.config_msix_vector = val as u16;
                self.msix_config_vector
                    .store(val as u16, std::sync::atomic::Ordering::Release);
            }
            bits::COMMON_CFG_DEVICE_STATUS => {
                let new_status = val as u8;
                if new_status == 0 {
                    self.reset_device(vs);
                } else {
                    self.set_status(vs, new_status);
                }
            }
            bits::COMMON_CFG_QUEUE_SELECT => {
                vs.queue_select = val as u16;
            }
            bits::COMMON_CFG_QUEUE_SIZE => {
                let idx = usize::from(vs.queue_select);
                let verdict =
                    vs.selected_queue_mut().map(|q| q.set_size(val as u16));
                if let Some(Err(error)) = verdict {
                    if self.set_needs_reset(vs) {
                        slog::debug!(self.log, "virtio-pci refused a queue size";
                            "queue" => idx, "reason" => %error);
                    }
                }
            }
            bits::COMMON_CFG_QUEUE_MSIX_VECTOR => {
                let idx = vs.queue_select as usize;
                if idx < vs.queue_msix_vectors.len() {
                    vs.queue_msix_vectors[idx] = val as u16;
                    if let Some(a) = self.msix_queue_vectors.get(idx) {
                        a.store(
                            val as u16,
                            std::sync::atomic::Ordering::Release,
                        );
                    }
                }
            }
            bits::COMMON_CFG_QUEUE_ENABLE => {
                let idx = vs.queue_select as usize;
                if idx < vs.queue_enabled.len() && val == 1 {
                    let desc = vs.queue_desc[idx];
                    let avail = vs.queue_avail[idx];
                    let used = vs.queue_used[idx];
                    let log = self.log.clone();
                    // Refuse rings the device cannot reach. Otherwise
                    // every later access fails silently.
                    let verdict = vs.queues.get_mut(idx).map(|q| {
                        q.set_log(log);
                        q.set_addr_modern(desc, avail, used);
                        q.validate_addrs(&self.physmap)
                    });
                    match verdict {
                        None => {}
                        Some(Err(error)) => {
                            if let Some(q) = vs.queues.get_mut(idx) {
                                q.reset();
                            }
                            self.refuse_queue(vs, idx, Some(error));
                        }
                        Some(Ok(())) => {
                            vs.queue_enabled[idx] = true;
                            if let Some(q) = vs.queues.get(idx) {
                                self.device.queue_addr_set(idx as u16, q);
                            }
                        }
                    }
                }
            }
            bits::COMMON_CFG_QUEUE_DESC_LO => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_desc.get_mut(idx) {
                    *a = (*a & 0xFFFF_FFFF_0000_0000) | u64::from(val);
                }
            }
            bits::COMMON_CFG_QUEUE_DESC_HI => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_desc.get_mut(idx) {
                    *a = (*a & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
                }
            }
            bits::COMMON_CFG_QUEUE_AVAIL_LO => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_avail.get_mut(idx) {
                    *a = (*a & 0xFFFF_FFFF_0000_0000) | u64::from(val);
                }
            }
            bits::COMMON_CFG_QUEUE_AVAIL_HI => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_avail.get_mut(idx) {
                    *a = (*a & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
                }
            }
            bits::COMMON_CFG_QUEUE_USED_LO => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_used.get_mut(idx) {
                    *a = (*a & 0xFFFF_FFFF_0000_0000) | u64::from(val);
                }
            }
            bits::COMMON_CFG_QUEUE_USED_HI => {
                let idx = vs.queue_select as usize;
                if let Some(a) = vs.queue_used.get_mut(idx) {
                    *a = (*a & 0x0000_0000_FFFF_FFFF) | (u64::from(val) << 32);
                }
            }
            _ => {}
        }
    }
}
