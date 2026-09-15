//! The linked configuration contract for platform binaries.
//!
//! Each library contributes the specs and read sites produced by its generated
//! declaration. Registry tests compare the linked target names against Cargo's
//! platform target metadata, so adding a binary without linking its settings
//! fails verification. Startup readiness does not exempt a declared target.
//!
//! This non-shipped checker is the allowed consumer of the CDC relay library;
//! production binaries must not link the relay. The Node migration CLI uses
//! its own private TOML configuration and does not declare Rust settings here.

use zeroship_core::config::{ConfigSpec, GeneratedConfig, ReadSite, CONFIG_READ_SITES};

/// Every binary whose generated registry is linked into this tool.
///
/// Each platform target is compared against Cargo metadata by the registry tests.
pub const DECLARING_BINARIES: &[&str] = &[
    "zeroship-auth",
    "zeroship-control",
    "zeroship-data-cdc-server",
    "zeroship-gate",
    "zeroship-migrate-server",
    "zeroship-worker",
    "zeroship-workflow-server",
];

/// Every declaration the platform binaries compile.
#[must_use]
pub fn platform_specs() -> Vec<ConfigSpec> {
    let mut specs = Vec::new();
    specs.extend_from_slice(zeroship_auth::config::AuthSettings::SPECS);
    specs.extend_from_slice(zeroship_control::config::ControlSettings::SPECS);
    specs.extend_from_slice(zeroship_data_cdc_server::config::CdcServerSettings::SPECS);
    specs.extend_from_slice(zeroship_gateway::config::GateSettings::SPECS);
    specs.extend_from_slice(zeroship_migrate_server::config::MigrateServerSettings::SPECS);
    specs.extend_from_slice(zeroship_worker::config::WorkerSettings::SPECS);
    specs.extend_from_slice(zeroship_workflow_server::config::WorkflowSettings::SPECS);
    specs
}

/// Every linked read site belonging to a platform binary.
///
/// Filtered rather than taken whole: this tool also links the fixture
/// registries in [`crate::fixtures`], whose sites are real entries in the same
/// slice and would otherwise be documented as production configuration.
#[must_use]
pub fn platform_read_sites() -> Vec<ReadSite> {
    CONFIG_READ_SITES
        .iter()
        .copied()
        .filter(|site| DECLARING_BINARIES.contains(&site.consumer().target()))
        .collect()
}

/// The binaries that actually appear in the linked declarations.
#[must_use]
pub fn declared_binaries(specs: &[ConfigSpec]) -> Vec<String> {
    let mut targets = specs
        .iter()
        .flat_map(|spec| spec.consumers().iter().map(|c| c.target().to_owned()))
        .collect::<Vec<_>>();
    targets.sort();
    targets.dedup();
    targets
}
