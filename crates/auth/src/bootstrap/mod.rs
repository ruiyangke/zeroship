//! First-boot bootstrap: JWK generation + OIDC client reconciliation.
//!
//! Task 13 lands the clients-config parser; Tasks 14 and 15 layer in
//! `keys` and the orchestrator. The stub here grows in lockstep so each
//! commit type-checks cleanly.

pub mod clients_config;
