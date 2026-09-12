//! Configuration contract for the separately deployed CDC relay.
//!
//! Capture, authentication, slots, and transport are private binary modules.
//! Workers depend on the wire contract and the ORM client, never this service.

pub mod config;
