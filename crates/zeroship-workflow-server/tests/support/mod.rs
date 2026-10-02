//! Shared fixtures for the workflow-server integration contracts.
//!
//! `tests/main.rs` declares this module once for the whole test binary; every
//! suite reaches a fixture through `crate::support::...` rather than including
//! the file a second time. `queue_control` is private to `server_process`,
//! which is its only consumer.

pub mod app_facts;
pub mod deployments;
pub mod holds;
pub mod journal;
pub mod platform;
pub mod policy;
pub mod run_journal;
pub mod server_process;
pub mod zone;
