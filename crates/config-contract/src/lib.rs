//! Non-shipped compiled checks for the zeroship configuration contract.
//!
//! This crate intentionally sits above `zeroship-core`: it can aggregate
//! per-binary registries and Cargo targets without making the core leaf depend
//! on platform services.

pub mod contract;
pub mod fixtures;
pub mod metadata;
pub mod raw_env;
