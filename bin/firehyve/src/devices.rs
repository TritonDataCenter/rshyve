// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! firehyve's device catalog: the map from a `-s slot,driver[,config]`
//! string to a concrete device.
//!
//! An unrecognized driver name is an error. A skip gives a VM that
//! boots without the device the operator asked for. The one exception
//! is `IGNORED`: names the bhyve zone brand puts on every zone that
//! firehyve has no device for.

use slog::{info, warn};

use vmm_devices::pci::Bdf;
use vmm_machine::devspec;
use vmm_virtio::attach;
use vmm_virtio::vsock::control::ControlSlot;

/// Driver names this binary implements, plus the two the chipset
/// already creates. One list, so the parse gate and the error text
/// always agree.
const SUPPORTED: &[&str] = &[
    "virtio-blk",
    "virtio-blk-pci",
    "virtio-net-viona",
    "virtio-rnd",
    "virtio-console",
    "virtio-fs",
    "virtio-vsock",
    "hostbridge",
    "lpc",
];

/// Driver names the bhyve zone brand emits that firehyve has no device
/// for. They are accepted and left unattached.
///
/// `add_fbuf` in `boot.c` puts a frame buffer and an xhci tablet on
/// every zone unless VNC is off, and VNC is on by default. Both serve a
/// graphical console. firehyve boots a kernel onto a serial console and
/// has no frame buffer, so the slots stay empty. A refusal means an
/// unmodified brand can never start this binary.
///
/// This list applies to the startup argv only. `hotplug_factory`
/// refuses these names, because a hot-add must not report success and
/// attach nothing.
const IGNORED: &[&str] = &["fbuf", "xhci"];

fn unsupported_driver(driver: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "firehyve does not support PCI driver '{driver}' (supported: {})",
        SUPPORTED.join(", ")
    )
}

/// Split a device spec into its address, driver name, and config.
///
/// Kept separate from device construction so a test of the driver-name
/// contract needs no live VM.
pub(crate) fn split_spec(spec: &str) -> anyhow::Result<(Bdf, &str, &str)> {
    let (bdf, driver, config) = devspec::split(spec)?;
    if !SUPPORTED.contains(&driver) && !IGNORED.contains(&driver) {
        return Err(unsupported_driver(driver));
    }
    Ok((bdf, driver, config))
}

/// Build one PCI device from its spec string.
///
/// A vsock device also carries the control slot that answers the
/// `CONTROL` verb, so `control` collects one slot per vsock device. The
/// caller fills the slots once the device registry is complete.
///
/// Every attach here uses the fallible form. This runs on a control
/// thread as well as on the startup path, and a panic in
/// `attach_device` poisons the bus lock and stops the whole VM.
pub(crate) fn create_pci_device(
    spec: &str,
    num_vcpus: u32,
    ctx: &vmm_machine::PciDeviceCtx<'_>,
    control: &mut Vec<ControlSlot>,
) -> anyhow::Result<vmm_machine::CreatedPciDevice> {
    let (bdf, driver, config) = split_spec(spec)?;
    if IGNORED.contains(&driver) {
        warn!(ctx.log, "PCI device ignored";
            "bdf" => %bdf,
            "driver" => driver,
            "reason" => "firehyve has no frame buffer; the guest console \
                         is serial",
        );
        return Ok((None, None));
    }
    let actx = attach::VirtioAttachCtx {
        physmap: ctx.physmap,
        bus_pio: ctx.bus_pio,
        bus_mmio: ctx.bus_mmio,
        hdl: ctx.vmm_hdl,
    };

    match driver {
        "virtio-blk" | "virtio-blk-pci" => {
            anyhow::ensure!(
                !config.is_empty(),
                "virtio-blk requires a disk path"
            );
            let (path, opts) = devspec::parse_blk_config(config);
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_blk(
                std::path::Path::new(path),
                &opts,
                num_vcpus,
                lintr,
                &actx,
            )?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-blk attached";
                "bdf" => %bdf, "path" => config);
            Ok(vmm_machine::created(dev))
        }
        "virtio-net-viona" => {
            let lintr = ctx.chipset.route_lintr(&bdf);
            // firehyve is never a migration destination, so the in-kernel
            // interrupt poll thread starts immediately.
            let (vnic, promiscphys) = devspec::parse_viona_config(config);
            let dev = attach::virtio_viona(
                vnic,
                promiscphys,
                lintr,
                false,
                &actx,
                ctx.log,
            )?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-net-viona attached";
                "bdf" => %bdf, "vnic" => config);
            Ok(vmm_machine::created(dev))
        }
        "virtio-fs" => {
            use vmm_virtio::fs::clamp_queue_size;
            let opts = devspec::parse_fs_config(config)?;
            let queue_size = clamp_queue_size(opts.queue_size);
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev =
                attach::virtio_fs(&opts, queue_size, lintr, &actx, ctx.log)?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-fs attached";
                "bdf" => %bdf,
                "tag" => &opts.tag,
                "path" => %opts.path.display());
            Ok(vmm_machine::created(dev))
        }
        "virtio-vsock" => {
            let (path, cid) = devspec::parse_vsock_config(config)?;
            let lintr = ctx.chipset.route_lintr(&bdf);
            let (dev, slot) = vmm_virtio::vsock::control::attach_with_control(
                &path, cid, lintr, &actx, ctx.log,
            )?;
            control.push(slot);
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-vsock attached";
                "bdf" => %bdf,
                "cid" => cid,
                "socket" => %path.display());
            Ok(vmm_machine::created(dev))
        }
        "virtio-rnd" => {
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_rng(lintr, &actx);
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-rnd attached"; "bdf" => %bdf);
            Ok(vmm_machine::created(dev))
        }
        "virtio-console" => {
            let path = devspec::parse_console_config(config)?;
            let lintr = ctx.chipset.route_lintr(&bdf);
            let dev = attach::virtio_console(
                std::path::Path::new(path),
                lintr,
                &actx,
                ctx.log,
            )?;
            ctx.chipset.try_attach_device(bdf, dev.clone())?;
            info!(ctx.log, "virtio-console attached";
                "bdf" => %bdf, "socket" => path);
            Ok(vmm_machine::created(dev))
        }
        // The chipset creates both. The names are accepted so a
        // bhyve-shaped command line works.
        "hostbridge" | "lpc" => Ok((None, None)),
        // Reachable only if SUPPORTED grows a name before its arm does.
        other => Err(unsupported_driver(other)),
    }
}

/// Refuse a spec that only the startup argv may carry.
///
/// Split out of the factory so a test needs no live VM. The spec string
/// alone decides the result.
fn check_hot_add(spec: &str) -> anyhow::Result<()> {
    let (_, driver, _) = split_spec(spec)?;
    // The startup argv accepts these names because the brand forces
    // them. Hot-add is firehyve's own interface, so a name that
    // attaches nothing is refused.
    anyhow::ensure!(
        !IGNORED.contains(&driver),
        "firehyve cannot add '{driver}' at run time: it has no frame buffer"
    );
    // A vsock control slot is armed once, at startup. A vsock device
    // added later has a socket that answers nothing.
    anyhow::ensure!(
        driver != "virtio-vsock",
        "virtio-vsock cannot be added at run time: its CONTROL slot is \
         armed at startup"
    );
    Ok(())
}

/// The catalog closure, bound to this VM and handed to the hotplug
/// engine.
///
/// SECURITY: the boot-time `-s` pass uses the same closure, so a
/// control request can name no driver and no path that a command line
/// cannot. See the trust note in `crate::control`.
pub(crate) fn hotplug_factory(num_cpus: u32) -> vmm_machine::HotplugFactory {
    Box::new(move |spec, ctx| {
        check_hot_add(spec)?;
        let mut slots = Vec::new();
        let created = create_pci_device(spec, num_cpus, ctx, &mut slots)?;
        debug_assert!(slots.is_empty(), "only vsock yields a control slot");
        Ok(created)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_driver_is_a_hard_error() {
        let err =
            split_spec("4,nvme,/tmp/d.img").expect_err("nvme is rshyve only");
        let msg = err.to_string();
        assert!(msg.contains("nvme"), "got: {msg}");
        assert!(
            msg.contains("virtio-blk"),
            "error should list what works: {msg}"
        );
    }

    #[test]
    fn the_error_names_every_accepted_driver() {
        // The list in the error must match the list the catalog
        // accepts, aliases included.
        let err = split_spec("4,virtio-blkk,/tmp/d.img").expect_err("typo");
        let msg = err.to_string();
        for name in SUPPORTED {
            assert!(msg.contains(name), "'{name}' missing from: {msg}");
        }
    }

    #[test]
    fn a_typo_is_rejected_rather_than_skipped() {
        // rshyve's catch-all gives a VM with a missing disk. firehyve
        // refuses to start.
        assert!(split_spec("4,virtio-blkk,/tmp/d.img").is_err());
        assert!(split_spec("8,passthru,/dev/ppt0").is_err());
        // A name that resembles one the brand forces is a typo, not an
        // ignored device.
        assert!(split_spec("6,fbufx").is_err());
        assert!(split_spec("7,xhc").is_err());
    }

    /// `boot.c` puts a frame buffer and an xhci tablet on every zone
    /// that has VNC on, which is the default. firehyve has no frame
    /// buffer. A refusal means an unmodified brand can never start it.
    ///
    /// Mutation this kills: removing `fbuf` or `xhci` from IGNORED, or
    /// moving them into SUPPORTED, which claims a device that does not
    /// exist.
    #[test]
    fn the_display_devices_the_brand_emits_are_accepted_and_skipped() {
        for spec in ["30:0,fbuf,vga=off,unix=/tmp/vm.vnc", "30:1,xhci,tablet"] {
            let (_, driver, _) =
                split_spec(spec).unwrap_or_else(|e| panic!("{spec}: {e}"));
            assert!(IGNORED.contains(&driver), "{driver} is not ignored");
            assert!(
                !SUPPORTED.contains(&driver),
                "{driver} has no device, so it must not be advertised"
            );
        }
    }

    /// The startup exception must not apply to hot-add. A hot-add must
    /// not report success and attach nothing.
    ///
    /// Mutation this kills: removing either guard from `check_hot_add`.
    #[test]
    fn a_display_device_is_still_refused_at_run_time() {
        for driver in IGNORED {
            let spec = format!("6,{driver}");
            let err = check_hot_add(&spec)
                .expect_err("an ignored startup name must not be hot-added");
            assert!(err.to_string().contains(driver), "{driver}: {err}");
        }

        let err = check_hot_add("6,virtio-vsock,/tmp/v.sock")
            .expect_err("a vsock CONTROL slot is armed at startup only");
        assert!(err.to_string().contains("CONTROL"), "{err}");

        check_hot_add("6,virtio-rnd").expect("a real device is addable");
    }

    #[test]
    fn supported_drivers_split_into_bdf_driver_config() {
        let (bdf, driver, config) =
            split_spec("4,virtio-blk,/tmp/d.img,ro").expect("accepted");
        assert_eq!(bdf.dev(), 4);
        assert_eq!(driver, "virtio-blk");
        assert_eq!(config, "/tmp/d.img,ro");

        let (_, driver, config) =
            split_spec("7,virtio-net-viona,net0").expect("accepted");
        assert_eq!(driver, "virtio-net-viona");
        assert_eq!(config, "net0");

        let (_, driver, config) = split_spec("5,virtio-rnd").expect("accepted");
        assert_eq!(driver, "virtio-rnd");
        assert_eq!(config, "");
    }

    #[test]
    fn every_supported_name_is_accepted() {
        // No name in the error text may fail the parser.
        for name in SUPPORTED {
            let spec = format!("4,{name},/tmp/d.img");
            let (_, driver, _) =
                split_spec(&spec).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(driver, *name);
        }
    }

    #[test]
    fn chipset_owned_names_are_accepted() {
        // A bhyve-shaped argv carries these. The chipset already made
        // them, so they parse and attach nothing.
        assert!(split_spec("0,hostbridge").is_ok());
        assert!(split_spec("31,lpc").is_ok());
    }

    #[test]
    fn malformed_specs_are_rejected() {
        let err = split_spec("4").expect_err("no driver");
        assert!(err.to_string().contains("slot,driver"), "got: {err}");
        let err = split_spec("notaslot,virtio-rnd").expect_err("bad BDF");
        assert!(err.to_string().contains("invalid BDF"), "got: {err}");
    }
}
