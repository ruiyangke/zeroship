//! `zeroship-migrate-js` — the FULL-build migration CLI carrying the JS schema
//! front-end (design §5.1). On top of the lean `zeroship-migrate` core it adds the
//! op.* DSL build/dev ergonomics that need V8 + the PR4a kernel-sandboxed recorder:
//!
//! - `new <name>` — scaffold a deterministic op.* `.ts` (deliverable C).
//! - `record <file.ts>` — record ONE `.ts` via the LOCAL sandboxed recorder → its
//!   sibling `.ir.json` (the local-record entry the vite-plugin shells; C-bis).
//! - `build <dir>` — discover `migrations/*.ts`, record each lacking a committed
//!   `.ir.json`, write the committed artifacts, print the bundle entries (A1).
//!   `--recorder-url` selects the hosted thin client with local fallback.
//! - `generate --schema <schema.js>` — autogenerate an op.* `.ts` + `.ir.json` from
//!   the declarative diff against the live DB (deliverable D).
//!
//! The LEAN `zeroship-migrate` binary (a SEPARATE crate) stays V8-free and is the
//! public dbmate-style apply/rollback/status tool. `main` is a THIN arg-parser that
//! delegates to the library. compio (NOT tokio): `#[compio::main]`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use clap::{Parser, Subcommand};
use zeroship_migrate_js::recorder_http::StructuredError;
use zeroship_migrate_js::{
    build_migrations, build_one_migration, generate_ops, scaffold_new_ts, timestamp_14, RecordVia,
    RecorderClient,
    ResourceBudget,
};

/// JS-schema-aware migration CLI for Postgres (the full build).
#[derive(Debug, Parser)]
#[command(name = "zeroship-migrate-js", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scaffold a new deterministic op.* migration `.ts` into the migrations dir.
    /// Does NOT record the `.ir.json` (the build/deploy step records).
    New {
        /// The migration name (`[A-Za-z0-9_]+`). Rejected + suggested otherwise;
        /// never auto-renamed.
        name: String,
        /// The migrations directory. Default `./migrations`.
        #[arg(long, default_value = "./migrations")]
        dir: PathBuf,
    },
    /// Record ONE `.ts` via the LOCAL sandboxed recorder and write its sibling
    /// `.ir.json` (the local-record entry the vite-plugin shells).
    Record {
        /// The migration `.ts` to record.
        file: PathBuf,
        /// The declaring/deploying app (`app_…`), stamped on the IR.
        #[arg(long, default_value = "app_local")]
        owner_app: String,
    },
    /// Build a migrations dir: discover `*.ts`, record each lacking a committed
    /// `.ir.json`, write the committed artifacts, and print the bundle entries.
    Build {
        /// The migrations directory. Default `./migrations`.
        #[arg(long, default_value = "./migrations")]
        dir: PathBuf,
        /// The declaring/deploying app (`app_…`), stamped on the IR.
        #[arg(long, default_value = "app_local")]
        owner_app: String,
        /// The hosted recorder URL (the §8.9.2 thin client). When set, the build
        /// ships each `.ts` to the recorder; recorder-unreachable falls back to the
        /// LOCAL recorder (NOT a build failure). When unset, the LOCAL recorder is
        /// used directly.
        #[arg(long)]
        recorder_url: Option<String>,
        /// The bearer token (PAT) for the hosted recorder.
        #[arg(long, env = "ZEROSHIP_TOKEN")]
        token: Option<String>,
    },
    /// Generate a versioned op.* migration by diffing a `schema.js` (the
    /// `@zeroship/db` `t.*` DSL) against the live database (deliverable D).
    Generate {
        /// Path to the (bundled, self-contained) `schema.js`.
        #[arg(long)]
        schema: PathBuf,
        /// Postgres DSN. Falls back to the `DATABASE_URL` env var.
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
        /// The project schema introspected for the live state. Default `public`.
        #[arg(long, default_value = "public")]
        project_schema: String,
        /// The declaring/deploying app (`app_…`).
        #[arg(long, default_value = "app_local")]
        owner_app: String,
        /// Human-readable migration name (the file suffix).
        #[arg(long, default_value = "schema_change")]
        name: String,
        /// Output migration directory. Default `./migrations`.
        #[arg(long, default_value = "./migrations")]
        dir: PathBuf,
    },
}

/// The hosted record path for `build --recorder-url` (the §8.9.2 thin-client seam).
///
/// **The dev CLI does NOT embed a hosted-recorder HTTP client.** Hosted, canonical
/// kernel-sandboxed recording happens on the platform via the CONTROL PLANE at
/// deploy (the §5.1 provenance re-record), NOT from this dev tool. So `record`
/// here unconditionally returns a RETRYABLE `RECORDER_UNREACHABLE`, and `build
/// --recorder-url` therefore ALWAYS falls back to the LOCAL recorder
/// (`RecordPath::HostedFellBackToLocal`). `cmd_build` makes that downgrade LOUD
/// (a warning per file) rather than silently substituting local for the requested
/// hosted path. Security is preserved either way — the deploy-time provenance gate
/// re-records under the platform sandbox.
struct HttpRecorderClient {
    #[allow(dead_code)]
    url: String,
    #[allow(dead_code)]
    token: String,
}

impl RecorderClient for HttpRecorderClient {
    fn record(
        &self,
        _ts_source: &str,
        _app_id: &str,
        _name: &str,
        _schema_types_blob: Option<&str>,
    ) -> Result<String, StructuredError> {
        // The CLI does not embed an HTTP client (zero-tokio; the platform's hosted
        // recorder is reached by the control plane, not this dev CLI). Surface a
        // RETRYABLE recorder-unreachable so `build` falls back to the LOCAL recorder
        // — the documented §8.9.2 behavior when the hosted recorder is unavailable.
        Err(StructuredError {
            code: "RECORDER_UNREACHABLE".into(),
            message: "the dev CLI does not embed a hosted-recorder HTTP client; \
                      use the LOCAL recorder (omit --recorder-url) or the control plane"
                .into(),
            http_status: 503,
            retryable: true,
        })
    }
}

#[compio::main]
async fn main() -> ExitCode {
    zeroship_core::observability::init_tracing("warn");

    let cli = Cli::parse();
    match cli.command {
        Command::New { name, dir } => cmd_new(&name, &dir),
        Command::Record { file, owner_app } => cmd_record(&file, &owner_app),
        Command::Build {
            dir,
            owner_app,
            recorder_url,
            token,
        } => cmd_build(&dir, &owner_app, recorder_url.as_deref(), token.as_deref()),
        Command::Generate {
            schema,
            database_url,
            project_schema,
            owner_app,
            name,
            dir,
        } => {
            cmd_generate(
                &schema,
                &database_url,
                &project_schema,
                &owner_app,
                &name,
                &dir,
            )
            .await
        }
    }
}

fn cmd_new(name: &str, dir: &Path) -> ExitCode {
    let ts = match scaffold_new_ts(name) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("new: {e}");
            return ExitCode::FAILURE;
        }
    };
    let filename = format!("{}_{}.ts", timestamp_14(SystemTime::now()), name);
    let path = dir.join(&filename);
    if path.exists() {
        eprintln!("new: {} already exists (refusing to clobber)", path.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("new: cannot create {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(&path, ts.as_bytes()) {
        eprintln!("new: cannot write {}: {e}", path.display());
        return ExitCode::FAILURE;
    }
    println!("new: wrote {}", path.display());
    ExitCode::SUCCESS
}

fn cmd_record(file: &Path, owner_app: &str) -> ExitCode {
    let via = RecordVia::Local {
        budget: ResourceBudget::default(),
    };
    // Record ONLY the requested file (a single-discovered-migration build), NOT the
    // whole dir — `record half_finished.ts` must never inadvertently record an
    // unrelated in-progress sibling. build_one_migration is still build-once: a file
    // with a committed .ir.json is read verbatim, so re-running record is idempotent.
    match build_one_migration(file, owner_app, &via) {
        Ok(outcome) => match outcome.migrations.first() {
            Some(m) => {
                for w in &m.warnings {
                    eprintln!(
                        "record: determinism warning [{}]: {} — {}",
                        w.code, w.accessor, w.suggested_fix
                    );
                }
                println!(
                    "record: wrote {} (checksum {})",
                    file.with_file_name(&m.filename).display(),
                    m.checksum
                );
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("record: {} produced no migration", file.display());
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("record: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_build(
    dir: &Path,
    owner_app: &str,
    recorder_url: Option<&str>,
    token: Option<&str>,
) -> ExitCode {
    let client;
    let via = match recorder_url {
        Some(url) => {
            client = HttpRecorderClient {
                url: url.to_string(),
                token: token.unwrap_or_default().to_string(),
            };
            RecordVia::Hosted {
                client: &client,
                local_fallback_budget: ResourceBudget::default(),
            }
        }
        None => RecordVia::Local {
            budget: ResourceBudget::default(),
        },
    };
    match build_migrations(dir, owner_app, &via) {
        Ok(outcome) => {
            // LOW #1 honesty: a `--recorder-url` was requested but the dev CLI has no
            // embedded hosted client, so the build fell back to LOCAL recording. Make
            // that downgrade LOUD (per file) rather than silently substituting local
            // for the requested hosted path.
            if recorder_url.is_some() {
                for m in &outcome.migrations {
                    if m.record_path
                        == zeroship_migrate_js::RecordPath::HostedFellBackToLocal
                    {
                        eprintln!(
                            "build: WARNING: --recorder-url was set but the dev CLI does not \
                             embed a hosted-recorder client; {} was recorded LOCALLY (fell back). \
                             Hosted, canonical recording happens via the control plane at deploy.",
                            m.stem
                        );
                    }
                }
            }
            for m in &outcome.migrations {
                for w in &m.warnings {
                    eprintln!(
                        "build: determinism warning in {} [{}]: {} — {}",
                        m.stem, w.code, w.accessor, w.suggested_fix
                    );
                }
                println!(
                    "build: {} -> {} (sha256 {}, checksum {}, via {:?})",
                    m.stem, m.filename, m.entry.hash, m.checksum, m.record_path
                );
            }
            println!("build: {} migration(s)", outcome.migrations.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("build: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_generate(
    schema: &Path,
    database_url: &str,
    project_schema: &str,
    owner_app: &str,
    name: &str,
    dir: &Path,
) -> ExitCode {
    let source = match std::fs::read_to_string(schema) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("generate: cannot read {}: {e}", schema.display());
            return ExitCode::FAILURE;
        }
    };

    // Eval schema.js → descriptor IR → desired snapshot; introspect the live DB.
    let descriptors = match zeroship_migrate_js::eval_schema_to_ir(&source, owner_app) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("generate: schema eval failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let desired =
        match zeroship_migrate::declarative::desired_snapshot(project_schema, &descriptors) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("generate: desired snapshot failed: {e}");
                return ExitCode::FAILURE;
            }
        };
    let client = match zeroship_migrate::db::connect(database_url).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("generate: db connect failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let live = match zeroship_migrate::drift::snapshot_schema(&client, project_schema).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("generate: live introspection failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let gen = match generate_ops(name, owner_app, &desired, &live) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("generate: {e}");
            return ExitCode::FAILURE;
        }
    };
    if gen.is_empty {
        println!("generate: no-op (schema already matches the live database)");
        return ExitCode::SUCCESS;
    }

    let stem = format!("{}_{}", timestamp_14(SystemTime::now()), name);
    let ts_path = dir.join(format!("{stem}.ts"));
    let ir_path = dir.join(format!("{stem}.ir.json"));
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("generate: cannot create {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    // The committed `.ir.json` is the source of truth (pretty + trailing newline,
    // the canonical byte convention).
    let mut ir_bytes = match serde_json::to_string_pretty(&gen.ir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("generate: serialize IR failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    ir_bytes.push('\n');
    if let Err(e) = std::fs::write(&ts_path, gen.ts_body.as_bytes()) {
        eprintln!("generate: cannot write {}: {e}", ts_path.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(&ir_path, ir_bytes.as_bytes()) {
        eprintln!("generate: cannot write {}: {e}", ir_path.display());
        return ExitCode::FAILURE;
    }
    for todo in &gen.todos {
        println!("generate: open obligation: {todo}");
    }
    println!(
        "generate: wrote {} + {} ({} op(s))",
        ts_path.display(),
        ir_path.display(),
        gen.ir.ops.len()
    );
    ExitCode::SUCCESS
}
