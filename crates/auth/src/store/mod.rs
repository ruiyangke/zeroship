//! `auth.*` schema CRUD. Phase 1 only creates the migrations; per-table
//! CRUD modules are added in later phases as they're needed.

pub mod migrations;
pub mod ratelimit;
