//! Dev-only fixtures shared by the workflow crates.
//!
//! The workflow server, the worker registry and the control plane all build the
//! same harnesses - the migrated platform database, its journal rows, a live
//! placement and a manager queue. They live here once rather than as a private
//! copy per crate.
//!
//! The surface speaks plain data and leaf crates only. It never names a domain
//! crate whose own unit tests reach it, so `zeroship-workflow`,
//! `zeroship-workflow-runner`, `zeroship-worker` and `zeroship-workflow-v8` can
//! dev-depend on it without building a second copy of themselves. Each consumer
//! converts in a thin module beside its tests.
//!
//! Nothing here is shipped; every consumer reaches it through
//! `[dev-dependencies]`.

#![expect(
    clippy::future_not_send,
    reason = "fixture connections and bindings stay on their compio runtime"
)]

pub mod journal;
pub mod manager_queue;
pub mod placement;
pub mod platform;
