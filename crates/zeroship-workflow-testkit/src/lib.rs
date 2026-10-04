//! Dev-only fixtures shared by the workflow crates.
//!
//! The workflow server, the worker registry and the control plane all build the
//! same harnesses - the migrated platform database, its journal rows, an app's
//! queue scope and a manager queue. They live here once rather than as a
//! private copy per crate.
//!
//! The surface speaks plain data and leaf crates only. It never names a domain
//! crate whose own unit tests reach it, so `zeroship-workflow`,
//! `zeroship-workflow-runner`, `zeroship-worker` and `zeroship-workflow-v8` can
//! dev-depend on it without building a second copy of themselves. The
//! workflow-typed adapters over the plain surface are held once in
//! [`workflow_fixtures!`] and expanded in the crate that uses them.
//!
//! Nothing here is shipped; every consumer reaches it through
//! `[dev-dependencies]`.

pub mod adapters;
pub mod deployments;
pub mod journal;
pub mod journal_server;
pub mod manager_queue;
pub mod zone;
pub mod platform;
