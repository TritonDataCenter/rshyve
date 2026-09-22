// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! VirtIO specification constants.
//!
//! Covers device status bits, virtqueue descriptor flags, feature bits,
//! legacy PCI transport register layout, and PCI identity values.

// ---------------------------------------------------------------------------
// Device status register bits (VirtIO 1.2 sec 2.1)
// ---------------------------------------------------------------------------

/// The guest found the device and recognized it as a virtio device.
pub const STATUS_ACKNOWLEDGE: u8 = 1;
/// Guest OS knows how to drive the device.
pub const STATUS_DRIVER: u8 = 2;
/// Driver is set up and ready to drive the device.
pub const STATUS_DRIVER_OK: u8 = 4;
/// Driver has acknowledged all the features it understands.
pub const STATUS_FEATURES_OK: u8 = 8;
/// Device has experienced an error from which it cannot recover.
pub const STATUS_DEVICE_NEEDS_RESET: u8 = 64;
/// Guest OS has given up on the device.
pub const STATUS_FAILED: u8 = 128;

// ---------------------------------------------------------------------------
// VirtQueue descriptor flags (VirtIO 1.2 sec 2.7.5)
// ---------------------------------------------------------------------------

/// Available ring: driver sets this to suppress used-buffer notifications.
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// The buffer continues at the descriptor in the `next` field.
pub const VRING_DESC_F_NEXT: u16 = 1;
/// The buffer is device write-only (otherwise device read-only).
pub const VRING_DESC_F_WRITE: u16 = 2;
/// The buffer contains a table of buffer descriptors.
pub const VRING_DESC_F_INDIRECT: u16 = 4;

// ---------------------------------------------------------------------------
// VirtIO feature bits (VirtIO 1.2 sec 6)
// ---------------------------------------------------------------------------

/// Device supports indirect descriptors.
pub const VIRTIO_F_RING_INDIRECT_DESC: u64 = 1 << 28;
/// Device supports the event index feature for suppressing notifications.
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << 29;
/// RING_INDIRECT_DESC as u32 for legacy transport feature advertisement.
pub const VIRTIO_F_RING_INDIRECT_DESC_U32: u32 = 1 << 28;
/// RING_EVENT_IDX as u32 for legacy transport feature advertisement.
pub const VIRTIO_F_RING_EVENT_IDX_U32: u32 = 1 << 29;
/// Device complies with VirtIO 1.0+ specification.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// Device supports packed virtqueues (virtio 1.1+).
pub const VIRTIO_F_RING_PACKED: u64 = 1 << 34;

// ---------------------------------------------------------------------------
// VirtIO block request types (VirtIO 1.2 sec 5.2.6)
// ---------------------------------------------------------------------------

/// Read from disk.
pub const VIRTIO_BLK_T_IN: u32 = 0;
/// Write to disk.
pub const VIRTIO_BLK_T_OUT: u32 = 1;
/// Flush write cache.
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// ---------------------------------------------------------------------------
// VirtIO block feature bits
// ---------------------------------------------------------------------------

/// Device supports read-only mode.
pub const VIRTIO_BLK_F_RO: u64 = 1 << 5;
/// Device supports flush command.
pub const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
/// Device exports block size.
pub const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
pub const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
pub const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
pub const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;
pub const VIRTIO_BLK_F_WRITE_ZEROES: u64 = 1 << 14;
/// Block device supports multiple request queues.
pub const VIRTIO_BLK_F_MQ: u64 = 1 << 12;

// ---------------------------------------------------------------------------
// Legacy PCI transport register offsets (VirtIO 1.2 sec 4.1.4.8)
// ---------------------------------------------------------------------------

/// Device features (4 bytes, read-only).
pub const LEGACY_REG_DEVICE_FEATURES: u16 = 0x00;
/// Guest (driver) features (4 bytes, write-only).
pub const LEGACY_REG_GUEST_FEATURES: u16 = 0x04;
/// Queue PFN (4 bytes, read/write). Page frame number of selected queue.
pub const LEGACY_REG_QUEUE_PFN: u16 = 0x08;
/// Queue size (2 bytes, read-only). Max descriptors in selected queue.
pub const LEGACY_REG_QUEUE_SIZE: u16 = 0x0C;
/// Queue select (2 bytes, write-only). Selects which queue to configure.
pub const LEGACY_REG_QUEUE_SELECT: u16 = 0x0E;
/// Queue notify (2 bytes, write-only). The driver writes a queue index.
pub const LEGACY_REG_QUEUE_NOTIFY: u16 = 0x10;
/// Device status (1 byte, read/write).
pub const LEGACY_REG_DEVICE_STATUS: u16 = 0x12;
/// ISR status (1 byte, read-only). Reading clears the register.
pub const LEGACY_REG_ISR_STATUS: u16 = 0x13;
/// Start of device-specific configuration space.
pub const LEGACY_REG_DEVICE_CONFIG: u16 = 0x14;

/// Size of legacy common config registers (excluding device-specific).
pub const LEGACY_COMMON_SIZE: u16 = 0x14;

/// Size of legacy common config with MSI-X vector registers.
pub const LEGACY_COMMON_SIZE_MSIX: u16 = 0x18;
/// MSI-X "no vector" sentinel.
pub const VIRTIO_MSI_NO_VECTOR: u16 = 0xFFFF;
/// MSI-X config vector register offset in legacy transport.
pub const LEGACY_REG_MSIX_CONFIG_VECTOR: u16 = 0x14;
/// MSI-X queue vector register offset in legacy transport.
pub const LEGACY_REG_MSIX_QUEUE_VECTOR: u16 = 0x16;
/// Device config start when MSI-X is present.
pub const LEGACY_REG_DEVICE_CONFIG_MSIX: u16 = 0x18;

// ---------------------------------------------------------------------------
// Modern PCI transport: capability types (VirtIO 1.3 sec 4.1.4.4)
// ---------------------------------------------------------------------------

/// PCI capability ID for vendor-specific (used by virtio).
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// Size of a virtio PCI capability structure (16 bytes).
pub const VIRTIO_PCI_CAP_SIZE: u8 = 16;
/// Size of the notify capability (16 + 4 byte multiplier = 20 bytes).
pub const VIRTIO_PCI_NOTIFY_CAP_SIZE: u8 = 20;

// ---------------------------------------------------------------------------
// Modern common config register offsets (VirtIO 1.3 sec 4.1.4.3)
// ---------------------------------------------------------------------------

/// Selects which 32-bit half of device_feature to read (0=low, 1=high).
pub const COMMON_CFG_DEVICE_FEATURE_SELECT: u16 = 0x00;
/// Device features (read-only, selected by device_feature_select).
pub const COMMON_CFG_DEVICE_FEATURE: u16 = 0x04;
/// Selects which 32-bit half of driver_feature to write.
pub const COMMON_CFG_DRIVER_FEATURE_SELECT: u16 = 0x08;
/// Driver features (write, selected by driver_feature_select).
pub const COMMON_CFG_DRIVER_FEATURE: u16 = 0x0C;
/// MSI-X vector for config changes.
pub const COMMON_CFG_MSIX_CONFIG: u16 = 0x10;
/// Number of virtqueues (read-only).
pub const COMMON_CFG_NUM_QUEUES: u16 = 0x12;
pub const COMMON_CFG_DEVICE_STATUS: u16 = 0x14;
/// Configuration generation counter (read-only).
pub const COMMON_CFG_CONFIG_GENERATION: u16 = 0x15;
pub const COMMON_CFG_QUEUE_SELECT: u16 = 0x16;
pub const COMMON_CFG_QUEUE_SIZE: u16 = 0x18;
/// MSI-X vector for selected queue.
pub const COMMON_CFG_QUEUE_MSIX_VECTOR: u16 = 0x1A;
/// Queue enable (0=disabled, 1=enabled).
pub const COMMON_CFG_QUEUE_ENABLE: u16 = 0x1C;
/// Queue notify offset (multiplied by notify_off_multiplier).
pub const COMMON_CFG_QUEUE_NOTIFY_OFF: u16 = 0x1E;
/// Queue descriptor table address (low 32 bits).
pub const COMMON_CFG_QUEUE_DESC_LO: u16 = 0x20;
/// Queue descriptor table address (high 32 bits).
pub const COMMON_CFG_QUEUE_DESC_HI: u16 = 0x24;
/// Queue available ring address (low 32 bits).
pub const COMMON_CFG_QUEUE_AVAIL_LO: u16 = 0x28;
/// Queue available ring address (high 32 bits).
pub const COMMON_CFG_QUEUE_AVAIL_HI: u16 = 0x2C;
/// Queue used ring address (low 32 bits).
pub const COMMON_CFG_QUEUE_USED_LO: u16 = 0x30;
/// Queue used ring address (high 32 bits).
pub const COMMON_CFG_QUEUE_USED_HI: u16 = 0x34;
/// Size of the common config structure.
pub const COMMON_CFG_SIZE: u16 = 0x38;

// ---------------------------------------------------------------------------
// Modern transport BAR2 layout (4 pages = 16 KB MMIO)
// ---------------------------------------------------------------------------

/// Common config at start of BAR2.
pub const MODERN_BAR_COMMON_OFFSET: u32 = 0x0000;
/// Device-specific config at page 1.
pub const MODERN_BAR_DEVICE_OFFSET: u32 = 0x1000;
/// Notification register at page 2.
pub const MODERN_BAR_NOTIFY_OFFSET: u32 = 0x2000;
/// ISR status register at page 3.
pub const MODERN_BAR_ISR_OFFSET: u32 = 0x3000;
/// Total BAR2 size (4 pages).
pub const MODERN_BAR_SIZE: u32 = 0x4000;
pub const MODERN_BAR_IDX: u8 = 2;

// ---------------------------------------------------------------------------
// PCI identity for VirtIO devices
// ---------------------------------------------------------------------------

/// VirtIO PCI vendor ID (Red Hat).
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1AF4;

// ---------------------------------------------------------------------------
// VirtIO device type IDs
// ---------------------------------------------------------------------------

pub const VIRTIO_DEV_TYPE_NET: u16 = 1;
pub const VIRTIO_DEV_TYPE_BLOCK: u16 = 2;
pub const VIRTIO_DEV_TYPE_CONSOLE: u16 = 3;
/// Entropy (RNG) device.
pub const VIRTIO_DEV_TYPE_RNG: u16 = 4;
/// Traditional memory balloon device.
pub const VIRTIO_DEV_TYPE_BALLOON: u16 = 5;
/// Filesystem device (virtio-fs). VirtIO 1.3 section 5.11.
pub const VIRTIO_DEV_TYPE_FS: u16 = 26;
/// virtio-vsock (VIRTIO 1.3 5.10). Modern id only, like virtio-fs.
pub const VIRTIO_DEV_TYPE_VSOCK: u16 = 19;

// ---------------------------------------------------------------------------
// Transitional PCI device IDs (virtio 1.3 section 4.1.2.1)
// ---------------------------------------------------------------------------

pub const VIRTIO_PCI_DEVICE_ID_NET: u16 = 0x1000;
pub const VIRTIO_PCI_DEVICE_ID_BLOCK: u16 = 0x1001;
pub const VIRTIO_PCI_DEVICE_ID_BALLOON: u16 = 0x1002;
pub const VIRTIO_PCI_DEVICE_ID_CONSOLE: u16 = 0x1003;
pub const VIRTIO_PCI_DEVICE_ID_RNG: u16 = 0x1005;
/// First PCI device id of the modern (non-transitional) range.
pub const VIRTIO_PCI_DEVICE_ID_MODERN_BASE: u16 = 0x1040;

/// Give the PCI device id to advertise for `dev_type`.
///
/// The specification assigns these ids by table, not by formula. The
/// formula `0x1000 + dev_type - 1` gives virtio-rng the virtio-console
/// id. Windows virtio drivers match the device id in their INF files,
/// so a wrong id binds the wrong driver.
pub const fn transitional_device_id(dev_type: u16) -> u16 {
    match dev_type {
        VIRTIO_DEV_TYPE_NET => VIRTIO_PCI_DEVICE_ID_NET,
        VIRTIO_DEV_TYPE_BLOCK => VIRTIO_PCI_DEVICE_ID_BLOCK,
        VIRTIO_DEV_TYPE_CONSOLE => VIRTIO_PCI_DEVICE_ID_CONSOLE,
        VIRTIO_DEV_TYPE_RNG => VIRTIO_PCI_DEVICE_ID_RNG,
        VIRTIO_DEV_TYPE_BALLOON => VIRTIO_PCI_DEVICE_ID_BALLOON,
        // Device types after the transitional range have only a modern
        // id. Saturate: no device type comes near the u16 limit.
        _ => VIRTIO_PCI_DEVICE_ID_MODERN_BASE.saturating_add(dev_type),
    }
}

/// Give the PCI (class, subclass) pair to advertise for `dev_type`.
///
/// A console is a communication device. virtio-fs is storage/other, as
/// QEMU's vhost-user-fs-pci reports. Windows picks the driver from the
/// device id, but the class sets where the guest puts the device in its
/// device tree.
///
/// A device must have a class. Linux does not assign BARs to a device
/// whose class is 0 (`__dev_sort_resources` skips
/// `PCI_CLASS_NOT_DEFINED`). On a direct kernel boot no firmware places
/// the BARs, so the driver cannot enable the device.
pub const fn pci_class_for(dev_type: u16) -> (u8, u8) {
    match dev_type {
        VIRTIO_DEV_TYPE_NET => (vmm_devices::pci::bits::CLASS_NETWORK, 0),
        VIRTIO_DEV_TYPE_BLOCK => (vmm_devices::pci::bits::CLASS_STORAGE, 0),
        VIRTIO_DEV_TYPE_CONSOLE => (
            vmm_devices::pci::bits::CLASS_COMMUNICATION,
            vmm_devices::pci::bits::SUBCLASS_COMMUNICATION_OTHER,
        ),
        VIRTIO_DEV_TYPE_FS => (
            vmm_devices::pci::bits::CLASS_STORAGE,
            vmm_devices::pci::bits::SUBCLASS_STORAGE_OTHER,
        ),
        // 0xFF, matching QEMU's virtio-rng-pci and the class the legacy
        // virtio table gives virtio-entropy.
        VIRTIO_DEV_TYPE_RNG => (vmm_devices::pci::bits::CLASS_OTHERS, 0),
        // Communication/other, matching QEMU's vhost-vsock-pci.
        VIRTIO_DEV_TYPE_VSOCK => (
            vmm_devices::pci::bits::CLASS_COMMUNICATION,
            vmm_devices::pci::bits::SUBCLASS_COMMUNICATION_OTHER,
        ),
        _ => (vmm_devices::pci::bits::CLASS_UNCLASSIFIED, 0),
    }
}

/// Sector size for virtio-block (always 512 bytes).
pub const SECTOR_SIZE: u64 = 512;

// ---------------------------------------------------------------------------
// ISR status bits
// ---------------------------------------------------------------------------

pub const ISR_QUEUE_INTR: u8 = 1;
pub const ISR_CFG_CHANGE: u8 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bits_are_distinct() {
        let all = STATUS_ACKNOWLEDGE
            | STATUS_DRIVER
            | STATUS_DRIVER_OK
            | STATUS_FEATURES_OK
            | STATUS_DEVICE_NEEDS_RESET
            | STATUS_FAILED;
        assert_eq!(all.count_ones(), 6);
    }

    #[test]
    fn descriptor_flags_are_distinct() {
        assert_eq!(VRING_DESC_F_NEXT & VRING_DESC_F_WRITE, 0);
        assert_eq!(VRING_DESC_F_NEXT & VRING_DESC_F_INDIRECT, 0);
        assert_eq!(VRING_DESC_F_WRITE & VRING_DESC_F_INDIRECT, 0);
    }

    #[test]
    fn legacy_register_offsets_non_overlapping() {
        assert!(LEGACY_REG_DEVICE_FEATURES < LEGACY_REG_GUEST_FEATURES);
        assert!(LEGACY_REG_GUEST_FEATURES < LEGACY_REG_QUEUE_PFN);
        assert!(LEGACY_REG_QUEUE_PFN < LEGACY_REG_QUEUE_SIZE);
        assert!(LEGACY_REG_QUEUE_SIZE < LEGACY_REG_QUEUE_SELECT);
        assert!(LEGACY_REG_QUEUE_SELECT < LEGACY_REG_QUEUE_NOTIFY);
        assert!(LEGACY_REG_QUEUE_NOTIFY < LEGACY_REG_DEVICE_STATUS);
        assert!(LEGACY_REG_DEVICE_STATUS < LEGACY_REG_ISR_STATUS);
        assert!(LEGACY_REG_ISR_STATUS < LEGACY_REG_DEVICE_CONFIG);
    }

    #[test]
    fn device_config_starts_at_0x14() {
        assert_eq!(LEGACY_REG_DEVICE_CONFIG, 0x14);
    }

    #[test]
    fn transitional_device_ids_follow_the_assigned_table() {
        assert_eq!(transitional_device_id(VIRTIO_DEV_TYPE_NET), 0x1000);
        assert_eq!(transitional_device_id(VIRTIO_DEV_TYPE_BLOCK), 0x1001);
        assert_eq!(transitional_device_id(VIRTIO_DEV_TYPE_CONSOLE), 0x1003);
        assert_eq!(transitional_device_id(VIRTIO_DEV_TYPE_RNG), 0x1005);
        assert_eq!(transitional_device_id(VIRTIO_DEV_TYPE_BALLOON), 0x1002);
    }

    #[test]
    fn rng_does_not_take_the_console_device_id() {
        // The formula 0x1000 + type - 1 gives virtio-rng the
        // virtio-console id.
        assert_eq!(transitional_device_id(VIRTIO_DEV_TYPE_RNG), 0x1005);
        assert_ne!(
            transitional_device_id(VIRTIO_DEV_TYPE_RNG),
            transitional_device_id(VIRTIO_DEV_TYPE_CONSOLE)
        );
    }

    #[test]
    fn device_types_outside_the_table_use_the_modern_base() {
        // virtio-fs (26) and every type after it have a modern id only.
        assert_eq!(transitional_device_id(26), 0x105A);
    }

    #[test]
    fn console_is_a_communication_device() {
        assert_eq!(
            pci_class_for(VIRTIO_DEV_TYPE_CONSOLE),
            (
                vmm_devices::pci::bits::CLASS_COMMUNICATION,
                vmm_devices::pci::bits::SUBCLASS_COMMUNICATION_OTHER
            )
        );
        assert_eq!(
            pci_class_for(VIRTIO_DEV_TYPE_BLOCK),
            (vmm_devices::pci::bits::CLASS_STORAGE, 0)
        );
        assert_eq!(
            pci_class_for(VIRTIO_DEV_TYPE_NET),
            (vmm_devices::pci::bits::CLASS_NETWORK, 0)
        );
        assert_eq!(
            pci_class_for(VIRTIO_DEV_TYPE_FS),
            (
                vmm_devices::pci::bits::CLASS_STORAGE,
                vmm_devices::pci::bits::SUBCLASS_STORAGE_OTHER
            )
        );
        assert_eq!(
            pci_class_for(VIRTIO_DEV_TYPE_RNG),
            (vmm_devices::pci::bits::CLASS_OTHERS, 0)
        );
        assert_eq!(
            pci_class_for(VIRTIO_DEV_TYPE_VSOCK),
            (
                vmm_devices::pci::bits::CLASS_COMMUNICATION,
                vmm_devices::pci::bits::SUBCLASS_COMMUNICATION_OTHER
            )
        );
    }

    #[test]
    fn vsock_uses_the_modern_device_id() {
        // 0x1040 + 19. vsock has no transitional id of its own.
        assert_eq!(transitional_device_id(VIRTIO_DEV_TYPE_VSOCK), 0x1053);
    }

    /// Linux skips a device whose class is 0 when it assigns BARs. On a
    /// direct kernel boot no firmware places the BARs, so an
    /// unclassified device cannot be enabled. Thus every device type
    /// that either catalog can attach needs a class.
    #[test]
    fn no_attachable_device_type_is_unclassified() {
        for dev_type in [
            VIRTIO_DEV_TYPE_NET,
            VIRTIO_DEV_TYPE_BLOCK,
            VIRTIO_DEV_TYPE_CONSOLE,
            VIRTIO_DEV_TYPE_FS,
            VIRTIO_DEV_TYPE_RNG,
            VIRTIO_DEV_TYPE_VSOCK,
        ] {
            let (class, _) = pci_class_for(dev_type);
            assert_ne!(
                class,
                vmm_devices::pci::bits::CLASS_UNCLASSIFIED,
                "device type {dev_type} has no PCI class"
            );
        }
    }
}
