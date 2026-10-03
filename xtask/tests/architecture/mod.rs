//! Data-plane boundary and database-posture checks over the workspace.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list
//! is a suite that silently stops running.

mod boundaries;
mod contracts;
mod posture;
pub(crate) mod repo;
mod source;
