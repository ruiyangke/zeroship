//! P7 — the MySQL compile-stub: the **CAPSTONE** of the multi-engine abstraction.
//!
//! This is NOT a working backend. It is a *compile-time proof* that the
//! [`MigrationBackend`](zeroship_migrate::MigrationBackend) trait + its three
//! capability traits ([`MigrationGuard`](zeroship_migrate::MigrationGuard),
//! [`OnlineSchemaChange`](zeroship_migrate::OnlineSchemaChange),
//! [`ShadowDryRun`](zeroship_migrate::ShadowDryRun)) are genuinely general for a
//! THIRD engine — one that is neither Postgres nor SQLite, and crucially one
//! whose DDL **auto-commits per statement** (the InflightMarker class).
//!
//! ## Why a stub, and why this is the whole point
//!
//! If any seam in the trait were secretly Postgres- or SQLite-shaped — a method
//! that takes a `&compio_postgres::Client`, returns a PG-only type, or assumes
//! transactional DDL in a way [`JournalAtomicity::InflightMarker`] doesn't cover
//! — then `MysqlBackend` could not be written without leaking that shape, and
//! THIS FILE WOULD NOT COMPILE. The value is the green build:
//! [`_assert_engine_drives_mysql`] and [`_assert_engine_apply_composes`] below
//! force the trait bound `MysqlBackend: MigrationBackend` and force the generic
//! [`MigrationEngine::apply`](zeroship_migrate::MigrationEngine::apply) /
//! `dry_run` / `apply_declarative` call sites to monomorphize over `MysqlBackend`.
//! A non-general seam fails the bound at compile time, here, never at runtime.
//!
//! The stub is built entirely against the crate's **public** surface (no
//! `pub(crate)` internals), so it doubles as a proof that a backend can be
//! implemented from *outside* the crate.
//!
//! ## Honest method classification (what's gated vs. expressible)
//!
//! Every method body is one of two kinds, never a fake:
//!
//! * **`unimplemented!("MySQL: …")`** — needs a real MySQL connection / the
//!   gated partial-commit recovery. These would be real bodies in a shipping
//!   MySQL backend; the stub cannot honestly fabricate them, so it states *why*
//!   it is gated. They never run (this file's asserts are `cfg(test)` and never
//!   invoked).
//! * **minimal-but-honest** — the body that a REAL MySQL backend would actually
//!   return, because the answer is structural, not connection-bound:
//!   - [`journal_atomicity`] ⇒ [`JournalAtomicity::InflightMarker`] — the TRUE
//!     MySQL answer (DDL auto-commits per statement, so no `BEGIN … COMMIT` can
//!     wrap the `<up>` and its journal row). This is the first backend in the
//!     codebase to return `InflightMarker`, exercising that arm of the enum.
//!   - [`online`] ⇒ `Some(&MysqlOnline)` — MySQL *does* have native online DDL
//!     (`ALGORITHM=INPLACE` / pt-osc), so unlike SQLite it advertises the
//!     capability (the `run_online` body itself is gated).
//!   - [`shadow`] ⇒ `Some(&MysqlShadow)` — a prod/untrusted non-PG engine WOULD
//!     preview untrusted DDL against a throwaway clone (the doc on
//!     [`MigrationBackend::shadow`] explicitly names MySQL as the future
//!     `ShadowDryRun` provider); the `dry_run` bodies are gated.
//!
//! ## KNOWN, EXPECTED gates (documented, not hidden — and NOT abstraction leaks)
//!
//! These are design follow-ups a real MySQL backend needs. They are distinct
//! from a "this trait method can't be MySQL" leak (of which the stub found
//! NONE — see the report):
//!
//! * **(a) `SqlDialect` needs a `MySql` variant.** `dialect()` returns a
//!   [`SqlDialect`](zeroship_schema::query::SqlDialect), a *closed* enum that
//!   today has only `Postgres` / `Sqlite`. This is THE one accepted closed-enum
//!   extension point: a shipping MySQL backend adds `SqlDialect::MySql` to
//!   `zeroship-schema`. The stub does NOT add it now (out of scope / cross-crate
//!   wire surface) and returns a documented placeholder dialect (see
//!   [`MysqlBackend::dialect`]). This is a known gate, not a trait leak — the
//!   trait method signature itself generalizes fine; only the enum it names is
//!   closed.
//! * **(b) Multi-statement partial-commit crash recovery.** `InflightMarker`
//!   routes a non-atomic `<up>` through the two-phase `started → completed`
//!   recovery, which re-runs the `<up>` VERBATIM — correct for a single
//!   non-atomic statement. A MySQL `<up>` with MULTIPLE auto-committing DDL
//!   statements can half-commit on a crash; recovering that needs per-statement
//!   inflight tracking, a gated design pass (called out in the
//!   [`JournalAtomicity::InflightMarker`] variant docs).
//! * **(c) MySQL DDL emission lives in the declarative differ / `zeroship-schema`,
//!   not this trait.** A real MySQL needs differ branches (the closed-enum
//!   dialect lowering) to *produce* MySQL DDL. That is the schema crate's job,
//!   not the backend trait's — so its absence is not a `MigrationBackend` leak.

#![allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use zeroship_migrate::{
    Approval, ApplyError, ApplyOutcome, BackfillSpec, BaselineError, BaselineOutcome,
    ChecksumDriftReport, DesiredSchema, DeclarativeDeployPlan, DriftError, DryRunError,
    DryRunReport, ExecutorConfig, GuardError, GuardOutcome, JournalError, Migration, MigrationGuard,
    MigrationBackend, OnlineError, OnlineIntent, OnlineSchemaChange, PreconditionVerdict,
    RollbackError, SchemaSnapshot, SessionSnapshot, ShadowConfig, ShadowDryRun, SqliteRebuildSpec,
};
use zeroship_migrate::AppliedEntry;
// `JournalAtomicity` / `LockMode` are NOT re-exported at the crate root; the
// crate's modules are `pub`, so name them through their modules.
use zeroship_migrate::backend::JournalAtomicity;
use zeroship_migrate::executor::LockMode;
use zeroship_schema::query::SqlDialect;

// ─────────────────────────────────────────────────────────────────────────────
// The capability-trait stubs (must TYPE-CHECK; bodies are honest-gated).
// ─────────────────────────────────────────────────────────────────────────────

/// MySQL **line-1 guard** — the per-engine line-1 defense for the MySQL dialect.
///
/// A real impl parses MySQL DDL (e.g. via a MySQL-aware parser / allowlist —
/// `libpg_query` cannot parse MySQL, exactly the reason the guard is per-engine)
/// and maps denials onto the neutral [`GuardOutcome`] / [`GuardError`]. The stub
/// proves the seam is parser-agnostic: it implements the trait with a gated body.
#[derive(Debug, Default)]
struct MysqlGuard;

impl MigrationGuard for MysqlGuard {
    fn check(&self, _up: &str) -> Result<GuardOutcome, GuardError> {
        // A shipping MySQL guard runs its own parser + deny-list here and returns
        // the neutral GuardOutcome (destructive flag + advisories). The neutral
        // seam type (GuardOutcome/GuardError) carries no PG/SQLite shape, so this
        // compiles for MySQL with no trait change — proving the guard seam is
        // genuinely engine-agnostic (G1).
        unimplemented!("MySQL: needs a MySQL-aware DDL parser + allowlist (no libpg_query)")
    }
}

/// MySQL **online schema-change** capability — `ALGORITHM=INPLACE` / pt-osc.
///
/// Unlike SQLite (`online() == None`, every change rebuilt offline), MySQL *does*
/// have a native online path, so `MysqlBackend::online()` returns `Some(self)`.
/// The seam takes the NEUTRAL [`OnlineIntent`] (not a PG `ExpandContractPlan` and
/// not a `Client`), which is exactly what lets a MySQL impl lower the *same*
/// intent to `ALTER TABLE … ALGORITHM=INPLACE` instead of PG's PL/pgSQL
/// dual-write trigger — proving the seam is the neutral intent, not the PG plan.
#[derive(Debug, Default)]
struct MysqlOnline;

impl OnlineSchemaChange for MysqlOnline {
    fn run_online<'a>(
        &'a self,
        _intent: &'a OnlineIntent,
        _expand: &'a [Migration],
        _backfill: &'a BackfillSpec,
        _approval: Approval,
        _cfg: &'a ExecutorConfig,
        _applied_by: &'a str,
        _lock_mode: LockMode,
    ) -> Pin<Box<dyn Future<Output = Result<ApplyOutcome, OnlineError>> + 'a>> {
        // A real impl lowers `intent` to MySQL-native online DDL (ALGORITHM=INPLACE,
        // or drives pt-online-schema-change) instead of PG's authored dual-write
        // `expand` steps. The signature already carries the neutral intent + the
        // version-stable expand/backfill instances, so no PG-named type leaks here.
        Box::pin(async move {
            unimplemented!("MySQL: lower OnlineIntent to ALGORITHM=INPLACE / pt-osc")
        })
    }
}

/// MySQL **shadow dry-run** capability — preview untrusted DDL on a throwaway DB.
///
/// MySQL is a prod/untrusted non-PG engine, so (unlike SQLite) it WOULD provide a
/// shadow: the [`MigrationBackend::shadow`] doc names MySQL explicitly as a future
/// provider. `MysqlBackend::shadow()` returns `Some(self)`; the dry-run bodies are
/// gated on a real MySQL admin connection (`CREATE/DROP DATABASE`).
#[derive(Debug, Default)]
struct MysqlShadow;

impl ShadowDryRun for MysqlShadow {
    fn dry_run<'a>(
        &'a self,
        _migrations: &'a [Migration],
        _cfg: &'a ExecutorConfig,
        _shadow_cfg: &'a ShadowConfig,
        _applied_by: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<DryRunReport, DryRunError>> + 'a>> {
        Box::pin(async move {
            unimplemented!("MySQL: CREATE/DROP DATABASE shadow clone needs a MySQL admin connection")
        })
    }

    fn dry_run_declarative<'a>(
        &'a self,
        _plan: &'a DeclarativeDeployPlan,
        _desired: &'a DesiredSchema,
        _cfg: &'a ExecutorConfig,
        _shadow_cfg: &'a ShadowConfig,
        _applied_by: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<DryRunReport, DryRunError>> + 'a>> {
        Box::pin(async move {
            unimplemented!("MySQL: declarative shadow validation needs a MySQL admin connection")
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The MySQL backend stub.
// ─────────────────────────────────────────────────────────────────────────────

/// The MySQL [`MigrationBackend`] **compile-stub** (P7 capstone).
///
/// Holds NO live MySQL connection (a real one would own a hardened MySQL
/// connection, the analog of `PostgresBackend`'s `&Client` /
/// `SqliteBackend`'s actor); the three capability values are owned so `online()`
/// / `shadow()` / `guard()` have something to borrow. Construction is trivial
/// because the stub never connects.
#[derive(Debug, Default)]
struct MysqlBackend {
    guard: MysqlGuard,
    online: MysqlOnline,
    shadow: MysqlShadow,
}

impl MigrationBackend for MysqlBackend {
    fn dialect(&self) -> SqlDialect {
        // KNOWN GATE (a): `SqlDialect` is a CLOSED enum (Postgres | Sqlite) in
        // `zeroship-schema`. A shipping MySQL backend adds `SqlDialect::MySql`
        // there — the one accepted closed-enum extension point. We do NOT add it
        // in this stub (cross-crate wire surface, out of scope). Returning
        // `Postgres` here is a *documented placeholder* purely so the stub
        // type-checks; it is never consulted (the asserts below never run). The
        // trait method signature itself generalizes — only the enum is closed, so
        // this is a known gate, NOT a `MigrationBackend` abstraction leak.
        SqlDialect::Postgres
    }

    fn guard(&self) -> &dyn MigrationGuard {
        &self.guard
    }

    fn journal_atomicity(&self) -> JournalAtomicity {
        // The TRUE MySQL answer (minimal-but-honest, NOT gated): MySQL DDL
        // auto-commits per statement, so no single `BEGIN … COMMIT` can wrap the
        // `<up>` and its journal row. EVERY MySQL migration routes through the
        // two-phase `started → completed` inflight recovery. This is the FIRST
        // backend to return `InflightMarker`, exercising that arm of the enum and
        // proving the executor's atomicity branch is genuinely engine-driven, not
        // PG/SQLite-hardcoded. (Multi-statement partial-commit recovery is the
        // gated follow-up (b) — see the variant docs.)
        JournalAtomicity::InflightMarker
    }

    async fn acquire_project_lock(&self, _project_id: &str) -> Result<(), ApplyError> {
        unimplemented!("MySQL: GET_LOCK() apply-serialization lock on a real MySQL connection")
    }

    async fn release_project_lock(&self, _project_id: &str) -> Result<(), ApplyError> {
        unimplemented!("MySQL: RELEASE_LOCK() on a real MySQL connection")
    }

    async fn snapshot_session(&self) -> Result<SessionSnapshot, ApplyError> {
        // `SessionSnapshot` is dialect-neutral (an owned struct the executor only
        // round-trips back to the backend, never inspects). A MySQL impl populates
        // whatever it needs (or nothing); the neutrality of this type is what makes
        // it generalize — proving the session seam is not PG-GUC-shaped.
        unimplemented!("MySQL: snapshot session vars (lock_wait_timeout, …) on a real connection")
    }

    async fn restore_session(&self, _snap: &SessionSnapshot) -> Result<(), ApplyError> {
        unimplemented!("MySQL: restore session vars from the neutral SessionSnapshot")
    }

    async fn reset_role_best_effort(&self) {
        unimplemented!("MySQL: drop any per-apply privilege confinement, best-effort")
    }

    async fn apply_up_transactional(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
        _applied_by: &str,
        _supersedes: &[&str],
        _kind: &str,
    ) -> Result<(), ApplyError> {
        // NOTE: because `journal_atomicity()` reports `InflightMarker`, the engine
        // routes MySQL through the NON-transactional path, so a correct MySQL
        // backend would generally not reach here. Kept as a gated body for trait
        // completeness; a real impl could still serve a transactional-storage edge.
        unimplemented!("MySQL: DDL auto-commits per statement — InflightMarker routes the non-txn path")
    }

    async fn configure_session_non_txn(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
    ) -> Result<(), ApplyError> {
        unimplemented!("MySQL: pin session vars for the non-txn (auto-commit) apply path")
    }

    async fn apply_up_non_transactional(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
        _applied_by: &str,
        _had_inflight: bool,
        _supersedes: &[&str],
    ) -> Result<bool, ApplyError> {
        // THE primary MySQL apply path (every migration, per `InflightMarker`).
        // Gated body: needs a real connection + the two-phase started→completed
        // marker writes. Multi-statement partial-commit recovery is gate (b).
        unimplemented!(
            "MySQL: two-phase started→completed inflight apply on a real connection \
             (multi-statement partial-commit recovery is gated follow-up (b))"
        )
    }

    async fn rollback_one_transactional(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
        _applied_by: &str,
    ) -> Result<(), RollbackError> {
        unimplemented!("MySQL: run <down> + append rolled_back (auto-commit caveat as for apply)")
    }

    fn validate_non_txn(&self, _m: &Migration) -> Result<(), ApplyError> {
        // The dialect-boundary parse-time idempotency vet. PG parses with
        // `pg_query`; SQLite rejects `transaction:false`; MySQL would enforce its
        // own idempotency rules (e.g. `IF NOT EXISTS` on the auto-committing DDL it
        // re-runs on recovery). Per-engine by construction — no raw `pg_query` in
        // the generic body — so this generalizes.
        unimplemented!("MySQL: enforce idempotency on the auto-committing DDL re-run on recovery")
    }

    async fn ensure_journal(&self, _cfg: &ExecutorConfig) -> Result<(), JournalError> {
        unimplemented!("MySQL: bootstrap the journal + immutability constructs on a real connection")
    }

    async fn applied(&self, _cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        // Returns the dialect-NEUTRAL owned `AppliedEntry` rows (never a
        // `compio_postgres::Row`), exactly the property that lets a MySQL journal
        // read cross this surface unchanged.
        unimplemented!("MySQL: read net-applied journal entries → neutral AppliedEntry rows")
    }

    async fn superseded_versions(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        unimplemented!("MySQL: read superseded versions from the journal")
    }

    async fn latest_completed_checksums(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<HashMap<String, String>, JournalError> {
        unimplemented!("MySQL: read latest completed checksum per identity (repeatable oracle)")
    }

    async fn check_checksum_drift(
        &self,
        _cfg: &ExecutorConfig,
        _migrations: &[Migration],
    ) -> Result<ChecksumDriftReport, DriftError> {
        // The comparison is dialect-agnostic; only the underlying journal read is
        // dialect-coupled (hence behind the trait). A MySQL impl reads the journal
        // and reuses the generic `ChecksumDriftReport` shape.
        unimplemented!("MySQL: read journal for the (dialect-agnostic) checksum-drift comparison")
    }

    async fn snapshot_schema(&self, _cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
        // PG introspects `information_schema`/`pg_catalog`; SQLite `sqlite_master`
        // + PRAGMAs; MySQL would introspect `information_schema` (MySQL flavor) into
        // the SAME neutral `SchemaSnapshot`. Neutral result type ⇒ generalizes.
        unimplemented!("MySQL: introspect information_schema (MySQL) → neutral SchemaSnapshot")
    }

    async fn evaluate_preconditions(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
    ) -> Result<PreconditionVerdict, ApplyError> {
        unimplemented!("MySQL: evaluate preconditions read-only on a real connection")
    }

    async fn record_squash(
        &self,
        _cfg: &ExecutorConfig,
        _squash_migration: &Migration,
        _applied_by: &str,
        _supersedes: &[&str],
    ) -> Result<(), ApplyError> {
        unimplemented!("MySQL: write a completed kind='squash' row + supersession edges")
    }

    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        _m: &Migration,
        _applied_by: &str,
    ) -> Result<(), ApplyError> {
        // A `SqliteRebuild` is produced ONLY by the SQLite differ. MySQL has native
        // `ALTER` (and online DDL), so it NEVER produces a rebuild — a rebuild
        // reaching the MySQL backend is a routing bug, surfaced as a clear error
        // (same fail-closed posture as `PostgresBackend::rebuild_one`). This is
        // expressible WITHOUT a connection, so it is a real (non-gated) body:
        Err(ApplyError::Backend(format!(
            "mysql backend: SQLite table rebuild requested for '{}' — the MySQL differ \
             never produces rebuilds (routing bug)",
            spec.table
        )))
    }

    fn online(&self) -> Option<&dyn OnlineSchemaChange> {
        // MySQL HAS a native online path (ALGORITHM=INPLACE / pt-osc), so it
        // advertises the capability — unlike SQLite's `None`. (The `run_online`
        // body is gated on a real connection.)
        Some(&self.online)
    }

    fn shadow(&self) -> Option<&dyn ShadowDryRun> {
        // MySQL is a prod/untrusted non-PG engine, so it WOULD preview untrusted
        // DDL against a throwaway shadow DB (the `shadow` doc names MySQL as the
        // future provider). Advertise the capability; the dry-run bodies are gated.
        Some(&self.shadow)
    }

    async fn baseline_one(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
        _applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError> {
        // The neutral baseline entry point. PG runs the baseline `up` through its
        // guard + advisory lock; MySQL would do the analogous first-entry-only
        // `kind='baseline'` journal write, mapping any internal failure onto the
        // neutral `BaselineError::Backend(String)` arm — which is precisely the
        // dialect-neutral escape arm that makes this generalize off PG.
        unimplemented!("MySQL: first-entry-only kind='baseline' journal write on a real connection")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// THE compile-time assertions — the capstone proof.
//
// None of these run. They exist so the COMPILER monomorphizes the generic
// engine over `MysqlBackend`. A non-general seam (a PG-typed method, a
// transactional-DDL assumption InflightMarker can't cover, a `&Client` /
// closed-`SqlDialect` leak) would fail the `B: MigrationBackend` bound HERE.
// ─────────────────────────────────────────────────────────────────────────────

/// Proves `MysqlBackend: MigrationBackend` — the trait bound itself holds.
fn _assert_mysql_is_a_backend() {
    fn drives<B: MigrationBackend>() {}
    drives::<MysqlBackend>();
}

/// Proves the capability traits are object-safe AND implemented for the MySQL
/// types (the `&dyn` returns in `online()` / `shadow()` / `guard()` type-check).
fn _assert_capabilities_compose(b: &MysqlBackend) {
    let _g: &dyn MigrationGuard = b.guard();
    let _o: Option<&dyn OnlineSchemaChange> = b.online();
    let _s: Option<&dyn ShadowDryRun> = b.shadow();
    // Direct object-safety check of each capability over the MySQL impl types.
    let mg = MysqlGuard;
    let mo = MysqlOnline;
    let ms = MysqlShadow;
    let _mg: &dyn MigrationGuard = &mg;
    let _mo: &dyn OnlineSchemaChange = &mo;
    let _ms: &dyn ShadowDryRun = &ms;
}

/// Proves the generic `MigrationEngine` *drives* `MysqlBackend` — the apply /
/// dry-run / declarative call sites monomorphize over it. This is the seam that
/// catches a non-general engine body at compile time.
///
/// The body is `unreachable` at runtime (it constructs no real plan / config and
/// is never called); only its *type-checking* matters. We name the generic
/// methods through fully-qualified turbofish so the bound is forced even though
/// the function never executes.
#[allow(unreachable_code, unused_variables, clippy::diverging_sub_expression)]
fn _assert_engine_apply_composes() {
    // Never constructed at runtime — these `unimplemented!()` placeholders only
    // need to TYPE-CHECK so the generic `apply::<MysqlBackend>` monomorphizes.
    let engine: zeroship_migrate::MigrationEngine = unimplemented!();
    let plan: zeroship_migrate::MigrationPlan = unimplemented!();
    let backend: MysqlBackend = unimplemented!();
    let cfg: ExecutorConfig = unimplemented!();
    let approval: Approval = unimplemented!();

    // The apply hot path generic over the backend — the core capstone call site.
    let _apply = engine.apply::<MysqlBackend>(&plan, approval, &backend, &cfg, "ci");
    // The dry-run path (routes through `backend.shadow()`).
    let migrations: Vec<Migration> = unimplemented!();
    let shadow_cfg: ShadowConfig = unimplemented!();
    let _dry = engine.dry_run::<MysqlBackend>(&backend, &migrations, &cfg, &shadow_cfg, "ci");
    // The declarative deploy path (routes through `rebuild_one` / `online()`).
    let dplan: DeclarativeDeployPlan = unimplemented!();
    let _decl = engine.apply_declarative::<MysqlBackend>(&dplan, approval, &backend, &cfg, "ci");
}

/// A real (runnable) sanity test: the two non-gated, connection-free method
/// bodies behave. Proves the stub is honest where it claims to be expressible,
/// and that `InflightMarker` is the reported atomicity.
#[compio::test]
async fn mysql_stub_non_gated_bodies_are_honest() {
    let b = MysqlBackend::default();

    // (1) journal_atomicity() is the TRUE structural MySQL answer.
    assert_eq!(
        b.journal_atomicity(),
        JournalAtomicity::InflightMarker,
        "MySQL DDL auto-commits per statement ⇒ InflightMarker (first backend to use this arm)"
    );

    // (2) online()/shadow() advertise the MySQL capabilities (Some, not None).
    assert!(b.online().is_some(), "MySQL has native online DDL (ALGORITHM=INPLACE/pt-osc)");
    assert!(b.shadow().is_some(), "MySQL is a prod/untrusted engine ⇒ shadow dry-run");

    // (3) rebuild_one is fail-closed (no connection needed): a SqliteRebuild
    //     reaching the MySQL backend is a routing bug, surfaced as a clear error.
    let spec = SqliteRebuildSpec {
        table: "widgets".to_string(),
        tmp_table: SqliteRebuildSpec::tmp_name("widgets"),
        new_table_create: "CREATE TABLE widgets__zsrebuild (id integer)".to_string(),
        copy_columns: vec![("id".to_string(), "id".to_string())],
        recreate_objects: Vec::new(),
        reason: "typecheck-only spec".to_string(),
    };
    let outcome = b.rebuild_one(&spec, &mig_stub(), "ci").await;
    match outcome {
        Err(ApplyError::Backend(msg)) => {
            assert!(msg.contains("widgets"), "error must name the table: {msg}");
            assert!(msg.contains("routing bug"), "must flag the routing bug: {msg}");
        }
        other => panic!("MySQL rebuild_one must fail-closed, got {other:?}"),
    }
}

/// Minimal `Migration` for the runnable rebuild test (the only spot that needs a
/// real instance). Built via the crate's public field + checksum surface — the
/// `rebuild_one` body ignores `m` (it fails closed before touching it), so the
/// content only needs to be a well-formed instance.
fn mig_stub() -> Migration {
    use zeroship_migrate::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
    let flags = MigrationFlags::default();
    let up = "-- typecheck-only".to_string();
    let checksum = Checksum::of(&ChecksumInput {
        up: &up,
        down: None,
        flags: &flags,
        owner_app: "app_typecheck",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version: MigrationId::generate(),
        name: "mysql_stub".to_string(),
        up,
        down: None,
        checksum,
        flags,
        owner_app: "app_typecheck".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }
}
