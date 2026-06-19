//! `zeroship-migrate-js` — the FULL-build migration CLI carrying the JS schema
//! front-end (design §5.1). It adds `generate --schema <schema.js>` on top of
//! the lean `zeroship-migrate` core: eval the creator schema in V8 → descriptor
//! IR → diff the live DB → emit a versioned dbmate migration. One self-contained
//! tool (no separate Node/vite step).
//!
//! The LEAN `zeroship-migrate` binary (a SEPARATE crate) stays V8-free and is
//! the public dbmate-style apply/rollback/status tool. This binary is the
//! opt-in platform/full build.
//!
//! `main` is a THIN arg-parser that delegates to the library
//! (`zeroship_migrate_js::generate_migration`). compio (NOT tokio):
//! `#[compio::main]` drives the same compio-native PG introspection the engine
//! uses.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// JS-schema-aware migration CLI for Postgres (the full build).
#[derive(Debug, Parser)]
#[command(name = "zeroship-migrate-js", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a versioned migration by diffing a `schema.js` (the
    /// `@zeroship/db` `t.*` DSL) against the live database.
    Generate {
        /// Path to the (bundled, self-contained) `schema.js`. A raw `.ts`
        /// importing npm packages is NOT a valid input — transpile/bundle it
        /// first (the JS build pipeline owns TS→JS + npm resolution).
        #[arg(long)]
        schema: PathBuf,

        /// Postgres DSN. Falls back to the `DATABASE_URL` env var.
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,

        /// The project schema the migration is qualified into and introspected
        /// for the live state (the app's schema). Default `public`.
        #[arg(long, default_value = "public")]
        project_schema: String,

        /// The declaring/deploying app (`app_…`), stamped on the IR + the
        /// ownership-enforcement subject.
        #[arg(long, default_value = "app_local")]
        owner_app: String,

        /// Human-readable migration name (the file suffix).
        #[arg(long, default_value = "schema_change")]
        name: String,

        /// Output migration directory. Default `./db/migrations`.
        #[arg(long, default_value = "./db/migrations")]
        dir: PathBuf,
    },
}

#[compio::main]
async fn main() -> ExitCode {
    zeroship_core::observability::init_tracing("warn");

    let cli = Cli::parse();
    match cli.command {
        Command::Generate {
            schema,
            database_url,
            project_schema,
            owner_app,
            name,
            dir,
        } => {
            let source = match std::fs::read_to_string(&schema) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("zeroship-migrate-js: cannot read {}: {e}", schema.display());
                    return ExitCode::FAILURE;
                }
            };
            match zeroship_migrate_js::generate_migration(
                &source,
                &database_url,
                &project_schema,
                &owner_app,
                &name,
                &dir,
            )
            .await
            {
                Ok(outcome) => match outcome.written {
                    Some(path) => {
                        println!(
                            "generate: wrote {} ({} statement-group(s))",
                            path.display(),
                            outcome.migration_count
                        );
                        ExitCode::SUCCESS
                    }
                    None => {
                        println!("generate: no-op (schema already matches the live database)");
                        ExitCode::SUCCESS
                    }
                },
                Err(e) => {
                    eprintln!("zeroship-migrate-js generate: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
