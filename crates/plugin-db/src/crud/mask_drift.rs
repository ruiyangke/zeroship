//! **P5.5 PR 7** — drift detection for masked sibling columns.
//!
//! The dual-write CRUD pass (PR 2), backfill (PR 6a) and rewrite
//! (PR 6b) jobs are responsible for keeping `<col>_masked` in lock-step
//! with the parent column's plaintext under the column's declared
//! `MaskKind`. Drift would let stale or wrong masked text leak into
//! defaults reads — silently weakening the privacy guarantee P5.5 makes.
//!
//! This module periodically samples rows for every masked column on
//! every collection and verifies the equation
//!
//! ```text
//! <col>_masked  ==  apply_mask_kind(decrypt(<col>), kind)
//! ```
//!
//! holds. Mismatches are logged via `tracing::error!` AND written into
//! a per-app `__zeroship_audit_mask_drift` audit table so operators can
//! query the historical record without a log search. The audit row
//! never stores plaintext — `stored_masked` and `expected_masked` are
//! both already masked, so the table itself carries the same privacy
//! posture as the parent collection.
//!
//! ## Scheduler hook
//!
//! [`schedule_drift_for_app`] is the wiring point for the time-based
//! firing. P6a's sweeper-half will replace the stub body without
//! touching call sites elsewhere. Today the body just logs a
//! debug-level event; callers that want to run the check now invoke
//! [`run_drift_check_for_app`] directly (as the unit + sqlite
//! integration tests do).
//!
//! ## Sampling
//!
//! - **PG**: `TABLESAMPLE BERNOULLI (<pct>)` — postgres samples each
//!   row independently with probability `pct/100`, plus an absolute
//!   `LIMIT` cap so a very large table doesn't produce a 100k-row
//!   sample on a 1 % sweep.
//! - **SQLite**: no native TABLESAMPLE; we ride on
//!   `WHERE (abs(random()) % 100) < <pct>` which is equivalent in
//!   distribution at this granularity.
//! - **Cap**: [`MAX_SAMPLE_ROWS_PER_COLUMN`] (= 1 000) bounds the
//!   per-column work so a single drift run can't accidentally page in
//!   the whole table on a multi-gigabyte collection. The constant is
//!   exposed so a future config struct can lift it per app.

use std::collections::HashMap;

use serde_json::Value;

use crate::crud::mask_pass::apply_mask_kind;
use crate::diff::MaskKind;
use crate::error::DbError;

/// Default sampling percentage per masked column per run. 1 % keeps
/// the cron cheap on million-row tables while still surfacing drift
/// inside a small number of runs (a 0.1 % real drift rate is
/// detectable within ~7 runs at 95 % confidence).
pub const DEFAULT_SAMPLE_PCT: f64 = 1.0;

/// Absolute cap on rows sampled per column per run. Even when the
/// percentage would yield more — say a 50 M-row table at 1 % = 500k
/// rows — we cap the work so each pass finishes inside the cron
/// budget. Operators who need a deeper sweep can call
/// [`run_drift_check_for_column`] directly with a smaller `sample_pct`
/// and rely on the LIMIT to bound it again.
pub const MAX_SAMPLE_ROWS_PER_COLUMN: i64 = 1_000;

/// One detected mismatch. Carries the masked representations of both
/// the stored sibling AND the recomputed expected value — NEVER the
/// underlying plaintext. The audit table inherits the same posture
/// (stored masked, expected masked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftSample {
    /// Collection (table) the mismatch was found in.
    pub collection: String,
    /// Parent column whose `<col>_masked` sibling drifted.
    pub column: String,
    /// Row PK (the primary-key `id` column of the row that drifted).
    pub row_pk: String,
    /// Value stored in the sibling column. Already a masked string.
    pub stored: String,
    /// Value derived from `apply_mask_kind(decrypt(<col>), kind)`.
    /// Already a masked string (the mask transform is deterministic).
    pub expected: String,
}

/// Aggregated outcome of a drift sweep — either a single column or
/// every masked column on an app. `sampled` counts every row we
/// actually examined; `drifted` is the subset where the sibling did
/// not match the recomputed mask.
#[derive(Debug, Clone, Default)]
pub struct DriftReport {
    /// Total rows examined across every column inspected.
    pub sampled: usize,
    /// Total mismatches found.
    pub drifted: usize,
    /// Each mismatch, in the order we encountered them.
    pub samples: Vec<DriftSample>,
}

impl DriftReport {
    /// Merge `other` into `self` — used by [`run_drift_check_for_app`]
    /// when iterating over every collection × every masked column.
    pub fn merge(&mut self, other: DriftReport) {
        self.sampled += other.sampled;
        self.drifted += other.drifted;
        self.samples.extend(other.samples);
    }
}

// ---------------------------------------------------------------------------
// Scheduler hook — stub until P6a's cron loop replaces it
// ---------------------------------------------------------------------------

/// **P5.5 PR 7 / P6a hook** — register the per-app drift schedule.
///
/// The body is intentionally a debug-level log message until P6a's
/// sweeper-half (the F1 maintenance cron) lands. P6a's PR replaces
/// the body to attach an interval timer; the signature and call sites
/// stay frozen so the wiring is a single-function diff. Documenting
/// the contract here lets us land the call sites in PR 7 without
/// blocking on the scheduler crate.
///
/// `interval`: how often the drift check should fire for this app.
/// Recommended default (weekly) is the proposal §11 PR 7 line.
pub fn schedule_drift_for_app(app_id: &str, interval: std::time::Duration) {
    tracing::debug!(
        target: "zeroship_plugin_db::mask_drift",
        app_id = %app_id,
        interval_secs = interval.as_secs(),
        "drift schedule registered (P6a wiring pending)",
    );
}

// ---------------------------------------------------------------------------
// Per-app entry — iterate every collection × every masked column.
// ---------------------------------------------------------------------------

/// **P5.5 PR 7** — sweep every masked column on every collection
/// registered for `app_id`.
///
/// The schema cache is the source of truth — we only inspect
/// collections that have been registered through `db.registerModel`
/// on the current isolate. A pre-warm pass would be required if we
/// wanted to drift-check apps that haven't booted yet; PR 7 leaves
/// that to P6a (the cron loop will iterate the catalog separately).
///
/// Returns a single [`DriftReport`] aggregating every per-column
/// sweep. Errors short-circuit — a single SQL failure aborts the rest
/// of the sweep so operators see one diagnostic, not a flood.
pub async fn run_drift_check_for_app(app_id: &str) -> Result<DriftReport, DbError> {
    let cached_schemas = crate::context::with(|c| c.cached_schemas_for_app(app_id));
    let mut aggregate = DriftReport::default();

    for (collection, schema) in cached_schemas {
        let Some(schema_obj) = schema.as_object() else {
            continue;
        };
        for (col, def) in schema_obj.iter() {
            let Some(mask_meta) = def.get("mask").and_then(|v| v.as_object()) else {
                continue;
            };
            let kind = mask_meta
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("full");
            if kind == "none" {
                continue;
            }
            let report = run_drift_check_for_column(
                app_id,
                &collection,
                col,
                DEFAULT_SAMPLE_PCT,
            )
            .await?;
            aggregate.merge(report);
        }
    }

    Ok(aggregate)
}

// ---------------------------------------------------------------------------
// Per-column entry — sample rows + diff sibling
// ---------------------------------------------------------------------------

/// **P5.5 PR 7** — drift-check one masked column on one collection.
///
/// Algorithm (per the module doc-comment):
/// 1. Resolve the column's `MaskKind` + optional `EncryptionMeta` from
///    the cached schema. A column with no mask declaration returns an
///    empty report (no-op).
/// 2. Sample rows: `SELECT id, <col>, <col>_masked FROM <coll>` with
///    a backend-appropriate sampling clause + a `LIMIT
///    MAX_SAMPLE_ROWS_PER_COLUMN`. Skips rows where parent OR sibling
///    is NULL (the dual-write contract leaves both NULL together).
/// 3. For each sampled row: derive the plaintext (decrypt if
///    encrypted; else read the parent value directly), apply the mask
///    transform, compare to the stored sibling.
/// 4. Record every mismatch into `DriftReport.samples`, emit
///    `tracing::error!` + write one audit row per mismatch.
///
/// `sample_pct` is in the range `(0.0, 100.0]`. Out-of-range values
/// surface `ValidationFailed { code: "invalid_sample_pct" }`.
pub async fn run_drift_check_for_column(
    app_id: &str,
    collection: &str,
    column: &str,
    sample_pct: f64,
) -> Result<DriftReport, DbError> {
    if !sample_pct.is_finite() || sample_pct <= 0.0 || sample_pct > 100.0 {
        return Err(DbError::ValidationFailed {
            code: "invalid_sample_pct",
            message: format!(
                "drift check: sample_pct must be in (0, 100], got {sample_pct}"
            ),
            hint: None,
        });
    }

    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(DriftReport::default());
    };
    let Some(col_meta) = lookup_drift_column_meta(&schema, column)? else {
        return Ok(DriftReport::default());
    };

    // Sibling column name is the canonical `<col>_masked` shape.
    let sibling = format!("{column}_masked");

    // Sample rows. The backend arm picks PG TABLESAMPLE or the SQLite
    // random-modulo equivalent. Both share the same `(id, parent,
    // sibling)` row shape so the diff loop is backend-agnostic.
    let rows = sample_rows(app_id, collection, column, &sibling, sample_pct).await?;
    let sampled = rows.len();

    let mut report = DriftReport {
        sampled,
        drifted: 0,
        samples: Vec::new(),
    };

    for row in rows {
        let SampledRow { row_pk, parent, stored } = row;
        // Sibling NULL with non-NULL parent IS a drift case (the
        // dual-write contract guarantees both populated together), but
        // we don't have an "expected" string to put in the audit row
        // — record the diff with a sentinel.
        let stored_text = match stored {
            Some(s) => s,
            None => {
                // Parent != NULL but sibling NULL — drift. Skip
                // ahead with a `__null__` sentinel rather than calling
                // decrypt for a fictional expected value.
                let sample = DriftSample {
                    collection: collection.to_string(),
                    column: column.to_string(),
                    row_pk,
                    stored: "__null__".to_string(),
                    expected: "__nonnull_expected__".to_string(),
                };
                tracing::error!(
                    target: "zeroship_plugin_db::mask_drift",
                    app_id = %app_id,
                    collection = %collection,
                    column = %column,
                    row_pk = %sample.row_pk,
                    "mask sibling drift detected (sibling NULL on non-NULL parent)",
                );
                write_drift_audit_row(app_id, &sample, sample_pct).await?;
                report.drifted += 1;
                report.samples.push(sample);
                continue;
            }
        };

        let expected = compute_expected_masked(
            app_id,
            collection,
            column,
            &col_meta,
            &row_pk,
            &parent,
        )
        .await?;

        if expected != stored_text {
            let sample = DriftSample {
                collection: collection.to_string(),
                column: column.to_string(),
                row_pk: row_pk.clone(),
                stored: stored_text.clone(),
                expected: expected.clone(),
            };
            tracing::error!(
                target: "zeroship_plugin_db::mask_drift",
                app_id = %app_id,
                collection = %collection,
                column = %column,
                row_pk = %row_pk,
                "mask sibling drift detected",
            );
            write_drift_audit_row(app_id, &sample, sample_pct).await?;
            report.drifted += 1;
            report.samples.push(sample);
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Column-meta lookup
// ---------------------------------------------------------------------------

/// Resolved per-column metadata the drift check needs: the masked
/// kind + (optional) encryption metadata. `wraps` is captured because
/// `compute_expected_masked` needs it to interpret the decrypted bytes.
#[derive(Debug)]
struct DriftColumnMeta {
    kind: MaskKind,
    /// `Some` iff the column is `t.encrypted(...)`-declared; carries
    /// the metadata `compute_expected_masked` needs to decrypt.
    enc: Option<EncMeta>,
}

#[derive(Debug)]
#[allow(dead_code)]
struct EncMeta {
    mode: crate::backend::EncryptionMode,
    key_id: String,
    wraps: &'static str,
}

fn lookup_drift_column_meta(
    schema: &Value,
    column: &str,
) -> Result<Option<DriftColumnMeta>, DbError> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(None);
    };
    let Some(def) = schema_obj.get(column) else {
        return Ok(None);
    };
    let Some(mask_meta) = def.get("mask").and_then(|v| v.as_object()) else {
        return Ok(None);
    };
    let kind_str = mask_meta
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    if kind_str == "none" {
        return Ok(None);
    }
    let kind = parse_mask_kind_str(kind_str)?;

    let enc = if let Some(enc_meta) = def.get("encrypted").and_then(|v| v.as_object()) {
        let mode = match enc_meta.get("mode").and_then(|v| v.as_str()) {
            Some("randomised") | Some("randomized") => crate::backend::EncryptionMode::Randomised,
            Some("deterministic") => crate::backend::EncryptionMode::Deterministic,
            other => {
                return Err(DbError::internal(format!(
                    "drift_check: encrypted.mode must be 'randomised' or \
                     'deterministic', got {other:?}"
                )));
            }
        };
        let key_id = enc_meta
            .get("keyId")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let wraps = match enc_meta.get("wraps").and_then(|v| v.as_str()) {
            Some("number") => "number",
            Some("bytes") => "bytes",
            _ => "string",
        };
        Some(EncMeta { mode, key_id, wraps })
    } else {
        None
    };

    Ok(Some(DriftColumnMeta { kind, enc }))
}

/// Re-implementation of the schema-wire MaskKind parser. Duplicated
/// here rather than `pub`-exporting `mask_pass::parse_mask_kind` so
/// the drift module doesn't bleed crate-private detail across the
/// `crud` boundary. The set is small and stable.
fn parse_mask_kind_str(s: &str) -> Result<MaskKind, DbError> {
    Ok(match s {
        "full" => MaskKind::Full,
        "last4" => MaskKind::Last4,
        "first4" => MaskKind::First4,
        "email" => MaskKind::Email,
        "name" => MaskKind::Name,
        "dateYear" | "date-year" => MaskKind::DateYear,
        "dateDecade" | "date-decade" => MaskKind::DateDecade,
        "none" => MaskKind::None,
        other => {
            return Err(DbError::internal(format!(
                "drift_check: unknown mask kind {other:?}"
            )));
        }
    })
}

// ---------------------------------------------------------------------------
// Sampled-row shape + sampling SQL
// ---------------------------------------------------------------------------

struct SampledRow {
    row_pk: String,
    /// Parent column's wire form. PG BYTEA arrives as `\xHH…` hex
    /// strings; SQLite BLOB arrives as base64 strings; plaintext
    /// columns arrive as plain strings. `compute_expected_masked`
    /// interprets accordingly.
    parent: ParentValue,
    /// Sibling column's text. `None` when the row's sibling is NULL —
    /// drift if the parent is non-NULL.
    stored: Option<String>,
}

/// Parent column shape after sampling. Drift only inspects rows with
/// a non-NULL parent (NULL parents have NULL siblings by design — the
/// `apply_mask_on_write` Q-MASK-L pass-through).
enum ParentValue {
    /// Plaintext column (`t.string().mask({...})`).
    Plain(String),
    /// Encrypted column: PG `\xHH…` hex string of the BYTEA.
    PgHex(String),
    /// Encrypted column: SQLite BLOB returned as raw bytes (we read
    /// blobs as `TypedCell::Blob` from the typed session API so no
    /// base64 intermediate is needed).
    #[cfg(feature = "sqlite")]
    SqliteBlob(Vec<u8>),
}

async fn sample_rows(
    app_id: &str,
    collection: &str,
    column: &str,
    sibling: &str,
    sample_pct: f64,
) -> Result<Vec<SampledRow>, DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    // ---- PG arm ----
    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_postgres() {
            return sample_rows_pg(pg, app_id, collection, column, sibling, sample_pct).await;
        }
    }

    // ---- SQLite arm ----
    #[cfg(feature = "sqlite")]
    {
        if let Some(sq) = backend.as_sqlite() {
            return sample_rows_sqlite(sq, app_id, collection, column, sibling, sample_pct).await;
        }
    }

    let _ = backend;
    let _ = sibling;
    let _ = sample_pct;
    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "db: no backend arm available for drift sampling".to_string(),
        hint: None,
    })
}

#[cfg(feature = "pg")]
async fn sample_rows_pg(
    pg: &crate::backend::PostgresBackend,
    app_id: &str,
    collection: &str,
    column: &str,
    sibling: &str,
    sample_pct: f64,
) -> Result<Vec<SampledRow>, DbError> {
    use crate::backend::PgSqlExecutor as _;

    let pool = pg.pool_handle();
    // TABLESAMPLE BERNOULLI accepts a numeric in the range [0, 100].
    // We render to a literal because Postgres rejects parameterised
    // TABLESAMPLE arguments. `sample_pct` is validated upstream to
    // sit in (0, 100].
    let sql = format!(
        r#"SELECT id, "{column}", "{sibling}"
           FROM "{app_id}"."{collection}"
           TABLESAMPLE BERNOULLI ({sample_pct})
           WHERE "{column}" IS NOT NULL
           LIMIT {limit}"#,
        limit = MAX_SAMPLE_ROWS_PER_COLUMN
    );
    let empty: Vec<&str> = Vec::new();
    let rows = pool
        .query_text_params(&sql, &empty)
        .await
        .map_err(|e| crate::error::DbError::from_pg(&e))?;

    let mut out: Vec<SampledRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Option<&str> = row
            .try_get::<_, Option<&str>>(0)
            .map_err(|e| DbError::internal(format!("drift_check: get id: {e}")))?;
        let parent: Option<&str> = row
            .try_get::<_, Option<&str>>(1)
            .map_err(|e| DbError::internal(format!("drift_check: get parent: {e}")))?;
        let sib: Option<&str> = row
            .try_get::<_, Option<&str>>(2)
            .map_err(|e| DbError::internal(format!("drift_check: get sibling: {e}")))?;
        let Some(id_s) = id else { continue };
        let Some(parent_s) = parent else { continue };
        // BYTEA values surface as `\xHHHH…` from compio-postgres' text
        // protocol; everything else is a plain string. The presence
        // of the `\x` prefix is the cheapest discriminator on the
        // text path.
        let parent_val = if parent_s.starts_with("\\x") {
            ParentValue::PgHex(parent_s.to_string())
        } else {
            ParentValue::Plain(parent_s.to_string())
        };
        out.push(SampledRow {
            row_pk: id_s.to_string(),
            parent: parent_val,
            stored: sib.map(str::to_string),
        });
    }
    Ok(out)
}

#[cfg(feature = "sqlite")]
async fn sample_rows_sqlite(
    sq: &crate::backend::sqlite::SqliteBackend,
    app_id: &str,
    collection: &str,
    column: &str,
    sibling: &str,
    sample_pct: f64,
) -> Result<Vec<SampledRow>, DbError> {
    use crate::backend::sqlite::session::TypedCell;
    use crate::backend::{DialectBuilder as _, SqlExecutor as _};

    let q_app = sq.quote_ident(app_id);
    let q_coll = sq.quote_ident(collection);
    let q_col = sq.quote_ident(column);
    let q_sib = sq.quote_ident(sibling);
    // SQLite has no native TABLESAMPLE. `abs(random()) % 100 < pct`
    // gives equivalent per-row Bernoulli selection. We bake `pct` as
    // a literal (validated upstream) for the same parameterisation
    // reason as the PG arm.
    let pct_int = sample_pct.clamp(0.0, 100.0) as i64;
    let sql = format!(
        "SELECT id, {q_col}, {q_sib} \
         FROM {q_app}.{q_coll} \
         WHERE {q_col} IS NOT NULL \
           AND (abs(random()) % 100) < {pct_int} \
         LIMIT {limit}",
        limit = MAX_SAMPLE_ROWS_PER_COLUMN
    );

    let handle = sq.acquire_dedicated_client().await?;
    let typed = handle.query_typed_internal(&sql, &[]).await?;
    let mut out: Vec<SampledRow> = Vec::with_capacity(typed.rows.len());
    for cells in typed.rows {
        // id, parent, sibling — in that column order.
        if cells.len() < 3 {
            return Err(DbError::internal(
                "drift_check: expected 3 cells per row (id, parent, sibling)",
            ));
        }
        let row_pk = match &cells[0] {
            TypedCell::Text(s) => s.clone(),
            TypedCell::Integer(n) => n.to_string(),
            TypedCell::Null => continue,
            other => {
                return Err(DbError::internal(format!(
                    "drift_check: id cell shape {other:?}"
                )));
            }
        };
        let parent_val = match &cells[1] {
            TypedCell::Null => continue,
            TypedCell::Text(s) => ParentValue::Plain(s.clone()),
            TypedCell::Integer(n) => ParentValue::Plain(n.to_string()),
            TypedCell::Real(f) => ParentValue::Plain(f.to_string()),
            TypedCell::Blob(b) => ParentValue::SqliteBlob(b.clone()),
        };
        let stored = match &cells[2] {
            TypedCell::Null => None,
            TypedCell::Text(s) => Some(s.clone()),
            other => {
                return Err(DbError::internal(format!(
                    "drift_check: sibling cell expected TEXT or NULL, got {other:?}"
                )));
            }
        };
        out.push(SampledRow { row_pk, parent: parent_val, stored });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Expected-masked computation
// ---------------------------------------------------------------------------

/// Compute `apply_mask_kind(decrypt(parent), kind)` for one row.
///
/// - Plaintext column: parent arrives as `ParentValue::Plain`; we
///   feed it directly to `apply_mask_kind`.
/// - Encrypted column: decrypt through the backend-arm
///   `EncryptedColumn` impl using the column's `EncryptionMode`-
///   appropriate AAD (Randomised binds `row_pk`; Deterministic
///   omits it). Plaintext bytes are then decoded per `wraps`
///   (string → UTF-8, number → f64, bytes → base64) so the result
///   is the same string `apply_mask_kind` would see if the original
///   write had used the plaintext directly.
async fn compute_expected_masked(
    app_id: &str,
    collection: &str,
    column: &str,
    meta: &DriftColumnMeta,
    row_pk: &str,
    parent: &ParentValue,
) -> Result<String, DbError> {
    // Plaintext column path — feed the parent's text directly through
    // the mask transform.
    if meta.enc.is_none() {
        let s = match parent {
            ParentValue::Plain(s) => s.clone(),
            // A plaintext-marked column that arrives as bytes is a
            // schema-vs-data mismatch — surface loud rather than
            // silently masking a guess.
            ParentValue::PgHex(_) => {
                return Err(DbError::internal(format!(
                    "drift_check: column '{column}' not encrypted but parent \
                     arrived as PG BYTEA hex"
                )));
            }
            #[cfg(feature = "sqlite")]
            ParentValue::SqliteBlob(_) => {
                return Err(DbError::internal(format!(
                    "drift_check: column '{column}' not encrypted but parent \
                     arrived as SQLite BLOB"
                )));
            }
        };
        return Ok(apply_mask_kind(meta.kind, &s));
    }

    // Encrypted column path — decrypt + decode-per-wraps + mask.
    let enc = meta.enc.as_ref().expect("checked just above");
    let plaintext_bytes = decrypt_parent_value(app_id, collection, column, enc, row_pk, parent).await?;
    let plaintext_string = decode_plaintext_per_wraps(&plaintext_bytes, enc.wraps)?;
    Ok(apply_mask_kind(meta.kind, &plaintext_string))
}

#[allow(unused_variables)]
async fn decrypt_parent_value(
    app_id: &str,
    collection: &str,
    column: &str,
    enc: &EncMeta,
    row_pk: &str,
    parent: &ParentValue,
) -> Result<Vec<u8>, DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    let aad = crate::encryption::aad::canonical_aad(
        collection,
        column,
        match enc.mode {
            crate::backend::EncryptionMode::Randomised => Some(row_pk.as_bytes()),
            crate::backend::EncryptionMode::Deterministic => None,
        },
    );

    // ---- PG arm ----
    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_encrypted_column_pg() {
            use crate::backend::EncryptedColumn as _;
            let hex_str = match parent {
                ParentValue::PgHex(s) => s.as_str(),
                ParentValue::Plain(_) => {
                    return Err(DbError::internal(
                        "drift_check: encrypted column on PG arrived as plain text",
                    ));
                }
                #[cfg(feature = "sqlite")]
                ParentValue::SqliteBlob(_) => {
                    return Err(DbError::internal(
                        "drift_check: PG path received SQLite BLOB value",
                    ));
                }
            };
            let bytes = hex_to_bytes(hex_str)?;
            let key = pg.resolve_key(app_id, &enc.key_id).await?;
            return Ok(pg.decrypt(&key, enc.mode, &bytes, &aad)?);
        }
    }

    // ---- SQLite arm ----
    #[cfg(feature = "sqlite")]
    {
        if let Some(sq) = backend.as_encrypted_column_sqlite() {
            use crate::backend::EncryptedColumn as _;
            let bytes: &[u8] = match parent {
                ParentValue::SqliteBlob(b) => b.as_slice(),
                ParentValue::PgHex(_) | ParentValue::Plain(_) => {
                    return Err(DbError::internal(
                        "drift_check: encrypted column on SQLite arrived in non-BLOB shape",
                    ));
                }
            };
            let key = sq.resolve_key(app_id, &enc.key_id).await?;
            return Ok(sq.decrypt(&key, enc.mode, bytes, &aad)?);
        }
    }

    let _ = backend;
    let _ = aad;
    Err(DbError::Configuration {
        code: "encryption_unavailable",
        message: "drift_check: encryption surface not available on this build".to_string(),
        hint: Some(
            "rebuild with `--features pg` or `--features sqlite`".into(),
        ),
    })
}

#[allow(dead_code)]
fn decode_plaintext_per_wraps(bytes: &[u8], wraps: &str) -> Result<String, DbError> {
    use base64::Engine as _;
    match wraps {
        "string" => Ok(std::str::from_utf8(bytes)
            .map_err(|e| DbError::internal(format!("drift_check: plaintext not UTF-8: {e}")))?
            .to_string()),
        "number" => {
            if bytes.len() != 8 {
                return Err(DbError::internal(format!(
                    "drift_check: number plaintext must be 8 bytes, got {}",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            Ok(f64::from_be_bytes(arr).to_string())
        }
        "bytes" => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
        other => Err(DbError::internal(format!(
            "drift_check: unknown wraps '{other}'"
        ))),
    }
}

/// Mirrors `unmask::hex_to_bytes` — small enough to duplicate rather
/// than `pub`-exporting across the crud boundary.
#[allow(dead_code)]
fn hex_to_bytes(s: &str) -> Result<Vec<u8>, DbError> {
    let hex = s.strip_prefix("\\x").unwrap_or(s);
    if hex.len() % 2 != 0 {
        return Err(DbError::internal(format!(
            "drift_check: BYTEA text has odd hex length: {}",
            hex.len()
        )));
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..bytes.len()).step_by(2) {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

#[allow(dead_code)]
fn nibble(c: u8) -> Result<u8, DbError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(DbError::internal(format!(
            "drift_check: BYTEA text contains non-hex byte 0x{c:02x}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Drift audit table — per-app, lazy CREATE TABLE IF NOT EXISTS
// ---------------------------------------------------------------------------

/// Write one row into `<app>.__zeroship_audit_mask_drift`. The audit
/// table lives in the per-app schema (PG) / per-app database (SQLite)
/// alongside `__zeroship_audit_unmask`, not in `__zeroship_admin`.
///
/// We deliberately do NOT polute the existing `__zeroship_audit_unmask`
/// table: drift fires periodically and would drown the unmask audit
/// log in detection events. Keeping the tables separate also lets a
/// future per-app dashboard query each cleanly without per-row
/// outcome-discriminator filters.
async fn write_drift_audit_row(
    app_id: &str,
    sample: &DriftSample,
    sample_pct: f64,
) -> Result<(), DbError> {
    ensure_drift_audit_table(app_id).await?;
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    // ---- PG arm ----
    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_postgres() {
            use crate::backend::PgSqlExecutor as _;
            let pool = pg.pool_handle();
            let sql = format!(
                r#"INSERT INTO "{app_id}"."__zeroship_audit_mask_drift"
                   (collection, column_name, row_pk, sample_pct, stored_masked, expected_masked)
                   VALUES ($1, $2, $3, $4, $5, $6)
                   ON CONFLICT (detected_at, collection, column_name, row_pk) DO NOTHING"#
            );
            let pct_s = format!("{sample_pct}");
            pool.query_text_params(
                &sql,
                &[
                    &sample.collection,
                    &sample.column,
                    &sample.row_pk,
                    &pct_s,
                    &sample.stored,
                    &sample.expected,
                ],
            )
            .await
            .map_err(|e| crate::error::DbError::from_pg(&e))?;
            return Ok(());
        }
    }

    // ---- SQLite arm ----
    #[cfg(feature = "sqlite")]
    {
        if let Some(sq) = backend.as_sqlite() {
            use crate::backend::{DialectBuilder as _, SqlExecutor as _};
            let q_app = sq.quote_ident(app_id);
            let sql = format!(
                r#"INSERT OR IGNORE INTO {q_app}."__zeroship_audit_mask_drift"
                   (collection, column_name, row_pk, sample_pct, stored_masked, expected_masked)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#
            );
            let pct_s = format!("{sample_pct}");
            sq.pool_exec(
                &sql,
                &[
                    sample.collection.as_str(),
                    sample.column.as_str(),
                    sample.row_pk.as_str(),
                    pct_s.as_str(),
                    sample.stored.as_str(),
                    sample.expected.as_str(),
                ],
            )
            .await?;
            return Ok(());
        }
    }

    let _ = backend;
    let _ = sample;
    let _ = sample_pct;
    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "drift_check: no backend arm available for drift audit".to_string(),
        hint: None,
    })
}

/// `CREATE TABLE IF NOT EXISTS <app>.__zeroship_audit_mask_drift`.
/// Idempotent — the cost on subsequent calls is a catalog probe.
///
/// Schema (PG):
/// ```sql
/// CREATE TABLE "<app>".__zeroship_audit_mask_drift (
///     detected_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
///     collection      TEXT NOT NULL,
///     column_name     TEXT NOT NULL,
///     row_pk          TEXT NOT NULL,
///     sample_pct      REAL NOT NULL,
///     stored_masked   TEXT NOT NULL,
///     expected_masked TEXT NOT NULL,
///     PRIMARY KEY (detected_at, collection, column_name, row_pk)
/// );
/// ```
async fn ensure_drift_audit_table(app_id: &str) -> Result<(), DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    // ---- PG arm ----
    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_postgres() {
            use crate::backend::PgSqlExecutor as _;
            let pool = pg.pool_handle();
            let sql = format!(
                r#"CREATE TABLE IF NOT EXISTS "{app_id}"."__zeroship_audit_mask_drift" (
                    detected_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    collection      TEXT NOT NULL,
                    column_name     TEXT NOT NULL,
                    row_pk          TEXT NOT NULL,
                    sample_pct      REAL NOT NULL,
                    stored_masked   TEXT NOT NULL,
                    expected_masked TEXT NOT NULL,
                    PRIMARY KEY (detected_at, collection, column_name, row_pk)
                )"#
            );
            let empty: Vec<&str> = Vec::new();
            pool.query_text_params(&sql, &empty)
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            let idx_coll = format!(
                r#"CREATE INDEX IF NOT EXISTS "__zeroship_audit_mask_drift_coll_idx"
                   ON "{app_id}"."__zeroship_audit_mask_drift" (collection, column_name, detected_at)"#
            );
            pool.query_text_params(&idx_coll, &empty)
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            return Ok(());
        }
    }

    // ---- SQLite arm ----
    #[cfg(feature = "sqlite")]
    {
        if let Some(sq) = backend.as_sqlite() {
            use crate::backend::{DialectBuilder as _, SqlExecutor as _};
            let q_app = sq.quote_ident(app_id);
            // SQLite mirror:
            //   - TIMESTAMPTZ → TEXT with CURRENT_TIMESTAMP default
            //   - REAL → REAL (native)
            //   - composite PK identical
            let sql = format!(
                r#"CREATE TABLE IF NOT EXISTS {q_app}."__zeroship_audit_mask_drift" (
                    detected_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    collection      TEXT NOT NULL,
                    column_name     TEXT NOT NULL,
                    row_pk          TEXT NOT NULL,
                    sample_pct      REAL NOT NULL,
                    stored_masked   TEXT NOT NULL,
                    expected_masked TEXT NOT NULL,
                    PRIMARY KEY (detected_at, collection, column_name, row_pk)
                )"#
            );
            sq.pool_exec(&sql, &[]).await?;
            let idx_coll = format!(
                r#"CREATE INDEX IF NOT EXISTS {q_app}."__zeroship_audit_mask_drift_coll_idx"
                   ON "__zeroship_audit_mask_drift" (collection, column_name, detected_at)"#
            );
            sq.pool_exec(&idx_coll, &[]).await?;
            return Ok(());
        }
    }

    let _ = backend;
    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "drift_check: no backend arm available to provision __zeroship_audit_mask_drift"
            .to_string(),
        hint: None,
    })
}

/// Helper for tests: read back every audit row for `app_id`. `pub`
/// under `test-helpers` so integration tests can pin the audit
/// contract without re-implementing the SELECT.
///
/// Returns an empty `Vec` if the `__zeroship_audit_mask_drift` table
/// hasn't been created yet (no drift fired → the lazy
/// `ensure_drift_audit_table` never ran). Operators reading the audit
/// log in production see an "unknown table" SQL error; the test
/// helper specifically tolerates the missing-table case so a
/// zero-drift run can still assert `audit.is_empty()` without
/// pre-provisioning the table.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn read_drift_audit_rows_for_tests(
    app_id: &str,
) -> Result<Vec<(String, String, String, String, String)>, DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    #[cfg(feature = "sqlite")]
    {
        if let Some(sq) = backend.as_sqlite() {
            use crate::backend::{DialectBuilder as _, SqlExecutor as _};
            let q_app = sq.quote_ident(app_id);
            let sql = format!(
                r#"SELECT collection, column_name, row_pk, stored_masked, expected_masked
                   FROM {q_app}."__zeroship_audit_mask_drift"
                   ORDER BY detected_at, collection, column_name, row_pk"#
            );
            let handle = sq.acquire_dedicated_client().await?;
            let rows = match handle.query_internal(&sql, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    // The lazy table-create only fires when drift
                    // writes a row; a zero-drift run leaves it
                    // missing. Surface as "no rows" so the test
                    // doesn't have to pre-provision.
                    let msg = format!("{e}");
                    if msg.contains("no such table") || msg.contains("does not exist") {
                        return Ok(Vec::new());
                    }
                    return Err(e);
                }
            };
            return Ok(rows
                .into_iter()
                .map(|r| {
                    (
                        r.first().cloned().flatten().unwrap_or_default(),
                        r.get(1).cloned().flatten().unwrap_or_default(),
                        r.get(2).cloned().flatten().unwrap_or_default(),
                        r.get(3).cloned().flatten().unwrap_or_default(),
                        r.get(4).cloned().flatten().unwrap_or_default(),
                    )
                })
                .collect());
        }
    }

    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_postgres() {
            use crate::backend::PgSqlExecutor as _;
            let pool = pg.pool_handle();
            let sql = format!(
                r#"SELECT collection, column_name, row_pk, stored_masked, expected_masked
                   FROM "{app_id}"."__zeroship_audit_mask_drift"
                   ORDER BY detected_at, collection, column_name, row_pk"#
            );
            let empty: Vec<&str> = Vec::new();
            let rows = pool
                .query_text_params(&sql, &empty)
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            return Ok(rows
                .into_iter()
                .map(|r| {
                    let get = |i: usize| -> String {
                        r.try_get::<_, Option<&str>>(i)
                            .ok()
                            .flatten()
                            .map(str::to_string)
                            .unwrap_or_default()
                    };
                    (get(0), get(1), get(2), get(3), get(4))
                })
                .collect());
        }
    }
    let _ = backend;
    Ok(Vec::new())
}

// ---------------------------------------------------------------------------
// Unit tests — pure helpers (no backend round-trip)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drift_report_default_is_empty() {
        let r = DriftReport::default();
        assert_eq!(r.sampled, 0);
        assert_eq!(r.drifted, 0);
        assert!(r.samples.is_empty());
    }

    #[test]
    fn drift_report_merge_combines_counts_and_samples() {
        let mut a = DriftReport {
            sampled: 100,
            drifted: 1,
            samples: vec![DriftSample {
                collection: "users".into(),
                column: "ssn".into(),
                row_pk: "u1".into(),
                stored: "***".into(),
                expected: "***-**-1234".into(),
            }],
        };
        let b = DriftReport {
            sampled: 50,
            drifted: 2,
            samples: vec![
                DriftSample {
                    collection: "users".into(),
                    column: "email".into(),
                    row_pk: "u2".into(),
                    stored: "a***@x.com".into(),
                    expected: "b***@x.com".into(),
                },
                DriftSample {
                    collection: "users".into(),
                    column: "email".into(),
                    row_pk: "u3".into(),
                    stored: "c***@x.com".into(),
                    expected: "d***@x.com".into(),
                },
            ],
        };
        a.merge(b);
        assert_eq!(a.sampled, 150);
        assert_eq!(a.drifted, 3);
        assert_eq!(a.samples.len(), 3);
    }

    #[test]
    fn lookup_drift_column_meta_plaintext_masked_column() {
        let schema = json!({
            "email": {
                "type": "string",
                "mask": { "kind": "email", "classification": "pii" }
            }
        });
        let meta = lookup_drift_column_meta(&schema, "email").unwrap().unwrap();
        assert!(matches!(meta.kind, MaskKind::Email));
        assert!(meta.enc.is_none());
    }

    #[test]
    fn lookup_drift_column_meta_encrypted_masked_column() {
        let schema = json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": "k1", "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let meta = lookup_drift_column_meta(&schema, "ssn").unwrap().unwrap();
        assert!(matches!(meta.kind, MaskKind::Last4));
        let enc = meta.enc.expect("encryption metadata");
        assert!(matches!(enc.mode, crate::backend::EncryptionMode::Randomised));
        assert_eq!(enc.key_id, "k1");
        assert_eq!(enc.wraps, "string");
    }

    #[test]
    fn lookup_drift_column_meta_kind_none_returns_none() {
        let schema = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        assert!(lookup_drift_column_meta(&schema, "ssn").unwrap().is_none());
    }

    #[test]
    fn lookup_drift_column_meta_no_mask_returns_none() {
        let schema = json!({
            "name": { "type": "string" }
        });
        assert!(lookup_drift_column_meta(&schema, "name").unwrap().is_none());
    }

    #[test]
    fn lookup_drift_column_meta_rejects_unknown_encryption_mode() {
        let schema = json!({
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "weird", "keyId": "k1", "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let err = lookup_drift_column_meta(&schema, "ssn").unwrap_err();
        match err {
            DbError::Internal { .. } => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn parse_mask_kind_str_accepts_all_named() {
        for (s, k) in [
            ("full", MaskKind::Full),
            ("last4", MaskKind::Last4),
            ("first4", MaskKind::First4),
            ("email", MaskKind::Email),
            ("name", MaskKind::Name),
            ("dateYear", MaskKind::DateYear),
            ("date-year", MaskKind::DateYear),
            ("dateDecade", MaskKind::DateDecade),
            ("date-decade", MaskKind::DateDecade),
            ("none", MaskKind::None),
        ] {
            let got = parse_mask_kind_str(s).unwrap();
            assert!(
                std::mem::discriminant(&got) == std::mem::discriminant(&k),
                "mask kind {s} round-trip mismatch"
            );
        }
    }

    #[test]
    fn parse_mask_kind_str_rejects_unknown() {
        let err = parse_mask_kind_str("absurdly-novel").unwrap_err();
        match err {
            DbError::Internal { .. } => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn decode_plaintext_per_wraps_string_round_trip() {
        let out = decode_plaintext_per_wraps(b"hello", "string").unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn decode_plaintext_per_wraps_number_round_trip() {
        let bytes = 1.5f64.to_be_bytes();
        let out = decode_plaintext_per_wraps(&bytes, "number").unwrap();
        assert_eq!(out, "1.5");
    }

    #[test]
    fn decode_plaintext_per_wraps_bytes_base64() {
        let out = decode_plaintext_per_wraps(&[0xde, 0xad, 0xbe, 0xef], "bytes").unwrap();
        assert_eq!(out, "3q2+7w==");
    }

    #[test]
    fn decode_plaintext_per_wraps_rejects_unknown() {
        let err = decode_plaintext_per_wraps(b"x", "weird").unwrap_err();
        match err {
            DbError::Internal { .. } => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn hex_to_bytes_round_trip() {
        assert_eq!(hex_to_bytes("\\xdeadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(hex_to_bytes("\\x").unwrap(), Vec::<u8>::new());
        assert_eq!(hex_to_bytes("deadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn hex_to_bytes_odd_length_rejected() {
        let err = hex_to_bytes("\\xabc").unwrap_err();
        match err {
            DbError::Internal { message, .. } => assert!(message.contains("odd")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn run_drift_check_for_column_rejects_sample_pct_out_of_range() {
        let app = "drift_unit_pct_oor_app";
        let coll = "users";
        let column = "ssn";
        // No backend installed — error is the sample_pct check, which
        // happens BEFORE the schema lookup.
        let runtime = compio::runtime::Runtime::new().unwrap();
        for bad in [-0.1f64, 0.0, 100.1, f64::NAN, f64::INFINITY] {
            let err = runtime
                .block_on(run_drift_check_for_column(app, coll, column, bad))
                .unwrap_err();
            match err {
                DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "invalid_sample_pct", "bad pct {bad} accepted");
                }
                other => panic!("expected ValidationFailed for pct {bad}, got {other:?}"),
            }
        }
    }

    #[test]
    fn run_drift_check_for_column_returns_empty_when_no_schema() {
        // No schema cached for this app — drift check is a no-op.
        let app = "drift_unit_no_schema_app";
        let runtime = compio::runtime::Runtime::new().unwrap();
        let report = runtime
            .block_on(run_drift_check_for_column(app, "users", "ssn", 1.0))
            .unwrap();
        assert_eq!(report.sampled, 0);
        assert_eq!(report.drifted, 0);
        assert!(report.samples.is_empty());
    }

    #[test]
    fn run_drift_check_for_column_returns_empty_when_column_not_masked() {
        // Schema cached but `ssn` carries no `mask` declaration.
        let app = "drift_unit_no_mask_app";
        let collection = "users";
        crate::context::with_mut(|c| {
            c.cache_schema(
                app,
                collection,
                json!({ "ssn": { "type": "string" } }),
            )
        });
        let runtime = compio::runtime::Runtime::new().unwrap();
        let report = runtime
            .block_on(run_drift_check_for_column(app, collection, "ssn", 1.0))
            .unwrap();
        assert_eq!(report.sampled, 0);
        assert_eq!(report.drifted, 0);
    }
}
