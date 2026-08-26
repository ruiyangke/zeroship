//! Inert declarations that exist only so the macro's output is compiled.
//!
//! These live in the LIBRARY, while the assertions live in an integration test
//! that links it. That split is deliberate: it proves the linked read-site
//! registry aggregates entries emitted in a different crate from the one doing
//! the enumeration, which is the property a five-service registry needs and the
//! reason the contract crate sits above `zeroship-core` rather than inside it.
//!
//! No process reads these. They name settings from the proposal's worked
//! examples so the projections under test are the real ones.

use std::path::PathBuf;

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, Operational, Secret,
};

/// A per-component declaration: both settings live under `[control]`.
///
/// The check-config control is not decoration: a declaration carrying a secret
/// is required to carry it, so the generated resolver can tell a dry run from a
/// real boot rather than reading the secret either way.
#[zeroship_config(binary = "zeroship-fixture-control", scope = "control")]
#[derive(Debug)]
pub struct FixtureControlConfig {
    /// Operational setting with a compiled default.
    #[config(name = "control.port", default = 9090)]
    pub port: Operational<u16>,
    /// Secret sitting in its component's table, not a `secrets` section.
    #[config(name = "control.database_url")]
    pub database_url: Secret<String>,
    /// The dry-run selector every secret-bearing declaration must carry.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,
}

/// A second binary, including a platform-global secret at the TOML top level.
#[zeroship_config(binary = "zeroship-fixture-worker", scope = "worker")]
#[derive(Debug)]
pub struct FixtureWorkerConfig {
    /// Scope-stripped operational flag.
    #[config(name = "worker.max_pinned_isolates_per_app", default = 4)]
    pub max_pinned_isolates_per_app: Operational<u32>,
    /// Platform-global secret: no component prefix, so no scope is stripped,
    /// and its identity comes from the shared table because several binaries
    /// read the one value.
    #[config(shared = CONTROL_KEY)]
    pub control_key: Secret<String>,
    /// The dry-run selector every secret-bearing declaration must carry.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,
}

/// The bootstrap/command controls every server binary carries.
///
/// Declared on a third fixture binary so the assertions can distinguish a class
/// with no TOML tier from one that merely happens to have no overlay entry.
#[zeroship_config(binary = "zeroship-fixture-gate", scope = "gateway")]
#[derive(Debug)]
pub struct FixtureControls {
    /// Overlay selector: read before any overlay exists.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,
    /// Discovery suppressor: also read before any overlay exists.
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,
    /// An action, not a setting.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,
    /// A valued command control, so the class is not only exercised as a flag.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,
    /// A safety control: enabling it must require an explicit act at launch,
    /// never a line someone left in a persisted overlay.
    #[config(name = "gateway.allow_unsigned_advance")]
    pub allow_unsigned_advance: BootstrapControl<bool>,
}
