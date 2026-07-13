//! Operator/lifecycle operations over the migration engine.
//!
//! These modules orchestrate already-defined apply/backend capabilities; backend
//! implementations and data seams live under [`mod@crate::apply`].

pub mod squash;
pub mod status;
// The confined submit path is PG-only (role provisioning over `admin_conn`, the
// shadow dry-run harness), so it rides the PG seam (`host-pg`).
