// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small device-agnostic parse helpers shared by every VMM binary.

use vmm_devices::pci::Bdf;

/// Parse a BDF string: "dev", "dev:func", or "bus:dev:func".
pub fn parse_bdf(s: &str) -> Option<Bdf> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            let dev: u8 = parts[0].parse().ok()?;
            Bdf::new(0, dev, 0)
        }
        2 => {
            let dev: u8 = parts[0].parse().ok()?;
            let func: u8 = parts[1].parse().ok()?;
            Bdf::new(0, dev, func)
        }
        3 => {
            let bus: u8 = parts[0].parse().ok()?;
            let dev: u8 = parts[1].parse().ok()?;
            let func: u8 = parts[2].parse().ok()?;
            Bdf::new(bus, dev, func)
        }
        _ => None,
    }
}

/// Extract a device config from `-l` flags (e.g. `com1,/dev/zconsole`).
pub fn find_lpc_device(lpc_args: &[String], device: &str) -> Option<String> {
    let prefix = format!("{},", device);
    for arg in lpc_args {
        if let Some(config) = arg.strip_prefix(&prefix) {
            return Some(config.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{find_lpc_device, parse_bdf};

    #[test]
    fn parse_bdf_accepts_the_three_forms() {
        let dev = parse_bdf("4").expect("bare device number");
        assert_eq!((dev.bus(), dev.dev(), dev.func()), (0, 4, 0));

        let df = parse_bdf("4:2").expect("dev:func");
        assert_eq!((df.bus(), df.dev(), df.func()), (0, 4, 2));

        let bdf = parse_bdf("1:4:2").expect("bus:dev:func");
        assert_eq!((bdf.bus(), bdf.dev(), bdf.func()), (1, 4, 2));
    }

    #[test]
    fn parse_bdf_rejects_out_of_range_and_garbage() {
        assert!(parse_bdf("32").is_none(), "device number max is 31");
        assert!(parse_bdf("0:0:8").is_none(), "function number max is 7");
        assert!(parse_bdf("1:2:3:4").is_none());
        assert!(parse_bdf("nvme").is_none());
        assert!(parse_bdf("").is_none());
    }

    #[test]
    fn find_lpc_device_strips_the_device_prefix() {
        let args = vec![
            "bootrom,/usr/share/bhyve/BHYVE_UEFI.fd".to_string(),
            "com1,/dev/zconsole".to_string(),
        ];
        assert_eq!(
            find_lpc_device(&args, "com1"),
            Some("/dev/zconsole".to_string())
        );
        assert_eq!(find_lpc_device(&args, "com2"), None);
    }
}
