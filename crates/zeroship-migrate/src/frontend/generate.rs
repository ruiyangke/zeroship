//! `generate --schema schema.js` — the full JS-front-end flow.
//!
//! ```text
//! schema.js  ──eval (V8 sandbox)──▶  descriptor IR  (CollectionDescriptor[])
//!                                         │
//!                                         ▼  desired_snapshot
//!                                    DesiredSchema (union + ownership)
//!                                         │
//! live DB ──snapshot_schema (introspect)──▶  SchemaSnapshot (live)
//!                                         │
//!                                         ▼  DeclarativeAuthor::diff
//!                                    Migration[]  (additive + gated destructive)
//!                                         │
//!                                         ▼  render
//!                            db/migrations/<14-digit-ts>_<name>.sql  (dbmate)
//! ```
//!
//! This is ONE self-contained tool — no separate Node/vite step. The eval
//! runs in zeroship-runtime's V8 sandbox (untrusted creator schema, same
//! security model as app code); the diff + render reuse the lean engine's
//! declarative differ + dbmate loader verbatim.
//!
//! ## Which generate surface to use (NOT redundant with `generate_ops`)
//!
//! There are TWO generate-from-schema-diff surfaces; pick by artifact shape:
//!
//! - **`generate_migration` (THIS module)** — the PLATFORM schema-authority path.
//!   Renders a deployable `.sql` (dbmate) migration via the declarative differ,
//!   covering the FULL goodie surface (`vector(N)` ANN indexes, PostGIS GiST
//!   spatial indexes, FOREIGN KEY / CHECK constraints, FTS). The control-plane
//!   deploy seam (`zeroship_control::deploy_migrate::apply_bundle_migrations`)
//!   consumes these `.sql` files, and the schema-authority capstone e2e exercises
//!   the whole `schema.js → generate → pack → deploy-apply → data-plane` pipeline
//!   through it. It is the only generate path that emits a goodie/constraint-bearing
//!   schema end-to-end. NOT a creator hand-authoring surface — property A's raw-SQL
//!   ban governs what a CREATOR writes by hand, not platform-emitted deploy `.sql`.
//!
//! - **`super::scaffold::generate_ops`** — the CREATOR-FACING PORTABLE autogenerate
//!   path (PR4). Emits the op.* DSL (`.ts` + committed `.ir.json`) for the portable
//!   structural subset (tables, columns, plain btree indexes) and FAILS CLOSED on
//!   goodies (vector/postgis/FK/CHECK — author those by hand) so its output always
//!   re-diffs to zero.
//!
//! Reach for `generate_ops` to produce committed creator op.* artifacts; reach for
//! `generate_migration` for the platform `.sql` schema-authority deploy path.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::apply::drift::snapshot_schema;
use crate::conn;
use crate::model::migration::Migration;
use crate::render::declarative::{
    desired_snapshot, DeclarativeAuthor, DeclarativeError,
};

use super::eval::{eval_schema_to_ir, EvalError};

/// A failure from [`generate_migration`].
#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    /// The `schema.js` eval / lowering failed.
    #[error(transparent)]
    Eval(#[from] EvalError),
    /// Building the desired snapshot from the IR failed (bad descriptor —
    /// unsupported type, unsafe identifier, conflicting declaration, …) or
    /// the diff was refused (ownership, cross-app FK, …).
    #[error(transparent)]
    Declarative(#[from] DeclarativeError),
    /// Connecting to / introspecting the live database failed.
    #[error("live database access failed: {0}")]
    Db(String),
    /// Writing the migration file failed.
    #[error("writing migration file failed: {0}")]
    Io(String),
}

/// The result of a successful [`generate_migration`].
#[derive(Debug)]
pub struct GenerateOutcome {
    /// The migration file written (absolute or as-passed dir + name). `None`
    /// when the schema already matches the live DB (no-op — nothing written).
    pub written: Option<PathBuf>,
    /// The number of migrations the differ produced (0 ⇒ no-op).
    pub migration_count: usize,
    /// The rendered dbmate file body, even on no-op (empty string then). Handy
    /// for `--dry-run` style callers / tests that assert the SQL shape.
    pub body: String,
}

/// Run the full `generate` flow: eval `schema_source` → IR → diff against the
/// live DB at `database_url` → render a dbmate migration into `dir`.
///
/// - `project_schema` is the schema the migration is qualified into and the
///   schema introspected for the live state (the app's schema, `"<app_id>"`).
/// - `owner_app` is the declaring/deploying app (`app_…`); stamped on the IR
///   descriptors and the enforcement subject for the differ's ownership pass.
/// - `name` is the human-readable migration slug (file suffix).
///
/// Single-app model (design §10 non-goal: full project-umbrella): every live
/// table is treated as owned by `owner_app` for the ownership guard.
///
/// # Errors
/// See [`GenerateError`].
pub async fn generate_migration(
    schema_source: &str,
    database_url: &str,
    project_schema: &str,
    owner_app: &str,
    name: &str,
    dir: &Path,
) -> Result<GenerateOutcome, GenerateError> {
    // 1. Eval schema.js → descriptor IR (V8 sandbox).
    let descriptors = eval_schema_to_ir(schema_source, owner_app)?;

    // 2. Build the DESIRED snapshot from the IR.
    let desired = desired_snapshot(project_schema, &descriptors)?;

    // 3. Introspect the LIVE schema.
    let client = conn::connect(database_url)
        .await
        .map_err(|e| GenerateError::Db(e.to_string()))?;
    let live = snapshot_schema(&client, project_schema)
        .await
        .map_err(|e| GenerateError::Db(e.to_string()))?;

    // 4. Single-app ownership: every live table is owned by the deploying app.
    //    (The full project-umbrella union is a design §10 non-goal here.)
    let live_ownership: std::collections::HashMap<String, String> = live
        .tables
        .keys()
        .map(|t| (t.clone(), owner_app.to_string()))
        .collect();

    // 5. Diff desired vs live → migrations (no rename hints in this surface).
    let author = DeclarativeAuthor::new(project_schema, owner_app);
    let plan = author.diff(&desired, &live, &live_ownership, &[])?;
    let migrations = plan.all_migrations();

    // 6. Render + write a dbmate file (no-op when the plan is empty).
    let body = render_dbmate(&migrations);
    if migrations.is_empty() {
        return Ok(GenerateOutcome {
            written: None,
            migration_count: 0,
            body,
        });
    }

    let filename = format!("{}_{}.sql", timestamp_14(SystemTime::now()), slug(name));
    let path = dir.join(filename);
    std::fs::create_dir_all(dir).map_err(|e| GenerateError::Io(e.to_string()))?;
    std::fs::write(&path, &body).map_err(|e| GenerateError::Io(e.to_string()))?;

    Ok(GenerateOutcome {
        written: Some(path),
        migration_count: migrations.len(),
        body,
    })
}

/// Render a set of migrations as a single dbmate file body
/// (`-- migrate:up` / `-- migrate:down`).
///
/// The declarative differ may emit several `Migration`s for one `generate`
/// (e.g. CREATE TABLE + a deferred ADD CONSTRAINT). We concatenate their `up`
/// SQL in order into one `-- migrate:up` section, and their `down` SQL (in
/// REVERSE order — teardown is the mirror of setup) into one `-- migrate:down`
/// section. A migration with no `down` makes the whole file's down a comment
/// marker (the engine treats a missing down as explicitly irreversible).
pub fn render_dbmate(migrations: &[Migration]) -> String {
    if migrations.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str("-- migrate:up\n");
    for m in migrations {
        write_statement(&mut s, &m.up);
    }

    s.push_str("-- migrate:down\n");
    let all_reversible = migrations.iter().all(|m| m.down.is_some());
    if all_reversible {
        for m in migrations.iter().rev() {
            if let Some(down) = &m.down {
                write_statement(&mut s, down);
            }
        }
    } else {
        s.push_str(
            "-- (no reverse SQL: at least one generated statement is \
             explicitly irreversible)\n",
        );
    }
    s
}

/// Append one migration's SQL as a semicolon-terminated statement.
///
/// Each engine-emitted `Migration.up`/`down` is a SINGLE statement WITHOUT a
/// trailing `;` (the executor runs it via `batch_execute(&m.up)` as one
/// statement). A dbmate migration file, however, can hold MANY statements per
/// section, split on `;` — so we terminate each one. The blank line between
/// statements is cosmetic; the `;` is what makes the file replayable by any
/// `;`-splitting applier (dbmate, psql, raw `batch_execute`).
fn write_statement(s: &mut String, sql: &str) {
    let sql = sql.trim_end().trim_end_matches(';');
    if sql.is_empty() {
        return;
    }
    let _ = writeln!(s, "{sql};");
    s.push('\n');
}

/// Slugify a migration name for the filename: lowercase, non-alphanumerics →
/// `_`, collapsed. Keeps the file portable + dbmate-compatible.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_us = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "migration".to_string()
    } else {
        trimmed
    }
}

/// Format a 14-digit `YYYYMMDDHHMMSS` UTC timestamp (dbmate file-name prefix).
/// Civil-from-days, no date crate — mirrors the lean CLI's `timestamp_14`.
fn timestamp_14(now: SystemTime) -> String {
    let secs: i64 = now
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}{hh:02}{mm:02}{ss:02}")
}

/// `civil_from_days` (Howard Hinnant, public domain) — UTC proleptic Gregorian.
const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalises() {
        assert_eq!(slug("Add Orders Table"), "add_orders_table");
        assert_eq!(slug("create__users!!"), "create_users");
        assert_eq!(slug("***"), "migration");
    }

    #[test]
    fn render_empty_is_empty() {
        assert_eq!(render_dbmate(&[]), "");
    }
}
