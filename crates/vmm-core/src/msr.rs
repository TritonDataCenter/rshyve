// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Synthetic MSR dispatch seam.
//!
//! A vCPU run loop offers every RDMSR and WRMSR exit to an optional
//! handler before it applies its own default. The handler is a trait
//! object so shared machine code does not name the crate that supplies
//! the MSRs. Only rshyve builds the Hyper-V enlightenment, and the
//! microVM must have no dependency path to it.

/// Outcome of a `rdmsr` dispatch.
#[derive(Debug)]
pub enum RdmsrOutcome {
    /// MSR was handled. Deliver `value` to the guest.
    Handled(u64),
    /// MSR is outside this handler's range. Caller should fall
    /// through to its default RDMSR behavior.
    NotHandled,
    /// MSR belongs to this handler but the guest's read is forbidden
    /// (for example a write-only MSR, or a disabled feature). Caller
    /// should inject `#GP`.
    GpException,
}

/// Outcome of a `wrmsr` dispatch.
#[derive(Debug)]
pub enum WrmsrOutcome {
    /// Write absorbed. Resume execution.
    Handled,
    /// MSR is outside this handler's range. Caller should fall
    /// through to its default WRMSR behavior.
    NotHandled,
    /// Reserved bits set, write to a read-only MSR, or feature
    /// disabled. Caller should inject `#GP`.
    GpException,
    /// Guest wrote the reset MSR. Caller should suspend the VM with
    /// `VM_SUSPEND_RESET`.
    Reset,
}

/// A source of synthetic MSRs for a guest.
///
/// `Send + Sync` because every vCPU thread shares one handler.
pub trait MsrHandler: Send + Sync {
    /// Offer a guest RDMSR of `msr` on `vcpu_id` to this handler.
    fn rdmsr(&self, vcpu_id: i32, msr: u32) -> RdmsrOutcome;

    /// Offer a guest WRMSR of `value` to `msr` on `vcpu_id` to this
    /// handler.
    fn wrmsr(&self, vcpu_id: i32, msr: u32, value: u64) -> WrmsrOutcome;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::{MsrHandler, RdmsrOutcome, WrmsrOutcome};

    const HANDLED: u32 = 0x4000_0000;
    const FORBIDDEN: u32 = 0x4000_0001;
    const RESETS: u32 = 0x4000_0002;
    const FOREIGN: u32 = 0x0000_001b;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Rd { vcpu_id: i32, msr: u32 },
        Wr { vcpu_id: i32, msr: u32, value: u64 },
    }

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<Call>>,
    }

    impl MsrHandler for Recorder {
        fn rdmsr(&self, vcpu_id: i32, msr: u32) -> RdmsrOutcome {
            self.calls.lock().unwrap().push(Call::Rd { vcpu_id, msr });
            match msr {
                HANDLED => RdmsrOutcome::Handled(0xfeed),
                FORBIDDEN => RdmsrOutcome::GpException,
                _ => RdmsrOutcome::NotHandled,
            }
        }

        fn wrmsr(&self, vcpu_id: i32, msr: u32, value: u64) -> WrmsrOutcome {
            self.calls.lock().unwrap().push(Call::Wr {
                vcpu_id,
                msr,
                value,
            });
            match msr {
                HANDLED => WrmsrOutcome::Handled,
                FORBIDDEN => WrmsrOutcome::GpException,
                RESETS => WrmsrOutcome::Reset,
                _ => WrmsrOutcome::NotHandled,
            }
        }
    }

    #[test]
    fn dyn_dispatch_carries_arguments_and_outcomes() {
        let recorder = Recorder::default();
        let handler: &dyn MsrHandler = &recorder;

        assert!(matches!(
            handler.rdmsr(3, HANDLED),
            RdmsrOutcome::Handled(0xfeed)
        ));
        assert!(matches!(
            handler.rdmsr(3, FORBIDDEN),
            RdmsrOutcome::GpException
        ));
        assert!(matches!(
            handler.rdmsr(3, FOREIGN),
            RdmsrOutcome::NotHandled
        ));
        assert!(matches!(
            handler.wrmsr(1, HANDLED, 0x55),
            WrmsrOutcome::Handled
        ));
        assert!(matches!(
            handler.wrmsr(1, FORBIDDEN, 0),
            WrmsrOutcome::GpException
        ));
        assert!(matches!(handler.wrmsr(1, RESETS, 1), WrmsrOutcome::Reset));
        assert!(matches!(
            handler.wrmsr(1, FOREIGN, 0),
            WrmsrOutcome::NotHandled
        ));

        assert_eq!(
            *recorder.calls.lock().unwrap(),
            vec![
                Call::Rd {
                    vcpu_id: 3,
                    msr: HANDLED
                },
                Call::Rd {
                    vcpu_id: 3,
                    msr: FORBIDDEN
                },
                Call::Rd {
                    vcpu_id: 3,
                    msr: FOREIGN
                },
                Call::Wr {
                    vcpu_id: 1,
                    msr: HANDLED,
                    value: 0x55
                },
                Call::Wr {
                    vcpu_id: 1,
                    msr: FORBIDDEN,
                    value: 0
                },
                Call::Wr {
                    vcpu_id: 1,
                    msr: RESETS,
                    value: 1
                },
                Call::Wr {
                    vcpu_id: 1,
                    msr: FOREIGN,
                    value: 0
                },
            ]
        );
    }

    #[test]
    fn one_handler_serves_every_vcpu_thread() {
        // The run loop gives each vCPU thread a clone of the same
        // handle, so the trait must stay Send + Sync.
        let recorder = Arc::new(Recorder::default());
        let handler: Arc<dyn MsrHandler> = recorder.clone();

        let threads: Vec<_> = (0..4)
            .map(|vcpu_id| {
                let handler = Arc::clone(&handler);
                thread::spawn(move || {
                    assert!(matches!(
                        handler.rdmsr(vcpu_id, HANDLED),
                        RdmsrOutcome::Handled(0xfeed)
                    ));
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(recorder.calls.lock().unwrap().len(), 4);
    }
}
