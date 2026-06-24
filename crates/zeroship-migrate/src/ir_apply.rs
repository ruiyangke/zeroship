//! **PR7 online-rename go-live (SQLite leg).** The deploy/dev entry point that
//! applies a bundle's `.ir.json` creator artifacts against a **SQLite** backend —
//! the SQLite peer of control's PG `apply_bundle_ir_migrations`
//! (`crates/control/src/deploy_migrate.rs`).
//!
//! Before PR7 there was NO production/dev path that built a SQLite-dialect
//! [`LiveSchema`] (the `table_snapshots` + `sqlite_schemas` SDK-`Value` facts a
//! SQLite `renameColumn` rebuild needs), so a SQLite-targeted IR rename was
//! engine-proven (PR2 unit + temp-file e2e) but never go-live-wired: it failed
//! closed before lowering. This module wires it.
//!
//! The SDK schema `Value` the SQLite rebuild renders the post-rename `CREATE TABLE`
//! from is NOT recoverable from a raw `sqlite_master` introspection (the
//! mask/encryption/ref facets live only in the SDK schema, not the catalog). The
//! authoritative source is a descriptor set, threaded in by the caller;
//! [`LiveSchema::for_sqlite_descriptors`] routes it through the SAME
//! `desired_snapshot_for_dialect` the differ uses, so the live facts the rename
//! rebuild consumes are byte-identical to a `t.*`-diff's.
//!
//! TWO ROLES, OPPOSITE STATE (see the DESCRIPTOR-SET CONTRACT note in
//! [`apply_bundle_ir_sqlite`]): the descriptor set is the union END-STATE for the
//! ownership/FK-inline registry, but a `renameColumn` rebuild needs the PRE-rename
//! LIVE column facts (the `from` column must be present). A descriptor set derived
//! from the app's registerModel = the POST-deploy DESIRED schema (post-rename `from`
//! is gone) makes a rename FAIL CLOSED (no data loss) but un-runnable.
//!
//! PRODUCTION-WIRING TODO (the SQLite go-live wave): a production caller naturally
//! derives descriptors from registerModel (the post-deploy desired schema), so it
//! MUST source the rename's PRE-rename column facts from a real pre-deploy
//! SQLite-catalog/snapshot read (the SDK-`Value` facets reconstructed at deploy
//! start), OR carry the pre-rename column in the IR — NOT from the post-deploy
//! desired descriptor set. Until wired, this surface is test-only (no production
//! caller; pinned by the control interlock regression test).
//!
//! Every `.ir.json` runs through the SAME fail-closed load + guard-per-fragment
//! lower gate (`IrAuthor::load_and_lower_guarded`) the PG path uses, then is applied
//! through the single shared `apply_plan` — so the SQLite leg inherits the IR-load
//! ownership/version/checksum-hint defenses and the per-op guard, identically.

use std::collections::BTreeMap;
use std::path::Path;

use crate::declarative::CollectionDescriptor;
use crate::executor::LockMode;
use crate::ir_author::{IrAuthor, LiveSchema, LoadAndLowerGuardedError};
use crate::{
    Approval, DeclarativeApplyError, DeclarativeError, GuardConfig, MigrationEngine, SqlDialect,
    SqliteBackend,
};

/// What a SQLite IR apply produced.
#[derive(Debug, Clone, Default)]
pub struct SqliteIrApplyOutcome {
    /// Migration version ids applied this run.
    pub applied: Vec<String>,
    /// Migration version ids skipped (already journaled).
    pub skipped: Vec<String>,
}

/// A failure applying a bundle's `.ir.json` set against a SQLite backend.
#[derive(Debug, thiserror::Error)]
pub enum SqliteIrApplyError {
    /// Reading the migrations directory / a `.ir.json` file failed.
    #[error("read IR file ({file}): {message}")]
    Read {
        /// The path/filename.
        file: String,
        /// The I/O error.
        message: String,
    },
    /// Building the SQLite-dialect live schema from the descriptor set failed (an
    /// invalid field/type token rejected at the author boundary).
    #[error("build SQLite live schema: {0}")]
    LiveSchema(#[source] DeclarativeError),
    /// A `.ir.json` failed the fail-closed LOAD GATE or its guard-per-fragment lower
    /// (malformed, future `ir_version`, ownership / structural reject, a guard-denied
    /// fragment with op-index attribution). A creator fault.
    #[error("IR load/guarded-lower ({file}): {source}")]
    Ir {
        /// The `.ir.json` filename.
        file: String,
        /// The fail-closed gate / guard error.
        #[source]
        source: LoadAndLowerGuardedError,
    },
    /// The engine refused or failed the apply (a guard denial, a destructive op /
    /// approval-gated rebuild without approval, checksum drift, or a mid-apply DB
    /// error).
    #[error("apply: {0}")]
    Apply(#[source] DeclarativeApplyError),
}

/// Apply a bundle's `.ir.json` creator artifacts against a **SQLite** backend
/// (§2.6.2 SQLite leg). Discovers `*.ir.json` files in `migrations_dir`
/// (version-ordered by filename), builds the SQLite-dialect [`LiveSchema`] from the
/// app's `descriptors`, then for each file runs the fail-closed load + guard lower
/// (`IrAuthor::load_and_lower_guarded`, SQLite dialect) and applies the resulting
/// plan through the single shared `apply_plan`.
///
/// A SQLite `renameColumn` lowers to an OFFLINE 12-step rebuild
/// (`RenameStep::SqliteRebuild`), applied via `MigrationBackend::rebuild_one` — there
/// is NO `pending_contract` partition on the SQLite leg (that is PG-only). A rebuild
/// on a populated table is destructive, so a rename apply requires
/// `Approval::Approved`; the routine `Approval::None` path refuses it before any DDL.
///
/// `project_schema` is the project id (the SQLite `main`-unqualified emit ignores
/// the qualifier but the shared emitter still validates it). `owner_app` stamps the
/// live-table ownership the differ's cross-app rebuild guard consults — every live
/// table in the app's descriptor set is owned by the deploying app.
///
/// An empty / IR-free directory is a clean no-op.
///
/// # Errors
/// [`SqliteIrApplyError`] on I/O / live-schema / fail-closed gate / apply failure.
pub async fn apply_bundle_ir_sqlite(
    backend: &SqliteBackend,
    project_schema: &str,
    owner_app: &str,
    descriptors: &[CollectionDescriptor],
    migrations_dir: &Path,
    exec_cfg: &crate::ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
) -> Result<SqliteIrApplyOutcome, SqliteIrApplyError> {
    // Discover `*.ir.json`, version-ordered by filename (deterministic).
    let mut ir_files: Vec<std::path::PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| SqliteIrApplyError::Read {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| SqliteIrApplyError::Read {
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
        return Ok(SqliteIrApplyOutcome::default());
    }
    ir_files.sort();

    // Build the SQLite-dialect live facts (table_snapshots + sqlite_schemas +
    // unique_indexes + ownership) from the app's descriptor set. This is the SQLite
    // analogue of the PG path's live introspection — the SDK schema `Value`s the
    // rename rebuild needs come from the descriptor set, not a raw catalog read.
    //
    // DESCRIPTOR-SET CONTRACT (the SQLite live-facts model). Unlike the PG path,
    // which introspects the REAL catalog (`backend.snapshot_schema`) and so naturally
    // observes intermediate states, the SQLite leg cannot re-introspect the structural
    // rebuild facts (`table_snapshots`/`sqlite_schemas` — the SDK-`Value` facets a
    // `sqlite_master` read can't recover). The descriptor set plays TWO DISTINCT roles
    // here, and they have OPPOSITE state requirements — do not conflate them:
    //
    //   (A) OWNERSHIP / FK-INLINE REGISTRY — the union END-STATE. Every table any file
    //       in this directory creates or references is present in the descriptor set
    //       (the union of all member apps' registerModel schemas), so the ownership +
    //       inline-FK registry is complete from the start and needs NO per-file advance
    //       (unlike the PG path, which discovers tables per file from a live catalog).
    //
    //   (B) RENAME COLUMN FACTS — the PRE-rename LIVE shape. A SQLite `renameColumn`
    //       lowers to a 12-step REBUILD that reads the live `from` column off
    //       `table_snapshots`/`sqlite_schemas` to author the post-rename CREATE +
    //       value-copy. So for a rename, the descriptor set MUST carry the table's
    //       PRE-rename column facts (the `from` column must be PRESENT). A descriptor
    //       set derived from the app's registerModel = the POST-deploy DESIRED schema
    //       (post-rename: only `to` exists, `from` is gone) makes the rebuild author
    //       FAIL CLOSED (`RenameNeedsLiveColumn` / `SqliteRenameNeedsLiveTable`) — no
    //       data loss, but the rename is un-runnable from a post-rename descriptor set.
    //       The production SQLite wiring wave (see the module-level TODO) MUST source
    //       these PRE-rename column facts from a real pre-deploy SQLite-catalog/snapshot
    //       read (the SDK-Value facets reconstructed at deploy start), OR carry the
    //       pre-rename column in the IR — NOT from the post-deploy desired descriptor set.
    //
    // Consequently a multi-file deploy whose LATER file depends on an INTERMEDIATE
    // structural state produced by an EARLIER file in the SAME directory is NOT
    // supported: the rebuild author sees the (single, fixed) descriptor shape, not the
    // post-earlier-op shape, and FAILS CLOSED rather than emit a wrong rebuild. (Pinned
    // fail-closed by the e2e tests
    // `deploy_multi_file_intermediate_rename_fails_closed_on_real_sqlite` and
    // `deploy_post_rename_descriptor_set_fails_closed_on_real_sqlite`.)
    let live_schema =
        LiveSchema::for_sqlite_descriptors(project_schema, owner_app, descriptors)
            .map_err(SqliteIrApplyError::LiveSchema)?;
    // The ownership registry the IR-load gate enforces: every table in the app's
    // descriptor set (the END-STATE union of all member apps' schemas) is owned by the
    // deploying app. Because the descriptor set IS the union end-state, every table any
    // file in this directory creates or references is ALREADY present here — there is
    // nothing to advance per file (unlike the PG path, which starts from a live catalog
    // introspection and discovers per-file). A file that creates a table absent from
    // the descriptor set therefore has no owner here and fails closed at the IR-load
    // ownership gate — the descriptor set is the single authoritative ownership source.
    let registry: BTreeMap<String, String> = descriptors
        .iter()
        .map(|d| (d.name.clone(), owner_app.to_string()))
        .collect();

    let engine = MigrationEngine::new();
    let mut applied: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for path in &ir_files {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let bytes = std::fs::read_to_string(path).map_err(|e| SqliteIrApplyError::Read {
            file: file.clone(),
            message: e.to_string(),
        })?;

        let author = IrAuthor::new(project_schema, owner_app, SqlDialect::Sqlite);
        let lowered = author
            .load_and_lower_guarded(&bytes, owner_app, &registry, &live_schema, guard_cfg)
            .map_err(|source| SqliteIrApplyError::Ir { file: file.clone(), source })?;

        let outcome = engine
            .apply_plan(
                &lowered.plan.steps,
                approval,
                backend,
                exec_cfg,
                "deploy-ir-sqlite",
                LockMode::Acquire,
            )
            .await
            .map_err(SqliteIrApplyError::Apply)?;
        applied.extend(outcome.applied.applied);
        skipped.extend(outcome.applied.skipped);
        // No per-file registry/live-set advance (unlike the PG path): the descriptor
        // set is the END-STATE union, so all tables are present from the start. See the
        // DESCRIPTOR-SET CONTRACT note above — the structural rebuild facts are
        // descriptor-pinned and a deploy needing an intermediate structural state
        // produced by an earlier file FAILS CLOSED, never silently emits a wrong rebuild.
    }

    Ok(SqliteIrApplyOutcome { applied, skipped })
}
