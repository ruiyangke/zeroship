//! [`ZoneId`] names one operator-declared execution zone: a set of worker
//! deployment units that share creator-side connectivity.
//!
//! An app and a worker instance each belong to exactly one zone, and Control
//! freezes both once written. The workflow service matches an app's zone to a
//! worker's before it lets that worker claim the app's work, so the id is named
//! on both sides of the wire and lives in this leaf beside [`crate::AppId`]
//! rather than in any one consumer.
//!
//! The default zone is seeded as DATA by
//! `db/migrations-ts/20260914000450_execution_zones_default_zone.ts`; a
//! single-zone deployment composes its worker into it.

use crate::entity_id::declare_entity_id;
use crate::typed_id::EXECUTION_ZONE_PREFIX;

declare_entity_id! {
    /// The typed id of one execution zone: `ezn_<base36(uuidv7)>`.
    ZoneId,
    EXECUTION_ZONE_PREFIX,
    zone_id_tests,
}

/// The fixed identity of the single seeded zone row.
const DEFAULT_ZONE: &str = "ezn_default000000000000000000";

impl ZoneId {
    /// The zone a single-zone deployment seeds
    /// (`db/migrations-ts/20260914000450_execution_zones_default_zone.ts`).
    /// The local host composes its in-process worker into it.
    ///
    /// # Panics
    /// Panics if [`DEFAULT_ZONE`] is not a canonical execution zone id, which
    /// the migration corpus pins.
    #[must_use]
    pub fn default_zone() -> Self {
        Self::parse(DEFAULT_ZONE).expect("the seeded default zone is canonical")
    }
}