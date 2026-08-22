pub mod backend;
pub mod baseline;
pub mod drift;
pub mod executor;
// The journal's dialect-neutral vocabulary moved down to the backend contract,
// where the three per-vendor journal writers can see it. Re-exported so every
// `crate::apply::journal::…` reference resolves unchanged.
pub use zero_migrate_backend::journal;
pub mod plan_precondition;
pub mod precondition;
pub mod role;
// The finite-timeout-budget rule moved down to the backend contract, where the
// vendors that resolve a budget can see it. Re-exported so
// `crate::apply::timeout::{resolve_timeout_ms, IndefiniteTimeoutError, TimeoutOrigin}`
// still resolve.
pub use zero_migrate_backend::timeout;
