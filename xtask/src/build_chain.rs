//! The ordered package chain `build_host` runs.
//!
//! `build_host` exists so the migration host builds from a checkout whose
//! `dist` directories are absent: `@zeroship/migrate` bundles
//! `@zeroship/schema`, so esbuild cannot resolve the import until the schema
//! bundle exists. Rewriting that bundle also makes every LATER consumer of it
//! stale, though, and the build-input freshness gate pairs each artifact with
//! its newest source (`xtask/tests/repository/build_inputs.rs`). A chain that
//! stopped at the migration host would leave `@zeroship/db` and the V8 adapter
//! older than the schema bundle it just rewrote, so the chain runs through
//! those consumers too. Nothing declares the migration addon or the migration
//! CLI as a source, so they end the chain.

/// One build step: the directory to run in, the package it names (asserted
/// against that directory's `package.json`), the package-free script to run,
/// and the generated artifacts that script writes.
pub struct BuildStep {
    pub directory: &'static str,
    pub package: &'static str,
    pub script: &'static str,
    pub artifacts: &'static [&'static str],
}

/// The whole chain, in dependency order.
pub const BUILD_CHAIN: [BuildStep; 6] = [
    BuildStep {
        directory: "crates/zeroship-migrate-node",
        package: "zeroship-migrate-node",
        script: "build",
        // The addon is named after the host triple, so the gate discovers the
        // files rather than declaring them; no build-input rule consumes one.
        artifacts: &[],
    },
    BuildStep {
        directory: "packages/schema",
        package: "@zeroship/schema",
        script: "build",
        artifacts: &["packages/schema/dist/index.js"],
    },
    BuildStep {
        directory: "packages/zero-migrate",
        package: "@zeroship/migrate",
        script: "build",
        artifacts: &[
            "packages/zero-migrate/dist/index.js",
            "packages/zero-migrate/dist/internal/recorder.js",
            "packages/zero-migrate/dist/embedded-recorder.js",
        ],
    },
    BuildStep {
        directory: "packages/zero-migrate-cli",
        package: "zero-migrate-cli",
        script: "build",
        artifacts: &["packages/zero-migrate-cli/dist/index.js"],
    },
    BuildStep {
        directory: "packages/db",
        package: "@zeroship/db",
        script: "build",
        artifacts: &["packages/db/dist/index.js"],
    },
    BuildStep {
        directory: "packages/db",
        package: "@zeroship/db",
        script: "build:adapter",
        artifacts: &["crates/zeroship-data-v8/dist/adapter.js"],
    },
];
