//! Shadow-DATABASE dry-run + migration testing (v3 Plan C).
//!
//! A **dry-run** proves a migration batch applies cleanly — and, for a
//! declarative deploy, that the resulting schema matches what was *desired* —
//! **without ever touching the real project database**. It is the safety net the
//! control plane drives before a destructive or AI-authored apply: preview the
//! plan against a faithful copy, surface failures + advisories + resulting
//! drift, then decide whether to apply for real.
//!
//! # Why a throwaway DATABASE clone (not a shadow schema)
//!
//! Migration SQL **hard-codes the `project_schema` name** (it is schema-qualified
//! throughout, and that exact text is in the migration's checksum). Re-running
//! the *same* SQL against a differently-named "shadow schema" would require
//! rewriting the untrusted SQL to swap the schema name — a fidelity hole (the
//! bytes that run no longer match the bytes that were guarded + checksummed) AND
//! an injection surface (rewriting untrusted SQL). So the shadow is a throwaway
//! **DATABASE** that carries the SAME `project_schema` name and runs the
//! **UNMODIFIED** [`executor::apply`](crate::executor::apply) path — exact bytes,
//! exact guard, exact least-privilege migrator role, exact checksums.
//!
//! # Flow ([`dry_run`], Mode A — full replay)
//!
//! 1. `CREATE DATABASE <prefix><rand>` on the admin connection;
//! 2. open a SECOND compio-postgres session to the shadow DB (its run-loop is a
//!    detached compio task, mirroring [`crate::db::connect`]);
//! 3. `CREATE SCHEMA "<project_schema>"` + [`provision_migrator`] on the shadow;
//! 4. run the UNMODIFIED `executor::apply(shadow, cfg, migrations,
//!    Approval::Approved, applied_by)` — **full replay**: the shadow journal is
//!    empty, so apply computes `pending == the whole set` and applies all in
//!    order, exactly as a fresh real apply would;
//! 5. capture per-migration outcome + advisories + (declarative) resulting drift;
//! 6. **TEARDOWN ON EVERY PATH** — drop the shadow client, then `DROP DATABASE
//!    <name> WITH (FORCE)` on the admin conn. The CREATE is paired with an
//!    unconditional drop, mirroring [`executor::apply`]'s unlock-on-every-path
//!    and `baseline.rs`'s teardown discipline.
//!
//! # The load-bearing invariants
//!
//! - **`Approval::Approved` is for the SHADOW apply ONLY.** A dry-run must
//!   preview a destructive plan (that is the whole point), so the shadow apply is
//!   unconditionally approved. This approval value is constructed *inside*
//!   [`dry_run`] and never returned, so it can NEVER leak to a real apply.
//! - **The admin DB is touched ONLY by `CREATE DATABASE` / `DROP DATABASE`.** All
//!   schema + journal + DDL work happens on the shadow DB via the shadow client.
//!   `dry_run` never creates or writes the real `project_schema` / `meta_schema`.
//! - **`CREATEDB` is required on the admin role.** Document + provide a clear
//!   error.

use compio_postgres::Client;

use crate::analyze::Advisory;
use crate::approval::Approval;
use crate::db::{connect, ConnectError, ExecutorConfig};
use crate::drift::{diff_snapshots, snapshot_schema, DriftError, StructuralDrift};
use crate::engine::{DeclarativeDeployPlan, MigrationEngine};
use crate::executor::{self, ApplyError};
use crate::guard::{GuardConfig, SqlGuard};
use crate::migration::Migration;
use crate::role::{provision_migrator, RoleError};

/// Where + how to provision a throwaway shadow database.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    /// A DSN for an **admin** connection whose role has `CREATEDB` (it issues the
    /// `CREATE DATABASE` / `DROP DATABASE`). Its `dbname` is swapped to the
    /// randomly-named shadow DB to open the second session — so it must be a DSN
    /// the second connection can reuse with only the database name changed
    /// (same host/port/user/password).
    pub admin_dsn: String,
    /// The prefix for the throwaway database name, e.g. `"zsmig_shadow_"`. The
    /// full name is `<prefix><rand>`; [`sweep_leaked_shadows`] matches `<prefix>%`
    /// to reap crash-leaked clones.
    pub db_name_prefix: String,
}

/// The per-migration outcome of a dry-run apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationResult {
    /// The migration's version (`mig_…`).
    pub version: String,
    /// Whether this migration's `up` applied cleanly on the shadow.
    pub applied_ok: bool,
    /// The error (guard denial, non-idempotent non-txn, or a DB failure) when
    /// `applied_ok == false`.
    pub error: Option<String>,
}

/// The result of a dry-run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunReport {
    /// Overall success: every migration applied AND (for a declarative dry-run)
    /// the resulting schema matched the desired snapshot.
    pub ok: bool,
    /// Per-migration outcome, in apply order. A denial / non-idempotent failure
    /// that aborts the batch before any DB work still records the offending
    /// version with `applied_ok == false` and an `error`.
    pub per_migration: Vec<MigrationResult>,
    /// For a declarative dry-run, the structural drift between the DESIRED schema
    /// and the schema the shadow ended up with after applying the plan. `None`
    /// for a plain [`dry_run`] (no desired schema to compare against), or `Some`
    /// with a clean/non-clean drift for [`dry_run_declarative`].
    pub resulting_drift: Option<StructuralDrift>,
    /// Operational advisories (lock-heavy ops, destructive shapes, missing FK
    /// indexes, …) per migration version. Advisory-only; never gates `ok`.
    pub advisories: Vec<(String, Vec<Advisory>)>,
}

/// A failure of the dry-run *harness itself*.
///
/// Distinct from a migration failing (that is captured in the [`DryRunReport`]):
/// these are infrastructure faults — CREATE/DROP DATABASE, connecting to the
/// shadow, provisioning the role, snapshotting the resulting schema.
#[derive(Debug, thiserror::Error)]
pub enum DryRunError {
    /// `CREATE DATABASE` / `DROP DATABASE` or another admin-connection op failed.
    /// A missing `CREATEDB` privilege on the admin role surfaces here.
    #[error("shadow admin db error: {0}")]
    Admin(#[source] compio_postgres::Error),
    /// Opening the second (shadow) compio-postgres session failed.
    #[error("connect to shadow db: {0}")]
    Connect(#[from] ConnectError),
    /// Provisioning the shadow schema or the least-privilege migrator role failed.
    #[error("provision shadow: {0}")]
    Provision(#[from] RoleError),
    /// Introspecting the resulting shadow schema (declarative drift) failed.
    #[error("snapshot shadow schema: {0}")]
    Drift(#[from] DriftError),
}

/// Build the shadow DSN from the admin DSN by swapping the database name.
///
/// Supports both keyword/value DSNs (`host=… dbname=foo …`) and URL DSNs
/// (`postgres://user:pw@host:port/foo`). The shadow name is a bare, validated
/// identifier (alnum + `_`), so it is never an injection vector.
fn shadow_dsn(admin_dsn: &str, shadow_db: &str) -> String {
    let trimmed = admin_dsn.trim_start();
    if trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://") {
        // URL form: replace the path segment (the dbname) after the authority.
        // Split off any `?query` first so we can rebuild it.
        let (base, query) = match admin_dsn.split_once('?') {
            Some((b, q)) => (b, Some(q)),
            None => (admin_dsn, None),
        };
        // Find the '/' that begins the path, AFTER the `scheme://authority`.
        let scheme_end = base.find("://").map_or(0, |i| i + 3);
        let after_scheme = &base[scheme_end..];
        let new_base = after_scheme.find('/').map_or_else(
            // No path at all — append one.
            || format!("{base}/{shadow_db}"),
            |rel| {
                let path_start = scheme_end + rel;
                format!("{}/{}", &base[..path_start], shadow_db)
            },
        );
        match query {
            Some(q) => format!("{new_base}?{q}"),
            None => new_base,
        }
    } else {
        // Keyword/value form: drop any existing dbname=… token and append ours.
        let mut parts: Vec<String> = Vec::new();
        for tok in admin_dsn.split_whitespace() {
            let is_dbname = tok
                .split_once('=')
                .is_some_and(|(k, _)| k.eq_ignore_ascii_case("dbname"));
            if !is_dbname {
                parts.push(tok.to_string());
            }
        }
        parts.push(format!("dbname={shadow_db}"));
        parts.join(" ")
    }
}

/// A fresh, validated shadow database name: `<prefix><rand>`. The random suffix
/// is hex from a UUIDv7-ish source so two concurrent dry-runs never collide. The
/// whole name is `[a-z0-9_]` so it is a safe bare identifier.
fn fresh_shadow_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    // A monotone-ish, process-unique, time-seeded suffix. No external rand crate;
    // mirror the test harness's token() shape (pid + nanos + counter).
    static N: AtomicU64 = AtomicU64::new(0);
    // Sanitize the prefix to the identifier charset (defense in depth — the
    // caller controls it, but a stray char must never reach DDL).
    let safe_prefix: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Keep within Postgres' 63-byte identifier limit.
    let name = format!("{safe_prefix}{pid}_{nanos:x}_{n}");
    name.chars().take(63).collect()
}

/// Quote a SQL identifier (mirrors `role::quote_ident` / `journal::quote_ident`).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Dry-run a migration batch against a throwaway shadow DATABASE clone (Mode A —
/// full replay), **without touching the real project database**.
///
/// `admin_conn` is an open admin session (its role needs `CREATEDB`); it issues
/// only `CREATE DATABASE` / `DROP DATABASE`. `shadow_cfg.admin_dsn` is the DSN
/// for the SECOND session opened against the new shadow DB (same connection
/// params, `dbname` swapped). `cfg` is the project's [`ExecutorConfig`] —
/// reused VERBATIM (same `project_schema`, same `meta_schema`, same migrator
/// role) so the shadow apply is byte-faithful to the real one.
///
/// Returns a [`DryRunReport`]: per-migration outcomes + advisories. The shadow
/// DB is dropped on EVERY path (success, migration failure, harness error,
/// early return).
///
/// # The "never touches prod" guarantee
/// All schema / journal / DDL work runs on the shadow client. The only ops on
/// `admin_conn` are `CREATE DATABASE` and `DROP DATABASE`. The real
/// `project_schema` / `meta_schema` in the admin DB are never created or written.
///
/// # Errors
/// [`DryRunError`] when the harness itself fails (CREATE/DROP DATABASE, the
/// shadow connection, role provisioning, or — declarative — the resulting-drift
/// snapshot). A *migration* failing is NOT an error: it is reported in the
/// [`DryRunReport`] with `ok == false`.
pub async fn dry_run(
    admin_conn: &Client,
    migrations: &[Migration],
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    applied_by: &str,
) -> Result<DryRunReport, DryRunError> {
    let shadow_db = fresh_shadow_name(&shadow_cfg.db_name_prefix);

    // CREATE DATABASE — the paired half. From here, EVERY exit path must DROP it.
    admin_conn
        .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&shadow_db)))
        .await
        .map_err(DryRunError::Admin)?;

    // Run the body; capture its result WITHOUT early-returning, so teardown runs
    // unconditionally (mirrors executor::apply's unlock-on-every-path).
    let result = dry_run_body(migrations, cfg, shadow_cfg, &shadow_db, applied_by, None).await;

    teardown_shadow(admin_conn, &shadow_db).await;
    result
}

/// Dry-run a DECLARATIVE deploy plan against a shadow DATABASE, then validate the
/// **resulting** schema against the DESIRED snapshot (Phase 2).
///
/// Seeds + applies the plan's plain migrations and drives each rename's EXPAND
/// through the SAME [`apply_declarative`](MigrationEngine::apply_declarative) path
/// the real deploy uses (faithful), then snapshots the shadow's `project_schema`
/// and diffs it against `desired.snapshot`. A non-clean drift sets
/// `resulting_drift = Some(non-clean)` and `ok = false` — the generated plan did
/// NOT realise the desired schema, caught before any real apply.
///
/// `Approval::Approved` is used for the shadow apply ONLY (a dry-run must preview
/// a destructive/gated plan); it never leaves this function.
///
/// # Errors
/// [`DryRunError`] on a harness failure (CREATE/DROP DATABASE, shadow connect,
/// provisioning, or the resulting-drift snapshot). A migration / plan failing is
/// captured in the report (`ok == false`), not returned as an error — except a
/// hard apply error, which surfaces as the body's report with the offending
/// version marked failed.
pub async fn dry_run_declarative(
    admin_conn: &Client,
    plan: &DeclarativeDeployPlan,
    desired: &crate::declarative::DesiredSchema,
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    applied_by: &str,
) -> Result<DryRunReport, DryRunError> {
    let shadow_db = fresh_shadow_name(&shadow_cfg.db_name_prefix);

    admin_conn
        .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&shadow_db)))
        .await
        .map_err(DryRunError::Admin)?;

    let result = dry_run_declarative_body(
        plan, desired, cfg, shadow_cfg, &shadow_db, applied_by,
    )
    .await;

    teardown_shadow(admin_conn, &shadow_db).await;
    result
}

/// Open the shadow session + provision the schema/role, shared by both bodies.
/// Returns the connected shadow [`Client`].
async fn open_and_provision_shadow(
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    shadow_db: &str,
) -> Result<Client, DryRunError> {
    let dsn = shadow_dsn(&shadow_cfg.admin_dsn, shadow_db);
    let shadow = connect(&dsn).await?;
    // Same project_schema name as the real DB (Plan C decision) — the migration
    // SQL hard-codes it, so the shadow must carry the identical name.
    shadow
        .batch_execute(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .map_err(DryRunError::Admin)?;
    // Provision the confined migrator role on the shadow, exactly as the real DB
    // has one — so the apply runs under the SAME least-privilege confinement.
    provision_migrator(&shadow, cfg).await?;
    Ok(shadow)
}

/// The plain dry-run body. `desired` (when `Some`) drives the resulting-drift
/// check; `None` is the plain [`dry_run`].
async fn dry_run_body(
    migrations: &[Migration],
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    shadow_db: &str,
    applied_by: &str,
    desired: Option<&crate::declarative::DesiredSchema>,
) -> Result<DryRunReport, DryRunError> {
    let shadow = open_and_provision_shadow(cfg, shadow_cfg, shadow_db).await?;

    // UP-FRONT guard pass — gather advisories per migration (lock-heavy /
    // destructive / missing-FK-index shapes), and pre-compute the advisory set for
    // a migration the guard denies (whose advisories the success arm would never
    // produce). The executor::apply call below re-runs the guard as the real gate,
    // so a denial is surfaced per-migration from its error — this pass only
    // enriches the report with advisories, never gates.
    let guard = SqlGuard::new(GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    });
    let mut per_migration: Vec<MigrationResult> = Vec::new();
    let mut advisories: Vec<(String, Vec<Advisory>)> = Vec::new();
    for m in migrations {
        let version = m.version.as_str().to_string();
        match guard.check(&m.up) {
            Ok(report) => advisories.push((version, report.advisories)),
            // A denied migration: keep its (advisory-only) analysis so the report
            // is still informative; the denial itself comes back as the apply error.
            Err(_) => advisories.push((version, crate::analyze::analyze(&m.up))),
        }
    }

    // Run the UNMODIFIED apply path (full replay; shadow journal empty). The
    // executor re-runs its own guard, so a denial aborts the batch with nothing
    // applied — we surface it per-migration from the apply error below.
    let outcome = executor::apply(&shadow, cfg, migrations, Approval::Approved, applied_by).await;

    let mut ok = true;
    match outcome {
        Ok(o) => {
            // Everything in `applied` succeeded; record each in order.
            for v in &o.applied {
                per_migration.push(MigrationResult {
                    version: v.clone(),
                    applied_ok: true,
                    error: None,
                });
            }
        }
        Err(e) => {
            ok = false;
            record_apply_error(&e, migrations, &mut per_migration);
        }
    }

    // Resulting-drift check (declarative only).
    let resulting_drift = if let Some(desired) = desired {
        let shadow_snap = snapshot_schema(&shadow, &cfg.project_schema).await?;
        let drift = diff_snapshots(&desired.snapshot, &shadow_snap);
        if !drift.is_clean() {
            ok = false;
        }
        Some(drift)
    } else {
        None
    };

    // Drop the shadow client BEFORE the caller drops the database (force-drop
    // tolerates a lingering session, but releasing first is cleaner).
    drop(shadow);

    Ok(DryRunReport {
        ok,
        per_migration,
        resulting_drift,
        advisories,
    })
}

/// The declarative dry-run body: apply the plan via the real
/// [`apply_declarative`](MigrationEngine::apply_declarative) path, then diff the
/// resulting schema against `desired`.
async fn dry_run_declarative_body(
    plan: &DeclarativeDeployPlan,
    desired: &crate::declarative::DesiredSchema,
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    shadow_db: &str,
    applied_by: &str,
) -> Result<DryRunReport, DryRunError> {
    let shadow = open_and_provision_shadow(cfg, shadow_cfg, shadow_db).await?;

    // Advisories from the plain plan's linted items (already guard-checked).
    let mut advisories: Vec<(String, Vec<Advisory>)> = Vec::new();
    for item in &plan.plain.items {
        advisories.push((
            item.migration.version.as_str().to_string(),
            item.report.advisories.clone(),
        ));
    }

    let engine = MigrationEngine::new();
    let mut per_migration: Vec<MigrationResult> = Vec::new();
    let mut ok = true;

    // Drive the SAME path the real declarative deploy uses (plain gated apply +
    // each rename's expand/backfill). Approval::Approved is SHADOW-only.
    match engine
        .apply_declarative(plan, Approval::Approved, &shadow, cfg, applied_by)
        .await
    {
        Ok(outcome) => {
            for v in &outcome.applied.applied {
                per_migration.push(MigrationResult {
                    version: v.clone(),
                    applied_ok: true,
                    error: None,
                });
            }
        }
        Err(e) => {
            ok = false;
            per_migration.push(MigrationResult {
                version: "<declarative-apply>".to_string(),
                applied_ok: false,
                error: Some(e.to_string()),
            });
        }
    }

    // Resulting-drift: did the plan realise the DESIRED schema?
    let shadow_snap = snapshot_schema(&shadow, &cfg.project_schema).await?;
    let drift = diff_snapshots(&desired.snapshot, &shadow_snap);
    if !drift.is_clean() {
        ok = false;
    }

    drop(shadow);

    Ok(DryRunReport {
        ok,
        per_migration,
        resulting_drift: Some(drift),
        advisories,
    })
}

/// Translate an [`ApplyError`] into per-migration results: the named offending
/// version is marked failed with the error; everything that applied before it is
/// not separately reported here (the apply error path means the batch aborted —
/// for the static gates nothing applied, for an execution failure the earlier
/// ones did, but the dry-run's job is to surface the FAILURE clearly).
fn record_apply_error(
    e: &ApplyError,
    migrations: &[Migration],
    per_migration: &mut Vec<MigrationResult>,
) {
    let (version, msg): (Option<String>, String) = match e {
        ApplyError::MigrationFailed { version, source } => {
            (Some(version.clone()), format!("apply failed: {source}"))
        }
        ApplyError::Guard { version, source } => {
            (Some(version.clone()), format!("denied by guard: {source}"))
        }
        ApplyError::NonIdempotentNonTxn { version, reason } => {
            (Some(version.clone()), format!("non-idempotent non-txn: {reason}"))
        }
        ApplyError::ChecksumDrift { version, .. } => {
            (Some(version.clone()), format!("checksum drift: {e}"))
        }
        ApplyError::MissingDependency { version, .. }
        | ApplyError::ExpandNotApplied { version, .. }
        | ApplyError::SquashAlreadyApplied { version }
        | ApplyError::SquashPartialOverlap { version, .. } => {
            (Some(version.clone()), e.to_string())
        }
        // No single offending version — record a synthetic batch-level entry.
        _ => (None, e.to_string()),
    };
    let version = version.unwrap_or_else(|| {
        // Fall back to the first migration's version for legibility if the error
        // names none; if the set is empty use a sentinel.
        migrations
            .first()
            .map_or_else(|| "<batch>".to_string(), |m| m.version.as_str().to_string())
    });
    per_migration.push(MigrationResult {
        version,
        applied_ok: false,
        error: Some(msg),
    });
}

/// Drop the shadow database unconditionally (the paired teardown half). Uses
/// `WITH (FORCE)` so a lingering session does not block the drop, and `IF EXISTS`
/// so a not-yet-created / already-dropped shadow is a no-op. Best-effort: a drop
/// failure is logged, never propagated (the original result must surface).
async fn teardown_shadow(admin_conn: &Client, shadow_db: &str) {
    let sql = format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quote_ident(shadow_db));
    if let Err(e) = admin_conn.batch_execute(&sql).await {
        tracing::warn!(
            error = %e,
            shadow = %shadow_db,
            "zeroship-migrate: failed to drop shadow database (will be reaped by sweep_leaked_shadows)"
        );
    }
}

/// Drop crash-leaked shadow databases — clones whose owning dry-run process died
/// before teardown could run (Phase 3).
///
/// Matches `<prefix>%` databases (via a bound `LIKE`) and drops each older than
/// `older_than`.
///
/// Postgres exposes **no per-database creation timestamp** (`pg_database` has no
/// `created_at`, and on-disk dir mtime is not queryable cross-DB), so creation
/// time is encoded **in the name**: [`fresh_shadow_name`] embeds the nanosecond
/// clock as `<prefix><pid>_<nanos_hex>_<n>`. The sweeper parses that timestamp
/// and drops candidates whose age exceeds `older_than`. A candidate sharing the
/// prefix but NOT matching the embedded-timestamp shape is left untouched
/// (fail-safe: never reap a DB we cannot date).
///
/// # Errors
/// [`DryRunError::Admin`] if listing or dropping the catalog fails.
pub async fn sweep_leaked_shadows(
    admin_conn: &Client,
    prefix: &str,
    older_than: std::time::Duration,
) -> Result<usize, DryRunError> {
    // List candidate databases by name prefix. `prefix` is bound, never
    // interpolated — a LIKE wildcard in the prefix would only widen the match,
    // never escape into SQL.
    let like = format!("{}%", like_escape(prefix));
    let rows = admin_conn
        .query(
            "SELECT datname FROM pg_database WHERE datname LIKE $1 ESCAPE '\\'",
            &[&like],
        )
        .await
        .map_err(DryRunError::Admin)?;

    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let cutoff_nanos = older_than.as_nanos();

    let mut dropped = 0usize;
    for r in &rows {
        let name: String = r.get("datname");
        let Some(created_nanos) = parse_embedded_nanos(&name, prefix) else {
            // A name we can't parse the timestamp from — leave it (do not drop an
            // ambiguous match: fail-safe, never reap what we can't date).
            continue;
        };
        let age = now_nanos.saturating_sub(created_nanos);
        if age >= cutoff_nanos {
            let sql = format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_ident(&name)
            );
            admin_conn
                .batch_execute(&sql)
                .await
                .map_err(DryRunError::Admin)?;
            dropped += 1;
        }
    }
    Ok(dropped)
}

/// Escape `_` and `%` in a LIKE pattern operand (so a prefix containing them
/// matches literally). Backslash is the ESCAPE char in the query above.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == '%' || c == '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Parse the embedded creation nanos out of a `<prefix><pid>_<nanos_hex>_<n>`
/// shadow name. Returns `None` if the shape does not match (so an unrelated DB
/// that merely shares the prefix is never reaped).
fn parse_embedded_nanos(name: &str, prefix: &str) -> Option<u128> {
    // Use the SAME prefix-sanitization fresh_shadow_name applied, so a caller
    // passing the raw (pre-sanitized) prefix still matches the on-disk name.
    let safe_prefix: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let rest = name.strip_prefix(&safe_prefix)?;
    // rest == "<pid>_<nanos_hex>_<n>" — split on '_'.
    let mut parts = rest.split('_');
    let _pid = parts.next()?;
    let nanos_hex = parts.next()?;
    let _n = parts.next()?;
    u128::from_str_radix(nanos_hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_dsn_keyword_form_swaps_dbname() {
        let admin = "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";
        let s = shadow_dsn(admin, "zsmig_shadow_abc");
        assert!(s.contains("dbname=zsmig_shadow_abc"));
        assert!(!s.contains("dbname=zeroship_migrate_test"));
        assert!(s.contains("host=localhost"));
        assert!(s.contains("port=5440"));
    }

    #[test]
    fn shadow_dsn_url_form_swaps_path() {
        let admin = "postgres://postgres:zeroship@localhost:5440/zeroship_migrate_test";
        let s = shadow_dsn(admin, "zsmig_shadow_xyz");
        assert_eq!(s, "postgres://postgres:zeroship@localhost:5440/zsmig_shadow_xyz");
    }

    #[test]
    fn shadow_dsn_url_form_preserves_query() {
        let admin = "postgresql://u:p@h:5432/olddb?sslmode=disable";
        let s = shadow_dsn(admin, "newdb");
        assert_eq!(s, "postgresql://u:p@h:5432/newdb?sslmode=disable");
    }

    #[test]
    fn fresh_shadow_name_is_prefixed_and_bare() {
        let n = fresh_shadow_name("zsmig_shadow_");
        assert!(n.starts_with("zsmig_shadow_"));
        assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(n.len() <= 63);
    }

    #[test]
    fn parse_embedded_nanos_roundtrips() {
        let n = fresh_shadow_name("zsmig_shadow_");
        let nanos = parse_embedded_nanos(&n, "zsmig_shadow_");
        assert!(nanos.is_some(), "should parse the embedded nanos from {n}");
    }

    #[test]
    fn parse_embedded_nanos_rejects_unrelated() {
        assert!(parse_embedded_nanos("some_other_db", "zsmig_shadow_").is_none());
        // Prefix matches but the shape after it is wrong (no underscores) → None.
        assert!(parse_embedded_nanos("zsmig_shadow_garbage", "zsmig_shadow_").is_none());
    }
}
