//! Shared optional file-overlay configuration.
//!
//! Split into focused submodules:
//! - [`auth_kind`] - the one auth-provider vocabulary (`AuthProviderKind`).
//! - [`file`] — the TOML schema (`FileConfig`, sections, `ConfigError`).
//! - [`source`] — overlay discovery (`ConfigSource`, `LoadedOverlay`, resolve/load).
//! - [`env`] - the sole raw process-environment boundary, plus pure truthiness.
//! - [`declared`] - typed keys for reads the config contract does not generate.
//! - [`secrets`] - the part of the secret policy typed on this crate's
//!   `Secret`. The policy proper - the table, the strength rules, every
//!   validator, the sentinel and secret-reference resolution - is the
//!   [`zeroship_secret_policy`] LEAF crate, which callers import directly.
//!   This module does not re-export it.
//! - [`credential_gate`] - the boot refusal and its banner, the per-subsystem
//!   audit, and the build-profile dev escape.
//! - [`diagnostics`] - the shared env-name scanner every binary's
//!   "this refusal names something settable" test drives.
//! - [`bootstrap`] — the shared boot dance + structured `--check-config` emitter.
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

// The sentinel itself (`SERVICE_CREDENTIAL_SENTINEL`, `REMEDIATION_COMMAND`,
// `is_unset_credential`, `unset_credential_message`) moved to the
// `zeroship-secret-policy` leaf with the validators that consume it, and is
// imported from there by whoever needs it. What stays here is the AUDIT built
// on top of it, which is typed on this crate's `Secret` and `ConfigSource`.
pub use credential_gate::{
    audit_credentials, dev_escape_active, mark_dev_escape_active, BuildProfile, CredentialPosture,
    CredentialVerdict, SubsystemCredential, WeakCredential,
};

pub use diagnostics::env_like_tokens;

pub use bootstrap::{
    bootstrap, bootstrap_or_exit, Bootstrap, CheckConfigReport, CheckFormat, CheckValue,
    ObservabilityControls, OverlaySelector,
};
pub use env::{env_is_exact, env_is_truthy, parse_bool_flag};
pub use file::{
    AuthSection, ConfigError, ControlSection, FileConfig, GatewaySection, MigratedSection,
    OauthClientRegistration, ObsSection, SchedulerSection, WorkerSection,
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
pub use secrets::validate_secret_material;
// AND NOTHING ELSE FROM THE SECRET POLICY. `PLATFORM_SECRETS`, the strength
// rules, every validator, the sentinel and secret-reference resolution are the
// `zeroship-secret-policy` crate's public API, and callers name that crate.
// This module deliberately does NOT re-export them.
//
// It did for one commit, and the reason given was that the ~19 existing callers
// would not have to change - which is a back-compat argument wearing a facade's
// clothes. The cost was real: `crates/cli/src/dev.rs` reached the table through
// here while `crates/zeroship-gatekit` reached it through the leaf, so one
// constant had two public paths and which one a file used was arbitrary.
//
// The rest of this module IS a facade over sibling submodules, and that is a
// different thing: those are parts of this crate, not a second crate with a
// public name of its own.
pub use source::{
    load_overlay, log_overlay_source, resolve_overlay_string, ConfigSource, LoadedOverlay,
    SYSTEM_CONFIG_PATH,
};
pub use test_overlay::{
    database_url as test_database_url, database_url_opt as test_database_url_opt,
    kv_url as test_kv_url, kv_url_opt as test_kv_url_opt, PROVISION_COMMAND, TEST_OVERLAY_PATH,
};
pub use topology::{
    resolve_origin_scheme, resolve_trusted_origins, OriginScheme, TrustedOrigin,
};
