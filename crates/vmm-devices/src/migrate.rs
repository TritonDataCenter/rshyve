// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Derived from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: lib/propolis/src/migrate.rs
// https://github.com/oxidecomputer/propolis

//! Traits and types that export and import device state for live
//! migration.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use vmm_core::MemCtx;

#[derive(Debug, Error)]
pub enum MigrateStateError {
    #[error("device not migratable")]
    NonMigratable,

    #[error("device's state is not ready to be exported")]
    NotReadyForExport,

    #[error("IO Error")]
    Io(#[from] std::io::Error),

    #[error("could not deserialize device state: {0}")]
    DeserializationFailed(String),

    #[error("failed to apply deserialized device state: {0}")]
    ImportFailed(String),

    #[error("failed to find suitable import payload")]
    DataMissing,

    #[error("kind/version of payload not expected: {0} v{1}")]
    UnexpectedPayload(String, u32),
}

impl From<erased_serde::Error> for MigrateStateError {
    fn from(err: erased_serde::Error) -> Self {
        MigrateStateError::DeserializationFailed(err.to_string())
    }
}

/// The migration support of a device.
pub enum Migrator<'a> {
    NonMigratable,
    /// No device-specific logic is necessary.
    Empty,
    Single(&'a dyn MigrateSingle),
    Multi(&'a dyn MigrateMulti),
}

/// A device migrated using a single typed payload.
pub trait MigrateSingle: Send + Sync + 'static {
    fn export(
        &self,
        ctx: &MigrateCtx<'_>,
    ) -> Result<PayloadOutput, MigrateStateError>;
    fn import(
        &self,
        offer: PayloadOffer<'_>,
        ctx: &MigrateCtx<'_>,
    ) -> Result<(), MigrateStateError>;
}

/// A device migrated using multiple typed payloads.
pub trait MigrateMulti: Send + Sync {
    fn export(
        &self,
        output: &mut PayloadOutputs,
        ctx: &MigrateCtx<'_>,
    ) -> Result<(), MigrateStateError>;
    fn import(
        &self,
        offer: &mut PayloadOffers<'_>,
        ctx: &MigrateCtx<'_>,
    ) -> Result<(), MigrateStateError>;
}

pub struct MigrateCtx<'a> {
    pub mem: &'a MemCtx,
}

pub struct PayloadOffer<'a> {
    pub kind: &'a str,
    pub version: u32,
    pub payload: Box<dyn erased_serde::Deserializer<'a> + 'a>,
}

impl<'a> PayloadOffer<'a> {
    /// Parse the payload if it matches the specified Schema.
    pub fn parse<T: Schema<'a>>(&mut self) -> Result<T, MigrateStateError> {
        if !self.matches::<T>() {
            return Err(MigrateStateError::UnexpectedPayload(
                self.kind.into(),
                self.version,
            ));
        }
        let res = erased_serde::deserialize(&mut self.payload)?;
        Ok(res)
    }

    fn matches<'x, T: Schema<'x>>(&self) -> bool {
        let id = T::id();
        id.0 == self.kind && id.1 == self.version
    }
}

/// Collection of [`PayloadOffer`] instances for multi-payload import.
pub struct PayloadOffers<'a>(Vec<PayloadOffer<'a>>);

impl<'a> PayloadOffers<'a> {
    pub fn new(items: impl IntoIterator<Item = PayloadOffer<'a>>) -> Self {
        Self(Vec::from_iter(items))
    }

    /// Take a payload matching the specified Schema.
    pub fn take<T: Schema<'a>>(&mut self) -> Result<T, MigrateStateError> {
        self.take_schema(T::id())?
            .ok_or(MigrateStateError::DataMissing)?
            .parse()
    }

    fn take_schema(
        &mut self,
        id: SchemaId,
    ) -> Result<Option<PayloadOffer<'a>>, MigrateStateError> {
        let matches: Vec<usize> = self
            .0
            .iter()
            .enumerate()
            .filter(|(_, offer)| offer.kind == id.0 && offer.version == id.1)
            .map(|(idx, _)| idx)
            .collect();

        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(self.0.remove(matches[0]))),
            n => Err(MigrateStateError::ImportFailed(format!(
                "expected 1 payload for {:?}, found {}",
                id, n,
            ))),
        }
    }
}

pub struct PayloadOutput {
    pub kind: &'static str,
    pub version: u32,
    pub payload: Box<dyn erased_serde::Serialize>,
}

/// Collection of [`PayloadOutput`] instances for multi-payload export.
pub struct PayloadOutputs(Vec<PayloadOutput>);

impl Default for PayloadOutputs {
    fn default() -> Self {
        Self::new()
    }
}

impl PayloadOutputs {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn push(
        &mut self,
        output: PayloadOutput,
    ) -> Result<(), MigrateStateError> {
        self.0.push(output);
        Ok(())
    }
}

impl IntoIterator for PayloadOutputs {
    type Item = PayloadOutput;
    type IntoIter = std::vec::IntoIter<PayloadOutput>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// (kind, version).
pub type SchemaId = (&'static str, u32);

/// The kind and version of a migration payload type.
pub trait Schema<'de>: Serialize + Deserialize<'de> + Sized + 'static {
    fn id() -> SchemaId;
}

impl<'a, T: Schema<'a>> From<T> for PayloadOutput {
    fn from(value: T) -> Self {
        let id = T::id();
        PayloadOutput {
            kind: id.0,
            version: id.1,
            payload: Box::new(value),
        }
    }
}
