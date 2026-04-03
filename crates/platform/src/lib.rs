//! # appbase-platform
//!
//! Multi-tenant platform layer for appbase. Re-exports all platform concerns
//! through a single crate boundary.
//!
//! ## Architecture
//!
//! ```text
//! Layer 1: Runtime (appbase-runtime / isolate_v8 + compiler)
//!   V8 isolate, ESM modules, crypto, fetch, URL, timers, KV
//!   esbuild bundler, storage, HTTP server
//!   Zero platform deps — works standalone for local dev
//!
//! Layer 2: Platform (this crate)
//!   Control plane, app registry, deploy API
//!   Metering, billing, enforcement, plans
//!   Multi-tenant routing, per-app secrets
//!
//! Layer 3: CLI (appbase binary)
//!   Thin wrapper dispatching to runtime or platform
//! ```
//!
//! ## Usage
//!
//! For local development, use the runtime directly (`appbase-run`).
//! For multi-tenant deployment, use this platform layer.

// Re-export platform components
pub use appbase_core as core;
pub use appbase_control as control;
pub use appbase_metering as metering;
pub use appbase_enforcement as enforcement;
pub use appbase_billing as billing;
pub use appbase_plan as plan;
pub use appbase_server as server;
