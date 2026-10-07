//! Workflow process contracts that own their migrated template and disposable
//! databases, in one binary so template cloning does not race sibling suites.
//!
//! `Cargo.toml` registers this file as its own `[[test]]` target: the shared
//! `workflow_postgres` fixture clones databases from one template, and a sibling
//! module holding a session on that template would make the clone fail. The
//! suites reach the shared fixtures at the target root as
//! `crate::support::workflow_*`.

#![recursion_limit = "256"]

mod support;

mod workflow_fleet_readiness_e2e;
mod worker_retirement_e2e;
mod workflow_private_zones_e2e;
mod workflow_two_worker_e2e;
mod workflow_worker_host_e2e;
