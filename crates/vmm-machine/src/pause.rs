// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One count over the kernel's single VM pause flag.
//!
//! `vm_pause_instance` keeps one boolean per instance and answers
//! `EALREADY` to a second `VM_PAUSE`. A `VM_RESUME` from either owner
//! starts the VM under the other. Two userspace paths stop the whole
//! VM: the operator's `pause` command, and the bus change a hot-unplug
//! makes. The guest picks the moment it runs `_EJ0`, so it picks when
//! the second one happens.
//!
//! Both go through one [`VmPauseGate`]. The first hold pauses the
//! instance and the last release resumes it, so neither owner reads the
//! other's hold as a failure and neither starts the VM while the other
//! still needs it stopped.

use std::sync::{Arc, Mutex};

use vmm_core::hdl::VmmHdl;

/// Stops every vCPU while something changes shape underneath them.
///
/// A trait, not the handle itself, so the pause order can be tested
/// without a live VM.
pub trait VmPause: Send + Sync {
    fn pause(&self) -> anyhow::Result<()>;
    fn resume(&self) -> anyhow::Result<()>;
}

impl VmPause for VmmHdl {
    fn pause(&self) -> anyhow::Result<()> {
        VmmHdl::pause(self).map_err(anyhow::Error::from)
    }

    fn resume(&self) -> anyhow::Result<()> {
        VmmHdl::resume(self).map_err(anyhow::Error::from)
    }
}

/// A refcount over one instance's pause.
pub struct VmPauseGate {
    instance: Arc<dyn VmPause>,
    holds: Mutex<u32>,
}

impl VmPauseGate {
    pub fn new(instance: Arc<dyn VmPause>) -> Arc<Self> {
        Arc::new(Self {
            instance,
            holds: Mutex::new(0),
        })
    }

    /// The gate over a running VM's handle.
    pub fn over_hdl(hdl: Arc<VmmHdl>) -> Arc<Self> {
        Self::new(hdl as Arc<dyn VmPause>)
    }

    /// Whether anything holds the pause.
    pub fn is_held(&self) -> bool {
        *self.lock() > 0
    }

    /// The count is one integer, so a panic elsewhere cannot leave it
    /// half written, and a pause must not take the VM down.
    fn lock(&self) -> std::sync::MutexGuard<'_, u32> {
        self.holds.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl VmPause for VmPauseGate {
    /// Take a hold, pausing the instance when it is the first.
    ///
    /// The lock is held across the ioctl: released around it, a second
    /// caller could read a count of zero while the first pause was still
    /// in flight and issue its own.
    fn pause(&self) -> anyhow::Result<()> {
        let mut holds = self.lock();
        if *holds == 0 {
            self.instance.pause()?;
        }
        // A VM cannot have 2^32 pause holders. Saturating keeps the
        // count from wrapping back to zero if it somehow did.
        *holds = holds.saturating_add(1);
        Ok(())
    }

    /// Give up a hold, resuming the instance when it was the last.
    fn resume(&self) -> anyhow::Result<()> {
        let mut holds = self.lock();
        match *holds {
            0 => anyhow::bail!("a resume with no pause to give up"),
            1 => {
                self.instance.resume()?;
                *holds = 0;
                Ok(())
            }
            // Somebody else still needs the VM stopped.
            _ => {
                *holds -= 1;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct RecordingInstance {
        pauses: AtomicUsize,
        resumes: AtomicUsize,
        refuses: bool,
    }

    impl VmPause for RecordingInstance {
        fn pause(&self) -> anyhow::Result<()> {
            self.pauses.fetch_add(1, Ordering::AcqRel);
            if self.refuses {
                anyhow::bail!("pause refused");
            }
            Ok(())
        }

        fn resume(&self) -> anyhow::Result<()> {
            self.resumes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    fn gate(
        instance: RecordingInstance,
    ) -> (Arc<RecordingInstance>, Arc<VmPauseGate>) {
        let instance = Arc::new(instance);
        let gate = VmPauseGate::new(Arc::clone(&instance) as Arc<dyn VmPause>);
        (instance, gate)
    }

    /// The guest picks the moment it runs `_EJ0`, so an operator pause
    /// can land inside the eject's bus change. Neither may be refused,
    /// and neither may start the VM under the other.
    #[test]
    fn a_second_holder_does_not_pause_again_or_resume_early() {
        let (instance, gate) = gate(RecordingInstance::default());

        gate.pause().expect("the eject takes the pause");
        gate.pause().expect("the operator takes it too");
        assert_eq!(instance.pauses.load(Ordering::Acquire), 1);

        // The eject finishes. The operator still needs the VM stopped.
        gate.resume().expect("the eject gives up its hold");
        assert_eq!(instance.resumes.load(Ordering::Acquire), 0);
        assert!(gate.is_held());

        gate.resume().expect("the operator resumes");
        assert_eq!(instance.resumes.load(Ordering::Acquire), 1);
        assert!(!gate.is_held());
    }

    #[test]
    fn a_resume_with_no_hold_is_refused() {
        let (instance, gate) = gate(RecordingInstance::default());

        assert!(gate.resume().is_err());
        assert_eq!(instance.resumes.load(Ordering::Acquire), 0);
    }

    /// A refused pause must not leave a hold behind: the caller aborts
    /// and nothing is left to release.
    #[test]
    fn a_refused_pause_takes_no_hold() {
        let (_instance, gate) = gate(RecordingInstance {
            refuses: true,
            ..RecordingInstance::default()
        });

        assert!(gate.pause().is_err());
        assert!(!gate.is_held());
    }

    #[test]
    fn a_pause_and_resume_pair_reaches_the_instance_once_each() {
        let (instance, gate) = gate(RecordingInstance::default());

        for _ in 0..2 {
            gate.pause().expect("pause");
            gate.resume().expect("resume");
        }

        assert_eq!(instance.pauses.load(Ordering::Acquire), 2);
        assert_eq!(instance.resumes.load(Ordering::Acquire), 2);
    }
}
