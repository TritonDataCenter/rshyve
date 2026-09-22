// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every interrupt this transport delivers, and the reset that ends
//! the driver it was raised for.
//!
//! A device backend authorises work on one thread and completes it on
//! another. A guest can reset the device between those two points, so
//! "is a session open now" is the wrong test for a completion. The
//! completion names the session it was authorised under, and
//! [`IntrGate::begin`] tests that name in the step that admits the
//! delivery.

use std::sync::{Arc, Condvar, Mutex, PoisonError};

use super::bits;
use super::{VirtioDevice, VirtioPciDevice};

use vmm_devices::pci::msix::MsixTable;

/// One driver session, as a device backend names it.
///
/// The transport makes it, and a backend keeps the value from when its
/// work was authorised. The backend gives that value back at
/// completion, so [`IntrGate::begin`] can tell "a session is open"
/// from "the session of this work is still open". A completion thread
/// parked across one whole reset sees the two differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntrSession(u64);

impl IntrSession {
    /// The session a device runs in before any reset.
    pub const INITIAL: Self = Self(0);

    /// The raw counter, for a backend that stores it in an atomic.
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    /// Only pass a value that came from [`Self::raw`]. The counter is
    /// private to the transport.
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// The transport interrupt path, as a device backend uses it.
///
/// A backend gets one after the transport is built and keeps it for
/// the life of the device. [`Self::session`] names the session of work
/// authorised now. [`Self::raise`] takes that name back and is refused
/// if the session ended.
pub struct BackendIntr {
    gate: Arc<IntrGate>,
    raise: Box<dyn Fn(IntrSession, u16) + Send + Sync>,
}

impl BackendIntr {
    pub(crate) fn new(
        gate: Arc<IntrGate>,
        raise: impl Fn(IntrSession, u16) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            gate,
            raise: Box::new(raise),
        })
    }

    /// The session a backend must name for work it authorises now.
    ///
    /// A reset ends the transport session before the backend reset.
    /// Thus a backend that reads this during its own reset gets the
    /// session of the next driver.
    pub fn session(&self) -> IntrSession {
        self.gate.session()
    }

    /// Raise for `queue_idx`, unless `session` has ended.
    pub fn raise(&self, session: IntrSession, queue_idx: u16) {
        (self.raise)(session, queue_idx);
    }

    /// A path with its own session counter, for a test that has no
    /// transport.
    #[cfg(test)]
    pub(crate) fn detached(
        raise: impl Fn(IntrSession, u16) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::new(Arc::new(IntrGate::new()), raise)
    }

    /// A path onto `gate` that records what the gate admits.
    ///
    /// The record is after admission, at the same point as the
    /// transport's delivery. Thus a test sees what reaches the guest,
    /// not what a backend asked for.
    #[cfg(test)]
    pub(crate) fn recording(
        gate: Arc<IntrGate>,
        seen: Arc<Mutex<Vec<(IntrSession, u16)>>>,
    ) -> Arc<Self> {
        let admit = Arc::clone(&gate);
        Self::new(gate, move |session, queue_idx| {
            let Some(_admitted) = admit.begin(session) else {
                return;
            };
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((session, queue_idx));
        })
    }
}

/// The transport session the next driver runs under.
///
/// A backend reads this during its own reset, or when it builds the
/// completion handler for a ring. The transport ends its session
/// before the backend reset, so the value is the new session.
///
/// `fallback` applies until the binary connects the transport. Nothing
/// can be raised before then, so the value only has to be stable.
pub(crate) fn next_intr_session(
    slot: &Mutex<Option<Arc<BackendIntr>>>,
    fallback: IntrSession,
) -> IntrSession {
    slot.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map_or(fallback, |intr| intr.session())
}

/// Where a backend keeps its transport interrupt path.
///
/// Empty until the binary installs the path. It can do that only after
/// the transport exists and the backend is moved into it. A raise
/// clones the path out of the lock first. Delivery is a kernel call and
/// a reset reads this slot, so a raise under the lock makes a vCPU
/// wait for that call.
#[derive(Default)]
pub struct IntrSlot(Mutex<Option<Arc<BackendIntr>>>);

impl IntrSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the path. The binary does this once per device.
    pub fn install(&self, intr: Arc<BackendIntr>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(intr);
    }

    /// The path, or `None` while the transport is not connected.
    pub fn get(&self) -> Option<Arc<BackendIntr>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Raise for `queue_idx` under `session`. Before the path is
    /// installed this does nothing, because no driver can have armed a
    /// ring.
    pub fn raise(&self, session: IntrSession, queue_idx: u16) {
        if let Some(path) = self.get() {
            path.raise(session, queue_idx);
        }
    }

    /// The session of the next driver, or `fallback` while the
    /// transport is not connected. See [`next_intr_session`].
    pub(crate) fn next_session(&self, fallback: IntrSession) -> IntrSession {
        next_intr_session(&self.0, fallback)
    }
}

/// Orders every interrupt this device delivers against its own reset.
///
/// A device reset runs on the vCPU that wrote DEVICE_STATUS. It must
/// return with the ISR clear and the interrupt line deasserted. It
/// cannot recall an injection that another thread already gave to the
/// kernel. So it shuts admission, waits for the admitted injections,
/// and then clears what it owns.
///
/// The two halves need different mechanisms.
///
/// `admission` decides which deliveries start. A raise names its
/// driver session. A reset first ends that session and shuts
/// admission. No holder of this lock calls the kernel, so a resetting
/// vCPU never waits behind an injection for it. [`Self::settle`] then
/// waits for the admitted deliveries. That set is closed and small: at
/// most one injection per thread already inside, and a guest cannot
/// add threads. The legacy "one write, no poll" reset contract needs
/// this wait, and VirtIO 1.3 sec 2.4.1 asks for the same order.
///
/// `line` keeps the ISR byte and the INTx level consistent. A driver
/// that reads a zero ISR claims nothing and lowers nothing. Thus a
/// level left asserted with a zero ISR blocks the line for every
/// device that shares it. This lock is held across the pin ioctl, but
/// each holder does exactly one injection.
pub(crate) struct IntrGate {
    admission: Mutex<Admission>,
    settled: Condvar,
    /// A lock-free copy of `Admission::session` for callers that only
    /// sample it. A stale value is safe: [`Self::begin`] checks again
    /// under the lock, and a stale sample is only refused.
    session: std::sync::atomic::AtomicU64,
    line: Mutex<()>,
}

struct Admission {
    /// Shut for the whole reset, not only for the session increment.
    /// A delivery admitted after the settle races the cleanup.
    open: bool,
    /// Driver session counter. Each reset adds one.
    session: u64,
    /// Deliveries admitted and not yet finished.
    in_flight: usize,
}

/// One admitted delivery, counted until it is dropped.
struct Admitted<'a> {
    gate: &'a IntrGate,
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        let mut a = self
            .gate
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        a.in_flight -= 1;
        if a.in_flight == 0 {
            self.gate.settled.notify_all();
        }
    }
}

impl IntrGate {
    pub(crate) fn new() -> Self {
        Self {
            admission: Mutex::new(Admission {
                open: true,
                session: IntrSession::INITIAL.0,
                in_flight: 0,
            }),
            settled: Condvar::new(),
            session: std::sync::atomic::AtomicU64::new(IntrSession::INITIAL.0),
            line: Mutex::new(()),
        }
    }

    /// The session a caller must name to be admitted.
    pub(crate) fn session(&self) -> IntrSession {
        IntrSession(self.session.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Admit one delivery for `session`.
    ///
    /// The session test and the count are one step under this lock.
    /// Thus a caller parked after it named its session cannot be
    /// admitted into the session that replaced it.
    fn begin(&self, session: IntrSession) -> Option<Admitted<'_>> {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !a.open || a.session != session.0 {
            return None;
        }
        a.in_flight += 1;
        Some(Admitted { gate: self })
    }

    /// Admit one delivery for the current session.
    fn begin_current(&self) -> Option<Admitted<'_>> {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !a.open {
            return None;
        }
        a.in_flight += 1;
        Some(Admitted { gate: self })
    }

    /// End the driver session and shut admission.
    pub(crate) fn end_session(&self) {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        a.open = false;
        a.session += 1;
        self.session
            .store(a.session, std::sync::atomic::Ordering::Release);
    }

    /// Wait for the deliveries admitted before admission shut.
    pub(crate) fn settle(&self) {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while a.in_flight != 0 {
            a = self.settled.wait(a).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Admit the next driver.
    pub(crate) fn reopen(&self) {
        let mut a = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        a.open = true;
    }

    /// Hold the ISR byte and the INTx level together.
    pub(crate) fn line(&self) -> std::sync::MutexGuard<'_, ()> {
        self.line.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    /// The interrupt path for completions of this device's backend.
    ///
    /// The reference to the transport is weak. The backend is inside
    /// the transport, so a strong reference makes a cycle that keeps
    /// both alive until the process exits.
    pub fn backend_intr(self: &Arc<Self>) -> Arc<BackendIntr> {
        let weak = Arc::downgrade(self);
        BackendIntr::new(Arc::clone(&self.intr), move |session, queue_idx| {
            if let Some(dev) = weak.upgrade() {
                dev.raise_queue_interrupt_in(session, queue_idx);
            }
        })
    }

    /// Assert the PCI interrupt pin.
    ///
    /// PCI INTx is level-triggered: assert while work is pending, and
    /// deassert when the guest reads the ISR. Do not deassert here. A
    /// deassert plus an assert makes a second edge. The guest sees that
    /// edge with ISR=0, reports "nobody cared" and disables the IRQ.
    ///
    /// Private to this module, because it has no session test. A
    /// backend that called it could deliver to the driver after a
    /// reset. Backends use [`BackendIntr::raise`], which names the
    /// session.
    fn raise_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.assert();
        }
    }

    /// Set the queue bit in the ISR and assert the pin.
    ///
    /// The ISR is atomic, so a worker and a vCPU can set it without the
    /// transport lock on the hot path.
    ///
    /// Private for the same reason as [`Self::raise_interrupt`].
    fn raise_queue_interrupt(&self) {
        let _line = self.intr.line();
        self.isr_status.fetch_or(
            bits::ISR_QUEUE_INTR,
            std::sync::atomic::Ordering::Release,
        );
        self.raise_interrupt();
    }

    /// Read and clear the ISR the way a legacy driver's handler does.
    ///
    /// The read, the clear and the pin change are one step under the
    /// line guard. Otherwise a raise between the swap and the deassert
    /// leaves the ISR at zero with the pin asserted. A level-triggered
    /// driver that reads zero claims nothing and never lowers the pin.
    pub(super) fn read_isr(&self) -> u8 {
        let _line = self.intr.line();
        let val = self.isr_status.swap(0, std::sync::atomic::Ordering::AcqRel);
        if val != 0 {
            self.lower_interrupt();
        }
        val
    }

    /// Deassert the interrupt after ISR is read or on device reset.
    pub(super) fn lower_interrupt(&self) {
        if let Some(pin) = &self.intr_pin {
            pin.deassert();
        }
    }

    /// Raise an interrupt for a queue, under the current session.
    ///
    /// The only caller is the migration restore. It raises on the
    /// session it just built, on the restore thread, before any vCPU
    /// can write DEVICE_STATUS. Any other caller can be parked after it
    /// names its work, and "is a session open now" is true again after
    /// a whole reset. Those callers use
    /// [`Self::raise_queue_interrupt_in`].
    ///
    /// Private to the transport for that reason. A backend uses
    /// [`BackendIntr::raise`], which carries the session of its work.
    pub(super) fn raise_queue_interrupt_current(&self, queue_idx: u16) {
        let Some(_admitted) = self.intr.begin_current() else {
            return;
        };
        self.deliver_queue_interrupt(queue_idx);
    }

    /// Raise an interrupt for a queue, for the driver session that
    /// asked for it.
    ///
    /// A reset ends the session and shuts admission before it clears
    /// the ISR and lowers the line. Thus a raise either completes before
    /// that cleanup or is refused. `session` is the session the caller
    /// was authorised under, not the current one. After a whole reset
    /// the raise is refused, not sent to the next driver.
    pub fn raise_queue_interrupt_in(
        &self,
        session: IntrSession,
        queue_idx: u16,
    ) {
        let Some(_admitted) = self.intr.begin(session) else {
            return;
        };
        self.deliver_queue_interrupt(queue_idx);
    }

    /// Route a queue interrupt the way the driver armed it.
    fn deliver_queue_interrupt(&self, queue_idx: u16) {
        // The atomic copy needs no virtio_state lock, so this is safe
        // inside notify_queue callbacks.
        let vector = self
            .msix_queue_vectors
            .get(queue_idx as usize)
            .map(|a| a.load(std::sync::atomic::Ordering::Acquire))
            .unwrap_or(bits::VIRTIO_MSI_NO_VECTOR);
        if self.route_msix(vector) {
            return;
        }
        // MSI-X is absent or disabled, so the driver uses INTx. This is
        // not an error: a guest built without CONFIG_PCI_MSI never
        // assigns a vector.
        self.raise_queue_interrupt();
    }

    /// Deliver a configuration interrupt the way the driver armed it.
    ///
    /// Both callers use this helper because a modern guest with MSI-X
    /// registers no INTx handler and never reads the ISR. The pin tells
    /// that driver nothing, and the level-triggered line stays asserted
    /// for the life of the VM. That stops every device on the line.
    fn raise_config_interrupt(&self) {
        let vector = self
            .msix_config_vector
            .load(std::sync::atomic::Ordering::Acquire);
        if self.route_msix(vector) {
            return;
        }
        let _line = self.intr.line();
        self.isr_status.fetch_or(
            bits::ISR_CFG_CHANGE,
            std::sync::atomic::Ordering::Release,
        );
        self.raise_interrupt();
    }

    /// Deliver the messages an unmask released, under the same
    /// admission as the device's own raises.
    ///
    /// A guest write to the MSI-X table or capability sends these, and
    /// no reset latch covers that write. They carry the address and
    /// data of the driver that recorded them. `session` is sampled
    /// before the write removes them from the pending array. Thus a
    /// reset from that point on refuses them, and a delivery admitted
    /// before the reset is in the set the reset waits for.
    pub(super) fn deliver_released(
        &self,
        session: IntrSession,
        msix: &MsixTable,
        released: impl IntoIterator<Item = (u64, u64)>,
    ) {
        let mut released = released.into_iter().peekable();
        if released.peek().is_none() {
            return;
        }
        #[cfg(test)]
        super::run_park(&self.parks.released_delivery_pending);
        let Some(_admitted) = self.intr.begin(session) else {
            return;
        };
        for (addr, data) in released {
            msix.send_message(addr, data);
        }
    }

    /// Send `vector` as a message, if the driver is on MSI-X.
    ///
    /// Returns true when MSI-X handles this interrupt. That includes
    /// `VIRTIO_MSI_NO_VECTOR`, which sends nothing: VirtIO 1.3 sec
    /// 4.1.5.1.2 says a vector set to NO_VECTOR gets no interrupt. An
    /// MSI-X driver registers no INTx handler, so the pin stays
    /// asserted and blocks every device that shares the line. A reset
    /// sets every vector to NO_VECTOR, so this case is frequent.
    fn route_msix(&self, vector: u16) -> bool {
        let Some(ref msix) = self.msix else {
            return false;
        };
        if !msix.is_enabled() {
            return false;
        }
        if vector != bits::VIRTIO_MSI_NO_VECTOR {
            msix.fire(vector);
        }
        true
    }

    /// Deliver a config interrupt, unless a reset ended the session
    /// that asked for it.
    ///
    /// The caller reports a device status change. It cannot hold the
    /// transport lock across the delivery, because an MSI-X write under
    /// that lock stalls every other register access. So a reset can
    /// start in the gap. The reset clears the status that this
    /// interrupt reports and lowers the line. A raise after that leaves
    /// the line asserted in the next driver's session, for a change
    /// that session never saw. Admission and the session check are one
    /// step under a lock that makes no kernel call, and the reset waits
    /// for admitted deliveries. Thus this interrupt goes to the driver
    /// that asked for it, or to no driver.
    pub(super) fn raise_config_interrupt_in(&self, session: IntrSession) {
        #[cfg(test)]
        super::run_park(&self.parks.config_interrupt_pending);
        let Some(_admitted) = self.intr.begin(session) else {
            return;
        };
        self.raise_config_interrupt();
    }
}
