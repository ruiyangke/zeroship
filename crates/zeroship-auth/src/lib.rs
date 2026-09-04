//! Library surface for integration tests. The binary is `main.rs`.

// `target_session_attrs` grew from three variants to six and each connect path
// now carries a recovery probe, which widened the async chain reaching this
// crate past rustc's default layout-query depth of 128. Structural fixes were
// tried first and do not help: the depth is cumulative across the whole
// pool -> connect -> handshake chain, so boxing any single future removes one
// level, not the ~130 reported. `crates/zeroship-plugin-db/src/lib.rs` and
// `crates/zeroship-gateway/src/main.rs` already carry this for the same reason. It is a
// compiler resource limit, not a correctness guard.
#![recursion_limit = "256"]

#[doc(hidden)]
pub mod advisory_lock;
pub mod audit;
pub mod config;
pub mod cron;
pub mod csrf;
pub mod error;
pub mod headers;
pub mod identity;
pub mod oidc;
pub mod return_to;
pub mod server;
pub mod sessions;
pub mod store;
pub mod startup_validation;
pub mod ui;
