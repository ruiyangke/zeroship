//! First-boot bootstrap: JWK generation + OIDC client reconciliation.
//!
//! Task 14 adds the `keys` module; Task 15 will turn this module into
//! the orchestrator that ties keys + client reconciliation together.

pub mod clients_config;
pub mod keys;
