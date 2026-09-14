//! Non-shipped compiled checks for the zeroship configuration contract.
//!
//! This crate intentionally sits above `zeroship-core`: it can aggregate
//! per-binary registries and Cargo targets without making the core leaf depend
//! on platform services.

pub mod audit;
pub mod contract;
pub mod docs;
pub mod fixtures;
pub mod inventory;
pub mod metadata;
pub mod registry;
