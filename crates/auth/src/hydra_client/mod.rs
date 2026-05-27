//! Hand-rolled hydra admin API client.
//!
//! Hydra's auto-generated `ory-hydra-client` crate pulls in reqwest+tokio
//! which conflicts with the zero-tokio invariant. We hand-roll a small
//! cyper-based client.

pub mod types;
