//! Shared optional file-overlay configuration.
//!
//! Split into focused submodules:
//! - [`file`] — the TOML schema (`FileConfig`, sections, `ConfigError`).
//! - [`source`] — overlay discovery (`ConfigSource`, `LoadedOverlay`, resolve/load).
//! - [`env`] — environment/CLI boolean truthiness helpers.
//! - [`secrets`] — secret-strength validation + literal loopback checks.
//! - [`bootstrap`] — the shared boot dance + structured `--check-config` emitter.
//!
//! Observability config (`ObservabilityFlags`, `LogFormat`, `resolve_observability`,
//! `resolve_log_filter`) lives in [`crate::observability`].

pub mod bootstrap;
pub mod env;
pub mod file;
pub mod secrets;
pub mod source;

pub use bootstrap::{
    bootstrap, bootstrap_or_exit, Bootstrap, CheckConfigReport, CheckFormat, CheckValue,
};
pub use env::{env_is_exact, env_is_truthy, parse_bool_flag};
pub use file::{AuthSection, ConfigError, FileConfig, ObsSection};
pub use secrets::{
    decoded_master_key_len, is_loopback_url, require_unless_dev, validate_master_key_material,
    validate_stash_key, DEV_STASH_SIGNING_KEY,
};
pub use source::{
    load_overlay, log_overlay_source, resolve_overlay_string, ConfigSource, LoadedOverlay,
    SYSTEM_CONFIG_PATH,
};
