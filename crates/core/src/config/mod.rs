//! Shared optional file-overlay configuration.
//!
//! Split into focused submodules:
//! - [`file`] — the TOML schema (`FileConfig`, sections, `ConfigError`).
//! - [`source`] — overlay discovery (`ConfigSource`, `LoadedOverlay`, resolve/load).
//! - [`env`] — the sole raw process-environment boundary, plus pure truthiness.
//! - [`declared`] — typed keys for reads the config contract does not generate.
//! - [`secrets`] — secret-strength validation + literal loopback checks.
//! - [`bootstrap`] — the shared boot dance + structured `--check-config` emitter.
//!
//! - [`names`] — canonical identities, source-policy wrappers, and the
//!   `zeroship_config` attribute that generates every spelling from one
//!   declaration.
//!
//! Observability types (`LogFormat`, `resolve_log_filter`, tracing init) live in
//! [`crate::observability`]; the observability SETTINGS are ordinary generated
//! `observability.*` declarations owned by each binary's config module.

pub mod bootstrap;
pub mod declared;
pub mod env;
pub mod file;
pub mod names;
pub mod secrets;
pub mod source;
pub mod topology;

pub use declared::{
    consumer_of, is_valid_env_name, read_declared_env_os_value, read_declared_env_value,
    read_process_env_snapshot_value, DeclaredEnvKey, DeclaredEnvRead, EnvClass, TestHarness,
    DECLARED_ENV_READS, PROCESS_ENV_SNAPSHOT,
};

pub use bootstrap::{
    bootstrap, bootstrap_or_exit, Bootstrap, CheckConfigReport, CheckFormat, CheckValue,
    ObservabilityControls, OverlaySelector,
};
pub use env::{env_is_exact, env_is_truthy, parse_bool_flag};
pub use file::{
    AuthSection, ConfigError, ControlSection, FileConfig, GatewaySection, MigratedSection,
    ObsSection, SchedulerSection, SecretSection, WorkerSection,
};
pub use names::{
    BootstrapControl, CanonicalName, CanonicalNameError, CliEnv, CommandControl, CommandEnv,
    ConfigConsumer, ConfigResolveError, ConfigSpec, Consumer, ConsumerToken, DevEnv, EnvKey,
    EnvReadError, ExternalEnv, ExternalEnvFamily, GeneratedConfig, Operational,
    OverlayLookupError, ReadSite, Secret, Sensitivity, SourceKind, SupplyClass, TestEnv,
    CONFIG_READ_SITES, lookup_overlay, read_typed_env, resolve_control, resolve_operational,
    resolve_secret_sources,
};
pub use zeroship_config_macros::zeroship_config;
pub use secrets::{
    decoded_master_key_len, is_loopback_url, is_secret_ref, obtain_secret, parse_secret_ref,
    require_nonempty, resolve_secret, resolve_secret_or_exit, validate_master_key_material,
    validate_pairwise_salt, validate_secret_ref, validate_secret_ref_or_exit, validate_stash_key,
    validate_worker_key, SecretError, SecretRef,
};
pub use source::{
    load_overlay, log_overlay_source, resolve_overlay_string, ConfigSource, LoadedOverlay,
    SYSTEM_CONFIG_PATH,
};
pub use topology::{
    resolve_origin_scheme, resolve_trusted_origins, OriginScheme, TrustedOrigin,
};
