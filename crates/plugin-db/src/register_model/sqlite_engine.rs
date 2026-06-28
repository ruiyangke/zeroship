//! The dev-tier SQLite `registerModel` arm — drives the security-hardened
//! `zeroship-migrate` engine instead of the retired bespoke `run_sqlite_pipeline`
//! (SQLite-engine wiring design, Option A / §7b).
//!
//! # Why this exists
//!
//! Before P6b the SQLite dev tier ran a stateless, additive-only,
//! journal-less diff (`run_sqlite_pipeline` + `apply_sqlite`). That path
//! **silently skipped destructive ops** and had no versioning / journal /
//! rollback / drift / 12-step rebuild. The engine (P1–P6a) is the
//! security-first migrator the platform builds and runs everywhere; P6b wires
//! the dev tier into it so dev gains a real `_mig` journal, destructive ops
//! that *actually apply* (auto-approved on the operator's own local file), and
//! the 12-step rebuild for type/rename changes.
//!
//! # The two-connection coordination (the crux — §7b)
//!
//! plugin-db's data-plane backend **A** (`crate::backend::sqlite::SqliteBackend`)
//! is CDC-armed and intentionally *un*-hardened for CRUD throughput. The
//! migration backend **B** (`zeroship_migrate::SqliteBackend`) is the hardened
//! actor (authorizer line-2 deny-list, journal immutability, ATTACH isolation).
//! DDL MUST run on B (the security invariant). Both touch the SAME app file
//! `zs-<app_id>.sqlite`, so we sequence a **single-owner window**:
//!
//! ```text
//! run_sqlite_via_engine(app_id, …):
//!   1. open B on (db_dir/zs-<app>.sqlite, …migrations.sqlite)
//!   2. ensure journal + baseline-if-needed (H3 adoption)
//!   3. plan_declarative(Sqlite) + apply via the engine  ← B owns the file
//!   4. drop B  (releases B's main+_mig handles)
//!   5. A.ensure_app_schema  → ATTACH zs-<app>.sqlite     ← A opens AFTER B is gone
//!   6. A.invalidate_cdc_name_cache(...) for changed collections (CDC bridge)
//! ```
//!
//! Because A opens the app file AFTER B closes, A's connection-level SQLite
//! caches (schema cookie, prepared statements) start clean — the cross-
//! connection staleness question is dissolved, not patched. Only A's
//! application-level CDC name cache needs the bridge (step 6).
//!
//! **The ordering barrier (C3):** all six steps run inside this awaited body,
//! which runs inside `exec_register_model`, whose `await` the
//! `register_model_dispatch` spawned op holds. The `installSchema` `ready`
//! promise resolves ONLY after this returns `Ok(())` — and no creator CRUD runs
//! until the dispatcher awaits that promise. So B-done + A-re-ATTACH + CDC-bridge
//! are all ordered-before the first `env.db.<coll>.find()`. This is the
//! load-bearing constraint: steps 4–6 MUST stay inside this awaited body, never
//! spawned/detached.
//!
//! # Dev auto-approve (structurally safe in prod)
//!
//! A rebuild on a populated table is destructive; the engine refuses it without
//! `Approval::Approved`. On the dev tier the operator owns the local file, so we
//! pass `Approval::Approved` — a developer's `DROP COLUMN` / type-narrow in
//! `schema.ts` just applies (data preserved per the 12-step rebuild), contrast
//! the old silent-skip. This is structurally safe in prod: the worker
//! hard-aborts on a SQLite DSN (P6b-1), so this whole arm is unreachable there.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;
use zeroship_migrate::apply::backend::MigrationBackend;
use zeroship_migrate::apply::backend::sqlite::SqliteBackend as MigrateBackend;
use zeroship_migrate::render::declarative::{
    CollectionDescriptor, FieldDescriptor, IndexDescriptor,
};
use zeroship_migrate::{
    desired_snapshot, Approval, Checksum, ChecksumInput, DeclarativeApplyError, DeclarativeAuthor,
    ExecutorConfig, GuardConfig, Migration, MigrationEngine, MigrationFlags, MigrationId,
};
use zeroship_schema::query::SqlDialect;

use crate::backend::NamespaceManager;
use crate::backend::SqliteBackend as DataBackend;
use crate::error::DbError;

/// Drive the dev-tier SQLite schema apply through the hardened migration engine.
///
/// `backend_a` is the data-plane backend (the one held in the per-isolate
/// context). `app_id` is the dev app (`"default"`). `collection` + `schema` +
/// `indexes` are this `registerModel` call's declared shape. `other_schemas` is
/// every OTHER collection already registered for this app on this isolate (read
/// from the per-isolate cache) so the engine sees the FULL project union as
/// `desired` and never authors a phantom DROP of a sibling table.
///
/// Returns `Ok(())` after the apply has committed on B, B has been dropped, A
/// has re-ATTACHed the app file, and A's CDC name cache has been invalidated for
/// the changed collections — all inside this awaited body (the barrier).
pub(crate) async fn run_sqlite_via_engine(
    backend_a: &DataBackend,
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    declared_collections: &[String],
) -> Result<(), DbError> {
    // Parse-time policy: cross-app FK rejection runs on BOTH backends (platform
    // policy, not a SQLite limitation) — same as the old pipeline's first step.
    crate::cross_app_fk::reject_cross_app_fk(schema, app_id)?;

    // M1/M2 — construct B's paths from A's db_dir() + the app_id mapping A uses,
    // so the two can NEVER diverge on which file is the app's (§7b.1):
    //   app_path     = <db_dir>/zs-<app_id>.sqlite
    //   journal_path = <db_dir>/zs-<app_id>.migrations.sqlite
    let db_dir = backend_a.db_dir().to_path_buf();
    let app_path = db_dir.join(format!("zs-{app_id}.sqlite"));
    let journal_path = db_dir.join(format!("zs-{app_id}.migrations.sqlite"));

    // Build the desired set for THIS register: the current collection PLUS every
    // sibling already registered on this isolate (parent-first topo order means
    // FK targets are already present). The differ then authors only the additive
    // ops for the new/changed collection; the siblings (desired == live) diff to
    // nothing.
    //
    // H1 — this union is necessarily PARTIAL on a warm multi-collection file: a
    // fresh isolate registers collections one-at-a-time (install-schema.ts), so
    // when the FIRST collection registers the sibling cache is empty and a live
    // sibling table (already in the file from a prior isolate) is absent from
    // `desired`. The desired-side `ownership` map (only desired tables) would then
    // carry NO entry for that live sibling, and the differ's fail-closed drop pass
    // would raise `DropOfUnownedTable` (availability bug — the app breaks on every
    // warm boot of any 2+-collection schema).
    let descriptors =
        build_union_descriptors(app_id, collection, schema, indexes, &other_schemas(app_id))?;

    // The desired snapshot.
    let desired = desired_snapshot(app_id, &descriptors)
        .map_err(|e| DbError::internal(format!("sqlite engine: desired_snapshot failed: {e}")))?;

    let engine = MigrationEngine::new();
    // SQLite ignores the schema/lock strings (single-actor; journal in `_mig`);
    // the engine still needs a config to thread. project_id == app_id is inert.
    let exec_cfg = ExecutorConfig::new(app_id, app_id);
    let guard_cfg = GuardConfig::confined_sqlite(app_id);
    let author = DeclarativeAuthor::new_for_dialect(app_id, app_id, SqlDialect::Sqlite);
    let deploy_id =
        std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string());

    // -- Step 1: open B (hardened migration actor) on the app file. -----------
    // B is the creator-of-record for a FRESH file and the opener-of-existing for
    // a WARM file. A's ensure_app_schema (step 5) then only ATTACHes. This is the
    // FIRST thing to touch zs-<app>.sqlite in this register pass.
    let backend_b = MigrateBackend::open(&app_path, &journal_path).map_err(|e| {
        DbError::internal(format!("sqlite engine: open hardened migration backend: {e}"))
    })?;

    // Compute everything that needs B (journal + baseline + plan + apply) inside a
    // scope, then DROP B before A touches the file (the single-owner window).
    let plan_changed: HashSet<String> = {
        // -- Step 2: ensure the `_mig` journal + baseline an existing journal-less
        //    file (H3 adoption) so the first engine boot against a warm
        //    run_sqlite_pipeline file does NOT drift-abort or re-create tables.
        backend_b
            .ensure_journal_sqlite()
            .await
            .map_err(|e| DbError::internal(format!("sqlite engine: ensure journal: {e}")))?;
        maybe_baseline(&backend_b, &exec_cfg, app_id, &deploy_id).await?;

        // -- Step 3: plan the descriptor diff vs live introspection, then apply. --
        let mut live = backend_b.snapshot_schema_sqlite().await.map_err(|e| {
            DbError::internal(format!("sqlite engine: live introspection failed: {e}"))
        })?;

        // H1 — reconcile the PARTIAL per-collection `desired` against `live` so the
        // diff never (a) fails-closed nor (b) phantom-DROPs a sibling that's merely
        // not-yet-registered on this isolate. A live table is one of three kinds:
        //
        //   * in `desired`   → diffed normally (create / add-column / rebuild).
        //   * NOT in desired, but its name IS in the FULL declared set
        //     (`declared_collections`) → a sibling that WILL register later this
        //     boot; it is NOT removed. Hide it from the diff so it is neither a
        //     drop candidate nor (it isn't in desired) a create — a pure no-op.
        //   * NOT in desired AND NOT declared anywhere → genuinely removed from the
        //     app's schema → a real drop candidate; leave it visible so the engine
        //     authors the (owned) drop.
        //
        // This keeps the drop pass DECLARED-SET-driven (the full union), not driven
        // by whichever partial per-collection union happens to be in flight — so a
        // warm 2+-collection boot is clean AND a real removal still drops, without
        // ever dropping a table merely absent from the current partial union.
        let declared_set: HashSet<&str> =
            declared_collections.iter().map(String::as_str).collect();
        live.tables.retain(|table, _| {
            desired.snapshot.tables.contains_key(table) || !declared_set.contains(table.as_str())
        });

        // `live_ownership` MUST carry an entry for EVERY live table the diff can
        // see (the fail-closed guard refuses to drop a table whose owner it cannot
        // confirm). On the dev tier ALL tables in the app file belong to the single
        // dev app, so map every (post-retain) live table → `app_id`. A genuine drop
        // candidate then resolves to owner == deploying_app and is authored; the
        // guard never misfires on a sibling because siblings were retained-out.
        let live_ownership: std::collections::HashMap<String, String> = live
            .tables
            .keys()
            .map(|t| (t.clone(), app_id.to_string()))
            .collect();

        let plan = engine
            .plan_declarative(&desired, &live, &live_ownership, &author, &[], &guard_cfg)
            .map_err(|e| DbError::internal(format!("sqlite engine: plan_declarative failed: {e}")))?;

        // The set of collections whose column shape changed — for the CDC bridge
        // (§7b.4). The plain plan for this register touches only `collection`'s
        // table (create / add-column); each rebuild rewrites its table (new
        // columns to the CDC decoder). `renames` is ALWAYS empty on SQLite (H1).
        let mut changed: HashSet<String> = HashSet::new();
        if !plan.plain.items.is_empty() {
            changed.insert(collection.to_string());
        }
        for rebuild in &plan.rebuilds {
            changed.insert(rebuild.spec.table.clone());
        }

        // Dev auto-approve: the operator's own local file. A rebuild (destructive)
        // applies rather than being refused/silent-skipped. Structurally safe in
        // prod (the worker hard-aborts on a SQLite DSN, so this is unreachable).
        engine
            .apply_declarative(&plan, Approval::Approved, &backend_b, &exec_cfg, &deploy_id)
            .await
            .map_err(map_apply_err)?;

        changed
    };

    // -- Step 4: drop B — releases B's main + `_mig` handles. The empirical probe
    //    (1000 iters, 0 failures) proved A can then ATTACH the app file cleanly
    //    on a different connection in the same process (the POSIX
    //    close()-drops-all-locks footgun does not bite this sequencing).
    drop(backend_b);

    // -- Step 5: A opens the app file (ATTACH) AFTER B is gone (single-owner
    //    window). On a fresh file B already created it; A only ATTACHes here.
    backend_a.ensure_app_schema(app_id).await?;

    // -- Step 6: bridge the CDC name-cache invalidation to A for every collection
    //    the engine plan changed. A's publisher re-reads column names before
    //    decoding the next CDC event for those (app, collection) pairs.
    for coll in &plan_changed {
        backend_a.invalidate_cdc_name_cache(app_id, coll);
    }

    Ok(())
}

/// Read every OTHER collection's declared schema already cached on this isolate
/// for the dev app (`"default"`). Returns `(collection, schema_json)` pairs.
///
/// These were stamped by `cache_schema` on each prior `registerModel`'s
/// `Ok(())`. registerModel runs parent-first (topo order), so every FK target
/// of the collection being registered is already in this set — which is why a
/// single-collection desired would otherwise fail the differ's FK-target check,
/// and a sibling absent from `desired` would be a phantom DROP candidate. We
/// fold them into the union so the diff is faithful and additive-only for the
/// sibling tables (desired == live ⇒ no ops).
fn other_schemas(app_id: &str) -> Vec<(String, Value)> {
    crate::context::with(|c| c.cached_schemas_for_app(app_id))
}

/// Baseline an existing journal-less app file (H3) before planning. If the
/// journal is EMPTY but the app file already has user tables (the
/// run_sqlite_pipeline legacy shape), record the live schema as a `baseline`
/// journal entry WITHOUT running its `up`, so the first engine boot adopts the
/// schema rather than drift-aborting. A fresh file (no tables) skips baseline.
async fn maybe_baseline(
    backend_b: &MigrateBackend,
    exec_cfg: &ExecutorConfig,
    app_id: &str,
    deploy_id: &str,
) -> Result<(), DbError> {
    let applied = backend_b
        .applied_sqlite()
        .await
        .map_err(|e| DbError::internal(format!("sqlite engine: read journal: {e}")))?;
    if !applied.is_empty() {
        // Already engine-managed (or already baselined) — nothing to adopt.
        return Ok(());
    }
    let live = backend_b
        .snapshot_schema_sqlite()
        .await
        .map_err(|e| DbError::internal(format!("sqlite engine: baseline introspection: {e}")))?;
    if live.tables.is_empty() {
        // Fresh file — no schema to adopt; the first apply creates everything.
        return Ok(());
    }

    // Record the live schema as the baseline. The `up` documents the adopted
    // shape (a CREATE-comment is enough — it is recorded, NOT run). The version
    // is freshly minted; the checksum certifies the baseline.
    let up = format!(
        "-- baseline: adopted {} existing table(s) from a pre-engine dev file",
        live.tables.len()
    );
    let flags = MigrationFlags::default();
    let mut m = Migration {
        version: MigrationId::generate(),
        name: "dev_baseline_adopt".to_string(),
        up: up.clone(),
        down: None,
        checksum: Checksum::of(&ChecksumInput {
            up: &up,
            down: None,
            flags: &flags,
            owner_app: app_id,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags,
        owner_app: app_id.to_string(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        existence_guard: None,
    };
    m.recompute_checksum();
    backend_b
        .baseline_one(exec_cfg, &m, deploy_id)
        .await
        .map_err(|e| DbError::internal(format!("sqlite engine: baseline adopt failed: {e}")))?;
    Ok(())
}

/// Map a declarative-apply failure onto a typed `DbError` for the JS error rail.
fn map_apply_err(e: DeclarativeApplyError) -> DbError {
    DbError::internal(format!("sqlite engine: declarative apply failed: {e}"))
}

/// Build the full project-union descriptor set the engine diffs: the current
/// collection PLUS every sibling already registered on this isolate.
fn build_union_descriptors(
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
    others: &[(String, Value)],
) -> Result<Vec<CollectionDescriptor>, DbError> {
    let mut by_name: BTreeMap<String, CollectionDescriptor> = BTreeMap::new();
    // Siblings first; the current collection overrides any stale cached copy of
    // itself (it must reflect THIS register's declared shape).
    for (name, sib_schema) in others {
        if name == collection {
            continue;
        }
        // A sibling carries no separately-passed index list here (its named
        // indexes were applied on its own register); the inline-unique indexes
        // are recovered from its field set, matching the desired snapshot the
        // sibling's own register produced. Pass an empty `_indexes`.
        let desc = schema_to_descriptor(app_id, name, sib_schema, &Value::Array(vec![]))?;
        by_name.insert(name.clone(), desc);
    }
    let current = schema_to_descriptor(app_id, collection, schema, indexes)?;
    by_name.insert(collection.to_string(), current);
    Ok(by_name.into_values().collect())
}

/// Convert plugin-db's per-collection schema JSON (the `registerModel` wire
/// shape: a flat record of `column_name → FieldDef` plus an optional `_meta`)
/// into the engine's [`CollectionDescriptor`].
///
/// The FieldDef wire shape (`toFieldDef()`) uses `refTarget` for a `t.ref`
/// target; the engine's [`FieldDescriptor`] uses `ref` (via serde rename). This
/// is the Rust analog of the JS IR adapter's `fieldDefToDescriptor` — the one
/// place the SDK FieldDef key names are translated to the engine descriptor key
/// names. We deserialize each FieldDef into a `FieldDescriptor` by re-keying
/// `refTarget → ref` and injecting the field `name`, then deserializing through
/// serde so every facet (vector/encrypted/mask/fts/enum/min/max/default/…) is
/// carried verbatim by the existing `#[serde(rename)]` mapping.
fn schema_to_descriptor(
    app_id: &str,
    collection: &str,
    schema: &Value,
    indexes: &Value,
) -> Result<CollectionDescriptor, DbError> {
    let obj = schema.as_object().ok_or_else(|| {
        DbError::internal(format!(
            "sqlite engine: schema for '{collection}' is not a JSON object"
        ))
    })?;

    let mut fields: Vec<FieldDescriptor> = Vec::new();
    for (col_name, def) in obj {
        // `_meta` / `_indexes` are reserved metadata keys, not columns.
        if col_name.starts_with('_') {
            continue;
        }
        let def_obj = def.as_object().ok_or_else(|| {
            DbError::internal(format!(
                "sqlite engine: field '{collection}.{col_name}' is not a JSON object"
            ))
        })?;
        // Re-key the SDK FieldDef into the engine FieldDescriptor wire shape:
        // inject `name`, translate `refTarget → ref` (the one wire-key rename the
        // JS IR adapter also does); every other key already matches the engine
        // descriptor's `#[serde(rename)]` mapping.
        let mut fd = serde_json::Map::new();
        fd.insert("name".to_string(), Value::String(col_name.clone()));
        for (k, v) in def_obj {
            let key = if k == "refTarget" { "ref" } else { k.as_str() };
            fd.insert(key.to_string(), v.clone());
        }
        let descriptor: FieldDescriptor = serde_json::from_value(Value::Object(fd))
            .map_err(|e| {
                DbError::internal(format!(
                    "sqlite engine: field '{collection}.{col_name}' is not a valid descriptor: {e}"
                ))
            })?;
        fields.push(descriptor);
    }
    // Deterministic field order so the union is order-independent (the snapshot
    // sorts by name anyway, but keep the input stable for reproducibility).
    fields.sort_by(|a, b| a.name.cmp(&b.name));

    // Named indexes (the separate `indexes` arg the SDK passes as
    // `[{ name, fields, unique? }]`). The engine's `IndexDescriptor` uses
    // `columns`, so re-key `fields → columns`.
    let mut index_descs: Vec<IndexDescriptor> = Vec::new();
    if let Some(arr) = indexes.as_array() {
        for idx in arr {
            let io = idx.as_object().ok_or_else(|| {
                DbError::internal(format!("sqlite engine: index entry for '{collection}' is not an object"))
            })?;
            let mut d = serde_json::Map::new();
            if let Some(n) = io.get("name") {
                d.insert("name".to_string(), n.clone());
            }
            // SDK spells the column list `fields`; the engine descriptor wants
            // `columns`. Accept either, preferring `fields` (the wire shape).
            if let Some(cols) = io.get("fields").or_else(|| io.get("columns")) {
                d.insert("columns".to_string(), cols.clone());
            }
            if let Some(u) = io.get("unique") {
                d.insert("unique".to_string(), u.clone());
            }
            let descriptor: IndexDescriptor =
                serde_json::from_value(Value::Object(d)).map_err(|e| {
                    DbError::internal(format!(
                        "sqlite engine: index for '{collection}' is not a valid descriptor: {e}"
                    ))
                })?;
            index_descs.push(descriptor);
        }
    }

    Ok(CollectionDescriptor {
        name: collection.to_string(),
        owner_app: app_id.to_string(),
        fields,
        indexes: index_descs,
        runtime_options: Default::default(),
    })
}
