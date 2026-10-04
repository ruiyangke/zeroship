//! Shared fixtures for the workflow-server test target.
//!
//! `tests/main.rs` declares this module once for the whole test binary; every
//! suite reaches a fixture through `crate::support::...`. `journal` and
//! `platform` (with its queue scope seeding) are shared with `zeroship-control`'s
//! tests, so they come from `zeroship-workflow-testkit`. `queue_control` is
//! private to `server_process`, which is its only consumer.

pub use zeroship_workflow_testkit::{journal, platform};

pub mod app_facts;
pub mod deployments;
pub mod holds;
pub mod leased_task;
pub mod policies;
pub mod policy;
pub mod provision;
pub mod run_journal;
pub mod scripted_app_facts;
pub mod server_process;
pub mod zone;
