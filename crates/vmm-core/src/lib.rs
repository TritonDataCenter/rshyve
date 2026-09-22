// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Core VMM abstractions for the bhyve hypervisor.
//!
//! Provides safe Rust wrappers over bhyve kernel ioctls including:
//! - `VmmHdl` - VM instance handle
//! - `PhysMap` - Guest physical address space management
//! - `MemCtx` - Safe guest memory access
//! - `Machine` - Aggregate VM hardware
//! - `Vcpu` - Per-vCPU execution context
//! - `PioBus` - Port I/O bus dispatch
//! - `MmioBus` - Memory-mapped I/O bus dispatch

pub mod aspace;
pub mod common;
pub mod cpuid;
pub mod exits;
pub mod hdl;
pub mod intr_pins;
pub mod machine;
pub mod mem;
pub mod metrics;
pub mod mmio;
pub mod msr;
pub mod pio;
pub mod poll;
pub mod ratelimit;
pub mod thread;
pub mod unixsock;
pub mod vcpu;

pub use common::{RWOp, ReadOp, WriteOp};
pub use hdl::{CreateOpts, SuspendOutcome, VmmHdl};
pub use machine::{Machine, MachineSetup, VM_MAXCPU};
pub use mem::{DevMemSeg, MemCtx, PhysMap, SegidAlloc, VM_MAX_MEMSEGS};
pub use mmio::MmioBus;
pub use pio::PioBus;
pub use vcpu::Vcpu;
