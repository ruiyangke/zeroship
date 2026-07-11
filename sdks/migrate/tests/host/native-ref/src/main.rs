//! Native-pg reference apply for the Phase-D differential oracle.
//!
//! Usage: `migrate-native-ref <DSN> <PROJECT_SCHEMA> <OWNER_APP> <MIGRATIONS_DIR>`
//!
//! Applies the `.ir.json` artifacts in `MIGRATIONS_DIR` on the given Postgres via
//! the GENUINE `native-pg` (compio-postgres) path (`apply_standalone`), into
//! `PROJECT_SCHEMA` with the meta journal at `<PROJECT_SCHEMA>_migrations`
//! (matching `ExecutorConfig::new`). Prints the `ApplyOutcome` (applied versions)
//! as JSON on stdout so the oracle harness can confirm it ran; the harness then
//! reads the journal directly and compares it to the host arm.
//!
//! This is the REAL native compio apply — the same `executor::apply` the addon
//! drives over the host seam, but here over compio-postgres — so a journal match
//! proves host == native-pg (Oracle 1), not host == self.

use std::path::Path;
use std::process::ExitCode;

use zeroship_migrate::{
    apply_standalone, connect, Approval, ExecutorConfig, GuardConfig, PostgresBackend,
};
use zeroship_migrate::model::profile::PolicyProfile;

#[compio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: migrate-native-ref <DSN> <PROJECT_SCHEMA> <OWNER_APP> <MIGRATIONS_DIR>");
        return ExitCode::from(2);
    }
    let dsn = &args[1];
    let project_schema = &args[2];
    let owner_app = &args[3];
    let dir = Path::new(&args[4]);

    let client = match connect(dsn).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("native-ref: connect failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Ensure the project schema exists (the oracle pre-creates it, but be robust).
    if let Err(e) = client
        .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{project_schema}\""))
        .await
    {
        eprintln!("native-ref: create schema failed: {e}");
        return ExitCode::FAILURE;
    }

    let backend = PostgresBackend::new(&client);
    // Meta journal at `<project_schema>_migrations` — the ExecutorConfig::new default,
    // matching the host `applyIr` arm.
    let exec_cfg = ExecutorConfig::new(project_schema, project_schema);
    let guard_cfg = GuardConfig::confined(project_schema.clone());
    let profile = PolicyProfile::confined();

    match apply_standalone(
        &backend,
        project_schema,
        owner_app,
        dir,
        &exec_cfg,
        &guard_cfg,
        &profile,
        Approval::None,
        "deploy",
    )
    .await
    {
        Ok(outcome) => {
            let json = serde_json::json!({
                "applied": outcome.applied,
                "skipped": outcome.skipped,
            });
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("native-ref: apply failed: {e}");
            ExitCode::FAILURE
        }
    }
}
