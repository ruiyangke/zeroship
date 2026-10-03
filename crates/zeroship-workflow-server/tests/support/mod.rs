//! Shared fixtures for the workflow-server test target.
//!
//! `tests/main.rs` declares this module once for the whole test binary; every
//! suite reaches a fixture through `crate::support::...`. `platform`, `journal`
//! and `placement` are also included by `zeroship-control`'s tests, so they stay
//! at this path. `queue_control` is private to `server_process`, which is its
//! only consumer.

pub mod app_facts;
pub mod deployments;
pub mod holds;
pub mod journal;
pub mod leased_task;
pub mod placement;
pub mod platform;
pub mod policy;
pub mod provision;
pub mod run_journal;
pub mod scripted_app_facts;
pub mod server_process;
pub mod zone;
