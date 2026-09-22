// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device model traits and implementations.
//!
//! All emulated devices implement [`Lifecycle`] for VM state
//! transitions and migration.

pub mod acpi;
pub mod acpi_gpe;
pub mod acpi_pm;
pub mod aml;
pub mod bhyve;
pub mod blkdev;
pub mod chipset;
pub mod fwcfg;
pub mod hotplug;
pub mod input;
pub mod lifecycle;
pub mod migrate;
pub mod pci;
pub mod pvpanic;
pub mod quiesce;
pub mod smbios;
pub mod uart;

pub use input::{InputBroker, KeyboardSink, PointerSink};
pub use lifecycle::{
    DeviceMigrateState, FlushError, FlushIntent, IndicatedState, Indicator,
    Lifecycle,
};
pub use migrate::{
    MigrateCtx, MigrateMulti, MigrateSingle, MigrateStateError, Migrator,
    PayloadOffer, PayloadOffers, PayloadOutput, PayloadOutputs, Schema,
    SchemaId,
};
pub use quiesce::{wait_all_quiesced, QuiesceGate, QuiesceReport};
