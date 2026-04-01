//! # appbase-server
//!
//! HTTP server for the appbase platform.
//!
//! - `router`: axum routes for RPC, static serving, health, stats
//! - `middleware`: Tower layers for CORS, compression, tracing, CPU time headers
//! - Supports both single-app and multi-tenant modes
//! - Dev mode adds WebSocket reload + save endpoint via optional layer

pub mod middleware;
pub mod router;
pub mod v8pool;
