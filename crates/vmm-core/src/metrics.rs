// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-vCPU exit counters.
//!
//! Lock-free atomic counters on the VM-exit path, read through the
//! control socket's `metrics` (JSON) and `metrics-prometheus` (text)
//! commands. There is no HTTP endpoint and no scrape thread.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::exits::VmExitKind;

/// Relaxed ordering for counters: no cross-counter consistency is
/// needed, only monotonic increments that become visible eventually.
const ORD: Ordering = Ordering::Relaxed;

/// Per-vCPU metrics.
#[derive(Default)]
pub struct VcpuMetrics {
    /// Total VM exits.
    pub exits: AtomicU64,
    /// VM exits by type.
    pub exits_pio: AtomicU64,
    pub exits_mmio: AtomicU64,
    pub exits_hlt: AtomicU64,
    pub exits_msr: AtomicU64,
    pub exits_other: AtomicU64,
}

impl VcpuMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one exit under the bucket its kind belongs to.
    ///
    /// The kind itself is the input, not its DTrace number: a bucket
    /// keyed on the number silently misclassifies every counter when
    /// [`VmExitKind::exit_code`] changes.
    #[inline]
    pub fn record_exit(&self, kind: &VmExitKind) {
        self.exits.fetch_add(1, ORD);
        let bucket = match kind {
            VmExitKind::Inout(_) => &self.exits_pio,
            VmExitKind::Mmio(_) | VmExitKind::InstEmul { .. } => {
                &self.exits_mmio
            }
            VmExitKind::Rdmsr(_) | VmExitKind::Wrmsr(_, _) => &self.exits_msr,
            VmExitKind::Hlt => &self.exits_hlt,
            _ => &self.exits_other,
        };
        bucket.fetch_add(1, ORD);
    }

    pub fn to_json(&self, vcpu_id: i32) -> String {
        format!(
            r#"{{"vcpu":{},"exits":{},"exits_pio":{},"exits_mmio":{},"exits_hlt":{},"exits_msr":{},"exits_other":{}}}"#,
            vcpu_id,
            self.exits.load(ORD),
            self.exits_pio.load(ORD),
            self.exits_mmio.load(ORD),
            self.exits_hlt.load(ORD),
            self.exits_msr.load(ORD),
            self.exits_other.load(ORD),
        )
    }

    pub fn to_prometheus(&self, vcpu_id: i32) -> String {
        let id = vcpu_id;
        format!(
            "vmm_vcpu_exits_total{{vcpu=\"{}\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"pio\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"mmio\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"hlt\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"msr\"}} {}\n\
             vmm_vcpu_exits{{vcpu=\"{}\",type=\"other\"}} {}\n",
            id,
            self.exits.load(ORD),
            id,
            self.exits_pio.load(ORD),
            id,
            self.exits_mmio.load(ORD),
            id,
            self.exits_hlt.load(ORD),
            id,
            self.exits_msr.load(ORD),
            id,
            self.exits_other.load(ORD),
        )
    }
}

#[cfg(test)]
mod tests;
