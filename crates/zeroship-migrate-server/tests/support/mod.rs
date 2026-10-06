//! Shared fixtures for the migration server's test executable.
//!
//! [`fixture`] owns the PostgreSQL servers the integration suites run against
//! and the throwaway tenant cluster some of them build. [`service`] builds the
//! process command the end-to-end suites share.

pub mod fixture;
pub mod service;
