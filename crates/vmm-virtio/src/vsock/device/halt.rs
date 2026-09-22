// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Lifecycle: pause, resume, and the bounded halt.
//!
//! The halt itself is [`crate::socket_halt`].

use std::sync::atomic::Ordering;
use std::time::Duration;

use vmm_devices::lifecycle::IndicatedState;
use vmm_devices::Lifecycle;

use super::VirtioVsock;

impl Lifecycle for VirtioVsock {
    fn type_name(&self) -> &'static str {
        "virtio-vsock"
    }

    fn lifecycle_state(&self) -> Option<IndicatedState> {
        Some(self.indicator.state())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.indicator.start();
        Ok(())
    }

    fn pause(&self) {
        self.indicator.pause();
        self.shared.gate.pause();
    }

    fn is_quiesced(&self) -> bool {
        self.shared.gate.is_quiesced()
    }

    fn resume(&self) {
        self.shared.gate.resume();
        self.indicator.resume();
        // Packets queued during the pause are owed to the guest.
        self.shared.deliver_rx();
    }

    fn halt_budget(&self) -> Duration {
        crate::socket_halt::halt_budget()
    }

    fn halt(&self) {
        self.indicator.halt();
        self.shared.shutdown.store(true, Ordering::Release);
        let accept = self.accept.lock().expect("accept lock").take();
        crate::socket_halt::halt_socket_device(
            &self.shared.log,
            "virtio-vsock",
            accept,
            || {
                for key in self.shared.socket_keys() {
                    self.shared.drop_socket(key);
                }
                self.shared.take_readers()
            },
            &self.shared.socket_path,
        );
    }
}
