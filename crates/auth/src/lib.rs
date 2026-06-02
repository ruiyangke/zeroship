//! Library surface for integration tests. The binary is `main.rs`.

#[doc(hidden)]
pub mod advisory_lock;
pub mod audit;
pub mod bootstrap;
pub mod config;
pub mod cron;
pub mod csrf;
pub mod error;
pub mod headers;
pub mod hydra_client;
pub mod identity;
pub mod mailer;
pub mod ratelimit;
pub mod server;
pub mod sessions;
pub mod store;
pub mod startup_validation;
pub mod ui;
