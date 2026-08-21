//! `zeroship-platform-migrate` - Phase F Stage 4a.
//!
//! The platform-schema migrate driver, rebuilt on the PUBLISHED `zero-migrate`
//! engine. This REPLACES the retired in-tree `zeroship-migrate` CLI (the
//! docker-compose `migrate` one-shot service) as the producer of the platform
//! schema.
//!
//! Pipeline (per `db/migrations-ts/*.ts`, in filename order):
//!
//! ```text
//!   db/migrations-ts/NNNN_*.ts
//!        |  zeroship-runtime V8 isolate + STANDALONE v1 recorder (S2 mechanism)
//!        v
//!   { ir_version:1, name, ops }  envelope (authored in V8 - NOT hand-built)
//!        |  published zero-migrate: fail-closed load gate + guarded lower
//!        v  (Platform guard: cross-schema public/citext + roles/grants/functions)
//!   LoweredArtifact (plan steps + touched/created tables)
//!        |  MigrationEngine::apply_plan_with_touched_and_depends_scoped
//!        v  PostgresBackend::new_generic(&CompioPgSession)  - compio io_uring
//!   real platform DDL + append-only journal (zeroship_migrations)
//! ```
//!
//! Usage:
//!   zeroship-platform-migrate --database-url-file /path/to/dsn \
//!       [--migrations-dir db/migrations-ts] [--project-schema zeroship] \
//!       [--project-id zeroship] [--cluster-lock-database postgres]
//!
//! THE PARSER IS GENERATED, NOT HAND-ROLLED, AND THAT IS THE POINT.
//! `zeroship_migrate_adapter::config::PlatformMigrateSettings` declares this
//! binary through the same `#[zeroship_config]` attribute the six servers use,
//! so this one-shot now appears in the linked configuration contract
//! (`crates/config-contract/src/registry.rs`) like every other platform target,
//! and `main` is the shared `bootstrap_or_exit` dance rather than a private
//! argument loop.
//!
//! IN A DEPLOY FILE THE DSN ARRIVES AS A PATH, NEVER AS A VALUE. A process
//! argument list is public: `docker inspect`, `docker ps --no-trunc`, `ps` and
//! /proc/<pid>/cmdline all publish it to anything sharing the PID namespace,
//! and it survives in shell history and CI logs afterwards. A DSN admits
//! userinfo, so it is credential material by grammar - the same reason every
//! converted binary declares its DSN `Secret<String>` (crates/migrated/src/
//! config.rs) and gets `--<name>-file PATH` and nothing else from the macro.
//!
//! THE `--database-url` VALUE FLAG IS GONE. It survived the 2026-08-16 move
//! only because 29 in-repo files still spelled it; they now build a per-run 0600
//! DSN file through `zs_platform_migrate` (`tests/lib/runtime_secrets.sh`) and
//! pass the path. Nothing re-adds the value flag by editing this file: a
//! `Secret<String>` field has exactly one clap carrier in the generator, the
//! `-file` one, so the old spelling is unreachable from the declaration.
//!
//! The DSN reader ENFORCES OWNER-ONLY PERMISSIONS, and it refuses rather than
//! warns. `read_secret_file` (crates/core/src/config/secrets.rs) calls
//! `enforce_owner_only`, which rejects any file with a bit set in 0o077.
//! MEASURED 2026-08-19 against the deployed image, one variable between the
//! arms: a 0644 DSN file gives
//!   secret file '...' has mode 100644; group and other permissions
//!   must be zero (chmod 600 '...')
//! while the same file at 0600 parses and the run proceeds to connect.

use clap::Parser;
use zeroship_core::config::{bootstrap_or_exit, CheckConfigReport, CheckValue};
use zeroship_migrate_adapter::config::{
    PlatformMigrateSettings, PlatformMigrateSettingsSources, DEFAULT_LOG_FILTER,
};
use zeroship_migrate_adapter::platform::{run_platform_migrations, PlatformMigrateConfig};

fn main() {
    let (settings, boot) = bootstrap_or_exit::<PlatformMigrateSettings>(
        PlatformMigrateSettingsSources::parse(),
        DEFAULT_LOG_FILTER,
        "zeroship-platform-migrate",
    );

    // A read-only dry run: report what was configured and exit BEFORE the DSN
    // file is opened and before anything is dialled. `resolve_secret_sources`
    // has already refused to dereference the secret in this mode, so the row
    // below reports presence and cannot report material.
    if *settings.check_config.get() {
        let mut report = CheckConfigReport::new();
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(boot.log_format.to_string()));
        report.field(
            "migrations_dir",
            CheckValue::Plain(settings.migrations_dir.get().display().to_string()),
        );
        report.field(
            "project_schema",
            CheckValue::Plain(settings.project_schema.get().clone()),
        );
        report.field(
            "project_id",
            CheckValue::Plain(settings.project_id.get().clone()),
        );
        report.field(
            "cluster_lock_database",
            CheckValue::Plain(settings.cluster_lock_database.get().clone()),
        );
        report.field(
            "database_url_configured",
            CheckValue::Secret(settings.database_url.is_configured()),
        );
        report.emit(*settings.check_config_format.get());
        return;
    }

    // "Required" is a property of THIS consumer, not of the class: an absent
    // secret resolves to `Secret::absent()` rather than to an error, so the
    // refusal that names the flag has to be written here. It is the arm the old
    // parser's `(None, None)` case occupied.
    let Some(database_url) = settings.database_url.expose_secret().cloned() else {
        eprintln!(
            "zeroship-platform-migrate: a DSN is required; pass \
             --database-url-file <PATH> (a file readable only by its owner)"
        );
        std::process::exit(2);
    };

    let cfg = PlatformMigrateConfig {
        database_url,
        migrations_dir: settings.migrations_dir.get().clone(),
        project_schema: settings.project_schema.get().clone(),
        project_id: settings.project_id.get().clone(),
        cluster_lock_database: settings.cluster_lock_database.get().clone(),
    };

    let result = compio::runtime::Runtime::new()
        .expect("build compio runtime")
        .block_on(run_platform_migrations(&cfg));

    match result {
        Ok(report) => {
            println!(
                "zeroship-platform-migrate: applied {} migration(s) across {} file(s) \
                 into schema '{}' (journal: {}_migrations)",
                report.applied.len(),
                report.files,
                cfg.project_schema,
                cfg.project_schema,
            );
            for name in &report.applied {
                println!("  applied  {name}");
            }
            for name in &report.skipped {
                println!("  skipped  {name} (already applied)");
            }
        }
        Err(e) => {
            eprintln!("zeroship-platform-migrate: FAILED: {e}");
            std::process::exit(1);
        }
    }
}
