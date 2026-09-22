// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What survives a restart, and what a closed engine refuses.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::super::*;
use super::{Fixture, StubLifecycle};

#[test]
fn only_hot_added_devices_are_replayed_on_a_restart() {
    // An argv device already has its own -s entry, with the bootindex
    // token the registry does not keep.
    let fixture = Fixture::new();
    fixture
        .registry
        .insert(RegisteredDevice::new(
            "virtio-blk@4",
            Bdf::new(0, 4, 0),
            None,
            None,
            Some("4,virtio-blk,/boot".to_string()),
        ))
        .expect("a boot device");
    fixture.add("5,virtio-blk,/added").expect("added");

    assert_eq!(hotplug_specs(&fixture.registry), ["5,virtio-blk,/added"]);
}

#[test]
fn a_slot_on_its_way_out_is_not_replayed() {
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/added").expect("added");
    fixture
        .registry
        .set_state(&id, SlotState::Present, SlotState::RemovePending)
        .expect("requested");
    // A guest that never runs _EJ0 keeps the device, so it is replayed.
    assert_eq!(hotplug_specs(&fixture.registry), ["5,virtio-blk,/added"]);

    // A device whose teardown has begun is going, so replaying its spec
    // in the next run would open its backing file again.
    fixture
        .registry
        .set_state(&id, SlotState::RemovePending, SlotState::Ejecting)
        .expect("ejecting");
    assert!(hotplug_specs(&fixture.registry).is_empty());

    fixture
        .registry
        .set_state(&id, SlotState::Ejecting, SlotState::Absent)
        .expect("absent");
    assert!(hotplug_specs(&fixture.registry).is_empty());
}

/// viona's release is an untimed `cv_wait` that heeds no signal. Run on
/// the drain thread it would answer no later eject, and teardown's join
/// would never return.
#[test]
fn a_release_that_never_returns_does_not_park_the_drain_thread() {
    let fixture = Fixture::new();
    let lifecycle = StubLifecycle::wedged_halt();
    let (added, device, _) =
        fixture.add_with("5,virtio-blk,/added", Arc::clone(&lifecycle));
    let id = added.expect("added");
    fixture.core.request_remove(&id).expect("requested");
    fixture.guest_ejects(5);

    let started = std::time::Instant::now();
    fixture.core.drain_ejects();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the drain took {:?}",
        started.elapsed(),
    );
    // The slot is not taken apart: a worker can still be inside the
    // device that is leaving.
    assert_eq!(
        fixture.registry.get_by_id(&id).map(|d| d.state),
        Some(SlotState::Ejecting),
    );
    assert_eq!(device.regions_detached.load(Ordering::Acquire), 0);
    assert_eq!(lifecycle.resumes.load(Ordering::Acquire), 0);

    lifecycle.halt_wedges.store(false, Ordering::Release);
}

/// A refused drain thread means nothing acts on the guest's `_EJ0`, so
/// the slot would report remove-pending for the life of the VM and be
/// replayed on the next boot.
#[test]
fn a_removal_is_refused_when_no_thread_would_drain_it() {
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/added").expect("added");

    fixture.core.drains.store(false, Ordering::Release);

    assert_eq!(
        fixture.core.request_remove(&id),
        Err(HotplugError::NotDrained),
    );
    // The slot is untouched, so the device goes on serving the guest.
    assert_eq!(
        fixture.registry.get_by_id(&id).map(|d| d.state),
        Some(SlotState::Present),
    );
}

/// Teardown takes its device list once. Anything added after that is
/// never halted, and its `vmm_drv` lease parks the VM destroy.
#[test]
fn a_closed_engine_refuses_every_add_and_removal() {
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/added").expect("added");

    fixture.core.closed.store(true, Ordering::Release);

    assert_eq!(fixture.add("6,virtio-blk,/late"), Err(HotplugError::Closed),);
    assert_eq!(fixture.core.request_remove(&id), Err(HotplugError::Closed),);
    // The device that was already there is untouched, so the teardown
    // sweep still finds it.
    assert!(fixture.registry.get_by_id(&id).is_some());
}

/// The guest picks the moment it runs `_EJ0`, so it can pick the instant
/// the VM powers off. An eject then halts the same device the teardown
/// sweep is halting, from a second thread.
#[test]
fn a_closed_engine_drains_no_eject() {
    let fixture = Fixture::new();
    let id = fixture.add("5,virtio-blk,/added").expect("added");
    fixture
        .registry
        .set_state(&id, SlotState::Present, SlotState::RemovePending)
        .expect("requested");
    fixture.guest_ejects(5);

    fixture.core.closed.store(true, Ordering::Release);
    fixture.core.drain_ejects();

    assert!(fixture.registry.get_by_id(&id).is_some());
    assert_eq!(
        fixture.registry.get_by_id(&id).map(|d| d.state),
        Some(SlotState::RemovePending),
    );
}
