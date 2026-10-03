//! Leaf declaration fixtures for the configuration contract.
//!
//! A leaf on purpose: the compile-fail suite drives trybuild over this crate's
//! dependency graph, so linking a platform service here would make every case
//! rebuild that service's closure for no contract coverage.

pub mod fixtures;
