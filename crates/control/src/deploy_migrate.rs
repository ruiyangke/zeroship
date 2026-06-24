//! Deploy-time DB migration — the schema-authority §8 wiring.
//!
//! P6 makes `zeroship-migrate` the **single schema authority** for creator
//! apps: a `.zship` ships its versioned migration files (carried on the
//! manifest as content-addressed blobs, see
//! [`zeroship_bundle::MigrationFileEntry`]), and the control-plane deploy step
//! provisions the app's per-app role + schema and **applies the pending
//! migrations BEFORE the go-live commit**. The runtime no longer migrates.
//!
//! # Where this slots into the deploy handler
//!
//! ```text
//! deploy::ingest (writes blobs + manifest)            [api.rs ~505]
//!   → reconstruct migration files from blobs           [api.rs, this module]
//!   → apply_bundle_migrations (THIS module)            ← migrate phase
//!       provision schema "<app_id>" + migrator role
//!       load_dir → engine.plan (Confined) → engine.apply
//!   → set_deploy_with_manifest (go-live)               [api.rs ~605]
//! ```
//!
//! Only on a **successful** migrate does the handler reach the go-live commit.
//! A migrate failure returns an error and the old bundle keeps serving its
//! already-migrated schema (schema-authority §8.3).
//!
//! # The app identity is the trusted path id
//!
//! `app_id` is the **path parameter** the deploy handler already authorized
//! (`AppsDeploy` on `Resource::App{id}`); it is never read from a request body.
//! The per-app schema is `"<app_id>"` and the project id seeding the advisory
//! lock + journal + the least-privilege `migrator_<app_id>` role is the same
//! `app_id` (the plugin-db per-app-schema model). So a creator can only ever
//! migrate the schema they were authorized to deploy.
//!
//! # Profile: Confined, no shadow dry-run (v1)
//!
//! Migrations run under the **Confined** profile (the full SQL deny-list +
//! single-schema confinement to `"<app_id>"`) and the least-privilege
//! `migrator_<app_id>` role (line-2 DB-privilege defense). We **skip the
//! shadow-DB dry-run** at deploy: the shadow path needs a `CREATEDB` admin DSN,
//! and the in-line safety (the engine re-runs the guard on every `up` + the
//! migrator role) is the defense. A future revision MAY add an optional
//! `--admin-db` (CREATEDB) config and run the shadow when present; v1 does not.
//!
//! # Destructive + approval-gated migrations (incl. online `renameColumn`)
//!
//! The deploy path passes [`Approval::None`]. A destructive migration (DROP /
//! TRUNCATE / lossy type change) is therefore **refused** at deploy (the engine
//! gate returns [`EngineError::ApprovalRequired`]) and no go-live happens —
//! destructive schema change goes through the out-of-band `submit_migration`
//! surface / expand-contract across deploys (schema-authority §8.4), not a
//! creator's routine deploy.
//!
//! An online `renameColumn` lowered on the IR path (§2.6) is in the SAME
//! approval-gated class — and this is symmetric across BOTH dialects:
//!
//! - **PG leg (deploy-wired, but refused here):** a PG `renameColumn` lowers to a
//!   `PlanStep::OnlineRename(PgExpandContract)`. Its EXPAND's backfill MUTATES data,
//!   so `run_expand_pg` requires [`Approval::Approved`] and returns
//!   [`zeroship_migrate::OnlineError::Approval`] otherwise. Because
//!   `apply_bundle_ir_migrations` applies under [`Approval::None`] (like every
//!   routine deploy), a `renameColumn` shipped in a `.zship` lowers successfully —
//!   the live-fact type-gate (the `table_snapshots` populated below) runs and
//!   reconciles the IR type against the live column — then is **refused at the
//!   approval gate** ([`DeployMigrateError::OnlineExpand`]); no go-live. So while
//!   the PG rename is type-reconciliation-wired, it is NOT completable through a
//!   routine deploy: like any approval-gated op it must go through the out-of-band
//!   APPROVED-apply surface, which PR2 does NOT wire. (Pinned by
//!   `deploy_migrate_renamecolumn_refused_at_approval_gate_on_routine_deploy`.)
//! - **SQLite leg (not deploy-wired at all):** the SQLite IR-rename rebuild leg is
//!   engine-proven but has no production/dev deploy entry point constructing a
//!   SQLite-dialect `LiveSchema` (see the `sqlite_schemas` note below); a
//!   SQLite-targeted IR rename fails closed before lowering.
//!
//! NEITHER leg of an IR `renameColumn` therefore COMPLETES through a routine wired
//! deploy in PR2 — the PG leg is type-reconciliation-wired but approval-gate-refused;
//! the SQLite leg is unwired. Wiring an approved IR-apply surface (and the SQLite
//! IR-deploy entry point) is the out-of-band / CLI-rewire wave (gated on this PR).
//!
//! ## FOLLOW-UP (named wave): approved IR-apply surface + SQLite-dialect LiveSchema
//!
//! Tracked, intentionally NOT in PR2's scope (spec §2.6.2 lines 1278/1281 scope the
//! PR2 e2e to engine-level dual-leg proof; an approved IR-apply surface is a later
//! CLI-rewire wave). To make an online IR `renameColumn` actually *shippable* a
//! follow-up wave must land TWO things:
//!   1. **Approved IR-apply surface** — an out-of-band entry (mirroring the `.sql`
//!      `submit_migration` surface) that drives `apply_bundle_ir_migrations` under
//!      [`Approval::Approved`] so the PG expand backfill (`run_expand_pg`) is no
//!      longer refused at the gate. Routine `.zship` deploy stays `Approval::None`.
//!   2. **SQLite-dialect `LiveSchema` construction** — a SQLite IR-deploy entry that
//!      populates `sqlite_schemas` from the live SDK `Value`s (the SQLite analogue of
//!      this PG `table_snapshots` introspection), so the SQLite rebuild leg has a
//!      production/dev entry point instead of failing closed before lowering.
//! Until both land, an online rename is engine-proven (PR2) but not go-live-wired.

use std::collections::BTreeMap;
use std::path::Path;

use uuid::Uuid;
use zeroship_migrate::{
    compute_manifest, connect, load_dir_migrations, provision_migrator, Approval, ConnectError,
    DeclarativeApplyError, DriftError, EngineError, ExecutorConfig, IrAuthor, LiveSchema,
    LoadAndLowerGuardedError, LoaderError, LockMode, MigrationBackend, MigrationEngine,
    PostgresBackend, RoleError, SqlDialect,
};

/// What a successful deploy-migrate produced (for logging / the deploy log).
#[derive(Debug, Clone, Default)]
pub struct MigrateOutcome {
    /// Migration version ids applied this deploy (empty ⇒ already up to date).
    pub applied: Vec<String>,
    /// Migration version ids skipped because already journaled.
    pub skipped: Vec<String>,
    /// **PR7 online-rename go-live.** The CONTRACT (C1/C2) migrations of any PG
    /// online `renameColumn` whose EXPAND completed this deploy, surfaced as
    /// *pending* — they are NOT applied in this deploy (the cross-deploy
    /// expand-contract partition, §2.0.2): the new column is live + dual-written,
    /// app code migrates from `<from>` to `<to>`, and a SUBSEQUENT approved deploy
    /// applies the contract to drop the old column. Empty on the routine
    /// (`Approval::None`) path — an online expand is refused there before it can
    /// produce a pending contract. The deploy log records these version ids so the
    /// operator/control plane knows a follow-up contract deploy is owed.
    pub pending_contract: Vec<String>,
}

/// A deploy-time migration failure. The deploy handler maps this to an HTTP
/// error and **does not commit go-live** — the old bundle keeps serving.
#[derive(Debug, thiserror::Error)]
pub enum DeployMigrateError {
    /// Opening the admin connection failed.
    #[error("deploy-migrate connect: {0}")]
    Connect(#[from] ConnectError),
    /// Creating the per-app schema `"<app_id>"` failed (admin DDL).
    #[error("deploy-migrate provision schema: {0}")]
    ProvisionSchema(compio_postgres::Error),
    /// Provisioning the least-privilege `migrator_<app_id>` role failed.
    #[error("deploy-migrate provision role: {0}")]
    ProvisionRole(#[from] RoleError),
    /// Loading / parsing the reconstructed migration directory failed (bad
    /// filename grammar, duplicate version, orphan down, unparseable body, …).
    #[error("deploy-migrate load migrations: {0}")]
    Load(#[from] LoaderError),
    /// The engine refused or failed the apply: a guard denial, a destructive
    /// migration without approval (refused at deploy), checksum drift, or a
    /// mid-apply DB error.
    #[error("deploy-migrate apply: {0}")]
    Apply(#[from] EngineError),
    /// A creator `.ir.json` failed the fail-closed LOAD GATE (malformed, future
    /// `ir_version`, structural reject incl. the bare-name DropIndex, ownership
    /// violation, checksum-hint mismatch) or its GUARD-PER-FRAGMENT lower (§6.1.1):
    /// a guard-denied rendered fragment carries the exact op-index + kind
    /// attribution (the production deploy path routes through
    /// `load_and_lower_guarded`, so this attribution reaches the 422 the creator
    /// sees). A creator-fault — the deploy handler maps this to a 422; no go-live.
    #[error("deploy-migrate IR load/guarded-lower ({file}): {source}")]
    Ir {
        /// The `.ir.json` filename the gate / guard refused.
        file: String,
        /// The fail-closed gate / guard-per-fragment lower error.
        #[source]
        source: LoadAndLowerGuardedError,
    },
    /// Reading the `.ir.json` file from the reconstructed migrations dir failed.
    #[error("deploy-migrate read IR file ({file}): {message}")]
    IrRead {
        /// The `.ir.json` filename.
        file: String,
        /// The I/O error.
        message: String,
    },
    /// Introspecting the live schema (to build the IR ownership registry + the
    /// FK-inline live-table set) failed.
    #[error("deploy-migrate live snapshot: {0}")]
    Snapshot(#[from] DriftError),
    /// A rename's online expand/backfill failed while applying an IR plan via
    /// `apply_plan`. REACHABLE since PR2: an IR `renameColumn` lowers to a
    /// `PlanStep::OnlineRename(PgExpandContract)`, whose EXPAND backfill is
    /// approval-gated. Because the routine deploy applies under [`Approval::None`],
    /// the dominant occurrence is [`zeroship_migrate::OnlineError::Approval`] — a
    /// `renameColumn` shipped in a `.zship` is REFUSED at this gate (no go-live) and
    /// must go through the out-of-band approved-apply surface (PR2 does not wire it;
    /// see the module-level "Destructive + approval-gated migrations" doc). Other
    /// `OnlineError` variants surface a genuine mid-expand failure.
    #[error("deploy-migrate IR online expand: {0}")]
    OnlineExpand(#[from] zeroship_migrate::OnlineError),
}

/// Map the plan orchestrator's [`DeclarativeApplyError`] onto the deploy error.
///
/// The IR deploy path routes through `MigrationEngine::apply_plan` (§5.2), which
/// returns [`DeclarativeApplyError`]: its `Plain` arm wraps the SAME
/// [`EngineError`] the prior `engine.apply` path returned (so a destructive-without-
/// approval refusal, a guard denial, or checksum drift stays a
/// [`DeployMigrateError::Apply`] — the tests' match arm is unchanged), and its
/// `Expand` arm (unreachable on PR1 pure-DDL) maps to [`DeployMigrateError::OnlineExpand`].
impl From<DeclarativeApplyError> for DeployMigrateError {
    fn from(e: DeclarativeApplyError) -> Self {
        match e {
            DeclarativeApplyError::Plain(inner) => DeployMigrateError::Apply(inner),
            DeclarativeApplyError::Expand(inner) => DeployMigrateError::OnlineExpand(inner),
        }
    }
}

/// Quote a SQL identifier (double embedded quotes, wrap in `"`). Mirrors the
/// engine's `quote_ident` so the schema name is never raw-interpolated.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Provision the app's per-app role + schema and apply the bundle's pending
/// migrations under the **Confined** profile, BEFORE go-live (§8).
///
/// `migrate_dsn` is a PRIVILEGED provisioning DSN with `CREATEROLE` + `CREATE`
/// on the database — a SEPARATE admin role, **not** the control-plane
/// `zeroship_control` role (which is BYPASSRLS but has neither privilege; see
/// V0025). It is wired from `--provision-db` / `PROVISION_DATABASE_URL`.
/// `app_id` is the trusted,
/// already-authorized path id; the per-app schema + project id + migrator role
/// are all derived from it. `migrations_dir` holds the migration files the
/// deploy handler reconstructed from the bundle's blobs (Flyway `V<NNNN>__…` or
/// dbmate-shaped). An **empty** directory is a clean no-op (the app ships no
/// schema).
///
/// On success the schema + journal are committed; the caller then commits
/// go-live. On ANY error the caller MUST NOT commit go-live.
///
/// # Errors
/// [`DeployMigrateError`] on connect / schema-provision / role-provision /
/// load / apply failure.
pub async fn apply_bundle_migrations(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // The routine `.zship` deploy is NEVER auto-approved: a destructive op or an
    // online expand is refused at the approval gate (no go-live). The AI/creator
    // never auto-applies a gated migration.
    apply_bundle_migrations_with_approval(migrate_dsn, app_id, migrations_dir, Approval::None).await
}

/// **PR7 online-rename go-live** — the APPROVED out-of-band apply surface (§2.6.2 /
/// §2.0.2). Identical to [`apply_bundle_migrations`] except it carries
/// [`Approval::Approved`] into the engine, so an approval-gated step **completes**:
/// a PG online `renameColumn`'s EXPAND (E1..E3 + the dual-write backfill) is applied
/// under the held project lock and its CONTRACT (C1/C2) is surfaced as
/// [`MigrateOutcome::pending_contract`] for a later approved contract deploy. This is
/// the deliberate, reviewed approval seam the routine deploy refuses — it is the
/// entry point the control plane drives ONLY after an explicit operator/AI approval
/// of the gated migration set (design §1.6: the AI never auto-rolls-forward a gated
/// change). A destructive DDL op also applies here (approval covers the whole set),
/// so callers MUST gate access to this surface on a real approval decision.
///
/// WIRING PRECONDITIONS (HARD — do NOT wire this surface into a production deploy
/// handler until BOTH are satisfied; the regression test
/// `production_deploy_handler_never_wires_the_unguarded_approved_go_live_surface`
/// fails RED the instant it is wired):
///
/// 1. §2.0.3 CROSS-DEPLOY PENDING-CONTRACT INTERLOCK. The [`MigrateOutcome::
///    pending_contract`] this surface returns when a PG EXPAND completes is a TRANSIENT
///    value only — it is NOT journaled as an outstanding obligation and no later deploy
///    reads it back. Before a production caller exists, the owed contract MUST be
///    persisted (a Pending phase keyed by table+version) AND the §2.0.3(2) fail-closed
///    refusal implemented (refuse a subsequent deploy whose ops touch a table with an
///    OUTSTANDING pending contract) together with §2.0.3(3) orphan handling. Without
///    this, a completed EXPAND whose follow-up contract deploy never runs leaves the old
///    column behind a forever-pending dual-write trigger with no engine-level guard.
/// 2. PER-VERSION APPROVAL SCOPING (the SCOPE WARNING below).
///
/// SCOPE WARNING (deferred to the approval-workflow wiring wave): this is a COARSE,
/// bundle-wide [`Approval::Approved`] — approving an online-rename also green-lights
/// any UNRELATED destructive op (e.g. a `dropTable`) co-bundled in the same
/// `migrations_dir`. It is NOT exploitable today (no production caller drives this
/// surface; the routine deploy at `api.rs` correctly uses [`Approval::None`]). When
/// the control-plane approval endpoint is wired, it MUST scope approval to the
/// specific reviewed version-ids (or split expand-approval from arbitrary-destructive
/// approval) rather than handing this whole-bundle flag to an attacker-influenced set,
/// so approving a rename cannot blanket-authorize co-bundled destructive DDL.
///
/// # Errors
/// [`DeployMigrateError`] on connect / provision / load / apply failure (incl. a
/// genuine mid-expand `OnlineExpand` failure that is NOT the approval refusal).
pub async fn apply_bundle_migrations_approved(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
) -> Result<MigrateOutcome, DeployMigrateError> {
    apply_bundle_migrations_with_approval(migrate_dsn, app_id, migrations_dir, Approval::Approved)
        .await
}

async fn apply_bundle_migrations_with_approval(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
    approval: Approval,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // The per-app schema + project id are the trusted path id. The plugin-db
    // model maps app_id → schema "<app_id>"; the engine uses the same id to seed
    // the advisory lock, journal, and the migrator_<app_id> role name.
    let schema = app_id.to_string();

    // Load the migration set FIRST — a malformed directory is a deploy error we
    // surface before touching the DB (no schema/role provisioned for a bundle
    // that can't load).
    // PR0 (`op.*` DSL §5.2): `load_dir` now returns `Vec<AppliedPlan>`. The
    // platform/control deploy path is the trusted `.sql` path (every file is a
    // single-step plan), and its apply runs over the FLAT `Migration` set
    // (`apply_verified` + the integrity-manifest fold), so we load the flat form
    // via `load_dir_migrations` — byte-identical to the pre-PR0 behavior. The
    // IR-path apply (PR1+) routes `Vec<AppliedPlan>` through `apply_plan`.
    let migrations = load_dir_migrations(migrations_dir)?;

    // Open the admin connection (CREATEROLE + CREATE SCHEMA). Detaches its
    // driver loop onto the compio runtime.
    let conn = connect(migrate_dsn).await?;

    // (a) Provision the per-app schema. Idempotent: IF NOT EXISTS. The migrator
    //     role provisioning (below) reassigns ownership to the migrator, so the
    //     migrator's DDL + ALTER DEFAULT PRIVILEGES resolve to an owner it
    //     controls.
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(&schema)
    ))
    .await
    .map_err(DeployMigrateError::ProvisionSchema)?;

    // Build the Confined executor config (full deny-list + single-schema
    // confinement to "<app_id>"), running migrations under the least-privilege
    // migrator_<app_id> role (line-2 DB defense).
    let role = zeroship_migrate::migrator_role_name(&schema)?;
    let exec_cfg =
        ExecutorConfig::new(schema.clone(), schema.clone()).with_migrator_role(role.clone());

    // (b) Provision the migrator role (idempotent). Owns the project schema,
    //     no access to the meta schema (unforgeable journal), no reach into
    //     control/auth/other schemas.
    provision_migrator(&conn, &exec_cfg).await?;

    // (c) Plan (Confined guard) + apply PENDING via the integrity-manifest seam
    //     (`apply_verified`). Approval::None ⇒ a destructive migration is refused
    //     at deploy (no go-live); additive-forward is the routine path. The engine
    //     independently re-runs the guard + the migrator role on every up (defense
    //     in depth) — we do not skip those.
    //
    //     H2 — INTEGRITY MANIFEST: the manifest gate (`manifest.rs`) detects a
    //     creator / AI-author / build-pipeline tampering the migration SET between
    //     authoring/review and apply (reorder / edit / insert / remove). For that
    //     guarantee to hold, the EXPECTED hash MUST come from a TRUSTED, OUT-OF-BAND
    //     source — NOT from the same `.zship` the migrations arrived in (an attacker
    //     who can edit the migrations can edit a hash shipped alongside them, and the
    //     check would vacuously pass; see manifest.rs "Trust model").
    //
    //     No such build-side stamp exists yet: the `.zship` manifest carries only
    //     per-file blob hashes (which travel WITH the migrations — self-consistency,
    //     not an independent expectation), and the control DB stores no migration
    //     manifest hash. So we CANNOT honestly pass an `expected` hash here — doing
    //     so against a bundle-derived value would be a FAKE "verified". Instead we
    //     compute the manifest over the loaded set and LOG it for traceability /
    //     incident forensics, and route through `apply_verified(expected: None)` so
    //     the gate is wired and threading a trusted stamp later is a one-line change.
    //
    //     FOLLOW-UP (REQUIRED for the SEC defense to bite): the build/review side
    //     must stamp `compute_manifest(...)` at authoring time and persist it
    //     out-of-band (control DB, keyed by app + bundle), and this call must then
    //     pass `Some(&expected)` so a tampered/reordered set is REFUSED before any
    //     DDL. Until then this is traceability only, NOT tamper-prevention.
    let manifest = compute_manifest(&migrations);
    tracing::info!(
        app_id = %app_id,
        migration_count = migrations.len(),
        manifest = %manifest.as_str(),
        "deploy-migrate: computed migration-set integrity manifest (traceability only — \
         no trusted build-side stamp to verify against yet; see H2 follow-up)"
    );
    let engine = MigrationEngine::new();
    let guard_cfg = zeroship_migrate::GuardConfig::confined(schema.clone());
    // P6a genericized `MigrationEngine::apply` over `MigrationBackend`; the
    // platform/control deploy path is Postgres, so wrap the connection in the
    // PG backend (behavior-identical to the pre-seam `&Client` call).
    let backend = PostgresBackend::new(&conn);
    let outcome = engine
        .apply_verified(
            &migrations,
            &guard_cfg,
            // No trusted expectation available (see the H2 note above). NEVER pass a
            // bundle-derived hash here — it would be a vacuous self-check.
            None,
            // PR7: the approval threads from the entry point — `Approval::None` on the
            // routine `.zship` deploy (a destructive `.sql` migration is refused), and
            // `Approval::Approved` on the out-of-band approved-apply surface.
            approval,
            &backend,
            &exec_cfg,
            "deploy",
        )
        .await?;

    // (d) CREATOR `.ir.json` PATH (§5.2/§6/§8.6). A `.zship` may ship `.ir.json`
    //     artifacts (the op.* DSL output) alongside / instead of `.sql`. Each is
    //     routed through the fail-closed IR LOAD GATE
    //     (`IrAuthor::load_and_lower`: deserialize → ir_version → validate_ir →
    //     server-stamped ownership → checksum-hint), with the deploy-target dialect
    //     (Postgres here) threaded in, then LOWERED + applied under the SAME
    //     Confined guard + migrator role. This is the production caller of the IR
    //     gate (previously the gate was exported but unreached).
    let ir_outcome = apply_bundle_ir_migrations(
        &backend,
        app_id,
        migrations_dir,
        &exec_cfg,
        &guard_cfg,
        approval,
    )
    .await?;

    let mut applied = outcome.applied;
    applied.extend(ir_outcome.applied);
    let mut skipped = outcome.skipped;
    skipped.extend(ir_outcome.skipped);
    Ok(MigrateOutcome {
        applied,
        skipped,
        pending_contract: ir_outcome.pending_contract,
    })
}

/// Discover + apply the bundle's `.ir.json` creator artifacts (§5.2/§8.6).
///
/// For each `*.ir.json` file in `migrations_dir` (version-ordered by filename),
/// the fail-closed IR LOAD GATE runs ([`IrAuthor::load_and_lower`]: deserialize →
/// `ir_version` → `validate_ir` → server-stamped ownership → advisory checksum
/// hint), with the deploy-target dialect (Postgres) threaded in (§2.4.1), then
/// the validated, owned ops are LOWERED to migrations and applied under the SAME
/// Confined guard + least-priv migrator role as the `.sql` path. The ownership
/// registry + the FK-inline live-table set are introspected from the LIVE schema
/// (the per-app schema `"<app_id>"`, all tables owned by `app_id`).
///
/// An empty / IR-free directory is a clean no-op.
///
/// # Errors
/// [`DeployMigrateError::Ir`] on a fail-closed gate refusal / lower failure (a
/// creator fault → 422); [`DeployMigrateError::Snapshot`] / [`DeployMigrateError::IrRead`]
/// / [`DeployMigrateError::Apply`] on introspection / I/O / apply failure.
async fn apply_bundle_ir_migrations(
    backend: &PostgresBackend<'_>,
    app_id: &Uuid,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &zeroship_migrate::GuardConfig,
    approval: Approval,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // Discover `*.ir.json` files, version-ordered by filename (deterministic).
    let mut ir_files: Vec<std::path::PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| DeployMigrateError::IrRead {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| DeployMigrateError::IrRead {
            file: migrations_dir.display().to_string(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".ir.json"))
        {
            ir_files.push(path);
        }
    }
    if ir_files.is_empty() {
        return Ok(MigrateOutcome::default());
    }
    ir_files.sort();

    let app = app_id.to_string();

    // Introspect the LIVE schema ONCE to SEED the IR ownership registry + the
    // FK-inline live-table set: every live table in the per-app schema is owned by
    // the deploying app, so the registry maps each live table → `app_id` and the
    // same key-set is the live-table set. Both are MUTABLE and ADVANCE as each
    // `.ir.json` applies (below) — a `createTable` in `0001.ir.json` makes that
    // table owned-by-the-deployer + live for `0002.ir.json`, so a same-deploy
    // migration that touches an earlier file's table resolves ownership / inlines
    // FKs correctly. (Pre-fix these were seeded once and never advanced, so a
    // legitimate multi-file deploy FAILED CLOSED on ownership / mis-deferred FKs.)
    let live = backend.snapshot_schema(exec_cfg).await?;
    let mut registry: BTreeMap<String, String> =
        live.tables.keys().map(|t| (t.clone(), app.clone())).collect();
    // The IR-path Lower's live facts: the live table set (FK inline-vs-defer) PLUS
    // the set of index NAMES the live catalog reports as UNIQUE. The latter is the
    // AUTHORITATIVE source for the `dropIndex` destructive/approval gate — a drop of
    // a live-unique index lowers `destructive + requires_approval` regardless of the
    // IR's advisory `unique` hint (a hostile/buggy author cannot under-declare it to
    // bypass the gate). Introspected the SAME way the differ's `render_drop_index`
    // reads `IndexSnapshot::unique`.
    let mut live_schema = LiveSchema {
        tables: live.tables.keys().cloned().collect(),
        unique_indexes: live
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect(),
        // PR2 — carry the FULL introspected per-table column structure so the PG
        // `renameColumn` leg can reconcile the IR-carried column type against the
        // LIVE `from` column's actual `data_type` (the IR-path mirror of the
        // declarative `RenameHintTypeMismatch`): a rename whose IR `ty` disagrees
        // with the live column fails closed BEFORE any dual-write is authored, and a
        // rename whose live `from` column is absent fails closed rather than trust
        // the IR type alone. The whole live snapshot is already in hand, so this is
        // free; the PG expand-contract author still needs only `{from,to,ty}` to
        // author the sequence — the snapshot is consulted ONLY for the type gate.
        //
        // DEPLOY-WIRING HONESTY (PG leg): populating this makes the type-gate REACH
        // the live column on the production path — but a PG `renameColumn` still does
        // NOT COMPLETE through this routine deploy. Its expand-contract EXPAND backfill
        // is approval-gated; this path applies under `Approval::None` (see the loop
        // below), so a lowered rename is REFUSED at the approval gate
        // (`DeployMigrateError::OnlineExpand` ⇐ `OnlineError::Approval`), exactly like a
        // destructive op. The type reconciliation is wired; the APPROVED apply is the
        // out-of-band wave (gated on this PR). This is symmetric with the SQLite leg's
        // not-deploy-wired note below — see the module-level "Destructive +
        // approval-gated migrations" doc. Pinned by the control-plane e2e
        // `deploy_migrate_renamecolumn_refused_at_approval_gate_on_routine_deploy`.
        table_snapshots: live.tables.clone(),
        // Every live table in this per-app schema is owned by the deploying app
        // (the registry is seeded from exactly this set, below). Carried for
        // completeness; the PG rename leg does not consult it (cross-app authority
        // is enforced upstream by the IR-load gate's registry check), but populating
        // it keeps the live-facts bundle honest rather than fabricating ownership.
        table_ownership: live.tables.keys().map(|t| (t.clone(), app.clone())).collect(),
        // The SQLite SDK-schema `Value`s (`sqlite_schemas`) are NOT introspectable
        // from a PG catalog and are unused on this PG-targeted deploy path (a PG
        // rename lowers to expand-contract, never the SQLite 12-step rebuild). The
        // SQLite IR-rename rebuild leg is ENGINE-PROVEN (the `IrAuthor`/differ unit +
        // temp-file e2e in `ir_rename_pr2_sqlite.rs`) but is NOT YET DEPLOY-WIRED:
        // no production or dev/CLI path constructs a SQLite-dialect `LiveSchema` with
        // these facts today. Wiring a SQLite IR-deploy entry point (the dev-tier peer
        // of this PG introspection) is the CLI-rewire wave (gated on this PR). Until
        // then a SQLite-targeted IR rename would fail closed (no `table_snapshots`/
        // `sqlite_schemas`), never silently emit a wrong rebuild.
        sqlite_schemas: std::collections::BTreeMap::new(),
    };

    let engine = MigrationEngine::new();
    let mut applied: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    // PR7: the CONTRACT migrations of any online rename whose EXPAND completed this
    // deploy (PG leg only; the cross-deploy expand-contract partition, §2.0.2). Empty
    // on the routine `Approval::None` path (the expand is refused before producing one).
    let mut pending_contract: Vec<String> = Vec::new();
    // The lowered DDL `Migration`s across ALL `.ir.json` files this deploy — folded
    // into a SET-LEVEL integrity manifest (below) so the IR path has the SAME
    // traceability/anti-tamper seam the `.sql` path has (H2 follow-up, §8 point 5).
    let mut ir_lowered_all: Vec<zeroship_migrate::Migration> = Vec::new();

    for path in &ir_files {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let bytes = std::fs::read_to_string(path).map_err(|e| DeployMigrateError::IrRead {
            file: file.clone(),
            message: e.to_string(),
        })?;

        // The FAIL-CLOSED gate + GUARD-PER-FRAGMENT lower (§6.1.1), with the
        // deploy-target dialect (Postgres). Routing through `load_and_lower_guarded`
        // (not plain `load_and_lower`) means a guard denial reaches the creator with
        // the exact op-index + kind attribution, not a bare whole-`up` denial. It
        // returns ONE `AppliedPlan` per file (§2.0 / §5.2) whose `checksum` is the
        // dialect-neutral `Checksum::of_ir` over the op list and whose `Ddl` steps'
        // journaled checksums are stamped with that SAME op-list anchor (§5.3 drift
        // anchor — NOT the per-dialect rendered SQL).
        let author = IrAuthor::new(app.clone(), app.clone(), SqlDialect::Postgres);
        let lowered = author
            .load_and_lower_guarded(&bytes, &app, &registry, &live_schema, guard_cfg)
            .map_err(|source| DeployMigrateError::Ir { file: file.clone(), source })?;

        // Fold this file's lowered migrations into the set-level manifest tally.
        ir_lowered_all.extend(lowered.migrations());

        // Route the file's plan through the SINGLE shared plan orchestrator
        // `apply_plan` (§5.2 — realizing the PR0 AppliedPlan/apply_plan plumbing on
        // the IR path), NOT the flat `engine.apply`. `LockMode::Acquire` takes the
        // project advisory lock once for the whole plan; `apply_with_lock_backend`
        // inside re-runs the Confined guard + the destructive/approval gate under
        // `Approval::None`, so a destructive op is refused at deploy exactly like the
        // `.sql` path. For PR1's pure-DDL ops every step is `Ddl` (coalesced into one
        // batch — byte-identical journaling to the pre-fix `engine.apply` path).
        let outcome = engine
            .apply_plan(
                &lowered.plan.steps,
                approval,
                backend,
                exec_cfg,
                "deploy-ir",
                LockMode::Acquire,
            )
            .await
            .map_err(DeployMigrateError::from)?;
        applied.extend(outcome.applied.applied);
        skipped.extend(outcome.applied.skipped);
        // PR7 go-live: a completed online-rename EXPAND surfaces its CONTRACT (C1/C2)
        // as pending — applied in a SUBSEQUENT approved deploy, not this one (§2.0.2).
        pending_contract.extend(
            outcome
                .pending_contract
                .iter()
                .map(|m| m.version.as_str().to_string()),
        );

        // ADVANCE the cross-file registry + live-set with THIS file's freshly-
        // created tables (now applied), so the NEXT `.ir.json` sees them as
        // owned-by-the-deployer + live.
        for t in lowered.created_tables {
            registry.entry(t.clone()).or_insert_with(|| app.clone());
            live_schema.tables.insert(t);
        }
    }

    // SET-LEVEL integrity manifest over the discovered+lowered `.ir.json` set
    // (§8 point 5). Mirrors the `.sql` path's `compute_manifest` traceability log:
    // the IR path is the higher-risk creator/AI-authored surface, so it must emit
    // an equivalent set-level record (a reorder/insert/remove of IR files moves
    // this hash) for incident forensics — and so the H2 trusted-stamp follow-up has
    // an IR-side seam to thread an out-of-band expected hash through (see the
    // `apply_bundle_migrations` H2 note). Traceability only today: there is no
    // trusted build-side stamp to verify against yet, so we never pass an `expected`.
    if !ir_lowered_all.is_empty() {
        let ir_manifest = compute_manifest(&ir_lowered_all);
        tracing::info!(
            app_id = %app_id,
            ir_file_count = ir_files.len(),
            ir_migration_count = ir_lowered_all.len(),
            ir_manifest = %ir_manifest.as_str(),
            "deploy-migrate: computed .ir.json-set integrity manifest (traceability only — \
             no trusted build-side stamp to verify against yet; see H2 follow-up)"
        );
    }

    if !pending_contract.is_empty() {
        tracing::info!(
            app_id = %app_id,
            pending_contract = ?pending_contract,
            "deploy-migrate: online-rename EXPAND completed; CONTRACT (drop old column) is \
             pending a subsequent approved deploy (§2.0.2 cross-deploy expand-contract)"
        );
    }

    Ok(MigrateOutcome { applied, skipped, pending_contract })
}
