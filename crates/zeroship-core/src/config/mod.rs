//! Shared optional file-overlay configuration.
//!
//! Split into focused submodules:
//! - [`auth_kind`] - the one auth-provider vocabulary (`AuthProviderKind`).
//! - [`mod@file`] — the TOML schema (`FileConfig`, sections, `ConfigError`).
//! - [`source`] — overlay discovery (`ConfigSource`, `LoadedOverlay`, resolve/load).
//! - [`mod@env`] - the sole raw process-environment boundary, plus pure truthiness.
//! - [`declared`] - typed keys for reads the config contract does not generate.
//! - [`secrets`] — secret-strength validation + literal loopback checks.
//! - [`credential_gate`] - the named sentinel, the boot refusal and its banner,
//!   the per-subsystem audit, and the build-profile dev escape.
//! - [`diagnostics`] - the shared env-name scanner every binary's
//!   "this refusal names something settable" test drives.
//! - [`mod@bootstrap`] — the shared boot dance + structured `--check-config` emitter.
//!
//! - [`names`] — canonical identities, source-policy wrappers, and the
//!   `zeroship_config` attribute that generates every spelling from one
//!   declaration.
//!
//! Observability types (`LogFormat`, `resolve_log_filter`, tracing init) live in
//! [`crate::observability`]; the observability SETTINGS are ordinary generated
//! `observability.*` declarations owned by each binary's config module.

pub mod auth_kind;
pub mod bootstrap;
pub mod credential_gate;
pub mod declared;
pub mod diagnostics;
pub mod env;
pub mod file;
pub mod names;
pub mod secrets;
pub mod source;
pub mod test_overlay;
pub mod topology;

pub use declared::{
    consumer_of, is_valid_env_name, read_declared_env_family_value, read_declared_env_os_value,
    read_declared_env_value, read_process_env_snapshot_value, DeclaredEnvFamily, DeclaredEnvKey,
    DeclaredEnvRead, EnvClass, TestHarness, DECLARED_ENV_READS, PROCESS_ENV_SNAPSHOT,
};

pub use auth_kind::AuthProviderKind;

pub use credential_gate::{
    audit_credentials, dev_escape_active, is_unset_credential, mark_dev_escape_active,
    unset_credential_message, BuildProfile, CredentialPosture, CredentialVerdict,
    SubsystemCredential, WeakCredential, REMEDIATION_COMMAND, SERVICE_CREDENTIAL_SENTINEL,
};

pub use diagnostics::env_like_tokens;

pub use bootstrap::{
    bootstrap, bootstrap_or_exit, default_http_threads, require_http_threads, Bootstrap,
    CheckConfigReport, CheckFormat, CheckValue, ObservabilityControls, OverlaySelector,
};
pub use env::{env_is_exact, env_is_truthy, parse_bool_flag};
pub use file::{
    AuthSection, CdcServerSection, ConfigError, ControlSection, FileConfig, GatewaySection,
    MigrateServerSection, OauthClientRegistration, ObsSection, WorkerSection,
};
pub use names::{
    BootstrapControl, CanonicalName, CanonicalNameError, CliEnv, CommandControl, CommandEnv,
    ConfigConsumer, ConfigResolveError, ConfigSpec, Consumer, ConsumerToken, DevEnv, EnvKey,
    EnvReadError, ExternalEnv, ExternalEnvFamily, GeneratedConfig, Operational,
    OverlayLookupError, ReadSite, Secret, SecretResolution, Sensitivity, SourceKind, SupplyClass,
    TestEnv, CONFIG_READ_SITES, lookup_overlay, read_typed_env, resolve_control,
    resolve_operational, resolve_secret_sources,
};
pub use zeroship_config_macros::zeroship_config;
pub use secrets::{
    decoded_master_key_len, is_loopback_url, parse_secret_ref, platform_secret, read_secret_file,
    require_nonempty, resolve_secret, validate_master_key_material, validate_pairwise_salt,
    validate_secret_material, validate_secret_ref, validate_stash_key,
    PlatformSecret, SecretError, SecretRef, SecretStrength, MIN_DECODED_KEY_BYTES,
    MIN_SECRET_BYTES, PLATFORM_SECRETS,
};
pub use source::{
    load_overlay, log_overlay_source, resolve_overlay_string, ConfigSource, LoadedOverlay,
    SYSTEM_CONFIG_PATH,
};
pub use test_overlay::{
    database_url as test_database_url, database_url_opt as test_database_url_opt,
    PROVISION_COMMAND, TEST_OVERLAY_PATH,
};
pub use topology::{
    resolve_origin_scheme, resolve_trusted_origins, OriginScheme, PlaintextPeer, PlaintextPeers,
    TrustedOrigin,
};
