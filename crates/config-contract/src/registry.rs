//! The linked contract: every declaring binary's compiled registry, in one place.
//!
//! This module is the reason `zeroship-config-contract` depends on the six
//! server libraries. `SPECS` is a `const` the proc macro emitted and rustc
//! placed in the declaring crate; `CONFIG_READ_SITES` is a linkme slice each
//! generated resolver contributes to. Both exist only if the code that declares
//! them is LINKED, which is what makes a check against them impossible to
//! satisfy by editing a list.
//!
//! A binary missing from [`platform_specs`] is invisible here, so
//! [`declared_binaries`] states the expected set and
//! `tests/real_registry.rs` compares it with the cargo-metadata classification
//! that `check-metadata` already enforces. That comparison is the anti-vacuity
//! guard: adding a seventh server and forgetting this file fails there rather
//! than silently shrinking every count in this tool.

use zeroship_core::config::{ConfigSpec, GeneratedConfig, ReadSite, CONFIG_READ_SITES};

/// Every binary whose generated registry is linked into this tool.
pub const DECLARING_BINARIES: [&str; 6] = [
    "zeroship-auth",
    "zeroship-control",
    "zeroship-gate",
    "zeroship-migrated",
    "zeroship-worker",
    "zeroship-workflow-scheduler",
];

/// Every declaration the six platform binaries compile.
#[must_use]
pub fn platform_specs() -> Vec<ConfigSpec> {
    let mut specs = Vec::new();
    specs.extend_from_slice(zeroship_auth::config::AuthSettings::SPECS);
    specs.extend_from_slice(zeroship_control::config::ControlSettings::SPECS);
    specs.extend_from_slice(zeroship_gateway::config::GateSettings::SPECS);
    specs.extend_from_slice(zeroship_migrated::config::MigratedSettings::SPECS);
    specs.extend_from_slice(zeroship_worker::config::WorkerSettings::SPECS);
    specs.extend_from_slice(zeroship_workflow_scheduler::config::SchedulerSettings::SPECS);
    specs
}

/// Every linked read site belonging to one of the six platform binaries.
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
