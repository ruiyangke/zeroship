//! **P5.5 PR 4** — `unmask()` RPC + authorization stub + audit trail.
//!
//! The MaskedValue surface (`sdks/db/src/types.ts`) calls into this
//! module via the `zeroship.db.unmaskField` native op (registered as a
//! `#[v8_method]` on `Db`). The flow follows §6 of
//! `docs/proposals/sensitive-field-masking.md`:
//!
//! 1. Look up the column's mask metadata (classification) from the
//!    cached schema. A column with no mask declaration cannot be
//!    unmasked — surface `unmask_column_not_masked`.
//! 2. Authorise via [`check_unmask_authorization`]. PR 4 ships a strict
//!    default-deny stub: only the `auto` actor kind (system / migrations
//!    / background jobs) can unmask any classification. PR 5 will
//!    replace this with a per-app [`MaskPolicy`] lookup. Denied attempts
//!    emit an audit row with `outcome = "denied"`.
//! 3. Look up the column's encryption metadata. If encrypted, SELECT
//!    the BYTEA / BLOB ciphertext, reconstruct the canonical AAD
//!    (Camp A — row_pk in AAD for Randomised, omitted for Deterministic),
//!    and decrypt through the backend's [`crate::backend::EncryptedColumn`]
//!    impl. If plaintext (mask-only, no encryption), SELECT the parent
//!    column directly.
//! 4. Emit a `granted`-outcome audit row to `__zeroship_audit_unmask`
//!    (per-app schema, NOT in `__zeroship_admin`).
//! 5. Return the plaintext.
//!
//! ## Audit-table location
//!
//! `__zeroship_audit_unmask` lives in the **per-app schema** (alongside
//! `__zeroship_migrations`), not in the platform-wide `__zeroship_admin`
//! schema. App-scoped audit data should not require platform-role
//! access to query — operators query via the per-app schema.
//!
//! ## Default-deny authorization stub
//!
//! PR 4's [`check_unmask_authorization`] is deliberately strict: most
//! real apps will need PR 5's `defineMaskPolicy()` to grant access.
//! PR 4 ships the machinery (RPC + audit + decrypt); PR 5 makes it
//! useful for non-system callers.

use base64::Engine as _;
use serde_json::Value;

use crate::error::DbError;

// ---------------------------------------------------------------------------
// Args / result shape
// ---------------------------------------------------------------------------

/// Inputs to `dispatch_unmask`. Mirrors the `MaskedValue._meta` payload
/// the SDK ships through `zeroship.db.unmaskField({...})`. The `actor`
/// argument is currently the bare `Actor = Record<string, unknown>`
/// shape (PR 1 placeholder); PR 5 will tighten it once
/// `defineMaskPolicy()` lands and the policy lookup needs structured
/// fields. PR 4 only inspects `actor.kind` and `actor.id`.
///
/// `pub` under `test-helpers` so `tests/sqlite_integration.rs` can drive
/// `dispatch_unmask` directly; production callers reach this through
/// the V8 dispatcher's `parse_args`.
#[derive(Debug, Clone)]
pub struct UnmaskFieldArgs {
    pub collection: String,
    pub row_pk: String,
    pub column: String,
    /// Opaque actor descriptor. PR 4 inspects `actor.kind` and `actor.id`
    /// only; PR 5 will widen the surface. `None` → unauthenticated
    /// caller (default-denied by the stub).
    pub actor: Option<Value>,
    /// Free-text rationale recorded on the audit row. Truncation is the
    /// caller's responsibility — long reasons are stored verbatim.
    pub reason: Option<String>,
}

/// Result of a successful unmask. The wire shape is `{ plaintext: <str> }`;
/// for `wraps = "bytes"` the SDK base64-decodes the string. The Rust
/// side never carries raw `Vec<u8>` over the V8 boundary because
/// `serde_json::Value` cannot represent binary directly.
///
/// Same visibility rationale as [`UnmaskFieldArgs`].
#[derive(Debug, Clone)]
pub struct UnmaskFieldResult {
    pub plaintext: String,
}

// ---------------------------------------------------------------------------
// Mask / encryption metadata lookup
// ---------------------------------------------------------------------------

/// Cached schema record for a single column. Pulled from the per-isolate
/// schema cache populated by `db.registerModel` (cached via
/// `context::cache_schema`).
struct ColumnMaskMeta {
    classification: String,
}

/// Walk the cached schema and return the mask metadata for `(collection,
/// column)`, or `None` if the column has no `mask` block (or is opted
/// out via `kind: "none"`).
fn lookup_mask_meta(
    app_id: &str,
    collection: &str,
    column: &str,
) -> Result<Option<ColumnMaskMeta>, DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(None);
    };
    let Some(obj) = schema.as_object() else {
        return Ok(None);
    };
    let Some(def) = obj.get(column) else {
        return Ok(None);
    };
    let Some(mask_meta) = def.get("mask").and_then(|v| v.as_object()) else {
        return Ok(None);
    };
    let kind = mask_meta.get("kind").and_then(|v| v.as_str()).unwrap_or("full");
    if kind == "none" {
        // Explicit opt-out — the parent column stays plaintext on read,
        // so unmask has nothing to do (and there's no `MaskedValue` for
        // the SDK to construct an `.unmask()` call from). Surface as
        // "not masked" so the caller sees the same error code they
        // would for a non-masked column.
        return Ok(None);
    }
    let classification = mask_meta
        .get("classification")
        .and_then(|v| v.as_str())
        .unwrap_or("pii")
        .to_string();
    Ok(Some(ColumnMaskMeta { classification }))
}

/// Encryption metadata for the target column (when present). The
/// `key_id` and `wraps` fields are only consumed under the
/// `feature = "pg" + hardening` or `feature = "sqlite"` arms of
/// `fetch_and_decrypt`; the bare default-feature build never reads
/// them. `#[allow(dead_code)]` keeps the build clean without
/// duplicating the metadata struct per arm.
#[allow(dead_code)]
struct ColumnEncryptionMeta {
    mode: crate::backend::EncryptionMode,
    key_id: String,
    wraps: &'static str,
}

/// Walk the cached schema and return the encryption metadata for
/// `(collection, column)`, or `None` if the column has no `encrypted`
/// block (mask-only / plaintext-storage case).
fn lookup_encryption_meta(
    app_id: &str,
    collection: &str,
    column: &str,
) -> Result<Option<ColumnEncryptionMeta>, DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(None);
    };
    let Some(obj) = schema.as_object() else {
        return Ok(None);
    };
    let Some(def) = obj.get(column) else {
        return Ok(None);
    };
    let Some(enc_meta) = def.get("encrypted").and_then(|v| v.as_object()) else {
        return Ok(None);
    };
    let mode_str = enc_meta
        .get("mode")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::internal("unmask: encrypted.mode missing in schema"))?;
    let mode = match mode_str {
        "randomised" | "randomized" => crate::backend::EncryptionMode::Randomised,
        "deterministic" => crate::backend::EncryptionMode::Deterministic,
        other => {
            return Err(DbError::internal(format!(
                "unmask: encrypted.mode must be 'randomised' or 'deterministic', got '{other}'"
            )));
        }
    };
    let key_id = enc_meta
        .get("keyId")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let wraps = match enc_meta.get("wraps").and_then(|v| v.as_str()) {
        Some("string") => "string",
        Some("number") => "number",
        Some("bytes") => "bytes",
        _ => "string",
    };
    Ok(Some(ColumnEncryptionMeta { mode, key_id, wraps }))
}

// ---------------------------------------------------------------------------
// Authorization stub
// ---------------------------------------------------------------------------

/// **PR 4 stub** — default-deny authorization for the unmask path.
///
/// PR 5 will replace this with a per-app policy lookup driven by
/// `defineMaskPolicy()`. Until then, the rule is intentionally strict:
///
/// - **`actor.kind == "auto"`** (system / migrations / background jobs)
///   may unmask any classification.
/// - **Anyone else** — including authenticated end users, the
///   AI-builder console, and operator-shaped actors — is denied.
/// - **`actor = None`** (unauthenticated caller) is also denied.
///
/// The denied path still writes an audit row (`outcome = "denied"`) so
/// operators can observe attempted access via the per-app audit table.
pub(crate) fn check_unmask_authorization(
    actor: &Option<Value>,
    _classification: &str,
) -> Result<bool, DbError> {
    let Some(actor_obj) = actor.as_ref().and_then(|v| v.as_object()) else {
        return Ok(false); // unauthenticated → denied
    };
    let kind = actor_obj.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    Ok(kind == "auto")
}

// ---------------------------------------------------------------------------
// Public dispatch entry
// ---------------------------------------------------------------------------

/// Public dispatch entry called from the V8 `unmaskField` glue.
///
/// Performs the §6 flow: metadata lookup → authorization → SELECT +
/// optional decrypt → audit row → plaintext return. Every path writes
/// an audit row before returning (granted on success, denied on
/// refusal); SQL failures inside the SELECT path also surface a denied
/// audit row when the row PK existed in the SDK's MaskedValue but the
/// SELECT returned zero rows.
/// Public under `test-helpers` so the integration suite can drive
/// the dispatch flow without standing up V8; the production V8 glue
/// in [`dispatch_unmask_field`] is the only crate-internal caller.
pub async fn dispatch_unmask(
    app_id: &str,
    args: UnmaskFieldArgs,
) -> Result<UnmaskFieldResult, DbError> {
    // Step 1 — mask metadata lookup. A column with no mask declaration
    // (or `kind: "none"` opt-out) cannot be unmasked — there's no
    // `MaskedValue` for the SDK to dispatch from, and we don't want a
    // forged RPC to silently read plaintext through this path.
    let mask_meta = lookup_mask_meta(app_id, &args.collection, &args.column)?
        .ok_or_else(|| DbError::ValidationFailed {
            code: "unmask_column_not_masked",
            message: format!(
                "Column '{}' on collection '{}' has no mask declaration; cannot unmask",
                args.column, args.collection
            ),
            hint: Some(
                "Apply `.mask({ kind, classification })` on the field via @zeroship/db".into(),
            ),
        })?;

    // Step 2 — authorization. PR 5 will swap the stub for a per-app
    // policy lookup; PR 4 ships default-deny for non-`auto` actors.
    let allowed = check_unmask_authorization(&args.actor, &mask_meta.classification)?;
    if !allowed {
        // Audit-then-refuse. The audit row carries `outcome = "denied"`
        // so operators see every attempted access — including the
        // `canUnmask()` probe path the SDK uses.
        write_audit_unmask_row(app_id, &args, &mask_meta.classification, "denied").await?;
        return Err(DbError::Coded {
            code: "unmask_not_permitted".into(),
            message: format!(
                "Actor not authorized to unmask classification '{}'",
                mask_meta.classification
            ),
            hint: Some(
                "Configure mask policy via defineMaskPolicy() in your app's bootstrap (PR 5+).".into(),
            ),
        });
    }

    // Step 3 — fetch + decrypt (or fetch-plaintext).
    let plaintext = match lookup_encryption_meta(app_id, &args.collection, &args.column)? {
        Some(enc_meta) => fetch_and_decrypt(app_id, &args, &enc_meta).await?,
        None => fetch_plaintext_parent(app_id, &args).await?,
    };

    // Step 4 — audit the granted unmask. We do this AFTER the plaintext
    // is in hand so a SELECT failure / decrypt failure doesn't leave a
    // ghost "granted" row in the audit log (the failure surfaces a
    // typed error; the audit table reflects only completed unmasks).
    write_audit_unmask_row(app_id, &args, &mask_meta.classification, "granted").await?;

    Ok(UnmaskFieldResult { plaintext })
}

// ---------------------------------------------------------------------------
// Fetch helpers — encrypted vs plaintext, PG vs SQLite
// ---------------------------------------------------------------------------

/// Encrypted-column path: SELECT the BYTEA / BLOB ciphertext for
/// `(collection, row_pk)`, reconstruct the canonical AAD per
/// `EncryptionMode`, and decrypt through the backend-arm `EncryptedColumn`
/// impl. Returns the plaintext as a UTF-8 string (for `wraps = string`)
/// or base64-encoded raw bytes (for `wraps = bytes`); `wraps = number`
/// surfaces the f64's `to_string()` form.
#[allow(unused_variables)]
async fn fetch_and_decrypt(
    app_id: &str,
    args: &UnmaskFieldArgs,
    enc_meta: &ColumnEncryptionMeta,
) -> Result<String, DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    let aad = crate::encryption::aad::canonical_aad(
        &args.collection,
        &args.column,
        match enc_meta.mode {
            crate::backend::EncryptionMode::Randomised => Some(args.row_pk.as_bytes()),
            crate::backend::EncryptionMode::Deterministic => None,
        },
    );

    // ---- PG arm (gated on pg + hardening, matching apply_encryption_on_read) ----
    #[cfg(all(feature = "pg", feature = "hardening"))]
    {
        if let Some(pg) = backend.as_encrypted_column_pg() {
            use crate::backend::{EncryptedColumn as _, PgSqlExecutor as _};
            let pool = pg.pool_handle();
            let sql = format!(
                "SELECT \"{}\" FROM \"{}\".\"{}\" WHERE id = $1",
                args.column, app_id, args.collection
            );
            let rows = pool
                .query_text_params(&sql, &[&args.row_pk])
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            if rows.is_empty() {
                return Err(DbError::ValidationFailed {
                    code: "unmask_not_found",
                    message: format!(
                        "row '{}' not found in '{}.{}'",
                        args.row_pk, app_id, args.collection
                    ),
                    hint: None,
                });
            }
            // BYTEA arrives over the text protocol as a `\xHHHH...`
            // hex string. We parse it back to raw bytes here (mirror
            // the decode in `decrypt_row_on_read`).
            let value: Option<&str> = rows[0]
                .try_get::<_, Option<&str>>(0)
                .map_err(|e| DbError::internal(format!("unmask: get column value: {e}")))?;
            let hex_str = value.ok_or_else(|| DbError::ValidationFailed {
                code: "unmask_value_null",
                message: format!(
                    "column '{}' on row '{}' is NULL; nothing to unmask",
                    args.column, args.row_pk
                ),
                hint: None,
            })?;
            let bytes = hex_to_bytes(hex_str)?;
            let key = pg.resolve_key(app_id, &enc_meta.key_id).await?;
            let plaintext_bytes = pg.decrypt(&key, enc_meta.mode, &bytes, &aad)?;
            return wrap_plaintext_per_wraps(&plaintext_bytes, enc_meta.wraps);
        }
    }

    // ---- SQLite arm ----
    #[cfg(feature = "sqlite")]
    {
        if let Some(sq) = backend.as_encrypted_column_sqlite() {
            use crate::backend::sqlite::session::TypedCell;
            use crate::backend::{DialectBuilder as _, EncryptedColumn as _, SqlExecutor as _};
            let q_app = sq.quote_ident(app_id);
            let q_coll = sq.quote_ident(&args.collection);
            let q_col = sq.quote_ident(&args.column);
            let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
            let handle = sq.acquire_dedicated_client().await?;
            let typed = handle
                .query_typed_internal(&sql, &[args.row_pk.as_str()])
                .await?;
            if typed.rows.is_empty() {
                return Err(DbError::ValidationFailed {
                    code: "unmask_not_found",
                    message: format!(
                        "row '{}' not found in '{}.{}'",
                        args.row_pk, app_id, args.collection
                    ),
                    hint: None,
                });
            }
            let bytes = match &typed.rows[0][0] {
                TypedCell::Blob(b) => b.clone(),
                TypedCell::Null => {
                    return Err(DbError::ValidationFailed {
                        code: "unmask_value_null",
                        message: format!(
                            "column '{}' on row '{}' is NULL; nothing to unmask",
                            args.column, args.row_pk
                        ),
                        hint: None,
                    });
                }
                other => {
                    return Err(DbError::internal(format!(
                        "unmask: expected BLOB for encrypted column, got {other:?}"
                    )));
                }
            };
            let key = sq.resolve_key(app_id, &enc_meta.key_id).await?;
            let plaintext_bytes = sq.decrypt(&key, enc_meta.mode, &bytes, &aad)?;
            return wrap_plaintext_per_wraps(&plaintext_bytes, enc_meta.wraps);
        }
    }

    let _ = backend; // silence unused under non-canonical feature combos
    let _ = aad;
    Err(DbError::Configuration {
        code: "encryption_unavailable",
        message: "db: column encryption surface not available on this build".to_string(),
        hint: Some(
            "rebuild with `--features hardening` (PG) or `--features sqlite`".into(),
        ),
    })
}

/// Plaintext-storage path: SELECT the parent column directly. Used
/// when the column carries `.mask({...})` WITHOUT `.encrypted(...)` —
/// the parent slot holds the plaintext on disk; the sibling
/// `<col>_masked` carries the safe display form.
async fn fetch_plaintext_parent(app_id: &str, args: &UnmaskFieldArgs) -> Result<String, DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    // ---- PG arm ----
    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_postgres() {
            use crate::backend::PgSqlExecutor as _;
            let pool = pg.pool_handle();
            let sql = format!(
                "SELECT \"{}\" FROM \"{}\".\"{}\" WHERE id = $1",
                args.column, app_id, args.collection
            );
            let rows = pool
                .query_text_params(&sql, &[&args.row_pk])
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            if rows.is_empty() {
                return Err(DbError::ValidationFailed {
                    code: "unmask_not_found",
                    message: format!(
                        "row '{}' not found in '{}.{}'",
                        args.row_pk, app_id, args.collection
                    ),
                    hint: None,
                });
            }
            let value: Option<&str> = rows[0]
                .try_get::<_, Option<&str>>(0)
                .map_err(|e| DbError::internal(format!("unmask: get column value: {e}")))?;
            return Ok(value
                .ok_or_else(|| DbError::ValidationFailed {
                    code: "unmask_value_null",
                    message: format!(
                        "column '{}' on row '{}' is NULL; nothing to unmask",
                        args.column, args.row_pk
                    ),
                    hint: None,
                })?
                .to_string());
        }
    }

    // ---- SQLite arm ----
    #[cfg(feature = "sqlite")]
    {
        if let Some(sq) = backend.as_sqlite() {
            use crate::backend::{DialectBuilder as _, SqlExecutor as _};
            let q_app = sq.quote_ident(app_id);
            let q_coll = sq.quote_ident(&args.collection);
            let q_col = sq.quote_ident(&args.column);
            let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
            let handle = sq.acquire_dedicated_client().await?;
            let rows = handle
                .query_internal(&sql, &[args.row_pk.as_str()])
                .await?;
            if rows.is_empty() {
                return Err(DbError::ValidationFailed {
                    code: "unmask_not_found",
                    message: format!(
                        "row '{}' not found in '{}.{}'",
                        args.row_pk, app_id, args.collection
                    ),
                    hint: None,
                });
            }
            let value = rows[0]
                .first()
                .and_then(|c| c.clone())
                .ok_or_else(|| DbError::ValidationFailed {
                    code: "unmask_value_null",
                    message: format!(
                        "column '{}' on row '{}' is NULL; nothing to unmask",
                        args.column, args.row_pk
                    ),
                    hint: None,
                })?;
            return Ok(value);
        }
    }

    let _ = backend;
    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "db: no backend arm available for unmask".to_string(),
        hint: None,
    })
}

/// Convert decrypted plaintext bytes into the JSON-wire string form per
/// the column's `wraps` declaration:
///
/// - `"string"` → UTF-8 decode.
/// - `"number"` → f64 big-endian decode + `to_string()`.
/// - `"bytes"`  → base64 encode.
///
/// Only reachable through `fetch_and_decrypt`'s feature-gated arms.
/// `#[allow(dead_code)]` keeps the default-feature build clean (no
/// arm in `fetch_and_decrypt` calls it under `--no-default-features`
/// or `pg`-only without `hardening`).
#[allow(dead_code)]
fn wrap_plaintext_per_wraps(bytes: &[u8], wraps: &str) -> Result<String, DbError> {
    match wraps {
        "string" => Ok(std::str::from_utf8(bytes)
            .map_err(|e| DbError::internal(format!("unmask: plaintext not UTF-8: {e}")))?
            .to_string()),
        "number" => {
            if bytes.len() != 8 {
                return Err(DbError::internal(format!(
                    "unmask: number plaintext must be 8 bytes, got {}",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            Ok(f64::from_be_bytes(arr).to_string())
        }
        "bytes" => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
        other => Err(DbError::internal(format!(
            "unmask: unknown wraps '{other}'"
        ))),
    }
}

/// Parse a Postgres `\x`-prefixed hex string into raw bytes. Mirrors
/// `crate::crud::encryption_pass::hex_to_bytes` — duplicated here so
/// `unmask` doesn't depend on that module's privacy boundary.
///
/// Only reachable through `fetch_and_decrypt`'s PG arm
/// (`#[cfg(all(feature = "pg", feature = "hardening"))]`).
#[allow(dead_code)]
fn hex_to_bytes(s: &str) -> Result<Vec<u8>, DbError> {
    let hex = s.strip_prefix("\\x").unwrap_or(s);
    if hex.len() % 2 != 0 {
        return Err(DbError::internal(format!(
            "unmask: BYTEA text has odd hex length: {}",
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
            "unmask: BYTEA text contains non-hex byte 0x{c:02x}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Audit row writer
// ---------------------------------------------------------------------------

/// Write one row into `<app>.__zeroship_audit_unmask`. Called from the
/// granted path on success AND the denied path on refusal — per
/// design Q-MASK-C "both granted and denied attempts logged".
///
/// The table is **per-app** (lives in the app's schema, NOT in
/// `__zeroship_admin`). Per-app placement keeps audit data accessible
/// to operators querying the app's schema directly, without needing
/// platform-role access.
///
/// Idempotency on schema: each call lazily ensures the table exists
/// (`CREATE TABLE IF NOT EXISTS`). The check is one cheap round-trip
/// per call against the planner — the actual DDL runs only on first
/// use per app.
async fn write_audit_unmask_row(
    app_id: &str,
    args: &UnmaskFieldArgs,
    classification: &str,
    outcome: &str,
) -> Result<(), DbError> {
    ensure_audit_unmask_table(app_id).await?;

    let (actor_id, actor_role) = args
        .actor
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|obj| {
            let id = obj
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let role = obj
                .get("kind")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (id, role)
        })
        .unwrap_or((None, None));

    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    // ---- PG arm ----
    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_postgres() {
            use crate::backend::PgSqlExecutor as _;
            let pool = pg.pool_handle();
            let sql = format!(
                r#"INSERT INTO "{app_id}"."__zeroship_audit_unmask"
                   (actor_id, actor_role, collection, row_pk, "column",
                    classification, reason, outcome)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#
            );
            let actor_id_s: String = actor_id.unwrap_or_default();
            let actor_role_s: String = actor_role.unwrap_or_default();
            let reason_s: String = args.reason.clone().unwrap_or_default();
            // text params: empty strings serve as NULL placeholders;
            // pg interprets `''` as TEXT, so we ROUTE truly-null fields
            // through `Option`-shaped params via NULLIF on the wire.
            // Simpler: just store empty strings as ''-typed text rows;
            // operators can `WHERE actor_id = ''` to filter. Trading
            // perfect NULL fidelity for codepath simplicity is fine
            // here — the audit table is operator-read-only.
            pool.query_text_params(
                &sql,
                &[
                    &actor_id_s,
                    &actor_role_s,
                    &args.collection,
                    &args.row_pk,
                    &args.column,
                    classification,
                    &reason_s,
                    outcome,
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
                r#"INSERT INTO {q_app}."__zeroship_audit_unmask"
                   (actor_id, actor_role, collection, row_pk, "column",
                    classification, reason, outcome)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#
            );
            let actor_id_s: String = actor_id.unwrap_or_default();
            let actor_role_s: String = actor_role.unwrap_or_default();
            let reason_s: String = args.reason.clone().unwrap_or_default();
            sq.pool_exec(
                &sql,
                &[
                    actor_id_s.as_str(),
                    actor_role_s.as_str(),
                    args.collection.as_str(),
                    args.row_pk.as_str(),
                    args.column.as_str(),
                    classification,
                    reason_s.as_str(),
                    outcome,
                ],
            )
            .await?;
            return Ok(());
        }
    }

    let _ = backend;
    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "db: no backend arm available for unmask audit".to_string(),
        hint: None,
    })
}

/// `CREATE TABLE IF NOT EXISTS <app>.__zeroship_audit_unmask` on the
/// active backend arm. Idempotent — re-running on every unmask
/// dispatch is cheap (the table-exists fast path in both PG and SQLite
/// is a catalog probe).
///
/// **Per-app placement** (design Q-MASK-J): the audit table lives in
/// the app's schema (PG) / per-app database (SQLite) alongside
/// `__zeroship_migrations`. App-scoped audit data should not require
/// platform-role access to query.
async fn ensure_audit_unmask_table(app_id: &str) -> Result<(), DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;

    // ---- PG arm ----
    #[cfg(feature = "pg")]
    {
        if let Some(pg) = backend.as_postgres() {
            use crate::backend::PgSqlExecutor as _;
            let pool = pg.pool_handle();
            // BIGSERIAL PRIMARY KEY mirrors the platform's other audit
            // tables; `outcome` is a CHECK-constrained text column so
            // a malformed insert refuses at the engine.
            let sql = format!(
                r#"CREATE TABLE IF NOT EXISTS "{app_id}"."__zeroship_audit_unmask" (
                    id              BIGSERIAL PRIMARY KEY,
                    ts              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    actor_id        TEXT NULL,
                    actor_role      TEXT NULL,
                    collection      TEXT NOT NULL,
                    row_pk          TEXT NOT NULL,
                    "column"        TEXT NOT NULL,
                    classification  TEXT NOT NULL,
                    reason          TEXT NULL,
                    request_id      TEXT NULL,
                    outcome         TEXT NOT NULL CHECK (outcome IN ('granted', 'denied'))
                )"#
            );
            let empty: Vec<&str> = Vec::new();
            pool.query_text_params(&sql, &empty)
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            // Indexes — `IF NOT EXISTS` keeps the per-call cost flat.
            let idx1 = format!(
                r#"CREATE INDEX IF NOT EXISTS "__zeroship_audit_unmask_ts_idx"
                   ON "{app_id}"."__zeroship_audit_unmask" (ts)"#
            );
            let idx2 = format!(
                r#"CREATE INDEX IF NOT EXISTS "__zeroship_audit_unmask_actor_idx"
                   ON "{app_id}"."__zeroship_audit_unmask" (actor_id, ts)"#
            );
            let idx3 = format!(
                r#"CREATE INDEX IF NOT EXISTS "__zeroship_audit_unmask_row_idx"
                   ON "{app_id}"."__zeroship_audit_unmask" (row_pk, "column", ts)"#
            );
            pool.query_text_params(&idx1, &empty)
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            pool.query_text_params(&idx2, &empty)
                .await
                .map_err(|e| crate::error::DbError::from_pg(&e))?;
            pool.query_text_params(&idx3, &empty)
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
            // SQLite analogues:
            //   - BIGSERIAL → INTEGER PRIMARY KEY (alias for ROWID)
            //   - TIMESTAMPTZ → TEXT (ISO 8601 via CURRENT_TIMESTAMP)
            let sql = format!(
                r#"CREATE TABLE IF NOT EXISTS {q_app}."__zeroship_audit_unmask" (
                    id              INTEGER PRIMARY KEY,
                    ts              TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    actor_id        TEXT,
                    actor_role      TEXT,
                    collection      TEXT NOT NULL,
                    row_pk          TEXT NOT NULL,
                    "column"        TEXT NOT NULL,
                    classification  TEXT NOT NULL,
                    reason          TEXT,
                    request_id      TEXT,
                    outcome         TEXT NOT NULL CHECK (outcome IN ('granted', 'denied'))
                )"#
            );
            sq.pool_exec(&sql, &[]).await?;
            // SQLite's `CREATE INDEX` syntax puts the schema BEFORE the
            // index name, NOT before the table — `CREATE INDEX
            // <schema>.<idx> ON <table>` is the dotted shape SQLite
            // accepts. The `<schema>.<table>` shape PG uses is rejected
            // at parse time on this arm. We also scope the index name
            // to the per-app schema so two attached apps don't collide
            // on `__zeroship_audit_unmask_ts_idx`.
            let idx1 = format!(
                r#"CREATE INDEX IF NOT EXISTS {q_app}."__zeroship_audit_unmask_ts_idx"
                   ON "__zeroship_audit_unmask" (ts)"#
            );
            let idx2 = format!(
                r#"CREATE INDEX IF NOT EXISTS {q_app}."__zeroship_audit_unmask_actor_idx"
                   ON "__zeroship_audit_unmask" (actor_id, ts)"#
            );
            let idx3 = format!(
                r#"CREATE INDEX IF NOT EXISTS {q_app}."__zeroship_audit_unmask_row_idx"
                   ON "__zeroship_audit_unmask" (row_pk, "column", ts)"#
            );
            sq.pool_exec(&idx1, &[]).await?;
            sq.pool_exec(&idx2, &[]).await?;
            sq.pool_exec(&idx3, &[]).await?;
            return Ok(());
        }
    }

    let _ = backend;
    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "db: no backend arm available to provision __zeroship_audit_unmask".to_string(),
        hint: None,
    })
}

// ---------------------------------------------------------------------------
// V8 dispatch glue
// ---------------------------------------------------------------------------

use zeroship_runtime::state::{OpResult, ResolveValue};

/// V8-facing dispatch helper. Returns the unresolved Promise; the
/// `dispatch_unmask` body runs as a spawned op and resolves with
/// `{ plaintext }` on success or rejects with the typed `OpError`.
///
/// Called from `v8_classes::db::Db::unmask_field` (the `#[v8_method]`
/// wrapping this entry point).
pub(crate) fn dispatch_unmask_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    args_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);

    // Parse the args eagerly so a malformed shape surfaces a typed
    // error synchronously rather than racing the spawn.
    let parsed = parse_args(&args_v);
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let args = match parsed {
            Ok(a) => a,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };
        match dispatch_unmask(&app, args).await {
            Ok(result) => {
                // Wire shape: `{ plaintext: <string> }`. The SDK reads
                // `result.plaintext` directly; for `wraps = bytes` the
                // SDK base64-decodes on its side.
                let payload = serde_json::json!({ "plaintext": result.plaintext });
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(payload.to_string()),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

/// Decode the JSON args coming from V8 into [`UnmaskFieldArgs`]. The
/// V8 boundary already converted the JS object to `serde_json::Value`
/// via `read_json_arg`; we just pluck the typed fields.
fn parse_args(v: &Value) -> Result<UnmaskFieldArgs, DbError> {
    let obj = v.as_object().ok_or_else(|| DbError::ValidationFailed {
        code: "invalid_unmask_args",
        message: "unmaskField: args must be an object".into(),
        hint: Some(
            "pass `{ collection, row_pk, column, actor?, reason? }`".into(),
        ),
    })?;
    let collection = require_string(obj, "collection")?;
    let row_pk = require_string(obj, "row_pk")?;
    let column = require_string(obj, "column")?;
    if row_pk.is_empty() {
        return Err(DbError::ValidationFailed {
            code: "invalid_unmask_args",
            message: "unmaskField: row_pk must be a non-empty string".into(),
            hint: Some(
                "MaskedValue rows without an `id` cannot be unmasked".into(),
            ),
        });
    }
    let actor = obj.get("actor").cloned().filter(|v| !v.is_null());
    let reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(UnmaskFieldArgs {
        collection,
        row_pk,
        column,
        actor,
        reason,
    })
}

fn require_string(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, DbError> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::ValidationFailed {
            code: "invalid_unmask_args",
            message: format!("unmaskField: '{key}' must be a string"),
            hint: None,
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn authz_stub_grants_auto_actor() {
        let actor = Some(json!({ "kind": "auto", "id": null }));
        assert!(check_unmask_authorization(&actor, "spi").unwrap());
        let actor = Some(json!({ "kind": "auto", "id": "system" }));
        assert!(check_unmask_authorization(&actor, "pii").unwrap());
    }

    #[test]
    fn authz_stub_denies_user_actor() {
        let actor = Some(json!({ "kind": "user", "id": "usr_xyz" }));
        assert!(!check_unmask_authorization(&actor, "spi").unwrap());
        let actor = Some(json!({ "kind": "user", "id": "usr_xyz" }));
        assert!(!check_unmask_authorization(&actor, "pii").unwrap());
    }

    #[test]
    fn authz_stub_denies_other_kinds() {
        for kind in ["operator", "ai-builder", "anonymous", "service", ""] {
            let actor = Some(json!({ "kind": kind }));
            assert!(
                !check_unmask_authorization(&actor, "pii").unwrap(),
                "kind={kind} must be denied by the PR 4 stub"
            );
        }
    }

    #[test]
    fn authz_stub_denies_unauthenticated() {
        assert!(!check_unmask_authorization(&None, "pii").unwrap());
        // Empty object — no `kind` field — also denied.
        let actor = Some(json!({}));
        assert!(!check_unmask_authorization(&actor, "pii").unwrap());
        // Actor that isn't an object (e.g. JS passed a string) — denied.
        let actor = Some(json!("auto"));
        assert!(!check_unmask_authorization(&actor, "pii").unwrap());
    }

    #[test]
    fn parse_args_round_trip() {
        let v = json!({
            "collection": "users",
            "row_pk": "usr_01",
            "column": "ssn",
            "actor": { "kind": "user", "id": "usr_xyz" },
            "reason": "support ticket #42",
        });
        let args = parse_args(&v).unwrap();
        assert_eq!(args.collection, "users");
        assert_eq!(args.row_pk, "usr_01");
        assert_eq!(args.column, "ssn");
        assert_eq!(args.reason.as_deref(), Some("support ticket #42"));
        let actor = args.actor.expect("actor present");
        assert_eq!(actor.get("kind").and_then(|v| v.as_str()), Some("user"));
    }

    #[test]
    fn parse_args_rejects_missing_field() {
        let v = json!({ "collection": "users", "row_pk": "x" });
        let err = parse_args(&v).unwrap_err();
        assert!(matches!(
            err,
            DbError::ValidationFailed { code: "invalid_unmask_args", .. }
        ));
    }

    #[test]
    fn parse_args_rejects_empty_row_pk() {
        let v = json!({
            "collection": "users",
            "row_pk": "",
            "column": "ssn",
        });
        let err = parse_args(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "invalid_unmask_args");
                assert!(
                    message.contains("non-empty"),
                    "expected empty-row_pk diagnostic, got: {message}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_treats_null_actor_as_none() {
        let v = json!({
            "collection": "users",
            "row_pk": "u1",
            "column": "ssn",
            "actor": null,
        });
        let args = parse_args(&v).unwrap();
        assert!(args.actor.is_none(), "JSON null actor must become None");
    }

    #[test]
    fn wrap_plaintext_string() {
        let s = wrap_plaintext_per_wraps(b"hello", "string").unwrap();
        assert_eq!(s, "hello");
    }

    #[test]
    fn wrap_plaintext_number() {
        let bytes = 3.14f64.to_be_bytes();
        let s = wrap_plaintext_per_wraps(&bytes, "number").unwrap();
        assert_eq!(s, "3.14");
    }

    #[test]
    fn wrap_plaintext_bytes_base64() {
        let s = wrap_plaintext_per_wraps(&[0xde, 0xad, 0xbe, 0xef], "bytes").unwrap();
        // base64 of [0xde, 0xad, 0xbe, 0xef] is `3q2+7w==`.
        assert_eq!(s, "3q2+7w==");
    }

    #[test]
    fn hex_to_bytes_round_trip_with_prefix() {
        assert_eq!(hex_to_bytes("\\xdeadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(hex_to_bytes("\\x").unwrap(), Vec::<u8>::new());
    }
}
