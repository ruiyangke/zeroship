//! The linked contract: every declaring binary's compiled registry, in one place.
//!
//! This module is the reason `zeroship-config-contract` depends on the seven
//! server libraries. `SPECS` is a `const` the proc macro emitted and rustc
//! placed in the declaring crate; `CONFIG_READ_SITES` is a linkme slice each
//! generated resolver contributes to. Both exist only if the code that declares
//! them is LINKED, which is what makes a check against them impossible to
//! satisfy by editing a list.
//!
//! A binary missing from [`platform_specs`] is invisible here, so
//! [`DECLARING_BINARIES`] states the expected set, [`declared_binaries`]
//! recovers the set the LINKED declarations actually name, and
//! `tests/real_registry.rs` compares both against the `platform` class that
//! `check-metadata` already enforces from the package manifests. That
//! comparison is the anti-vacuity guard: adding a platform binary and
//! forgetting this file fails there rather than silently shrinking every count
//! in this tool.
//!
//! Cargo metadata classifies SEVEN targets `platform`, and all seven declare, so
//! `real_registry.rs` compares the two sets for exact equality with no named
//! exception.
//!
//! IT WAS SIX UNTIL 2026-09-03. The seventh is `zeroship-data-cdc-server`, the
//! CDC relay: 6 + 1 = 7. It is the process that will hold `REPLICATION` so the
//! worker does not, and it is registered here for the reason this module states
//! above rather than as a courtesy - a `platform` target absent from
//! [`platform_specs`] has an unaudited configuration surface, and every count in
//! this tool simply gets smaller. Its `main` currently refuses to start, exactly
//! as `zeroship-workflow-scheduler`'s does; that is a property of the process,
//! not of its operator-visible configuration.
//!
//! IT IS ALSO THE ONE CRATE IN THIS WORKSPACE THAT NOTHING ELSE MAY LINK. This
//! tool is the single named exception, and it qualifies because it is
//! `class = "test-dev-tool"` and is never shipped.
//! `tests/data_crate_closure_gate.sh` arm 3 asserts that inversion directly.
//!
//! IT WAS SEVEN UNTIL 2026-08-28. The seventh was `zeroship-platform-migrate`, the
//! platform-schema migrate one-shot, whose declaration lived in
//! `zeroship-migrate-adapter`'s library so this tool could link its configuration
//! without linking V8. The binary is deleted - the platform schema is migrated by
//! the general `zero-migrate` CLI now - so it is absent from BOTH sides of that
//! comparison rather than exempted from one. The count shrinking is therefore not
//! the silent-shrink failure this module warns about above: cargo metadata no
//! longer classifies the target either, because the `[[bin]]` and its
//! `[[package.metadata.zeroship-config.targets]]` block went with it.
//!
//! The CLI is a Node program and declares nothing here. What it reads is a 0600
//! TOML config file its callers write per run (tests/lib/runtime_secrets.sh,
//! deploy/ops/db-migrate.sh); that is outside the `#[zeroship_config]` contract by
//! construction, and is named here so the gap is recorded rather than assumed.

use zeroship_core::config::{ConfigSpec, GeneratedConfig, ReadSite, CONFIG_READ_SITES};

/// Every binary whose generated registry is linked into this tool.
///
/// SEVEN since 2026-09-03: the six servers plus `zeroship-data-cdc-server`
/// (6 + 1 = 7). See the module header for why the relay is registered here even
/// though nothing else in the workspace may link it.
pub const DECLARING_BINARIES: [&str; 7] = [
    "zeroship-auth",
    "zeroship-control",
    "zeroship-data-cdc-server",
    "zeroship-gate",
    "zeroship-migrate-server",
    "zeroship-worker",
    "zeroship-workflow-scheduler",
];

/// Every declaration the seven platform binaries compile.
#[must_use]
pub fn platform_specs() -> Vec<ConfigSpec> {
    let mut specs = Vec::new();
    specs.extend_from_slice(zeroship_auth::config::AuthSettings::SPECS);
    specs.extend_from_slice(zeroship_control::config::ControlSettings::SPECS);
    specs.extend_from_slice(zeroship_data_cdc_server::config::CdcServerSettings::SPECS);
    specs.extend_from_slice(zeroship_gateway::config::GateSettings::SPECS);
    specs.extend_from_slice(zeroship_migrate_server::config::MigrateServerSettings::SPECS);
    specs.extend_from_slice(zeroship_worker::config::WorkerSettings::SPECS);
    specs.extend_from_slice(zeroship_workflow_scheduler::config::SchedulerSettings::SPECS);
    specs
}

/// Every linked read site belonging to one of the seven platform binaries.
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
