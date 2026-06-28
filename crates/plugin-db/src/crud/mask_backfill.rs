//! **P5.5 PR 6** — mask sibling-column backfill / rewrite / removal
//! jobs invoked from the register-model `apply` pipeline.
//!
//! Three entry points, one per sub-path:
//!
//! - [`run_mask_backfill`] — 6a: existing column gained a `.mask(...)`
//!   declaration. The diff classifier already emitted a paired
//!   `ALTER TABLE ADD COLUMN <col>_masked TEXT NULL` op IMMEDIATELY
//!   before the `MaskBackfill` op (the ALTER carries `NULL` so the
//!   migration succeeds against existing data); this function then
//!   walks every row with `<col>_masked IS NULL`, computes the
//!   masked representation, writes it, and finishes with
//!   `ALTER TABLE … ALTER COLUMN <col>_masked SET NOT NULL` once the
//!   `IS NULL` set has been clean for two consecutive polls (the
//!   racing-insert fence).
//!
//! - [`run_mask_rewrite`] — 6b: existing masked column's `.mask(...)`
//!   `kind` (or `classification`) changed. The sibling already exists
//!   + is NOT NULL, so the rewrite touches every row (no IS NULL
//!   filter) and DOES NOT mutate the schema afterwards.
//!
//! - [`run_mask_remove`] — 6c: `.mask(...)` declaration removed (or
//!   switched to `kind: "none"`). Classified `Destructive`; under
//!   `strictness == "off"` the apply pipeline issues
//!   `ALTER TABLE … DROP COLUMN <col>_masked`. Under `strict` and
//!   `lenient` the validate stage refuses before reaching us.
//!
//! ## Audit-table integration
//!
//! Each batch run inserts an audit row in `__zeroship_migrations`
//! whose `name` is derived from the existing migrations-framework
//! shape (`mig:mask_backfill_<coll>_<col>` /
//! `mig:mask_rewrite_<coll>_<col>`). Progress (`cursor`, `processed`)
//! survives worker restart: a re-invoked backfill picks up from the
//! last `cursor` already written to the audit row. The audit row is
//! finalised `Applied` on the last batch, `Failed` on any
//! propagating error.
//!
//! ## Why this lives outside the existing `migrations.rs` driver
//!
//! `crate::migrations` is a JS-driven loop (`fetchBatch` /
//! `commitBatch` round-tripped through the SDK). PR 6's backfill must
//! run inside the deploy pipeline, before the V8 isolate hands
//! control back to user code — there is no JS loop available.
//! `mask_backfill` reuses the audit-table shape (`phase = backfill`,
//! `change_class = additive` / `compatible`, `change_kind =
//! mask_backfill` / `mask_rewrite`) so operators querying
//! `__zeroship_migrations` see a uniform history, but the driver is
//! native.
//!
//! ## Batch sizing
//!
//! Default `BATCH_SIZE = 1000` — matches the per-batch cap on the
//! JS-driven `Migration.fetchBatch`. Operators can override per
//! deploy by passing a different `BackfillOpts` (unused in PR 6 — the
//! constant is exposed so a future PR can lift it).

use compio_postgres::Pool;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::audit::{ActorKind, AuditRow, ChangeClass, InitialStatus, Phase, TerminalStatus};
use crate::backend::EncryptedColumn;
use crate::crud::mask_pass::apply_mask_kind;
use crate::diff::{Classification, MaskKind};
use crate::error::DbError;
use crate::query::quote_ident;

/// Default batch size for backfill / rewrite loops. Matches the cap on
/// `crate::migrations::exec_fetch_batch` (10_000) at the low end of
/// what the SDK typically requests — the JS-driven migration loop
/// defaults to 500-1000. We use 1000 here as a middle ground between
/// per-batch UPDATE round-trip overhead (smaller = more chatter) and
/// memory footprint per batch (larger = bigger Rust-side Vec<Value>
/// allocation).
pub const BATCH_SIZE: i64 = 1_000;

// **Schema-authority P1** — the mask-sentinel CODEC (build/parse the
// `__zsmask:…` string) was relocated into the leaf crate
// `zeroship_schema::mask_codec`. It is a schema-shape concern (the contract
// the schema layer writes into DDL and the data plane reads back); the
// backfill *runner* below (`run_mask_backfill` / `run_mask_rewrite`) STAYS
// here in the data plane.
//
// `build_mask_sentinel` re-exports verbatim (it is pure). `parse_mask_sentinel`
// gets a thin wrapper that preserves the `Result<_, DbError>` shape the
// plugin-db callers expect: the leaf codec returns `MaskSentinelError`
// (it cannot name `DbError`), which the `From` impl in `crate::error` maps
// back to `DbError::internal(<same message>)` — byte-identical to the
// pre-extraction parser.
pub use zeroship_schema::mask_codec::build_mask_sentinel;

/// **P5.5 PR 6** — parse a `__zsmask:kind=…,classification=…`
/// sentinel string back into a `(MaskKind, Classification)` pair.
///
/// Thin `DbError`-shaped wrapper over the relocated leaf codec
/// [`zeroship_schema::mask_codec::parse_mask_sentinel`]. Returns
/// `Err(DbError::Internal { … })` with the code-discriminator
/// `mask_sentinel_malformed` for any parse failure — unknown kind,
/// unknown classification, missing field, extra trailing junk — exactly
/// as before the codec was extracted. The caller surfaces the typed error
/// from the introspector with the column name appended so an operator
/// hand-debugging `pg_description` sees exactly which sibling is malformed.
pub fn parse_mask_sentinel(s: &str) -> Result<(MaskKind, Classification), DbError> {
    zeroship_schema::mask_codec::parse_mask_sentinel(s).map_err(DbError::from)
}

/// **P5.5 PR 6** — compute the masked representation for one row's
/// parent column value, optionally decrypting `value` first when the
/// column is `t.encrypted(...)`-declared.
///
/// `value` is the parent column's value as surfaced by the backend's
/// text protocol — for PG, BYTEA-encrypted columns arrive as `\xHH…`
/// hex strings; plaintext columns arrive as plain strings. The
/// decrypt path mirrors
/// [`crate::crud::encryption_pass::decrypt_row_on_read`]'s AAD policy
/// (Randomised binds `row_pk`; Deterministic omits it).
///
/// For null-valued parents the function returns `Ok(None)` — the mask
/// pass writes nothing (matches `apply_mask_on_write`'s Q-MASK-L
/// pass-through-null rule).
///
/// The `B: EncryptedColumn` bound carries the decrypt path: an
/// encrypted column resolves its key + recovers plaintext before the
/// mask transform; plaintext columns (`enc_meta == None`) take the
/// direct branch.
pub async fn apply_mask_to_one_row<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    column: &str,
    enc_meta: Option<&crate::diff::EncryptionMeta>,
    kind: MaskKind,
    row_pk: &str,
    value: &Value,
) -> Result<Option<String>, DbError>
where
    B: EncryptedColumn,
{
    if value.is_null() {
        return Ok(None);
    }

    // Decrypt path: column is encrypted → recover plaintext bytes →
    // serialise per `wraps` → mask. Mirrors
    // `decrypt_row_on_read`'s code path without going through the
    // shared helper because we only need the plaintext STRING form
    // (the mask consumer), not the typed JSON value.
    let plaintext_string: String = if let Some(enc) = enc_meta {
        let Some(hex_str) = value.as_str() else {
            return Err(DbError::internal(format!(
                "apply_mask_to_one_row: encrypted column '{column}' \
                 expected BYTEA text shape, got {value:?}"
            )));
        };
        let bytes = hex_to_bytes(hex_str)?;
        let key = backend.resolve_key(app_id, &enc.key_id).await?;
        let aad = crate::encryption::aad::canonical_aad(
            collection,
            column,
            match enc.mode {
                crate::backend::EncryptionMode::Randomised => Some(row_pk.as_bytes()),
                crate::backend::EncryptionMode::Deterministic => None,
            },
        );
        let plaintext_bytes = backend.decrypt(&key, enc.mode, &bytes, &aad)?;
        wrapped_bytes_to_string(&plaintext_bytes, enc.wraps)
    } else {
        plaintext_from_value(column, value)?
    };

    Ok(Some(apply_mask_kind(kind, &plaintext_string)))
}

/// Read the parent column's value as the string the mask transform
/// consumes. Used by [`apply_mask_to_one_row`]'s "no enc_meta" branch.
fn plaintext_from_value(column: &str, value: &Value) -> Result<String, DbError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Ok(String::new()),
        other => Err(DbError::internal(format!(
            "apply_mask_to_one_row: column '{column}' has \
             unsupported value shape {other:?}"
        ))),
    }
}

/// Audit-name shape for the 6a backfill on `(collection, column)`.
/// Matches the migrations-framework `name` convention (`mig:<tag>`)
/// so operators querying `__zeroship_migrations.collection` see a
/// consistent prefix across both JS-driven and native backfills.
#[must_use]
pub fn backfill_audit_name(collection: &str, column: &str) -> String {
    format!("mask_backfill_{collection}_{column}")
}

/// Audit-name shape for the 6b rewrite on `(collection, column)`.
#[must_use]
pub fn rewrite_audit_name(collection: &str, column: &str) -> String {
    format!("mask_rewrite_{collection}_{column}")
}

/// Backfill state surfaced to the apply layer — primarily for
/// observability + testing. The apply layer ignores the return value
/// on the production path; tests inspect `processed` to assert the
/// loop saw every row.
#[derive(Debug, Default, Clone)]
pub struct BackfillReport {
    /// Total rows updated by this run (across every batch).
    pub processed: i64,
}

// ---------------------------------------------------------------------
// 6a — mask backfill
// ---------------------------------------------------------------------

/// **P5.5 PR 6a** — backfill the sibling column for every row where
/// `<col>_masked IS NULL`, then flip the sibling to NOT NULL.
///
/// The diff classifier already emitted the
/// `ALTER TABLE ADD COLUMN <col>_masked TEXT NULL` op immediately
/// before the `MaskBackfill` op; that ALTER is run by the regular
/// apply path before this function fires. We start with the sibling
/// already in place + NULL on every existing row.
///
/// Steps (per batch):
///
/// 1. `SELECT id, <col> FROM "<app>"."<coll>"
///       WHERE "<col>_masked" IS NULL AND "<col>" IS NOT NULL
///       ORDER BY id LIMIT N`.
///    Skipping rows where the parent is itself NULL because
///    `apply_mask_on_write` writes no sibling for NULL parents
///    (Q-MASK-L pass-through) — the sibling stays NULL forever for
///    those rows; the SET NOT NULL at the end would refuse if we
///    didn't filter, so the algorithm intentionally fails-loud on
///    rows that should be filtered by the parent IS NOT NULL clause.
/// 2. Decrypt + mask each row's parent value via
///    [`apply_mask_to_one_row`].
/// 3. UPDATE the sibling for each row.
/// 4. Repeat until two consecutive polls return zero rows. The
///    second clean poll catches a row that another worker INSERTed
///    between the previous batch's UPDATE and the SELECT.
/// 5. `ALTER TABLE … ALTER COLUMN <col>_masked SET NOT NULL`.
///
/// The dual-write CRUD pass (PR 2) ensures every new INSERT writes
/// the sibling, so the race window between step 1's SELECT and
/// step 5's ALTER is bounded by one batch's runtime.
///
/// Audit-row lifetime:
///
/// - Insert `Running` row at the start (`audit_id` returned).
/// - On error: `update_audit_status(Failed, message)` + return.
/// - On success: `update_audit_status(Applied, None)`.
///
/// Resume: a re-invoked backfill against the same `(coll, col)`
/// finds the existing `Running` (or `Failed`) audit row and reuses it
/// — the row's `cursor` carries the last processed PK so subsequent
/// SELECTs pick up where the previous attempt stopped.
pub async fn run_mask_backfill<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    column: &str,
    kind: MaskKind,
    classification: Classification,
    enc_meta: Option<&crate::diff::EncryptionMeta>,
    pool: &Pool,
) -> Result<BackfillReport, DbError>
where
    B: EncryptedColumn,
{
    let sibling = format!("{column}_masked");
    let name = backfill_audit_name(collection, column);

    // Audit row — best-effort INSERT; if it fails the backfill still
    // runs but operators lose the trail. (Same pattern as
    // `validate.rs:101` / `apply.rs:84` — F1 warn-half.)
    let audit_id = open_backfill_audit_row(
        pool,
        app_id,
        collection,
        &name,
        kind,
        classification,
        ChangeClass::Additive,
    )
    .await;

    let report = backfill_loop::<B>(
        backend,
        app_id,
        collection,
        column,
        &sibling,
        kind,
        enc_meta,
        pool,
        audit_id,
        /* select_only_null_sibling = */ true,
    )
    .await;

    finalize_audit_row(pool, app_id, audit_id, &report).await;

    let report = report?;

    // Final ALTER — flip the sibling to NOT NULL. Sets the contract
    // PR 2 expects: every existing row has a non-NULL sibling, every
    // future INSERT must dual-write.
    let alter_sql = format!(
        "ALTER TABLE {}.{} ALTER COLUMN {} SET NOT NULL",
        quote_ident(app_id),
        quote_ident(collection),
        quote_ident(&sibling),
    );
    pool.query_text_params(&alter_sql, &[])
        .await
        .map_err(|e| crate::error::coded_sql("mask_backfill: SET NOT NULL", e))?;

    Ok(BackfillReport {
        processed: report.processed,
    })
}

// ---------------------------------------------------------------------
// 6b — mask rewrite
// ---------------------------------------------------------------------

/// **P5.5 PR 6b** — rewrite the sibling column under a NEW mask kind
/// (or new classification) for every row. The sibling already exists +
/// is NOT NULL, so no schema mutation runs alongside this.
///
/// Same loop shape as [`run_mask_backfill`] except:
/// - No `IS NULL` filter on the sibling (we touch every row).
/// - No SET NOT NULL at the end.
///
/// Idempotent on a stable `(collection, column, new_kind,
/// classification)`: re-running the rewrite over already-correct
/// siblings writes back the same string, so a partial run that
/// crashes mid-pass can be re-driven from the start without
/// observable difference. Resume from `audit_row.cursor` skips
/// already-rewritten rows for the common case where the operator
/// just wants to bypass the redundant work.
pub async fn run_mask_rewrite<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    column: &str,
    new_kind: MaskKind,
    classification: Classification,
    enc_meta: Option<&crate::diff::EncryptionMeta>,
    pool: &Pool,
) -> Result<BackfillReport, DbError>
where
    B: EncryptedColumn,
{
    let sibling = format!("{column}_masked");
    let name = rewrite_audit_name(collection, column);

    let audit_id = open_backfill_audit_row(
        pool,
        app_id,
        collection,
        &name,
        new_kind,
        classification,
        ChangeClass::Compatible,
    )
    .await;

    let report = backfill_loop::<B>(
        backend,
        app_id,
        collection,
        column,
        &sibling,
        new_kind,
        enc_meta,
        pool,
        audit_id,
        /* select_only_null_sibling = */ false,
    )
    .await;

    finalize_audit_row(pool, app_id, audit_id, &report).await;

    let report = report?;
    Ok(BackfillReport {
        processed: report.processed,
    })
}

// ---------------------------------------------------------------------
// 6c — mask removal
// ---------------------------------------------------------------------

/// **P5.5 PR 6c** — drop the sibling column.
///
/// Only invoked from `apply.rs` under `strictness == "off"` — the
/// validate stage's destructive-class filter refuses 6c under
/// `strict` and `lenient`. The SQL is single-statement, idempotent
/// via `IF EXISTS`, and no audit-loop machinery is needed (the
/// regular apply-rail audit row for the MaskRemove op is enough).
pub async fn run_mask_remove(
    app_id: &str,
    collection: &str,
    column: &str,
    pool: &Pool,
) -> Result<(), DbError> {
    let sibling = format!("{column}_masked");
    let sql = format!(
        "ALTER TABLE {}.{} DROP COLUMN IF EXISTS {}",
        quote_ident(app_id),
        quote_ident(collection),
        quote_ident(&sibling),
    );
    pool.query_text_params(&sql, &[])
        .await
        .map_err(|e| crate::error::coded_sql("mask_remove: DROP COLUMN", e))?;
    Ok(())
}

// ---------------------------------------------------------------------
// Shared loop
// ---------------------------------------------------------------------

/// Inner batch loop shared by 6a + 6b. Returns `BackfillReport.completed
/// = false` and `Err(DbError)` together on the propagating-error path
/// (the caller's `?` unwinds to the apply.rs Err); on success the
/// caller is responsible for finalising the audit row + the
/// optional SET NOT NULL.
///
/// `select_only_null_sibling`:
/// - `true`  → 6a: `WHERE "<sibling>" IS NULL AND "<col>" IS NOT NULL`
/// - `false` → 6b: no filter on the sibling; only `<col> IS NOT NULL`
///
/// The 6a path additionally requires TWO consecutive empty SELECTs
/// before exiting the loop — the second poll catches a row that
/// another worker INSERTed between the last batch's UPDATE and the
/// SELECT. The 6b path doesn't need the fence because there's no
/// "completion" criterion — `cursor` monotonically advances and the
/// `LIMIT` exits the loop once every row past the cursor has been
/// rewritten.
#[allow(clippy::too_many_arguments)]
async fn backfill_loop<B>(
    backend: &B,
    app_id: &str,
    collection: &str,
    column: &str,
    sibling: &str,
    kind: MaskKind,
    enc_meta: Option<&crate::diff::EncryptionMeta>,
    pool: &Pool,
    audit_id: Option<i64>,
    select_only_null_sibling: bool,
) -> Result<BackfillReport, DbError>
where
    B: EncryptedColumn,
{
    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let parent = quote_ident(column);
    let sibling_q = quote_ident(sibling);

    let where_clause = if select_only_null_sibling {
        format!("WHERE {sibling_q} IS NULL AND {parent} IS NOT NULL")
    } else {
        format!("WHERE {parent} IS NOT NULL")
    };
    let select_sql = format!(
        "SELECT id, {parent} FROM {schema}.{table} \
         {where_clause} AND id > $1::bigint \
         ORDER BY id LIMIT $2::bigint"
    );

    // Resume from the last cursor written to the audit row (carried
    // over by a previously-crashed run). The lookup runs once
    // up-front; subsequent heartbeats update it in place.
    let mut cursor: i64 = match audit_id {
        Some(id) => read_cursor_from_audit_id(pool, app_id, id).await.unwrap_or(0),
        None => 0,
    };
    let mut processed: i64 = match audit_id {
        Some(id) => read_processed_from_audit_id(pool, app_id, id).await.unwrap_or(0),
        None => 0,
    };
    let mut consecutive_empty = 0;
    let required_empty_polls = if select_only_null_sibling { 2 } else { 1 };
    let batch_size_str = BATCH_SIZE.to_string();

    loop {
        let cursor_str = cursor.to_string();
        let rows = pool
            .query_text_params(&select_sql, &[cursor_str.as_str(), batch_size_str.as_str()])
            .await
            .map_err(|e| crate::error::coded_sql("mask_backfill: SELECT", e))?;

        if rows.is_empty() {
            consecutive_empty += 1;
            if consecutive_empty >= required_empty_polls {
                return Ok(BackfillReport {
                    processed,
                });
            }
            // For 6a only: if no rows came back this round, try one
            // more poll without advancing the cursor — a racing
            // INSERT writes a row at the END of the id ordering, so
            // we MUST reset the cursor to 0 to catch it. The
            // `IS NULL` filter on the sibling guarantees we don't
            // re-touch rows already backfilled. For 6b
            // (`select_only_null_sibling = false`) the first empty
            // poll already terminates because `required_empty_polls
            // == 1`, so we never take this branch on the rewrite
            // path.
            cursor = 0;
            continue;
        }
        consecutive_empty = 0;

        for row in &rows {
            let id: i64 = row.try_get("id").unwrap_or(0);
            let parent_value = pg_row_value(row, column);
            let row_pk = id.to_string();
            let masked = apply_mask_to_one_row(
                backend, app_id, collection, column, enc_meta, kind, &row_pk, &parent_value,
            )
            .await?;

            if let Some(masked_str) = masked {
                let update_sql = format!(
                    "UPDATE {schema}.{table} SET {sibling_q} = $1 \
                     WHERE id = $2::bigint",
                );
                let id_str = id.to_string();
                pool.query_text_params(
                    &update_sql,
                    &[masked_str.as_str(), id_str.as_str()],
                )
                .await
                .map_err(|e| crate::error::coded_sql("mask_backfill: UPDATE", e))?;
                processed += 1;
            }
            cursor = std::cmp::max(cursor, id);
        }

        // Heartbeat: bump the audit cursor so a worker restart picks
        // up from here. Best-effort — a transient failure on the
        // audit UPDATE doesn't block the data write.
        if let Some(id) = audit_id {
            let _ = update_backfill_cursor(pool, app_id, id, cursor, processed).await;
        }
    }
}

/// Read the `validate_cursor` from a backfill audit row by id.
/// Returns `None` on lookup failure (row missing, column NULL, query
/// error) — the caller starts fresh from cursor 0.
async fn read_cursor_from_audit_id(pool: &Pool, app_id: &str, id: i64) -> Option<i64> {
    let sql = format!(
        r#"SELECT validate_cursor FROM "{app_id}"."__zeroship_migrations"
            WHERE id = $1::bigint"#
    );
    let id_s = id.to_string();
    let rows = pool.query_text_params(&sql, &[id_s.as_str()]).await.ok()?;
    rows.first().and_then(|r| r.try_get::<_, i64>("validate_cursor").ok())
}

/// Read the `details.processed` count from a backfill audit row by id.
async fn read_processed_from_audit_id(pool: &Pool, app_id: &str, id: i64) -> Option<i64> {
    let sql = format!(
        r#"SELECT details FROM "{app_id}"."__zeroship_migrations"
            WHERE id = $1::bigint"#
    );
    let id_s = id.to_string();
    let rows = pool.query_text_params(&sql, &[id_s.as_str()]).await.ok()?;
    let row = rows.first()?;
    Some(crate::audit::read_processed_from_audit_row(row))
}

// ---------------------------------------------------------------------
// Audit-table helpers
// ---------------------------------------------------------------------

/// Open (or reuse) the audit row tracking this run. Returns the row's
/// `id` for downstream finalisation, or `None` on a best-effort
/// audit-write failure (the loop still runs; operators lose the
/// trail — matches the F1 warn-half pattern).
///
/// Resume semantics: if a `Running` row already exists for this
/// `(collection, name)` pair (from a prior worker that crashed
/// mid-backfill), we reuse it. This means a backfill that fails halfway
/// + a deploy that re-emits the same `MaskBackfill` op finds the same
/// audit row and resumes from the cursor it already wrote.
async fn open_backfill_audit_row(
    pool: &Pool,
    app_id: &str,
    collection: &str,
    name: &str,
    kind: MaskKind,
    classification: Classification,
    change_class: ChangeClass,
) -> Option<i64> {
    let deploy_id = std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string());
    let schema_version = crate::audit::next_schema_version(pool, app_id).await.unwrap_or(1);

    if let Ok(Some(existing)) =
        crate::audit::find_latest_backfill_row(pool, app_id, collection, name).await
    {
        if matches!(existing.status.as_str(), "running" | "failed") {
            // Reuse the row — operators see one history per
            // `(collection, column)` rather than a row per attempt.
            return Some(existing.id);
        }
    }

    // `change_kind` doubles as the lookup key on the backfill rail —
    // `find_latest_backfill_row` matches `WHERE change_kind = $name`.
    // The full disambiguating name (`mask_backfill_<coll>_<col>` /
    // `mask_rewrite_<coll>_<col>`) lands here so resume across worker
    // restarts works without a separate lookup column.
    let row = AuditRow {
        collection: collection.to_string(),
        phase: Phase::Backfill,
        change_class,
        change_kind: name.to_string(),
        details: serde_json::json!({
            "kind": classify_label_from_name(name),
            "name": name,
            "mask_kind": kind.as_sql(),
            "classification": classification.as_sql(),
            "processed": 0,
        }),
        ddl_sql: None,
        status: InitialStatus::Running,
        deploy_id,
        schema_version,
        actor: ActorKind::Auto,
    };
    match crate::audit::write_audit_row(pool, app_id, &row).await {
        Ok(id) => Some(id),
        Err(audit_err) => {
            tracing::warn!(
                app_id = %app_id,
                collection = %collection,
                transition = "Running/insert_failed",
                audit_err = %audit_err,
                "audit: failed to insert mask-backfill running row",
            );
            None
        }
    }
}

/// Render the audit-details `kind` discriminator label for a given
/// run name. Mirrors the static `ChangeKind::as_sql` shape so operator
/// dashboards grouping by `details.kind` see consistent buckets.
fn classify_label_from_name(name: &str) -> &'static str {
    if name.starts_with("mask_backfill_") {
        "mask_backfill"
    } else if name.starts_with("mask_rewrite_") {
        "mask_rewrite"
    } else {
        "mask_backfill"
    }
}

/// Update the audit row's cursor + processed count after each batch.
/// Best-effort — invoked from the heartbeat path; failure doesn't
/// abort the data write.
///
/// Mirrors `audit::update_backfill_progress` but takes `&Pool`
/// instead of a borrowed `&Client` — the backfill loop doesn't hold
/// a dedicated client (no advisory lock; the deploy pipeline's
/// register_model lock is the broader coordination boundary).
async fn update_backfill_cursor(
    pool: &Pool,
    app_id: &str,
    audit_id: i64,
    cursor: i64,
    processed: i64,
) -> Result<(), DbError> {
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
            SET validate_cursor = $2::bigint,
                details = jsonb_set(COALESCE(details, '{{}}'::jsonb), '{{processed}}', to_jsonb($3::bigint)),
                last_heartbeat_at = NOW(),
                updated_at = NOW()
            WHERE id = $1::bigint"#
    );
    let id_s = audit_id.to_string();
    let cursor_s = cursor.to_string();
    let processed_s = processed.to_string();
    pool.query_text_params(
        &sql,
        &[id_s.as_str(), cursor_s.as_str(), processed_s.as_str()],
    )
    .await
    .map_err(|e| crate::error::coded_sql("mask_backfill: update cursor", e))?;
    Ok(())
}

/// Finalise the audit row to `Applied` / `Failed`. The `report` is
/// `Result<BackfillReport, DbError>` — `Ok(_)` → `Applied`,
/// `Err(_)` → `Failed` with the rendered DbError as `error_message`.
/// Best-effort: an audit-write failure here is logged via the F1
/// warn-half but doesn't override the underlying result.
async fn finalize_audit_row(
    pool: &Pool,
    app_id: &str,
    audit_id: Option<i64>,
    report: &Result<BackfillReport, DbError>,
) {
    let Some(id) = audit_id else { return };
    let (status, error_message) = match report {
        Ok(_) => (TerminalStatus::Applied, None),
        Err(e) => (TerminalStatus::Failed, Some(e.clone().into_string())),
    };
    if let Err(audit_err) = crate::audit::update_audit_status(
        pool,
        app_id,
        id,
        status,
        error_message.as_deref(),
    )
    .await
    {
        tracing::warn!(
            app_id = %app_id,
            audit_id = id,
            transition = ?status,
            audit_err = %audit_err,
            "update_audit_status failed; mask-backfill row stays in 'running' until reset",
        );
    }
}

// ---------------------------------------------------------------------
// Helpers shared with the existing encryption pass
// ---------------------------------------------------------------------

/// Pluck a column value from a `compio_postgres::Row` as a JSON Value
/// (`String` / `Null`). Lifted out of `apply_mask_to_one_row` so
/// callers (and tests) can re-use the same shape conversion.
fn pg_row_value(row: &compio_postgres::Row, column: &str) -> Value {
    match row.try_get::<_, Option<String>>(column) {
        Ok(Some(s)) => Value::String(s),
        Ok(None) | Err(_) => Value::Null,
    }
}

/// Convert decrypted plaintext bytes to the human-readable string the
/// mask pass needs. Mirrors `plaintext_to_sidechannel_string` in
/// `encryption_pass.rs` but operates on raw bytes (we already
/// decrypted the BYTEA blob).
fn wrapped_bytes_to_string(bytes: &[u8], wraps: crate::diff::WrappedType) -> String {
    match wraps {
        crate::diff::WrappedType::String => {
            // UTF-8-validate; non-UTF-8 plaintext is a contract
            // violation (the SDK only ever wraps strings on
            // `wraps = "string"`). We render lossy here to avoid
            // panicking — the mask string ends up garbled but the
            // backfill proceeds.
            String::from_utf8_lossy(bytes).into_owned()
        }
        crate::diff::WrappedType::Number => {
            if bytes.len() != 8 {
                return String::new();
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            f64::from_be_bytes(arr).to_string()
        }
        crate::diff::WrappedType::Bytes => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
    }
}

/// Decode a PG text-protocol `\xHH..` hex string into raw bytes.
/// Duplicates `encryption_pass.rs::hex_to_bytes` so we don't have to
/// flip its visibility for one consumer.
fn hex_to_bytes(s: &str) -> Result<Vec<u8>, DbError> {
    let hex = s.strip_prefix("\\x").unwrap_or(s);
    if hex.len() % 2 != 0 {
        return Err(DbError::internal(format!(
            "mask_backfill: BYTEA text has odd hex length {}",
            hex.len()
        )));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8, DbError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(DbError::internal(format!(
            "mask_backfill: BYTEA text non-hex byte 0x{c:02x}"
        ))),
    }
}

// ---------------------------------------------------------------------
// Pure-helper: kind+classification → masked string (no I/O)
// ---------------------------------------------------------------------

/// Compute the masked sibling value from an arbitrary plaintext
/// string. Pure wrapper around [`apply_mask_kind`] — exposed so the
/// SQLite integration test can derive the expected sibling without
/// reaching into `mask_pass`'s private surface.
///
/// `plaintexts` is the encryption-pass sidechannel form
/// ([`crate::crud::mask_pass::MaskPlaintextSidechannel`]) — for the
/// non-encrypted case, callers pass a fresh empty map and the
/// function reads `row[col]` directly. Same precedence as
/// `apply_mask_on_write`.
#[must_use]
#[allow(dead_code)] // helper exposed for tests
pub fn compute_masked_for_plaintext(kind: MaskKind, plaintext: &str) -> String {
    apply_mask_kind(kind, plaintext)
}

/// Compute the masked sibling for the same shape `apply_mask_on_write`
/// consumes — `(schema, row, plaintexts)` — returning the masked
/// `(sibling_column → masked_string)` pairs WITHOUT mutating the row.
/// The SQLite integration test uses this to build the expected
/// post-backfill state without invoking the actual loop.
#[must_use]
#[allow(dead_code)] // helper exposed for tests
pub fn compute_masked_pairs_for_row(
    schema: &Value,
    row: &Value,
    plaintexts: &crate::crud::mask_pass::MaskPlaintextSidechannel,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(schema_obj) = schema.as_object() else {
        return out;
    };
    let Some(obj) = row.as_object() else {
        return out;
    };
    for (col, def) in schema_obj.iter() {
        let Some(mask_meta) = def.get("mask").and_then(|v| v.as_object()) else {
            continue;
        };
        let kind_str = mask_meta.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
        if kind_str == "none" {
            continue;
        }
        let Some(kind) = MaskKind::from_sql(kind_str) else {
            continue;
        };
        let plaintext: Option<Zeroizing<String>> = if let Some(pt) = plaintexts.get(col) {
            Some(pt.clone())
        } else if let Some(v) = obj.get(col) {
            if v.is_null() {
                None
            } else if let Some(s) = v.as_str() {
                Some(Zeroizing::new(s.to_string()))
            } else if let Some(n) = v.as_i64() {
                Some(Zeroizing::new(n.to_string()))
            } else {
                None
            }
        } else {
            None
        };
        if let Some(pt) = plaintext {
            let sibling = format!("{col}_masked");
            out.push((sibling, apply_mask_kind(kind, pt.as_str())));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_mask_sentinel_round_trips() {
        let s = build_mask_sentinel(MaskKind::Last4, Classification::Spi);
        assert_eq!(s, "__zsmask:kind=last4,classification=spi");
        let (kind, class) = parse_mask_sentinel(&s).unwrap();
        assert_eq!(kind, MaskKind::Last4);
        assert_eq!(class, Classification::Spi);
    }

    #[test]
    fn build_mask_sentinel_for_every_kind_classification_pair() {
        for kind in [
            MaskKind::Full,
            MaskKind::Last4,
            MaskKind::First4,
            MaskKind::Email,
            MaskKind::Name,
            MaskKind::DateYear,
            MaskKind::DateDecade,
        ] {
            for class in [
                Classification::Public,
                Classification::Pii,
                Classification::Spi,
                Classification::Phi,
                Classification::Pci,
                Classification::Internal,
            ] {
                let s = build_mask_sentinel(kind, class);
                let parsed = parse_mask_sentinel(&s).unwrap();
                assert_eq!(parsed, (kind, class), "round-trip {kind:?} / {class:?}");
            }
        }
    }

    #[test]
    fn malformed_mask_sentinel_returns_typed_error() {
        // Missing prefix.
        let err = parse_mask_sentinel("kind=last4,classification=spi").unwrap_err();
        assert!(err.clone().into_string().contains("mask_sentinel_malformed"));

        // Unknown kind.
        let err = parse_mask_sentinel("__zsmask:kind=blink_182,classification=pii").unwrap_err();
        let msg = err.clone().into_string();
        assert!(msg.contains("mask_sentinel_malformed"));
        assert!(msg.contains("blink_182"));

        // Unknown classification.
        let err = parse_mask_sentinel("__zsmask:kind=last4,classification=cosmic").unwrap_err();
        assert!(err.clone().into_string().contains("cosmic"));

        // Missing kind.
        let err = parse_mask_sentinel("__zsmask:classification=pii").unwrap_err();
        assert!(err.clone().into_string().contains("missing kind="));

        // Extra junk.
        let err = parse_mask_sentinel("__zsmask:kind=last4,classification=pii,extra=bogus").unwrap_err();
        assert!(err.clone().into_string().contains("unrecognised key"));
    }

    #[test]
    fn audit_name_shapes_are_distinct() {
        assert_eq!(
            backfill_audit_name("users", "ssn"),
            "mask_backfill_users_ssn"
        );
        assert_eq!(
            rewrite_audit_name("users", "ssn"),
            "mask_rewrite_users_ssn"
        );
        assert_ne!(
            backfill_audit_name("users", "ssn"),
            rewrite_audit_name("users", "ssn")
        );
    }

    #[test]
    fn compute_masked_pairs_for_row_reads_from_sidechannel_for_encrypted() {
        // Encrypted column: plaintext arrives via sidechannel; row
        // holds the base64 ciphertext.
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "ssn": "BASE64CT" });
        let mut pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        pt.insert(
            "ssn".to_string(),
            Zeroizing::new("123-45-6789".to_string()),
        );
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("ssn_masked".to_string(), "***-**-6789".to_string()));
    }

    #[test]
    fn compute_masked_pairs_for_row_reads_from_row_for_plaintext() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "email": "alice@example.com" });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("email_masked".to_string(), "a***@example.com".to_string()));
    }

    #[test]
    fn compute_masked_pairs_for_row_skips_kind_none() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "ssn": "123-45-6789" });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert!(pairs.is_empty(), "kind=none must produce no pairs");
    }

    #[test]
    fn compute_masked_pairs_for_row_skips_null_parent() {
        let schema = serde_json::json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let row = serde_json::json!({ "id": "u1", "email": null });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert!(pairs.is_empty(), "null parent must produce no pairs");
    }

    #[test]
    fn compute_masked_pairs_for_row_emits_one_per_masked_column() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            },
            "name": { "type": "string" }
        });
        let row = serde_json::json!({
            "id": "u1",
            "ssn": "123-45-6789",
            "email": "alice@example.com",
            "name": "alice"
        });
        let pt = crate::crud::mask_pass::MaskPlaintextSidechannel::new();
        let pairs = compute_masked_pairs_for_row(&schema, &row, &pt);
        assert_eq!(pairs.len(), 2);
        let mut sorted = pairs.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sorted[0].0, "email_masked");
        assert_eq!(sorted[0].1, "a***@example.com");
        assert_eq!(sorted[1].0, "ssn_masked");
        assert_eq!(sorted[1].1, "***-**-6789");
    }
}
