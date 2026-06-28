//! Operator/lifecycle operations over the migration engine.
//!
//! These modules orchestrate already-defined apply/backend capabilities; backend
//! implementations and data seams live under [`mod@crate::apply`].

pub mod squash;
pub mod status;
pub mod submit;
