// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `-s slot,driver[,config]` grammar and each driver's config.
//!
//! One home for the text an operator types, so a device crate carries
//! no command-line syntax and both binaries parse a spec the same way.
//! A device takes typed options. The text stops here.

use std::path::PathBuf;

use vmm_devices::pci::Bdf;
use vmm_virtio::block::VirtioBlockOpts;
use vmm_virtio::fs::{
    VirtioFsOpts, FS_QUEUE_SIZE_MAX, FS_QUEUE_SIZE_MIN, FS_TAG_LEN,
};
use vmm_virtio::vsock::packet::CID_GUEST_MIN;

use crate::parse::parse_bdf;

/// The three text fields of a spec, before any of them is checked.
///
/// Lenient on purpose: the registry ids a spec by its slot even when
/// the driver field is missing, so the split must not refuse one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecParts<'a> {
    pub slot: &'a str,
    /// Empty when the spec has no driver field.
    pub driver: &'a str,
    /// Everything after the second comma, or empty.
    pub config: &'a str,
}

/// Split a spec into its fields.
pub fn parts(spec: &str) -> SpecParts<'_> {
    let mut fields = spec.splitn(3, ',');
    SpecParts {
        slot: fields.next().unwrap_or("").trim(),
        driver: fields.next().map(str::trim).unwrap_or(""),
        config: fields.next().unwrap_or(""),
    }
}

/// Stable, operator-facing id for a spec: `driver@slot`, or the slot
/// alone when the spec names no driver.
///
/// The slot makes it unique: the PCI bus refuses a second device at one
/// BDF, so no two specs share a slot. One rule for the boot-time attach
/// pass and for hot-add, so a device has one id however it arrived.
pub fn device_id(spec: &str) -> String {
    let SpecParts { slot, driver, .. } = parts(spec);
    if driver.is_empty() {
        slot.to_string()
    } else {
        format!("{driver}@{slot}")
    }
}

/// Split a spec and resolve its address. Refuses a spec with no driver
/// field or an address that is not a BDF.
pub fn split(spec: &str) -> anyhow::Result<(Bdf, &str, &str)> {
    let SpecParts {
        slot,
        driver,
        config,
    } = parts(spec);
    anyhow::ensure!(
        !driver.is_empty(),
        "invalid PCI slot spec '{spec}': need slot,driver[,config]"
    );
    let bdf = parse_bdf(slot).ok_or_else(|| {
        anyhow::anyhow!("invalid BDF '{slot}' in spec '{spec}'")
    })?;
    Ok((bdf, driver, config))
}

/// Parse a virtio-blk or nvme config:
/// `path[,nodelete][,ro][,sectorsize=N][,num-queues=N]`.
///
/// Unknown options are ignored so an older binary still boots a config
/// written for a newer one. A path cannot contain a comma, so the split
/// is unambiguous.
pub fn parse_blk_config(config: &str) -> (&str, VirtioBlockOpts) {
    let mut opts = VirtioBlockOpts::default();
    let mut path = config;

    if let Some(comma) = config.find(',') {
        path = &config[..comma];
        for opt in config[comma + 1..].split(',') {
            match opt {
                "nodelete" => opts.nodelete = true,
                "ro" => opts.read_only = true,
                s if s.starts_with("sectorsize=") => {
                    if let Ok(sz) = s["sectorsize=".len()..].parse::<u32>() {
                        if sz.is_power_of_two() && (512..=65536).contains(&sz) {
                            opts.sector_size = sz;
                        }
                    }
                }
                s if s.starts_with("num-queues=") => {
                    if let Ok(n) = s["num-queues=".len()..].parse::<u16>() {
                        if (1..=8).contains(&n) {
                            opts.num_queues = Some(n);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    (path, opts)
}

/// Split a viona config into the VNIC name and the promiscphys flag.
///
/// promiscphys puts the VNIC in physical promiscuous mode, which a zone
/// with MAC spoofing allowed needs.
pub fn parse_viona_config(config: &str) -> (&str, bool) {
    match config.find(',') {
        Some(comma) => {
            let promisc = config[comma + 1..]
                .split(',')
                .any(|o| o == "promiscphys" || o == "promiscphys=true");
            (&config[..comma], promisc)
        }
        None => (config, false),
    }
}

/// Parse a virtio-console config, which is a socket path and nothing
/// else.
pub fn parse_console_config(config: &str) -> anyhow::Result<&str> {
    if config.is_empty() {
        anyhow::bail!("virtio-console requires a socket path");
    }
    let mut fields = config.split(',');
    let path = fields.next().unwrap_or(config);
    if let Some(opt) = fields.next() {
        // Rejecting unknown options stops a misspelled flag from
        // looking as though it were honored.
        anyhow::bail!("virtio-console takes no options, got '{}'", opt);
    }
    Ok(path)
}

/// Parse a virtio-vsock config: `path,cid=<n>`.
///
/// The CID has no default: two guests sharing one is a routing bug
/// that only shows up under load.
pub fn parse_vsock_config(config: &str) -> anyhow::Result<(PathBuf, u64)> {
    let mut fields = config.split(',');
    let path = fields.next().unwrap_or("");
    anyhow::ensure!(!path.is_empty(), "virtio-vsock requires a socket path");

    let mut cid: Option<u64> = None;
    for opt in fields.filter(|o| !o.is_empty()) {
        if let Some(v) = opt.strip_prefix("cid=") {
            cid =
                Some(v.parse().map_err(|_| {
                    anyhow::anyhow!("virtio-vsock: bad cid '{v}'")
                })?);
        } else {
            anyhow::bail!("virtio-vsock: unknown option '{opt}'");
        }
    }
    let cid =
        cid.ok_or_else(|| anyhow::anyhow!("virtio-vsock requires cid=<n>"))?;
    anyhow::ensure!(
        cid >= CID_GUEST_MIN,
        "virtio-vsock cid must be {CID_GUEST_MIN} or above, got {cid}"
    );
    Ok((PathBuf::from(path), cid))
}

/// Parse a virtio-fs config: `path[,tag=NAME][,ro][,queue-size=N]`.
///
/// The tag defaults to the last path component. virtio-fs has no older
/// configs to stay compatible with, so an unknown option is a hard
/// error, unlike the virtio-blk grammar.
pub fn parse_fs_config(config: &str) -> anyhow::Result<VirtioFsOpts> {
    let mut fields = config.split(',');
    let path = fields.next().unwrap_or("");
    if path.is_empty() {
        anyhow::bail!("virtio-fs requires a host directory path");
    }

    let mut opts = VirtioFsOpts {
        path: PathBuf::from(path),
        ..VirtioFsOpts::default()
    };
    let mut tag: Option<String> = None;

    for opt in fields.filter(|o| !o.is_empty()) {
        if let Some(v) = opt.strip_prefix("tag=") {
            tag = Some(v.to_string());
        } else if opt == "ro" {
            opts.read_only = true;
        } else if let Some(v) = opt.strip_prefix("queue-size=") {
            let n: u32 = v.parse().unwrap_or(0);
            if !(u32::from(FS_QUEUE_SIZE_MIN)..=u32::from(FS_QUEUE_SIZE_MAX))
                .contains(&n)
            {
                anyhow::bail!(
                    "virtio-fs queue-size must be {}..={}, got {}",
                    FS_QUEUE_SIZE_MIN,
                    FS_QUEUE_SIZE_MAX,
                    v
                );
            }
            opts.queue_size = n as u16;
        } else {
            anyhow::bail!("virtio-fs: unknown option '{}'", opt);
        }
    }

    opts.tag = match tag {
        Some(t) => t,
        None => opts
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    anyhow::ensure!(
        opts.tag.len() <= FS_TAG_LEN,
        "virtio-fs tag '{}' exceeds {} bytes",
        opts.tag,
        FS_TAG_LEN
    );
    Ok(opts)
}

/// Parse an ahci-cd config, which is an ISO path and nothing else.
///
/// Rejecting options prevents a read-only device from appearing to
/// honor a writable configuration.
pub fn parse_ahci_cd_path(config: &str) -> anyhow::Result<&str> {
    if config.is_empty() {
        anyhow::bail!("ahci-cd requires an ISO path");
    }
    let path = config.split(',').next().unwrap_or(config);
    if let Some(opt) = config.split(',').skip(1).find(|o| !o.is_empty()) {
        anyhow::bail!("ahci-cd takes no options, got '{}'", opt);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use vmm_virtio::fs::FS_QUEUE_SIZE_DEFAULT;

    use super::*;

    #[test]
    fn a_spec_is_identified_by_driver_and_slot() {
        assert_eq!(device_id("4,virtio-blk,/disk"), "virtio-blk@4");
        assert_eq!(device_id("0"), "0");
        assert_eq!(device_id("0,"), "0");
    }

    #[test]
    fn split_needs_a_driver_and_a_bdf() {
        let (bdf, driver, config) =
            split("4,virtio-blk,/tmp/d.img,ro").expect("accepted");
        assert_eq!(bdf.dev(), 4);
        assert_eq!(driver, "virtio-blk");
        assert_eq!(config, "/tmp/d.img,ro");
        let (_, driver, config) = split("5,virtio-rnd").expect("accepted");
        assert_eq!((driver, config), ("virtio-rnd", ""));
        assert!(split("4").unwrap_err().to_string().contains("need slot"));
        assert!(split("notaslot,virtio-rnd")
            .unwrap_err()
            .to_string()
            .contains("invalid BDF"));
    }

    #[test]
    fn blk_config_path_only() {
        let (path, opts) = parse_blk_config("/dev/zvol/rdsk/tank/disk0");
        assert_eq!(path, "/dev/zvol/rdsk/tank/disk0");
        assert!(!opts.read_only);
        assert_eq!(opts.sector_size, 512);
        assert_eq!(opts.num_queues, None);
    }

    #[test]
    fn blk_config_options() {
        let (path, opts) = parse_blk_config(
            "/tmp/d.img,ro,nodelete,sectorsize=4096,num-queues=2",
        );
        assert_eq!(path, "/tmp/d.img");
        assert!(opts.read_only);
        assert!(opts.nodelete);
        assert_eq!(opts.sector_size, 4096);
        assert_eq!(opts.num_queues, Some(2));
    }

    #[test]
    fn blk_config_rejects_out_of_range_values() {
        // Values outside the accepted range leave the defaults in place
        // rather than handing a bad geometry to the guest.
        let (_, opts) =
            parse_blk_config("/tmp/d.img,sectorsize=777,num-queues=99");
        assert_eq!(opts.sector_size, 512);
        assert_eq!(opts.num_queues, None);
    }

    #[test]
    fn viona_config_splits_name_and_promisc() {
        assert_eq!(parse_viona_config("vnic0"), ("vnic0", false));
        assert_eq!(parse_viona_config("vnic0,promiscphys"), ("vnic0", true));
        assert_eq!(
            parse_viona_config("vnic0,promiscphys=true"),
            ("vnic0", true)
        );
        // An unknown option must not turn promiscuous mode on.
        assert_eq!(parse_viona_config("vnic0,mtu=9000"), ("vnic0", false));
    }

    #[test]
    fn console_config_is_a_path_and_nothing_else() {
        let err = parse_console_config("").expect_err("empty must fail");
        assert_eq!(err.to_string(), "virtio-console requires a socket path");
        let err = parse_console_config("/tmp/c.sock,rw")
            .expect_err("option must fail");
        assert_eq!(
            err.to_string(),
            "virtio-console takes no options, got 'rw'"
        );
        // A trailing comma is a typo, not an empty option list.
        let err = parse_console_config("/tmp/c.sock,")
            .expect_err("trailing comma must fail");
        assert_eq!(err.to_string(), "virtio-console takes no options, got ''");
        assert_eq!(
            parse_console_config("/tmp/c.sock").expect("path"),
            "/tmp/c.sock"
        );
    }

    #[test]
    fn vsock_config_needs_a_path_and_a_cid() {
        let (path, cid) = parse_vsock_config("/tmp/v.sock,cid=3").expect("ok");
        assert_eq!(path, PathBuf::from("/tmp/v.sock"));
        assert_eq!(cid, 3);
        assert!(parse_vsock_config("/tmp/v.sock").is_err());
    }

    /// 0, 1 and 2 are the hypervisor, loopback and host. A guest given
    /// one of them would collide with the host's own address.
    #[test]
    fn vsock_config_rejects_a_reserved_cid() {
        for cid in [0, 1, 2] {
            assert!(
                parse_vsock_config(&format!("/tmp/v.sock,cid={cid}")).is_err(),
                "accepted reserved cid {cid}"
            );
        }
        assert!(parse_vsock_config("/tmp/v.sock,cid=3").is_ok());
    }

    #[test]
    fn vsock_config_rejects_junk() {
        assert!(parse_vsock_config("").is_err());
        assert!(parse_vsock_config("/tmp/v.sock,cid=x").is_err());
        assert!(parse_vsock_config("/tmp/v.sock,cid=3,bogus").is_err());
        assert!(parse_vsock_config("/tmp/v.sock,tag=nope,cid=3").is_err());
    }

    #[test]
    fn fs_config_requires_a_path() {
        let err = parse_fs_config("").expect_err("empty config");
        assert_eq!(err.to_string(), "virtio-fs requires a host directory path");
    }

    #[test]
    fn fs_tag_defaults_to_the_last_path_component() {
        let opts = parse_fs_config("/export/shared").expect("parse");
        assert_eq!(opts.tag, "shared");
        assert_eq!(opts.path, PathBuf::from("/export/shared"));
        assert!(!opts.read_only);
        assert_eq!(opts.queue_size, FS_QUEUE_SIZE_DEFAULT);
    }

    #[test]
    fn fs_options_are_parsed() {
        let opts = parse_fs_config("/export/x,tag=data,ro,queue-size=256")
            .expect("parse");
        assert_eq!(opts.tag, "data");
        assert!(opts.read_only);
        assert_eq!(opts.queue_size, 256);
    }

    #[test]
    fn fs_oversized_tag_is_rejected() {
        let tag = "t".repeat(FS_TAG_LEN + 1);
        let err = parse_fs_config(&format!("/export/x,tag={tag}"))
            .expect_err("long tag");
        assert_eq!(
            err.to_string(),
            format!("virtio-fs tag '{tag}' exceeds {FS_TAG_LEN} bytes")
        );
    }

    #[test]
    fn fs_out_of_range_queue_size_is_rejected() {
        let err =
            parse_fs_config("/export/x,queue-size=4").expect_err("too small");
        assert_eq!(
            err.to_string(),
            "virtio-fs queue-size must be 8..=1024, got 4"
        );
    }

    #[test]
    fn fs_unknown_option_is_rejected() {
        let err = parse_fs_config("/export/x,dax").expect_err("unknown");
        assert_eq!(err.to_string(), "virtio-fs: unknown option 'dax'");
    }

    #[test]
    fn ahci_cd_is_a_path_and_nothing_else() {
        let error = parse_ahci_cd_path("").unwrap_err();
        assert_eq!(error.to_string(), "ahci-cd requires an ISO path");
        let error = parse_ahci_cd_path("/x.iso,rw").unwrap_err();
        assert_eq!(error.to_string(), "ahci-cd takes no options, got 'rw'");
        assert_eq!(parse_ahci_cd_path("/x.iso").expect("path"), "/x.iso");
    }
}
