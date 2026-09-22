// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the guest expects the host to hot-add, and parsers for the
//! sysfs CPU list and `/proc/meminfo`.
//!
//! Host independent, so `cargo test` runs the parsing on any host. The
//! checks are in `hotplug_checks.rs`, which builds only for Linux.
//!
//! # The rendezvous
//!
//! A hot-add needs the host to act during the run, in order: the host
//! must not add anything until the guest records what is already there,
//! and the guest must not power off until the host adds it. The two
//! sides synchronize on the serial console:
//!
//! ```text
//! guest  take a snapshot of /sys, then print "NOTE HOTPLUG-READY ..."
//! host   wait for that line, then send the CONTROL hot-add commands
//! guest  poll for each expected resource until it arrives or the
//!        deadline passes, print one line per expectation, power off
//! ```
//!
//! The snapshot comes first so a resource that was already there cannot
//! satisfy an expectation. The deadline stops a host that adds nothing
//! from hanging the run: the guest reports what it saw, and
//! `tools/hotplug-run.sh` reads the failure.
//!
//! # Command line
//!
//! | parameter | default | meaning |
//! |---|---|---|
//! | `hotplug.expect=` | none, required | `disk`, `cpu`, `mem`, comma separated |
//! | `hotplug.timeout=` | `30` | seconds to wait for every expectation |
//! | `hotplug.disk_bytes=` | none | size the host added, checked against sysfs |
//! | `hotplug.disk_sum=` | none | sum of the first 4096 bytes, read on the host |
//! | `hotplug.mem_bytes=` | none | bytes the host asked `mem-add` for |
//!
//! The three optional keys carry the host view of what it added. Without
//! them a check proves only that a device appeared. With them it proves
//! the guest reads the same bytes as the host.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::cmdline::param;

/// Marker the host waits for before it hot-adds anything.
pub const READY: &str = "HOTPLUG-READY";

/// Seconds the guest waits when the command line does not say.
const DEFAULT_TIMEOUT_S: u64 = 30;

/// Longest wait accepted. A run that needs more has already failed. The
/// cap stops a typo from blocking the VM until the host harness times
/// out.
const MAX_TIMEOUT_S: u64 = 600;

/// One kind of resource the host can add to a running guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Disk,
    Cpu,
    Mem,
}

impl Resource {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "disk" => Ok(Self::Disk),
            "cpu" => Ok(Self::Cpu),
            "mem" => Ok(Self::Mem),
            other => Err(format!(
                "hotplug.expect names '{other}', want disk, cpu or mem"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Cpu => "cpu",
            Self::Mem => "mem",
        }
    }
}

/// What this run expects, and what the host says it added.
#[derive(Debug)]
pub struct HotplugSpec {
    pub expect: Vec<Resource>,
    pub timeout: Duration,
    /// Size of the hot-added disk, in bytes.
    pub disk_bytes: Option<u64>,
    /// Sum of the first 4096 bytes of that disk, as the host read them.
    pub disk_sum: Option<u64>,
    /// Bytes the host asked `mem-add` for.
    pub mem_bytes: Option<u64>,
}

impl HotplugSpec {
    pub fn from_cmdline(cmdline: &str) -> Result<Self, String> {
        // No default: a run that expects nothing would report OK and
        // prove nothing.
        let raw = param(cmdline, "hotplug.expect").ok_or_else(|| {
            "hotplug.expect= is missing; a run that expects nothing \
             proves nothing"
                .to_string()
        })?;

        let mut expect: Vec<Resource> = Vec::new();
        for name in raw.split(',').filter(|s| !s.is_empty()) {
            let resource = Resource::parse(name)?;
            if !expect.contains(&resource) {
                expect.push(resource);
            }
        }
        if expect.is_empty() {
            return Err(format!(
                "hotplug.expect={raw} names no resource; want disk, cpu \
                 or mem"
            ));
        }

        Ok(Self {
            expect,
            timeout: timeout(cmdline)?,
            disk_bytes: number(cmdline, "hotplug.disk_bytes")?,
            disk_sum: number(cmdline, "hotplug.disk_sum")?,
            mem_bytes: number(cmdline, "hotplug.mem_bytes")?,
        })
    }

    pub fn wants(&self, resource: Resource) -> bool {
        self.expect.contains(&resource)
    }

    /// The expectation list, as it went on the command line.
    pub fn expect_text(&self) -> String {
        self.expect
            .iter()
            .map(|r| r.name())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// One line naming every setting, for the report.
    pub fn summary(&self) -> String {
        format!(
            "expect={} timeout={}s disk_bytes={} disk_sum={} mem_bytes={}",
            self.expect_text(),
            self.timeout.as_secs(),
            opt(self.disk_bytes),
            opt(self.disk_sum),
            opt(self.mem_bytes),
        )
    }
}

fn timeout(cmdline: &str) -> Result<Duration, String> {
    let Some(text) = param(cmdline, "hotplug.timeout") else {
        return Ok(Duration::from_secs(DEFAULT_TIMEOUT_S));
    };
    let seconds: u64 = text
        .parse()
        .map_err(|_| format!("hotplug.timeout={text} is not a second count"))?;
    if seconds == 0 || seconds > MAX_TIMEOUT_S {
        return Err(format!(
            "hotplug.timeout={seconds} is outside 1..{MAX_TIMEOUT_S}"
        ));
    }
    Ok(Duration::from_secs(seconds))
}

fn number(cmdline: &str, key: &str) -> Result<Option<u64>, String> {
    match param(cmdline, key) {
        None => Ok(None),
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| format!("{key}={text} is not a number")),
    }
}

fn opt(value: Option<u64>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "-".to_string(),
    }
}

/// Read a sysfs CPU list, the `0-3,7` form.
///
/// A malformed entry is dropped, not guessed: the set is evidence for a
/// report, and an absent range is better than a wrong one.
pub fn parse_cpu_list(text: &str) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            None => {
                if let Ok(id) = part.parse() {
                    ids.insert(id);
                }
            }
            Some((first, last)) => {
                let (Ok(first), Ok(last)) =
                    (first.parse::<u32>(), last.parse::<u32>())
                else {
                    continue;
                };
                // A reversed range is malformed. It is dropped, as is an
                // unparsable one.
                if first <= last {
                    ids.extend(first..=last);
                }
            }
        }
    }
    ids
}

/// `MemTotal` out of `/proc/meminfo`, in kB, the unit the file uses.
pub fn mem_total_kb(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expectation_list_parses() {
        let spec = HotplugSpec::from_cmdline("hotplug.expect=disk,cpu,mem")
            .expect("parse");
        assert_eq!(
            spec.expect,
            vec![Resource::Disk, Resource::Cpu, Resource::Mem]
        );
        assert!(spec.wants(Resource::Cpu));
        assert_eq!(spec.timeout, Duration::from_secs(DEFAULT_TIMEOUT_S));
    }

    #[test]
    fn a_repeated_resource_is_listed_once() {
        let spec =
            HotplugSpec::from_cmdline("hotplug.expect=cpu,cpu").expect("parse");
        assert_eq!(spec.expect, vec![Resource::Cpu]);
    }

    /// The harness must never report a pass that checked nothing.
    #[test]
    fn a_run_that_expects_nothing_is_refused() {
        assert!(HotplugSpec::from_cmdline("console=ttyS0").is_err());
        assert!(HotplugSpec::from_cmdline("hotplug.expect=").is_err());
        assert!(HotplugSpec::from_cmdline("hotplug.expect=,,").is_err());
    }

    #[test]
    fn an_unknown_resource_is_refused() {
        let error = HotplugSpec::from_cmdline("hotplug.expect=disk,nic")
            .expect_err("nic is not a resource");
        assert!(error.contains("nic"), "{error}");
    }

    #[test]
    fn the_timeout_is_bounded() {
        let spec =
            HotplugSpec::from_cmdline("hotplug.expect=cpu hotplug.timeout=45")
                .expect("parse");
        assert_eq!(spec.timeout, Duration::from_secs(45));
        for bad in ["0", "601", "thirty", "-5"] {
            let line = format!("hotplug.expect=cpu hotplug.timeout={bad}");
            assert!(
                HotplugSpec::from_cmdline(&line).is_err(),
                "timeout={bad} was accepted"
            );
        }
    }

    #[test]
    fn the_host_numbers_parse() {
        let spec = HotplugSpec::from_cmdline(
            "hotplug.expect=disk,mem hotplug.disk_bytes=1048576 \
             hotplug.disk_sum=522240 hotplug.mem_bytes=134217728",
        )
        .expect("parse");
        assert_eq!(spec.disk_bytes, Some(1048576));
        assert_eq!(spec.disk_sum, Some(522240));
        assert_eq!(spec.mem_bytes, Some(134217728));
        assert!(HotplugSpec::from_cmdline(
            "hotplug.expect=disk hotplug.disk_bytes=1M"
        )
        .is_err());
    }

    #[test]
    fn a_cpu_list_reads_singles_and_ranges() {
        assert_eq!(parse_cpu_list("0-1"), BTreeSet::from([0, 1]));
        assert_eq!(parse_cpu_list("0-2,5"), BTreeSet::from([0, 1, 2, 5]));
        assert_eq!(parse_cpu_list("3\n"), BTreeSet::from([3]));
        assert!(parse_cpu_list("").is_empty());
        assert!(parse_cpu_list("2-1").is_empty());
        assert!(parse_cpu_list("x-y").is_empty());
    }

    #[test]
    fn memtotal_reads_the_meminfo_line() {
        let meminfo = "MemFree:          123 kB\nMemTotal:      499876 kB\n";
        assert_eq!(mem_total_kb(meminfo), Some(499876));
        assert_eq!(mem_total_kb("MemFree: 1 kB\n"), None);
        assert_eq!(mem_total_kb("MemTotal:  many kB\n"), None);
    }
}
