//! `auth.*` schema CRUD. The schema itself is owned by Liquibase
//! (`db/changelog`); these modules are the per-table read/write helpers.

pub mod audit;
pub mod identities;
pub mod ratelimit;
pub mod sessions;
pub mod suppressions;
pub mod users;
