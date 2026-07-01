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
//! **UNMODIFIED** [`executor::apply`](crate::apply::executor::apply) path — exact bytes,
//! exact guard, exact least-privilege migrator role, exact checksums.
//!
//! # Flow ([`dry_run`], Mode A — full replay)
//!
//! 1. `CREATE DATABASE <prefix><rand>` on the admin connection;
//! 2. open a SECOND compio-postgres session to the shadow DB (its run-loop is a
//!    detached compio task, mirroring [`crate::conn::connect`]);
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
//! # Flow ([`dry_run_declarative`], Mode B — snapshot-seed then incremental plan)
//!
//! Mode A's full-replay faithfulness holds only when the supplied set is the
//! WHOLE schema (a from-scratch deploy). A DECLARATIVE deploy is usually
//! INCREMENTAL: the plan is the delta vs the live schema (e.g. a rename whose
//! expand `ALTER TABLE "contacts" ADD COLUMN …` targets a table created in a
//! PRIOR deploy). Against the empty shadow that delta would fail with
//! `relation "contacts" does not exist` — a FALSE NEGATIVE (H4). So
//! [`dry_run_declarative`] adds one step before the plan applies:
//!
//! - **Seed the shadow with the CURRENT LIVE project structure**
//!   ([`seed_shadow_from_live`]): snapshot the live project schema read-only
//!   ([`snapshot_schema`]), reconstruct it in the empty shadow by REUSING the
//!   declarative differ (diff `desired = live-snapshot` vs `live = empty` ⇒ the
//!   CREATE statements), and apply those under the shadow migrator role + guard.
//!   The shadow then starts from the same state the real deploy sees, so the
//!   incremental plan applies cleanly and the dry-run is faithful.
//!
//! The seed reconstructs **tables/columns/types/nullability/indexes/constraints**
//! — enough to validate an incremental STRUCTURAL diff. It does NOT reconstruct
//! triggers/functions/sequences/DEFAULTs/data (a known limitation, see
//! [`seed_shadow_from_live`]). `CREATE DATABASE … TEMPLATE live` is rejected
//! (TEMPLATE needs no active connections to the live DB — Plan C design).
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

use compio::runtime::JoinHandle;
use compio_postgres::Client;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

use std::collections::{BTreeMap, HashMap};

use crate::analysis::analyze::Advisory;
use crate::approval::Approval;
use crate::apply::backend::capability::{
    DryRunError, DryRunReport, MigrationResult, SeedError, ShadowConfig, ShadowDryRun,
};
use super::PostgresBackend;
use crate::conn::{connect_with_handle, ExecutorConfig};
use crate::render::declarative::{DeclarativeAuthor, DesiredSchema};
use crate::apply::drift::{diff_snapshots, snapshot_schema};
use crate::model::snapshot::SchemaSnapshot;
use crate::engine::{DeclarativeDeployPlan, MigrationEngine};
use crate::apply::executor::{self, ApplyError};
use crate::guard::{GuardConfig, SqlGuard};
use crate::model::migration::Migration;
use crate::apply::role::{deprovision_migrator, migrator_role_name, provision_migrator, RoleError};

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
    crate::render::dml::escape_quote_ident(ident)
}

thread_local! {
    /// The C1 panic-injection seam. A regression test arms it (per-thread, so
    /// parallel sibling tests are unaffected) via [`arm_panic_after_provision`] to
    /// force a REAL unwind through the body→teardown path and prove [`dry_run`]'s
    /// teardown survives a panic. Never armed in production (dead by default).
    static PANIC_AFTER_PROVISION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm/disarm the C1 panic-injection seam on the CURRENT thread (test-only hook).
///
/// `#[doc(hidden)]`: not part of the stable API — it exists solely so the C1
/// regression test can drive a panic in the crash-before-teardown window. The
/// seam is a thread-local (parallelism-safe) and defaults to off, so a normal
/// dry-run never triggers it.
#[doc(hidden)]
pub fn arm_panic_after_provision(on: bool) {
    PANIC_AFTER_PROVISION.with(|f| f.set(on));
}

/// True when the C1 panic seam is armed on the current thread.
fn panic_seam_armed() -> bool {
    PANIC_AFTER_PROVISION.with(std::cell::Cell::get)
}

/// Derive the shadow's own [`ExecutorConfig`] from the real `cfg` + the shadow
/// DB name — so the shadow apply mirrors the SOURCE config's TRUST PROFILE
/// faithfully (the role model the real apply uses), against a throwaway DB whose
/// `project_id` is unique.
///
/// # Why mirror the trust profile (the faithfulness fix)
///
/// The REAL apply runs under a profile-specific role model (design §1.3 / §8):
/// - **Confined** (creator path) applies under a least-privilege NOSUPERUSER
///   `migrator` role via `SET ROLE`.
/// - **Platform** (operator path) applies as the ADMIN connection with NO
///   `SET ROLE`, because privileged platform DDL (`CREATE ROLE` / `GRANT` /
///   `CREATE SCHEMA` / `CREATE EXTENSION`) is exactly what a NOSUPERUSER role
///   CANNOT do.
///
/// If the shadow always provisioned + applied under a NOSUPERUSER migrator role,
/// a privileged Platform set would FAIL on the shadow (`dry_run.ok == false`) —
/// a FALSE NEGATIVE for the migrations the Platform profile exists to handle. So
/// the shadow derives its role model from `cfg.trust`:
///
/// - **Platform source** ⇒ a shadow Platform [`ExecutorConfig`]
///   ([`ExecutorConfig::platform`], same schema + extension allowlist) with
///   `migrator_role = None` — the apply runs as the shadow's admin connection,
///   exactly as the real Platform apply does (§8). The deny-list guard still runs
///   on the shadow SQL; the shadow is a throwaway DB; so this is NOT escalation.
/// - **Confined source** ⇒ UNCHANGED: a shadow-UNIQUE NOSUPERUSER migrator role
///   (`migrator_role_name(shadow project_id)`) provisioned + `SET ROLE`'d, so the
///   creator confinement stays faithful on the shadow too (H1).
///
/// In BOTH cases `project_id` → the shadow DB name (unique + sanitized): the
/// advisory-lock key is `hashtext(project_id)` and the migrator role name (when
/// any) derives from it, so the shadow never contends with, nor touches, the real
/// project's apply lock or cluster-global role. `project_schema` / `meta_schema`
/// / timeouts are UNCHANGED — the migration SQL hard-codes `project_schema`, so
/// the shadow must carry the identical name (the whole reason Plan C clones a
/// DATABASE, not a schema).
fn shadow_executor_cfg(cfg: &ExecutorConfig, shadow_db: &str) -> Result<ExecutorConfig, RoleError> {
    match cfg.trust {
        crate::model::policy::TrustProfile::Platform => {
            // Platform source: mirror the operator-side admin-connection apply (§8).
            // Mint a token via the shadow operator seam (the shadow harness is
            // operator-side, like the CLI runners). NO migrator role / SET ROLE, and
            // the SAME schema + extension allowlist as the source so the widened guard
            // and `search_path` are identical to the real Platform apply.
            let cap = crate::model::capability::mint_shadow_operator_capability();
            let mut sc = ExecutorConfig::platform(
                &cap,
                shadow_db.to_string(),
                cfg.project_schema.clone(),
                cfg.platform_schemas.clone(),
                cfg.platform_exts.clone(),
            );
            // Carry the byte-faithful fields the `platform` ctor does not take.
            sc.pg.meta_schema.clone_from(&cfg.pg.meta_schema);
            sc.pg.statement_timeout = cfg.pg.statement_timeout;
            sc.pg.lock_timeout = cfg.pg.lock_timeout;
            // Platform applies as the admin connection — NO SET ROLE (§8).
            sc.pg.migrator_role = None;
            Ok(sc)
        }
        #[cfg(any(test, feature = "standalone-cli"))]
        crate::model::policy::TrustProfile::Trusted => {
            // Trusted source (public dbmate-like posture): mirror the
            // connecting-role apply faithfully so the dry-run does NOT re-deny SQL
            // the real Trusted apply allows. Same operator token mint seam, NO
            // migrator role / SET ROLE (Trusted runs as the connecting role). The
            // shadow's own `guard_config()` returns the Trusted guard, so its
            // deny-list is OFF — identical to the real apply.
            let cap = crate::model::capability::mint_shadow_operator_capability();
            let mut sc =
                ExecutorConfig::trusted(&cap, shadow_db.to_string(), cfg.project_schema.clone());
            sc.pg.meta_schema.clone_from(&cfg.pg.meta_schema);
            sc.pg.statement_timeout = cfg.pg.statement_timeout;
            sc.pg.lock_timeout = cfg.pg.lock_timeout;
            sc.pg.migrator_role = None;
            Ok(sc)
        }
        _ => {
            // Confined source: UNCHANGED — a shadow-UNIQUE least-privilege migrator
            // role (H1), provisioned + `SET ROLE`'d so the creator confinement stays
            // faithful on the shadow too.
            let mut sc = cfg.clone();
            sc.project_id = shadow_db.to_string();
            sc.pg.migrator_role = Some(migrator_role_name(shadow_db)?);
            Ok(sc)
        }
    }
}

/// Dry-run a migration batch against a throwaway shadow DATABASE clone (Mode A —
/// full replay), **without touching the real project database**.
///
/// `admin_conn` is an open admin session (its role needs `CREATEDB`); it issues
/// only `CREATE DATABASE` / `DROP DATABASE`. `shadow_cfg.admin_dsn` is the DSN
/// for the SECOND session opened against the new shadow DB (same connection
/// params, `dbname` swapped). `cfg` is the project's [`ExecutorConfig`] — reused
/// for the byte-faithful fields (same `project_schema`, same `meta_schema`) so the
/// shadow apply runs the EXACT migration bytes; only the migrator role is swapped
/// for a shadow-UNIQUE one ([`shadow_executor_cfg`]) so the dry-run never touches
/// the real project's cluster-global role (H1).
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
    // The shadow's own confined role (H1): unique, never the real project's.
    let shadow_exec_cfg = shadow_executor_cfg(cfg, &shadow_db)?;

    // CREATE DATABASE — the paired half. From here, EVERY exit path must DROP it.
    admin_conn
        .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&shadow_db)))
        .await
        .map_err(DryRunError::Admin)?;

    // Run the body inside catch_unwind so a PANIC in the apply path (e.g. an
    // executor invariant trip on an untrusted plan graph) STILL reaches teardown
    // (C1 fix) — without catch_unwind the unwind would skip the drop and leak the
    // shadow DB + role. `AssertUnwindSafe` mirrors the crate's panic_util seam:
    // the catch is for teardown safety, not shared-state recovery.
    let caught = AssertUnwindSafe(dry_run_body(
        migrations,
        &shadow_exec_cfg,
        shadow_cfg,
        &shadow_db,
        applied_by,
        None,
    ))
    .catch_unwind()
    .await;

    finish_dry_run(admin_conn, &shadow_db, &shadow_exec_cfg, caught).await
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
    desired: &crate::render::declarative::DesiredSchema,
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    applied_by: &str,
) -> Result<DryRunReport, DryRunError> {
    let shadow_db = fresh_shadow_name(&shadow_cfg.db_name_prefix);
    let shadow_exec_cfg = shadow_executor_cfg(cfg, &shadow_db)?;

    // Snapshot the CURRENT LIVE project schema, read-only, on the admin
    // connection (its dbname IS the project DB), BEFORE any shadow is created.
    // This is the seed source for the faithful INCREMENTAL dry-run (H4): the
    // shadow must START from the live structure so a plan whose expand targets a
    // prior-deploy table applies cleanly. `snapshot_schema` is read-only and
    // hits only `information_schema`/`pg_catalog` of the REAL project schema — it
    // never mutates prod and never touches the migrator role.
    let live = snapshot_schema(admin_conn, &cfg.project_schema).await?;

    admin_conn
        .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&shadow_db)))
        .await
        .map_err(DryRunError::Admin)?;

    // catch_unwind so a panic in the declarative apply path still tears down the
    // shadow DB + role (C1 fix).
    let caught = AssertUnwindSafe(dry_run_declarative_body(
        plan,
        desired,
        &live,
        &shadow_exec_cfg,
        shadow_cfg,
        &shadow_db,
        applied_by,
    ))
    .catch_unwind()
    .await;

    finish_dry_run(admin_conn, &shadow_db, &shadow_exec_cfg, caught).await
}

/// Dry-run an arbitrary migration set against a shadow DATABASE **seeded from the
/// current LIVE project structure** (the H4 live-seed reuse), without touching the
/// real project DB.
///
/// This is the faithful primitive for an **INCREMENTAL** submission: the supplied
/// `migrations` are a delta vs the live schema (an `ALTER TABLE … ADD COLUMN` on a
/// table created in a prior deploy, a `DROP TABLE` of an existing table, …).
/// Against an EMPTY shadow such a delta would false-negative with `relation … does
/// not exist`; so before applying it we reconstruct the live structure in the
/// fresh shadow via [`seed_shadow_from_live`] (the exact mechanism
/// [`dry_run_declarative`] uses) and only then run the UNMODIFIED
/// [`apply`](crate::apply::executor::apply) path over `migrations`.
///
/// It is the generic peer of [`dry_run`] (which assumes the set is the WHOLE
/// schema / a from-scratch deploy) and the non-declarative peer of
/// [`dry_run_declarative`] (which validates a generated declarative plan against a
/// desired snapshot). Use it when you have an ad-hoc migration set to validate
/// against the live structure — e.g. the [`crate::ops::submit`] adapter.
///
/// `Approval::Approved` is used for the shadow apply ONLY (a dry-run must preview a
/// destructive set), constructed inside this function and never returned — it can
/// never leak to a real apply. The shadow DB + role are torn down on EVERY path.
///
/// # Fidelity scope
/// Inherits [`seed_shadow_from_live`]'s known limitation: the seed reconstructs
/// tables/columns/types/nullability/indexes/constraints, NOT
/// triggers/functions/sequences/DEFAULTs/row data. A set that depends on a
/// prior-deploy trigger/function/sequence/data can therefore false-negative.
///
/// # Errors
/// [`DryRunError`] on a harness failure (CREATE/DROP DATABASE, shadow connect,
/// provisioning, the live snapshot, or the live-seed). A *migration* failing is
/// NOT an error — it is reported in the [`DryRunReport`] with `ok == false`.
pub async fn dry_run_incremental(
    admin_conn: &Client,
    migrations: &[Migration],
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    applied_by: &str,
) -> Result<DryRunReport, DryRunError> {
    let shadow_db = fresh_shadow_name(&shadow_cfg.db_name_prefix);
    let shadow_exec_cfg = shadow_executor_cfg(cfg, &shadow_db)?;

    // Snapshot the CURRENT LIVE project schema, read-only, on the admin connection
    // (its dbname IS the project DB), BEFORE any shadow is created — the seed
    // source for the faithful incremental dry-run. `snapshot_schema` is read-only
    // (information_schema / pg_catalog of the REAL project schema), never mutates
    // prod, never touches the migrator role.
    let live = snapshot_schema(admin_conn, &cfg.project_schema).await?;

    admin_conn
        .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&shadow_db)))
        .await
        .map_err(DryRunError::Admin)?;

    // catch_unwind so a panic in the apply path still tears down the shadow (C1).
    let caught = AssertUnwindSafe(dry_run_incremental_body(
        migrations,
        &live,
        &shadow_exec_cfg,
        shadow_cfg,
        &shadow_db,
        applied_by,
    ))
    .catch_unwind()
    .await;

    finish_dry_run(admin_conn, &shadow_db, &shadow_exec_cfg, caught).await
}

/// The body of [`dry_run_incremental`]: open + provision the shadow, SEED it from
/// the live snapshot, then run the UNMODIFIED apply path over `migrations`.
async fn dry_run_incremental_body(
    migrations: &[Migration],
    live: &SchemaSnapshot,
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    shadow_db: &str,
    applied_by: &str,
) -> Result<DryRunReport, DryRunError> {
    let (shadow, run_loop) = open_and_provision_shadow(cfg, shadow_cfg, shadow_db).await?;

    // SEED the shadow to match the current LIVE structure before the incremental
    // apply (the H4 fix, reused). An empty live schema is a no-op (from-scratch
    // path unchanged).
    if let Err(e) = seed_shadow_from_live(&shadow, cfg, live, applied_by).await {
        close_shadow_session(shadow, run_loop).await;
        return Err(DryRunError::Seed(e));
    }

    // Up-front guard pass — gather advisories per migration (advisory-only; the
    // executor::apply below re-runs the guard as the real gate). Build the guard
    // from the SHADOW cfg's profile (`cfg.guard_config()`) so a Platform set is
    // linted under the widened guard, not the Confined deny-list (faithfulness).
    let guard = SqlGuard::new(cfg.guard_config());
    let mut advisories: Vec<(String, Vec<Advisory>)> = Vec::new();
    for m in migrations {
        let version = m.version.as_str().to_string();
        match guard.check(&m.up) {
            Ok(report) => advisories.push((version, report.advisories)),
            Err(_) => advisories.push((version, crate::analysis::analyze::analyze(&m.up))),
        }
    }

    // Run the UNMODIFIED apply path over the seeded shadow. `Approval::Approved` is
    // shadow-only (a dry-run must preview a destructive set).
    let outcome = executor::apply(&shadow, cfg, migrations, Approval::Approved, applied_by).await;

    let mut per_migration: Vec<MigrationResult> = Vec::new();
    let mut ok = true;
    match outcome {
        Ok(o) => {
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

    close_shadow_session(shadow, run_loop).await;

    Ok(DryRunReport {
        ok,
        per_migration,
        resulting_drift: None,
        advisories,
        teardown_ok: true,
        teardown_error: None,
    })
}

/// The Postgres [`ShadowDryRun`] — owns its admin [`Client`] internally (the
/// connection NEVER appears on the neutral trait surface; C3 fixed).
///
/// Each method lowers the neutral inputs to the existing throwaway-DATABASE
/// harness ([`dry_run`] / [`dry_run_declarative`]) VERBATIM — the temp shadow-DB
/// CREATE/DROP, the second compio-postgres session, the shadow-unique migrator
/// role, the catch-unwind-then-teardown discipline, and the `Approval::Approved`
/// shadow-only invariant all stay inside those functions, behavior-identical. This
/// is the regression bar: the entire existing shadow/dry_run suite must pass
/// unchanged.
///
/// [`Client`]: compio_postgres::Client
#[derive(Debug)]
pub struct PgShadow<'a> {
    /// The admin connection (its `dbname` IS the project DB; its role has
    /// `CREATEDB`). Private — it never crosses the [`ShadowDryRun`] surface.
    admin_conn: &'a Client,
}

impl<'a> PgShadow<'a> {
    /// Wrap an admin connection as the Postgres shadow-dry-run capability.
    #[must_use]
    pub fn new(admin_conn: &'a Client) -> Self {
        Self { admin_conn }
    }
}

impl ShadowDryRun for PgShadow<'_> {
    fn dry_run<'a>(
        &'a self,
        migrations: &'a [Migration],
        cfg: &'a ExecutorConfig,
        shadow_cfg: &'a ShadowConfig,
        applied_by: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DryRunReport, DryRunError>> + 'a>,
    > {
        // Forward to the byte-identical free harness; the admin `&Client` stays
        // private to this impl and never crosses the neutral trait surface.
        Box::pin(dry_run(self.admin_conn, migrations, cfg, shadow_cfg, applied_by))
    }

    fn dry_run_declarative<'a>(
        &'a self,
        plan: &'a DeclarativeDeployPlan,
        desired: &'a DesiredSchema,
        cfg: &'a ExecutorConfig,
        shadow_cfg: &'a ShadowConfig,
        applied_by: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DryRunReport, DryRunError>> + 'a>,
    > {
        Box::pin(dry_run_declarative(
            self.admin_conn,
            plan,
            desired,
            cfg,
            shadow_cfg,
            applied_by,
        ))
    }
}

/// Tear down the shadow (DB + role) and reconcile with the body's outcome.
///
/// The single funnel both dry-run entry points pass through after the body —
/// whether the body returned normally OR panicked. Teardown runs on EVERY path
/// (C1 invariant). On a body panic the teardown still runs, then the panic is
/// re-raised with [`std::panic::resume_unwind`] so the original failure is never
/// silently swallowed — the contract is "always tear down", not "swallow the
/// panic".
///
/// `teardown_ok` / `teardown_error` from the teardown are stamped onto a
/// successful report (H2 fix) so the control plane can trigger an immediate
/// sweep on a leaked clone instead of waiting for the periodic sweeper.
async fn finish_dry_run(
    admin_conn: &Client,
    shadow_db: &str,
    shadow_exec_cfg: &ExecutorConfig,
    caught: std::thread::Result<Result<DryRunReport, DryRunError>>,
) -> Result<DryRunReport, DryRunError> {
    let teardown = teardown_shadow(admin_conn, shadow_db, shadow_exec_cfg).await;
    match caught {
        Ok(Ok(mut report)) => {
            // Surface teardown hygiene in the report (H2).
            report.teardown_ok = teardown.is_ok();
            report.teardown_error = teardown.err();
            Ok(report)
        }
        // The body returned a harness error — teardown already ran; propagate it.
        Ok(Err(e)) => Err(e),
        // The body PANICKED — teardown already ran; re-raise so nothing is lost.
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Open the shadow session + provision the schema/role, shared by both bodies.
/// Returns the connected shadow [`Client`] AND its run-loop [`JoinHandle`] (so
/// the body can deterministically close the session before the FORCE drop; H2).
///
/// `cfg` here is the SHADOW [`ExecutorConfig`] (its `project_id` is the shadow DB
/// name, so [`provision_migrator`] creates a shadow-unique role; H1).
async fn open_and_provision_shadow(
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    shadow_db: &str,
) -> Result<(Client, JoinHandle<()>), DryRunError> {
    let dsn = shadow_dsn(&shadow_cfg.admin_dsn, shadow_db);
    let (shadow, run_loop) = connect_with_handle(&dsn).await?;
    // Same project_schema name as the real DB (Plan C decision) — the migration
    // SQL hard-codes it, so the shadow must carry the identical name.
    shadow
        .batch_execute(&format!(
            "CREATE SCHEMA {}",
            quote_ident(&cfg.project_schema)
        ))
        .await
        .map_err(DryRunError::Admin)?;
    // Provision the confined migrator role on the shadow ONLY for a Confined
    // dry-run — exactly as the real creator DB has one, so the apply runs under the
    // SAME least-privilege confinement (role name shadow-unique, derived from
    // `cfg.project_id` == shadow DB). A PLATFORM dry-run mirrors the real Platform
    // apply (§8): it runs as the admin connection with NO migrator role, so there
    // is no role to provision (privileged platform DDL needs admin, not a
    // NOSUPERUSER role). The branch is keyed on `migrator_role` (set by
    // [`shadow_executor_cfg`] from the source's trust profile), so the role-skip is
    // strictly Platform-only — a Confined dry-run ALWAYS provisions + SET ROLEs.
    if cfg.pg.migrator_role.is_some() {
        provision_migrator(&shadow, cfg).await?;
    }
    Ok((shadow, run_loop))
}

/// Seed the fresh shadow's project schema to MATCH the current LIVE project
/// structure — the "Mode B snapshot-seed" required for a faithful INCREMENTAL
/// declarative dry-run (H4).
///
/// # Mechanism (snapshot → reconstruct-via-differ)
///
/// The live structure is captured up-front by
/// [`snapshot_schema`](crate::apply::drift::snapshot_schema) (read-only, on the admin
/// connection — never the migrator role) and handed in as `live`. Here we turn
/// that snapshot back into CREATE statements by REUSING the declarative differ:
/// we diff `desired = live-snapshot` against `live = EMPTY`, which yields exactly
/// the CREATE TABLE / CREATE INDEX / ADD CONSTRAINT migrations that rebuild the
/// live structure on the empty shadow. We then apply them through the SAME gated
/// [`apply`](crate::engine::MigrationEngine::apply) path any migration uses — so
/// the seed runs under the shadow's least-privilege migrator role + the guard,
/// under `Approval::Approved` (shadow-only, the seed is pure additive CREATEs).
///
/// Ownership: every reconstructed table is attributed to `applied_by` (and the
/// caller-supplied `live_ownership` map is left empty: every table is "new" on
/// the empty shadow, so only [`enforce_ownership`](crate::render::declarative) on the
/// union side runs, and it passes because we own them all). This is sound for the
/// shadow because it is a THROWAWAY — ownership there has no security meaning; the
/// real ownership/cross-app gates already ran when the caller built `plan`.
///
/// # Fidelity scope (KNOWN LIMITATION)
///
/// `snapshot_schema` captures **tables, columns, types, nullability, indexes,
/// and constraints (PK/FK/UNIQUE/CHECK bodies)** — sufficient to validate an
/// incremental STRUCTURAL diff (the dry-run's job). It does NOT reconstruct
/// **triggers, functions, sequences, column DEFAULTs, or row DATA**. So a
/// contract that, say, DROPs a prior-deploy dual-write TRIGGER could false-
/// negative on the shadow (the trigger never existed there to drop). A fuller
/// clone (e.g. `pg_dump --schema-only` replayed into the shadow) is the future
/// option; `CREATE DATABASE … TEMPLATE live` is rejected because TEMPLATE
/// requires NO active connections to the live DB (Plan C design).
///
/// # An empty live schema is a no-op
///
/// When the live schema has no tables (a from-scratch first deploy) the differ
/// emits nothing and this returns `Ok(())` immediately — the from-scratch path is
/// unchanged, so it cannot ever wrongly fail a valid dry-run.
///
/// # Errors
/// [`SeedError`] if authoring the reconstruction migrations from the live
/// snapshot fails ([`SeedError::Author`]) or applying them on the shadow fails
/// ([`SeedError::Apply`]).
async fn seed_shadow_from_live(
    shadow: &Client,
    cfg: &ExecutorConfig,
    live: &SchemaSnapshot,
    applied_by: &str,
) -> Result<(), SeedError> {
    // Nothing live ⇒ nothing to seed (the from-scratch full-schema deploy path is
    // unchanged: no seed migrations, no apply).
    if live.tables.is_empty() {
        return Ok(());
    }

    // Reconstruct the live structure as a DESIRED schema owned (on the shadow) by
    // the deploying actor — so the differ's ownership enforcement passes (the
    // shadow is a throwaway; ownership has no security meaning there).
    let ownership: BTreeMap<String, String> = live
        .tables
        .keys()
        .map(|t| (t.clone(), applied_by.to_string()))
        .collect();
    let desired_live = DesiredSchema {
        snapshot: live.clone(),
        ownership,
        // PHASE 4 — the shadow seed reconstructs from a LIVE snapshot, not from
        // descriptors, and is a PG-only path; no SDK schemas to carry (the SQLite
        // leg is never taken here). Empty side-map.
        sqlite_schemas: BTreeMap::new(),
    };

    // Diff desired = live-snapshot vs live = EMPTY ⇒ the CREATE statements that
    // rebuild the live structure. `live_ownership` is empty: every table is "new"
    // on the empty shadow (no DROP candidates), so the fail-closed drop-ownership
    // guard is never reached. No rename hints — the seed is pure reconstruction.
    let author = DeclarativeAuthor::new(cfg.project_schema.clone(), applied_by);
    let diff = author.diff(
        &desired_live,
        &SchemaSnapshot::default(),
        &HashMap::new(),
        &[],
    )?;

    // The reconstruction is pure additive CREATEs (no renames, no drops); apply
    // it on the shadow through the SAME gated path as any migration (guard + the
    // shadow's least-privilege migrator role). Approval::Approved is shadow-only.
    if diff.migrations.is_empty() {
        return Ok(());
    }
    // Declarative is Confined-only by construction (no operator token reaches the
    // declarative path), so the hard-wired confined planner here can't drift into
    // a privileged profile.
    let guard_cfg = GuardConfig::confined(cfg.project_schema.clone());
    let engine = MigrationEngine::new();
    let plan = engine.plan(&diff.migrations, &guard_cfg);
    engine
        .apply(
            &plan,
            Approval::Approved,
            &PostgresBackend::new(shadow),
            cfg,
            applied_by,
        )
        .await?;
    Ok(())
}

/// The plain dry-run body. `desired` (when `Some`) drives the resulting-drift
/// check; `None` is the plain [`dry_run`].
async fn dry_run_body(
    migrations: &[Migration],
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    shadow_db: &str,
    applied_by: &str,
    desired: Option<&crate::render::declarative::DesiredSchema>,
) -> Result<DryRunReport, DryRunError> {
    let (shadow, run_loop) = open_and_provision_shadow(cfg, shadow_cfg, shadow_db).await?;

    // C1 test seam: force a panic in the exact crash-before-teardown window —
    // AFTER the shadow DB + role exist — to prove teardown is panic-safe (the
    // catch_unwind in `dry_run` reaches teardown anyway, then re-raises the
    // panic). Armed ONLY by the C1 regression test (a per-thread cell); a normal
    // run never sets it, so production is unaffected.
    if panic_seam_armed() {
        // Keep the session/run-loop alive across the panic so teardown (not this
        // body) is what tears them down — exactly the production unwind path.
        std::mem::forget(shadow);
        std::mem::forget(run_loop);
        panic!("test-injected panic after shadow provision");
    }

    // UP-FRONT guard pass — gather advisories per migration (lock-heavy /
    // destructive / missing-FK-index shapes), and pre-compute the advisory set for
    // a migration the guard denies (whose advisories the success arm would never
    // produce). The executor::apply call below re-runs the guard as the real gate,
    // so a denial is surfaced per-migration from its error — this pass only
    // enriches the report with advisories, never gates.
    // Build the advisory guard from the SHADOW cfg's profile (`cfg.guard_config()`)
    // so a Platform set is linted under the widened guard, not the Confined
    // deny-list — the executor::apply below re-runs the same guard as the real gate.
    let guard = SqlGuard::new(cfg.guard_config());
    let mut per_migration: Vec<MigrationResult> = Vec::new();
    let mut advisories: Vec<(String, Vec<Advisory>)> = Vec::new();
    for m in migrations {
        let version = m.version.as_str().to_string();
        match guard.check(&m.up) {
            Ok(report) => advisories.push((version, report.advisories)),
            // A denied migration: keep its (advisory-only) analysis so the report
            // is still informative; the denial itself comes back as the apply error.
            Err(_) => advisories.push((version, crate::analysis::analyze::analyze(&m.up))),
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

    // Deterministically close the shadow session BEFORE the caller's FORCE drop
    // (H2): drop the client, then cancel its run-loop so the backend is gone and
    // DROP DATABASE has nothing to fight.
    close_shadow_session(shadow, run_loop).await;

    Ok(DryRunReport {
        ok,
        per_migration,
        resulting_drift,
        advisories,
        // Stamped by finish_dry_run after teardown actually runs.
        teardown_ok: true,
        teardown_error: None,
    })
}

/// The declarative dry-run body: apply the plan via the real
/// [`apply_declarative`](MigrationEngine::apply_declarative) path, then diff the
/// resulting schema against `desired`.
async fn dry_run_declarative_body(
    plan: &DeclarativeDeployPlan,
    desired: &crate::render::declarative::DesiredSchema,
    live: &SchemaSnapshot,
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    shadow_db: &str,
    applied_by: &str,
) -> Result<DryRunReport, DryRunError> {
    let (shadow, run_loop) = open_and_provision_shadow(cfg, shadow_cfg, shadow_db).await?;

    // H4 — SEED the shadow to match the CURRENT LIVE project structure BEFORE
    // applying the incremental plan. Without this the shadow is an EMPTY schema,
    // so an INCREMENTAL declarative plan (e.g. a rename whose expand
    // `ALTER TABLE "contacts" ADD COLUMN …` targets a table created in a PRIOR
    // deploy) fails with `relation "contacts" does not exist` — a FALSE NEGATIVE
    // for a plan the real apply runs cleanly. Reconstructing the live structure
    // first makes the dry-run faithful for the COMMON incremental case (not only
    // a from-scratch full-schema deploy). See [`seed_shadow_from_live`] for the
    // mechanism + fidelity scope.
    if let Err(e) = seed_shadow_from_live(&shadow, cfg, live, applied_by).await {
        // A seed failure is a HARNESS fault (the reconstruction itself failed),
        // not a migration failure — close the session deterministically so the
        // caller's FORCE drop has nothing to fight, then surface it as a
        // DryRunError (teardown still runs via finish_dry_run).
        close_shadow_session(shadow, run_loop).await;
        return Err(DryRunError::Seed(e));
    }

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
    //
    // The deploy-N apply applies only the EXPAND for each rename; the CONTRACT
    // (DROP TRIGGER + DROP COLUMN `<from>`) is DEFERRED to deploy N+1 and returned
    // as `pending_contract`. If we snapshot here, `<from>` is still present, so a
    // CORRECT rename would false-positive on drift (`desired` lacks `<from>`).
    let pending_contract = match engine
        .apply_declarative(
            plan,
            Approval::Approved,
            &PostgresBackend::new(&shadow),
            cfg,
            applied_by,
        )
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
            outcome.pending_contract
        }
        Err(e) => {
            ok = false;
            per_migration.push(MigrationResult {
                version: "<declarative-apply>".to_string(),
                applied_ok: false,
                error: Some(e.to_string()),
            });
            Vec::new()
        }
    };

    // #5d: in the shadow (a throwaway, so we can simulate BOTH deploys) apply the
    // DEFERRED contract too — simulating deploy N+1, which the real fleet runs
    // AFTER app code switches to `<to>`. The cross-deploy gate is satisfied here
    // because the expand's E3 is already journaled in the shadow (apply_declarative
    // journaled it above). Now the shadow reflects the FINAL post-contract state:
    // `<from>` is dropped, so diffing against `desired` (which lacks `<from>`)
    // validates the rename's true end state instead of false-positiving on the
    // mid-rename intermediate.
    if ok && !pending_contract.is_empty() {
        // Declarative is Confined-only by construction (no operator token reaches
        // the declarative path), so the hard-wired confined planner here can't
        // drift into a privileged profile.
        let guard_cfg = GuardConfig::confined(cfg.project_schema.clone());
        let contract_plan = engine.plan(&pending_contract, &guard_cfg);
        match engine
            .apply(
                &contract_plan,
                Approval::Approved,
                &PostgresBackend::new(&shadow),
                cfg,
                applied_by,
            )
            .await
        {
            Ok(outcome) => {
                for v in &outcome.applied {
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
                    version: "<deferred-contract>".to_string(),
                    applied_ok: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // Resulting-drift: did the plan (expand + deferred contract) realise the
    // DESIRED schema?
    let shadow_snap = snapshot_schema(&shadow, &cfg.project_schema).await?;
    let drift = diff_snapshots(&desired.snapshot, &shadow_snap);
    if !drift.is_clean() {
        ok = false;
    }

    // H2: deterministically close the shadow session before the FORCE drop.
    close_shadow_session(shadow, run_loop).await;

    Ok(DryRunReport {
        ok,
        per_migration,
        resulting_drift: Some(drift),
        advisories,
        teardown_ok: true,
        teardown_error: None,
    })
}

/// Deterministically close a shadow session before a `DROP DATABASE … FORCE`
/// (H2): drop the client (releasing its protocol handle), then `cancel().await`
/// the run-loop task so the backend connection is fully gone. Without this the
/// detached run-loop could still hold a registered backend when FORCE runs — a
/// race the FORCE has to fight; closing first removes that contention.
async fn close_shadow_session(shadow: Client, run_loop: JoinHandle<()>) {
    drop(shadow);
    // `cancel` resolves once the task is finished/aborted; the client is already
    // dropped so the run-loop will observe the closed half and wind down.
    let _ = run_loop.cancel().await;
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

/// Tear down the shadow unconditionally (the paired teardown half): DROP the
/// DATABASE, then DROP the shadow-unique migrator ROLE.
///
/// `DROP DATABASE` uses `WITH (FORCE)` so a lingering session does not block it,
/// and `IF EXISTS` so a not-yet-created / already-dropped shadow is a no-op. The
/// ROLE is cluster-global — it SURVIVES `DROP DATABASE` — so it must be dropped
/// explicitly (H1): drop it AFTER the database, since `DROP DATABASE` removes the
/// objects the role owned in the shadow, leaving `DROP ROLE`/[`deprovision_migrator`]
/// nothing to reassign.
///
/// Returns `Ok(())` on full teardown, or `Err(message)` describing the FIRST
/// failure (H2): the message is surfaced in [`DryRunReport::teardown_error`] so
/// the control plane can trigger an immediate sweep. A failure is also logged.
/// Best-effort across BOTH steps: a DB-drop failure does not skip the role drop.
async fn teardown_shadow(
    admin_conn: &Client,
    shadow_db: &str,
    shadow_exec_cfg: &ExecutorConfig,
) -> Result<(), String> {
    let mut first_err: Option<String> = None;

    // 1. Drop the shadow database (FORCE + IF EXISTS).
    let drop_db = format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
        quote_ident(shadow_db)
    );
    if let Err(e) = admin_conn.batch_execute(&drop_db).await {
        tracing::warn!(
            error = %e,
            shadow = %shadow_db,
            "zeroship-migrate: failed to drop shadow database (will be reaped by sweep_leaked_shadows)"
        );
        first_err.get_or_insert_with(|| format!("drop shadow database: {e}"));
    }

    // 2. Drop the cluster-global shadow migrator role (survives DROP DATABASE).
    //    Runs after the DB drop so the role owns nothing left to reassign. ONLY a
    //    Confined dry-run provisioned a role (Platform runs as admin, no role; §8),
    //    so skip the drop when none was provisioned — keyed on `migrator_role` so
    //    no cluster-global role can leak from a Platform shadow (there is none).
    if shadow_exec_cfg.pg.migrator_role.is_some() {
        if let Err(e) = deprovision_migrator(admin_conn, shadow_exec_cfg).await {
            tracing::warn!(
                error = %e,
                shadow = %shadow_db,
                role = ?shadow_exec_cfg.pg.migrator_role,
                "zeroship-migrate: failed to drop shadow migrator role (cluster-global leak)"
            );
            first_err.get_or_insert_with(|| format!("drop shadow role: {e}"));
        }
    }

    first_err.map_or(Ok(()), Err)
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
            // Continue-on-error (H2c): one undroppable clone (e.g. a session the
            // FORCE can't evict in time) must NOT abort reaping the rest. Log it
            // and move on; the next sweep retries it.
            match admin_conn.batch_execute(&sql).await {
                Ok(()) => dropped += 1,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        shadow = %name,
                        "zeroship-migrate: sweep failed to drop a leaked shadow database (continuing)"
                    );
                }
            }
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
