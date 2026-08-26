//! Operator/lifecycle operations over the migration engine.
//!
//! These modules orchestrate already-defined apply/backend capabilities; backend
//! implementations and data seams live under [`mod@crate::apply`].

pub mod squash;
pub mod status;
// No submit module is declared here, and that is the point of saying so. A confined
// submit path is PostgreSQL-only: it provisions roles over a separately-privileged
// connection and runs a shadow dry-run harness, and neither is an operation the other
// registered backends can answer.
