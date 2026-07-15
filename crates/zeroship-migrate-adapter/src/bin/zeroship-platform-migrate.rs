//! `zeroship-platform-migrate` — Phase F Stage 4a.
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
//!        │  zeroship-runtime V8 isolate + STANDALONE v1 recorder (S2 mechanism)
//!        ▼
//!   { ir_version:1, name, ops }  envelope (authored in V8 — NOT hand-built)
//!        │  published zero-migrate: fail-closed load gate + guarded lower
//!        ▼  (Platform guard: cross-schema public/citext + roles/grants/functions)
//!   LoweredArtifact (plan steps + touched/created tables)
//!        │  MigrationEngine::apply_plan_with_touched_and_depends_scoped
//!        ▼  PostgresBackend::new_generic(&CompioPgSession)  — compio io_uring
//!   real platform DDL + append-only journal (zeroship_migrations)
//! ```
//!
//! Usage:
//!   zeroship-platform-migrate --database-url postgres://…:5440/db \
//!       [--migrations-dir db/migrations-ts] [--project-schema zeroship] \
//!       [--project-id zeroship]

use std::path::{Path, PathBuf};

use zeroship_migrate_adapter::platform::{run_platform_migrations, PlatformMigrateConfig};

fn main() {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("zeroship-platform-migrate: {msg}");
            eprintln!(
                "\nusage: zeroship-platform-migrate --database-url <DSN> \
                 [--migrations-dir <dir>] [--project-schema <schema>] [--project-id <id>]"
            );
            std::process::exit(2);
        }
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

fn parse_args() -> Result<PlatformMigrateConfig, String> {
    let mut database_url: Option<String> = None;
    let mut migrations_dir: Option<PathBuf> = None;
    let mut project_schema = String::from("zeroship");
    let mut project_id = String::from("zeroship");

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--database-url" => {
                database_url =
                    Some(args.next().ok_or("--database-url needs a value")?);
            }
            "--migrations-dir" => {
                migrations_dir =
                    Some(PathBuf::from(args.next().ok_or("--migrations-dir needs a value")?));
            }
            "--project-schema" => {
                project_schema = args.next().ok_or("--project-schema needs a value")?;
            }
            "--project-id" => {
                project_id = args.next().ok_or("--project-id needs a value")?;
            }
            "-h" | "--help" => return Err("help".to_string()),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }

    let database_url = database_url
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .ok_or("a --database-url (or DATABASE_URL) is required")?;

    let migrations_dir = migrations_dir.unwrap_or_else(default_migrations_dir);

    Ok(PlatformMigrateConfig {
        database_url,
        migrations_dir,
        project_schema,
        project_id,
    })
}

/// The default `db/migrations-ts` directory, resolved relative to the repo root
/// (the crate is two levels below it: `crates/zeroship-migrate-adapter`).
fn default_migrations_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .map(|root| root.join("db").join("migrations-ts"))
        .unwrap_or_else(|| PathBuf::from("db/migrations-ts"))
}
