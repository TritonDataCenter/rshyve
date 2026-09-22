// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use serde_json::Value;
use std::collections::BTreeSet;

/// The control plane parses these names, so a rename is a breaking
/// change to the `metrics` command.
#[test]
fn to_json_field_names_unchanged() {
    let metrics = VcpuMetrics::new();
    let parsed: Value = serde_json::from_str(&metrics.to_json(1))
        .expect("to_json emits valid JSON");
    let names: BTreeSet<&str> = parsed
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "vcpu",
            "exits",
            "exits_pio",
            "exits_mmio",
            "exits_hlt",
            "exits_msr",
            "exits_other",
        ])
    );
}

#[test]
fn every_exit_kind_lands_in_the_bucket_its_name_promises() {
    let metrics = VcpuMetrics::new();
    metrics.record_exit(&VmExitKind::Hlt);
    metrics.record_exit(&VmExitKind::Rdmsr(0x10));
    metrics.record_exit(&VmExitKind::Wrmsr(0x10, 0));
    metrics.record_exit(&VmExitKind::Debug);
    metrics.record_exit(&VmExitKind::Unknown(99));

    assert_eq!(metrics.exits.load(ORD), 5);
    assert_eq!(metrics.exits_hlt.load(ORD), 1);
    assert_eq!(metrics.exits_msr.load(ORD), 2);
    assert_eq!(metrics.exits_other.load(ORD), 2);
    assert_eq!(metrics.exits_pio.load(ORD), 0);
    assert_eq!(metrics.exits_mmio.load(ORD), 0);
}

#[test]
fn a_prometheus_dump_names_the_vcpu_in_every_series() {
    let metrics = VcpuMetrics::new();
    metrics.record_exit(&VmExitKind::Hlt);
    let text = metrics.to_prometheus(3);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|l| l.contains("vcpu=\"3\"")), "{text}");
    assert!(
        lines.iter().any(|l| l.contains("type=\"hlt\"} 1")),
        "{text}"
    );
}
