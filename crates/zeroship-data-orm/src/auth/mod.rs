//! Per-app PostgreSQL role names and provisioning fixtures.
//!
//! Production session setup uses roles provisioned by the migration service.
//! Helpers here prepare role grants for test hosts.
//!
//! Workers must not receive replication privileges or definer-rights shortcuts.
//! Reserved system names protect migration and runtime tables in each app schema.

// Role provisioning helpers for test hosts.
pub mod bootstrap;

/// Template membership used when provisioning per-app test roles; it carries no grants.
pub const APP_ROLE_TEMPLATE: &str = "__zeroship_app_role_template";
