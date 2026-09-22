// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `-s` PCI device catalog: every driver argv or a hot-add can
//! name, and the checks that run over the whole set.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use slog::{info, warn, Logger};

use vmm_config::Cli;
use vmm_core::machine::Machine;
use vmm_devices::chipset::i440fx::I440FxChipset;
use vmm_devices::InputBroker;
use vmm_machine::{devspec, CreatedPciDevice, DeviceRegistry};
use vmm_virtio::attach::{self, VirtioAttachCtx};

use crate::{bootorder, msix};

/// The registry, the AHCI-CD flag, and the plumbing a device needs to
/// be built. The context outlives startup so hotplug can build one
/// after the VM is running.
pub type PciDeviceInventory = (
    Arc<DeviceRegistry>,
    bool,
    Arc<vmm_machine::pci::OwnedPciCtx>,
);

/// Parse all `-s` PCI slot specifications and create the devices.
/// Returns the run-time registry, which owns every handle. The run,
/// control and teardown paths read it. The AHCI flag lets the control
/// path reject migration without parsing the specifications again.
pub fn init_pci_devices(
    machine: &Machine,
    chipset: &Arc<I440FxChipset>,
    cli: &Cli,
    num_vcpus: u32,
    input: &Arc<InputBroker>,
    log: &Logger,
) -> anyhow::Result<PciDeviceInventory> {
    let is_migrate = cli.migrate_listen.is_some();

    reject_unmigratable_destination(is_migrate, &cli.pci_slot)?;

    let mut specs: Vec<String> = Vec::with_capacity(cli.pci_slot.len());
    for slot_spec in &cli.pci_slot {
        let (device_spec, _) = bootorder::strip_bootindex(slot_spec)?;
        specs.push(device_spec);
    }
    let has_ahci_cd = specs_have_ahci_cd(&specs);

    let owned = Arc::new(vmm_machine::pci::OwnedPciCtx::new(
        machine, chipset, input, log,
    ));
    let ctx = vmm_machine::PciDeviceCtx::borrow_from(&owned);
    let vm_name = &cli.vm_name;
    let attached =
        vmm_machine::attach_pci_devices(&ctx, &specs, &mut |spec, ctx| {
            parse_and_create_pci_device(
                spec, num_vcpus, is_migrate, vm_name, ctx,
            )
        })?;

    Ok((attached.registry, has_ahci_cd, owned))
}

/// The catalog closure, bound to this VM and handed to the hotplug
/// engine.
///
/// SECURITY: argv drives the same closure, so a control request can name
/// no driver and no path that a command line cannot. It does open a host
/// file that a socket peer chose, which needs authority equal to argv.
/// The peercred check in `control::listener` grants that authority: the
/// peer must have this process's uid and be in this zone or the global
/// zone.
pub fn hotplug_factory(
    cli: &Cli,
    num_vcpus: u32,
) -> vmm_machine::HotplugFactory {
    let vm_name = cli.vm_name.clone();
    Box::new(move |spec, ctx| {
        // Never a migration destination: a hot-add happens on a running
        // VM, and the guest programs the new rings itself.
        parse_and_create_pci_device(spec, num_vcpus, false, &vm_name, ctx)
    })
}

fn pci_spec_driver(spec: &str) -> Option<&str> {
    Some(devspec::parts(spec).driver).filter(|driver| !driver.is_empty())
}

/// True when any bootindex-stripped spec names the ahci-cd driver.
///
/// Read from the spec text alone. The attach seam is device-agnostic and
/// reports no driver names, so the flag cannot come from the handles it
/// returns.
fn specs_have_ahci_cd(specs: &[String]) -> bool {
    specs
        .iter()
        .any(|spec| pci_spec_driver(spec) == Some("ahci-cd"))
}

fn reject_unmigratable_destination(
    is_migrate_dest: bool,
    specs: &[String],
) -> anyhow::Result<()> {
    if is_migrate_dest {
        if let Some(driver) = specs.iter().find_map(|spec| {
            let driver = pci_spec_driver(spec)?;
            matches!(driver, "fbuf" | "ahci-cd").then_some(driver)
        }) {
            anyhow::bail!(
                "{} is not supported on a migration destination",
                driver
            );
        }
    }
    Ok(())
}

fn reject_ahci_hd<T>() -> anyhow::Result<T> {
    anyhow::bail!(
        "AHCI device type 'ahci-hd' is not implemented; use virtio-blk or nvme"
    );
}

/// Parse a `-s` PCI slot specification and create the device.
///
/// Format: `[bus:]dev[:func],driver[,config...]`
/// Examples:
///   `4,virtio-blk,/dev/zvol/rdsk/zones/uuid/disk0`
///   `0:5:0,virtio-blk,/path/to/disk.raw`
///
/// Every attach here uses the fallible variant. A panic in
/// `attach_device` would poison the bus lock and stop the whole VM, and
/// this function runs on a control thread as well as at startup.
fn parse_and_create_pci_device(
    spec: &str,
    num_vcpus: u32,
    is_migrate_dest: bool,
    vm_name: &str,
    ctx: &vmm_machine::PciDeviceCtx<'_>,
) -> anyhow::Result<CreatedPciDevice> {
    let chipset = ctx.chipset;
    let bus_pio = ctx.bus_pio;
    let bus_mmio = ctx.bus_mmio;
    let physmap = ctx.physmap;
    let vmm_hdl = ctx.vmm_hdl;
    let segids = ctx.segids;
    let input = ctx.input;
    let log = ctx.log;

    let virtio_ctx = VirtioAttachCtx {
        physmap,
        bus_pio,
        bus_mmio,
        hdl: vmm_hdl,
    };

    let (bdf, driver, config) = devspec::split(spec)?;

    match driver {
        "virtio-blk" | "virtio-blk-pci" => {
            anyhow::ensure!(
                !config.is_empty(),
                "virtio-blk requires a disk path"
            );
            let (path, blk_opts) = devspec::parse_blk_config(config);
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_blk(
                std::path::Path::new(path),
                &blk_opts,
                num_vcpus,
                lintr,
                &virtio_ctx,
            )?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-blk attached";
                "bdf" => %bdf,
                "path" => config,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "virtio-net-viona" => {
            let (vnic_name, promiscphys) = devspec::parse_viona_config(config);

            // On a migrate destination the rings are not programmed yet, so
            // the poll thread waits for start_poll_deferred() after restore.
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_viona(
                vnic_name,
                promiscphys,
                lintr,
                is_migrate_dest,
                &virtio_ctx,
                log,
            )?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-net-viona attached";
                "bdf" => %bdf,
                "vnic" => vnic_name,
                "promiscphys" => promiscphys,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "nvme" => {
            if config.is_empty() {
                anyhow::bail!("nvme requires a disk path");
            }
            let (path, blk_opts) = devspec::parse_blk_config(config);
            use std::fs::OpenOptions;
            let file = OpenOptions::new()
                .read(true)
                .write(!blk_opts.read_only)
                .open(path)
                .with_context(|| format!("failed to open disk: {}", path))?;

            let nvme = vmm_storage::nvme::NvmeController::new_with_logger(
                file,
                blk_opts.read_only,
                bdf.dev(), // unique instance ID per slot
                Arc::clone(physmap),
                Arc::clone(vmm_hdl),
                Arc::clone(bus_mmio),
                log.new(slog::o!("component" => "nvme")),
            );

            chipset.try_attach_device(bdf, nvme.clone())?;
            info!(log, "nvme attached";
                "bdf" => %bdf,
                "path" => path,
            );
            Ok(vmm_machine::created(nvme))
        }
        "virtio-console" => {
            let path = devspec::parse_console_config(config)?;
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_console(
                std::path::Path::new(path),
                lintr,
                &virtio_ctx,
                log,
            )?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-console attached";
                "bdf" => %bdf,
                "socket" => path,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "virtio-fs" => {
            use vmm_virtio::fs::clamp_queue_size;
            let opts = devspec::parse_fs_config(config)?;
            let queue_size = clamp_queue_size(opts.queue_size);
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev =
                attach::virtio_fs(&opts, queue_size, lintr, &virtio_ctx, log)?;
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-fs attached";
                "bdf" => %bdf,
                "tag" => &opts.tag,
                "path" => %opts.path.display(),
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "virtio-rnd" => {
            let lintr = chipset.route_lintr(&bdf);
            let pci_dev = attach::virtio_rng(lintr, &virtio_ctx);
            chipset.try_attach_device(bdf, pci_dev.clone())?;
            info!(log, "virtio-rnd attached";
                "bdf" => %bdf,
                "msix_vectors" => msix::vector_count(pci_dev.as_ref()),
            );
            Ok(vmm_machine::created(pci_dev))
        }
        "fbuf" => {
            let segid = segids.alloc().ok_or_else(|| {
                anyhow::anyhow!(
                    "no free memory segment for fbuf (kernel limit is {} per VM)",
                    vmm_core::mem::VM_MAX_MEMSEGS
                )
            })?;

            let FbufConfig {
                vnc_path,
                vnc_password,
                width,
                height,
            } = parse_fbuf_config(config, vm_name, log)?;

            let fbuf = vmm_hid::fbuf::Framebuffer::new(
                width,
                height,
                &vnc_path,
                vnc_password,
                Arc::clone(bus_mmio),
                Arc::clone(vmm_hdl),
                segid,
                Arc::clone(input),
                log.new(slog::o!("component" => "fbuf")),
            )?;
            chipset.try_attach_device(bdf, fbuf.clone())?;
            info!(log, "fbuf attached";
                "bdf" => %bdf,
                "vnc" => vnc_path.display().to_string(),
                "resolution" => format!("{}x{}", width, height),
            );
            Ok(vmm_machine::created(fbuf))
        }
        "xhci" => {
            // The config names the USB device. "tablet" is the only one.
            let lintr = chipset.route_lintr(&bdf);
            let intr_pin = lintr.map(|(_, pin)| pin);
            let xhci = vmm_hid::xhci::XhciController::new(
                Arc::clone(physmap),
                Arc::clone(bus_mmio),
                intr_pin,
            );
            chipset.try_attach_device(bdf, xhci.clone())?;
            input.set_pointer(xhci.clone());
            info!(log, "xhci tablet attached"; "bdf" => %bdf);
            Ok(vmm_machine::created(xhci))
        }
        "passthru" => {
            if config.is_empty() {
                anyhow::bail!(
                    "passthru requires a device path (e.g., /dev/ppt0)"
                );
            }
            // The first config field is the /dev/pptN path.
            let path = config.split(',').next().unwrap_or(config);
            let dev = vmm_passthru::PciPassthru::new(
                path,
                Arc::clone(vmm_hdl),
                Arc::clone(bus_pio),
                log.clone(),
            )
            .with_context(|| {
                format!("failed to init passthru device: {}", path)
            })?;
            chipset.try_attach_device(bdf, dev.clone())?;
            info!(log, "passthru attached";
                "bdf" => %bdf,
                "path" => path,
            );
            Ok((Some(dev as Arc<dyn vmm_devices::pci::PciDevice>), None))
        }
        "ahci-cd" => {
            let path = devspec::parse_ahci_cd_path(config)?;
            let media = vmm_storage::ahci::media::IsoMedia::open(
                std::path::Path::new(path),
            )
            .with_context(|| format!("failed to open ISO: {}", path))?;
            let lintr = chipset.route_lintr(&bdf);
            let ahci = vmm_storage::ahci::AhciCtrl::new(
                media,
                Arc::clone(physmap),
                Arc::clone(bus_mmio),
                lintr.map(|(_, pin)| pin),
                log.new(slog::o!("component" => "ahci-cd")),
            );
            chipset.try_attach_device(bdf, ahci.clone())?;
            info!(log, "ahci-cd attached"; "bdf" => %bdf, "path" => path);
            Ok(vmm_machine::created(ahci))
        }
        // A separate arm stops an unsupported disk from becoming a
        // read-only ATAPI device with different guest-visible semantics.
        "ahci-hd" => reject_ahci_hd(),
        "hostbridge" => {
            // The chipset creates this device.
            Ok((None, None))
        }
        "lpc" => {
            // The chipset creates this device.
            Ok((None, None))
        }
        other => {
            warn!(log, "unknown PCI driver, skipping";
                "driver" => other,
                "bdf" => %bdf,
            );
            Ok((None, None))
        }
    }
}

/// What an `fbuf` spec asks for.
#[derive(Debug)]
struct FbufConfig {
    vnc_path: PathBuf,
    vnc_password: Option<String>,
    width: u16,
    height: u16,
}

/// Parse `vga=off,unix=/path[,password=pw][,password-file=/path][,w=N][,h=N]`.
///
/// A key that does not parse is refused, not replaced by the default.
/// Otherwise a typo such as `w=abc` boots a 1024x768 framebuffer with no
/// error, and the VMM seems to ignore the operator.
fn parse_fbuf_config(
    config: &str,
    vm_name: &str,
    log: &Logger,
) -> anyhow::Result<FbufConfig> {
    let mut parsed = FbufConfig {
        vnc_path: PathBuf::from("/var/run/rshyve")
            .join(vm_name)
            .join("vnc.sock"),
        vnc_password: None,
        width: 1024,
        height: 768,
    };
    let mut password_in_argv = false;

    for opt in config.split(',') {
        if let Some(p) = opt.strip_prefix("unix=") {
            parsed.vnc_path = PathBuf::from(p);
        } else if let Some(p) = opt.strip_prefix("password-file=") {
            parsed.vnc_password =
                Some(crate::secret::read_file(std::path::Path::new(p), log)?);
        } else if let Some(p) = opt.strip_prefix("password=") {
            parsed.vnc_password = Some(p.to_string());
            password_in_argv = true;
        } else if let Some(w) = opt.strip_prefix("w=") {
            parsed.width = parse_fbuf_dimension("w", w)?;
        } else if let Some(h) = opt.strip_prefix("h=") {
            parsed.height = parse_fbuf_dimension("h", h)?;
        } else if !opt.is_empty() && opt != "vga=off" {
            anyhow::bail!("fbuf: '{opt}' is not an fbuf option");
        }
    }

    if password_in_argv {
        crate::secret::warn_argv_exposure(
            log,
            "fbuf password=",
            "fbuf password-file=",
        );
    }
    Ok(parsed)
}

fn parse_fbuf_dimension(key: &str, value: &str) -> anyhow::Result<u16> {
    let parsed: u16 = value
        .parse()
        .with_context(|| format!("fbuf: {key}={value} is not 1..=65535"))?;
    if parsed == 0 {
        anyhow::bail!("fbuf: {key}=0 describes no framebuffer");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn an_fbuf_size_that_does_not_parse_is_refused() {
        // A silent 1024x768 boot makes a typo look like the VMM ignores
        // the operator.
        for spec in ["w=abc", "h=abc", "w=100000", "w=0", "h=0", "w="] {
            let error = parse_fbuf_config(spec, "vm", &null_log())
                .expect_err("a size that does not parse must be refused");
            assert!(format!("{error:#}").contains("fbuf"), "{spec}: {error:#}");
        }
    }

    #[test]
    fn an_fbuf_spec_is_parsed_whole() {
        let parsed = parse_fbuf_config(
            "vga=off,unix=/tmp/vnc.sock,password=pw,w=800,h=600",
            "vm",
            &null_log(),
        )
        .expect("a complete spec");

        assert_eq!(parsed.vnc_path, PathBuf::from("/tmp/vnc.sock"));
        assert_eq!(parsed.vnc_password.as_deref(), Some("pw"));
        assert_eq!((parsed.width, parsed.height), (800, 600));
    }

    #[test]
    fn an_fbuf_spec_defaults_to_the_vm_socket_path() {
        let parsed =
            parse_fbuf_config("", "testvm", &null_log()).expect("no options");

        assert_eq!(
            parsed.vnc_path,
            PathBuf::from("/var/run/rshyve/testvm/vnc.sock"),
        );
        assert_eq!((parsed.width, parsed.height), (1024, 768));
    }

    #[test]
    fn an_unknown_fbuf_option_is_refused() {
        let error = parse_fbuf_config("widht=800", "vm", &null_log())
            .expect_err("a misspelled key");

        assert!(format!("{error:#}").contains("widht=800"), "{error:#}");
    }

    #[test]
    fn ahci_hd_is_rejected() {
        let error = reject_ahci_hd::<()>().unwrap_err();
        let message = error.to_string();

        assert!(message.contains("not implemented"), "{message}");
        // The refusal has to name what to use instead, or an operator
        // is left with a driver name and no next step.
        assert!(message.contains("virtio-blk"), "{message}");
    }

    #[test]
    fn specs_have_ahci_cd_reads_the_driver_field() {
        let specs = vec![
            "4,virtio-blk,/dev/dsk/test".to_string(),
            "5,ahci-cd,/x.iso".to_string(),
        ];

        assert!(specs_have_ahci_cd(&specs));
        assert!(!specs_have_ahci_cd(&specs[..1]));
        // A path that merely mentions the driver name is not a match.
        assert!(!specs_have_ahci_cd(&["4,nvme,/ahci-cd".to_string()]));
    }

    #[test]
    fn reject_unmigratable_destination_rejects_ahci_cd() {
        let specs = vec!["5,ahci-cd,/x.iso".to_string()];

        let error = reject_unmigratable_destination(true, &specs).unwrap_err();
        assert!(error.to_string().contains("ahci-cd"));
    }

    #[test]
    fn reject_unmigratable_destination_still_rejects_fbuf() {
        let fbuf = vec!["30,fbuf,unix=/tmp/vm.vnc".to_string()];

        let err = reject_unmigratable_destination(true, &fbuf).unwrap_err();
        assert_eq!(
            err.to_string(),
            "fbuf is not supported on a migration destination"
        );
        assert!(reject_unmigratable_destination(false, &fbuf).is_ok());
    }

    #[test]
    fn reject_unmigratable_destination_allows_normal_specs() {
        let specs = vec![
            "4,virtio-blk,/dev/dsk/test".to_string(),
            "5,nvme,/tmp/fbuf".to_string(),
        ];

        assert!(reject_unmigratable_destination(true, &specs).is_ok());
    }
}
