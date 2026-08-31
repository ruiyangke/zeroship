//! `auth.*` schema CRUD. The schema itself is owned by zeroship-migrate
//! (`db/migrations-ts`, applied by the compose `migrate` service /
//! `deploy/ops/db-migrate.sh`); these modules are the per-table read/write helpers.

pub mod audit;
pub mod identities;
pub mod relay;
pub mod sessions;
pub mod totp;
pub mod users;
