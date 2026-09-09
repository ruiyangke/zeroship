//! Fixture builders shared by the wire test files.
//!
//! Deliberately independent of `zeroship-core`: the round-trip and rejection
//! corpora must exercise THIS crate's parser, not the platform one. The only
//! file that brings the two together is `typed_id_oracle.rs`, and it is the only
//! place the two are allowed to be compared.

use zeroship_cdc_wire::{
    AppId, ClusterId, DatabaseEpoch, DatabaseId, DatastoreId, GrantGeneration, LeaderTerm, RelayId,
    WorkerId,
};

/// A canonical body of the encoder's width, ending in `n`.
///
/// `0` is a valid base62 digit and leading zeros are exactly how the fixed-width
/// encoding pads, so this produces genuinely canonical ids without needing the
/// platform encoder.
#[must_use]
pub fn body(n: u32) -> String {
    format!("{n:0>25}")
}

#[must_use]
pub fn app_id(n: u32) -> AppId {
    AppId::parse(&format!("app_{}", body(n))).expect("canonical app id")
}

#[must_use]
pub fn database_id(n: u32) -> DatabaseId {
    DatabaseId::parse(&format!("dbs_{}", body(n))).expect("canonical database id")
}

#[must_use]
pub fn datastore_id(n: u32) -> DatastoreId {
    DatastoreId::parse(&format!("ds_{}", body(n))).expect("canonical datastore id")
}

#[must_use]
pub fn cluster_id(n: u32) -> ClusterId {
    ClusterId::parse(&format!("clu_{}", body(n))).expect("canonical cluster id")
}

#[must_use]
pub fn worker_id(n: u32) -> WorkerId {
    WorkerId::parse(&format!("wrk_{}", body(n))).expect("canonical worker id")
}

#[must_use]
pub fn relay_id(n: u32) -> RelayId {
    RelayId::parse(&format!("rly_{}", body(n))).expect("canonical relay id")
}

#[must_use]
pub fn epoch(n: u64) -> DatabaseEpoch {
    DatabaseEpoch::new(n).expect("in-domain epoch")
}

#[must_use]
pub fn grant(n: u64) -> GrantGeneration {
    GrantGeneration::new(n).expect("in-domain grant generation")
}

#[must_use]
pub fn term(n: u64) -> LeaderTerm {
    LeaderTerm::new(n).expect("in-domain leader term")
}
