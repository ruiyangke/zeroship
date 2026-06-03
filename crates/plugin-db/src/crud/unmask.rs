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
    canonical_column: String,
    classification: String,
}

fn to_snake_case_alias(column: &str) -> String {
    let mut out = String::with_capacity(column.len() + 4);
    for ch in column.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn to_camel_case_alias(column: &str) -> String {
    let mut out = String::with_capacity(column.len());
    let mut upper_next = false;
    for ch in column.chars() {
        if ch == '_' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.push(ch.to_ascii_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn resolve_schema_column(app_id: &str, collection: &str, column: &str) -> Result<Option<String>, DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(None);
    };
    let Some(obj) = schema.as_object() else {
        return Ok(None);
    };
    if obj.contains_key(column) {
        return Ok(Some(column.to_string()));
    }
    let snake = to_snake_case_alias(column);
    if snake != column && obj.contains_key(&snake) {
        return Ok(Some(snake));
    }
    let camel = to_camel_case_alias(column);
    if camel != column && camel != snake && obj.contains_key(&camel) {
        return Ok(Some(camel));
    }
    Ok(None)
}

/// Walk the cached schema and return the mask metadata for `(collection,
/// column)`, or `None` if the column has no `mask` block (or is opted
/// out via `kind: "none"`).
fn lookup_mask_meta(
    app_id: &str,
    collection: &str,
    column: &str,
) -> Result<Option<ColumnMaskMeta>, DbError> {
    let Some(canonical_column) = resolve_schema_column(app_id, collection, column)? else {
        return Ok(None);
    };
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(None);
    };
    let Some(obj) = schema.as_object() else {
        return Ok(None);
    };
    let Some(def) = obj.get(&canonical_column) else {
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
    Ok(Some(ColumnMaskMeta {
        canonical_column,
        classification,
    }))
}

/// Encryption metadata for the target column (when present). The
/// `key_id` and `wraps` fields are only consumed under the
/// `feature = "pg"` or `feature = "sqlite"` arms of
/// `fetch_and_decrypt`; a build with no backend feature never reads
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
// Authorization (P5.5 PR 5 — per-app policy lookup)
// ---------------------------------------------------------------------------

/// **P5.5 PR 5** — real authorization for the unmask path.
///
/// Replaces PR 4's default-deny stub. Resolution rules:
///
/// 1. **Unauthenticated** (`actor = None` or `actor.kind` missing) →
///    deny. The denied path still writes an audit row.
///
/// 2. **Cached per-app policy present** → consult
///    [`crate::crud::mask_policy::MaskPolicy::allows`]. That helper
///    enforces the `auto`-actor fallback rule (system actor allowed
///    by default unless the policy explicitly restricts it).
///
/// 3. **No cached policy + sync caller** → fall back to PR 4's
///    default-deny stub (`auto` allowed; everyone else denied). The
///    real first-use load happens inside [`dispatch_unmask`] before
///    this helper runs — that load is async, so cannot live here.
///
/// **Sync entry point**: this function does not perform I/O. The
/// per-app policy MUST be cached (via [`ensure_mask_policy_cached`])
/// before [`dispatch_unmask`] reaches the auth check.
/// Actor `kind`s reserved for genuine platform/system callers (migration,
/// backfill, drift). The default-deny stub and the `auto`-fallback rule grant
/// these broad access; app JS must never be able to claim one.
pub(crate) const RESERVED_SYSTEM_ACTOR_KINDS: &[&str] = &["auto"];

/// DB-3: sanitize an actor descriptor that originated from **app JS** (the
/// `{ actor }` field of an `unmask` / `find({unmask})` call). The unmask
/// authorization read `actor.kind` straight off this app-supplied object, and
/// `kind: "auto"` is the privileged system default that the default-deny stub
/// (and the no-policy fallback) grant EVERY classification — so any handler
/// could `unmask({ actor: { kind: "auto" } })` and read its own PII/PHI/PCI at
/// will. Strip an actor that claims a reserved system kind to `None`
/// (→ "unauthenticated → denied"), so app code can never impersonate the
/// system actor. Genuine system callers build their actor in Rust and never
/// pass through this V8 boundary, so they are unaffected.
pub(crate) fn sanitize_app_actor(actor: Option<Value>) -> Option<Value> {
    let kind = actor
        .as_ref()
        .and_then(|v| v.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or("");
    if RESERVED_SYSTEM_ACTOR_KINDS.contains(&kind) {
        None
    } else {
        actor
    }
}

pub(crate) fn check_unmask_authorization(
    app_id: &str,
    actor: &Option<Value>,
    classification: &str,
) -> Result<bool, DbError> {
    let Some(actor_obj) = actor.as_ref().and_then(|v| v.as_object()) else {
        return Ok(false); // unauthenticated → denied
    };
    let kind = actor_obj.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let policy = crate::context::with(|c| c.mask_policy_for(app_id));
    match policy {
        Some(p) => Ok(p.allows(kind, classification)),
        None => {
            // PR 4 default-deny stub: only `auto` allowed when the app
            // has not declared a policy.
            Ok(kind == "auto")
        }
    }
}

/// **P5.5 PR 5** — best-effort lazy load of the durable policy for
/// `app_id` into the per-isolate cache. Called by [`dispatch_unmask`]
/// before the auth check. A storage miss is a no-op (cache stays
/// empty, default-deny stub applies on the auth path); a storage hit
/// installs the loaded policy via
/// [`crate::context::IsolateDbContext::set_mask_policy_for_app`].
///
/// Errors propagate (a corrupt sidecar JSON or PG SQL failure surfaces
/// as `DbError`); the unmask flow then rejects with the typed error
/// instead of silently default-denying — operators see the real fault.
async fn ensure_mask_policy_cached(app_id: &str) -> Result<(), DbError> {
    if crate::context::with(|c| c.has_mask_policy(app_id)) {
        return Ok(());
    }
    let backend = match crate::context::with(|c| c.backend()) {
        Some(b) => b,
        None => return Ok(()), // backend not initialised — auth path's default-deny stub handles it
    };

    // ---- PG arm ----
    if let Some(pg) = backend.as_postgres() {
        let loaded = crate::crud::mask_policy::load_pg(pg, app_id).await?;
        if let Some(p) = loaded {
            crate::context::with_mut(|c| c.set_mask_policy_for_app(app_id, Some(p)));
        }
        return Ok(());
    }

    // ---- SQLite arm ----
    if let Some(sq) = backend.as_sqlite() {
        let loaded = crate::crud::mask_policy::load_sqlite(sq, app_id).await?;
        if let Some(p) = loaded {
            crate::context::with_mut(|c| c.set_mask_policy_for_app(app_id, Some(p)));
        }
        return Ok(());
    }
    Ok(())
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
    mut args: UnmaskFieldArgs,
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
    args.column = mask_meta.canonical_column.clone();

    // Step 2 — authorization. **P5.5 PR 5**: load the per-app policy
    // into the cache (best-effort) THEN consult `check_unmask_authorization`,
    // which honours the cached policy or falls back to PR 4's
    // default-deny stub on a miss.
    ensure_mask_policy_cached(app_id).await?;
    let allowed = check_unmask_authorization(app_id, &args.actor, &mask_meta.classification)?;
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

    // ---- PG arm ----
    if let Some(pg) = backend.as_encrypted_column_pg() {
        use crate::backend::{EncryptedColumn as _, PgSqlExecutor as _};
        let pool = pg.pool_handle();
        let sql = format!(
            "SELECT \"{}\" FROM \"{}\".\"{}\" WHERE id = $1",
            args.column, app_id, args.collection
        );
        let rows =
            crate::exec::query_postgres_pool_with_autocommit_role(pool, app_id, &sql, &[&args.row_pk])
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

    // ---- SQLite arm ----
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

    Err(DbError::Configuration {
        code: "encryption_unavailable",
        message: "db: no backend arm available for column encryption".to_string(),
        hint: None,
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
    if let Some(pg) = backend.as_postgres() {
        use crate::backend::PgSqlExecutor as _;
        let pool = pg.pool_handle();
        let sql = format!(
            "SELECT \"{}\" FROM \"{}\".\"{}\" WHERE id = $1",
            args.column, app_id, args.collection
        );
        let rows =
            crate::exec::query_postgres_pool_with_autocommit_role(pool, app_id, &sql, &[&args.row_pk])
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

    // ---- SQLite arm ----
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
/// `#[allow(dead_code)]` keeps a no-backend build clean (no arm in
/// `fetch_and_decrypt` calls it under `--no-default-features`).
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
/// Only reachable through `fetch_and_decrypt`'s PG arm.
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

    // ---- SQLite arm ----
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

    // ---- SQLite arm ----
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

    Err(DbError::Configuration {
        code: "backend_unsupported",
        message: "db: no backend arm available to provision __zeroship_audit_unmask".to_string(),
        hint: None,
    })
}

// ===========================================================================
// P5.5 PR 7 — bulk unmask + per-query unmask hint
// ===========================================================================
//
// These two entry points wrap the single-column unmask machinery for
// callers that want to amortise the V8↔Rust round-trip when many
// columns / rows need plaintext at once. They share the same
// `MaskPolicy::allows` authorization contract as the single-cell path
// but apply it as a single ATOMIC fence — one denied pair refuses the
// WHOLE call (Q-MASK-F atomic-or-partial = atomic). The atomic fence
// is critical for correctness: a partial-grant would leak the fact
// that a particular pair was rejected through the side-channel of
// "which columns came back populated".
//
// Audit rows reuse the existing `__zeroship_audit_unmask` table to
// avoid a CHECK-constraint migration on the per-app schema. Each call
// writes ONE audit row regardless of how many (row, column) pairs are
// requested; the `column` field carries the comma-joined column list
// and the `row_pk` field carries the comma-joined row PK list (or, for
// per-query hint, the SQL filter's JSON representation). The `reason`
// text is prefixed with `[bulk_unmask] <caller-reason>` /
// `[query_hint] <caller-reason>` so operators querying the audit log
// can filter by dispatch shape without needing a new column.

/// **P5.5 PR 7** — args for the bulk unmask dispatcher.
///
/// `items[i].columns` is the list of column names to unmask on
/// `items[i].row_pk`. An item with an empty `columns` list is treated
/// as a no-op for that row.
#[derive(Debug, Clone)]
pub struct BulkUnmaskItem {
    pub row_pk: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BulkUnmaskArgs {
    pub collection: String,
    pub items: Vec<BulkUnmaskItem>,
    pub actor: Option<Value>,
    pub reason: Option<String>,
}

/// **P5.5 PR 7** — result of a successful bulk unmask.
///
/// `results[row_pk][column]` carries the plaintext for every requested
/// pair. The shape mirrors the SDK's `Map<id, Record<col, plaintext>>`
/// so the JS caller materialises it directly.
#[derive(Debug, Clone, Default)]
pub struct BulkUnmaskResult {
    pub results: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

/// **P5.5 PR 7** — public dispatch entry for `zeroship.db.bulkUnmaskFields`.
///
/// Atomic authorization (Q-MASK-F): BEFORE any decrypt happens, every
/// (row_pk, column) pair is authorised against the per-app policy. If
/// ANY pair is denied, the call refuses entirely with a single
/// `bulk_unmask_partial_unauthorized` audit row; the authorised pairs
/// are NOT returned. This prevents inferring authorisation results
/// from "which columns came back populated".
///
/// On full authorisation, the decrypt loop runs (one round-trip per
/// pair through the existing single-cell helpers) and ONE
/// `bulk_granted` audit row covering the whole call is written on
/// success.
///
/// Unknown columns (`unmask_column_not_masked`) or unknown
/// collections fail the call up-front before any audit row is written
/// — the error surface is unchanged from the single-cell path.
pub async fn dispatch_bulk_unmask(
    app_id: &str,
    args: BulkUnmaskArgs,
) -> Result<BulkUnmaskResult, DbError> {
    if args.items.is_empty() {
        return Ok(BulkUnmaskResult::default());
    }

    // ---- Step 1 — load policy into cache, then resolve every (row,
    // col) pair's classification + check authorization.
    ensure_mask_policy_cached(app_id).await?;

    // Per-column classification cache so we don't re-walk the schema
    // map N×M times. `None` slot = column has no mask declaration →
    // typed error.
    let mut classifications: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut unauthorized: Vec<(String, String)> = Vec::new(); // (row_pk, column)
    let mut normalized_items: Vec<(String, Vec<(String, String)>)> =
        Vec::with_capacity(args.items.len());
    let mut normalized_audit_items: Vec<BulkUnmaskItem> = Vec::with_capacity(args.items.len());

    for item in &args.items {
        let mut normalized_columns: Vec<(String, String)> = Vec::with_capacity(item.columns.len());
        let mut audit_columns: Vec<String> = Vec::with_capacity(item.columns.len());
        for col in &item.columns {
            let mask_meta = lookup_mask_meta(app_id, &args.collection, col)?
                .ok_or_else(|| DbError::ValidationFailed {
                    code: "unmask_column_not_masked",
                    message: format!(
                        "bulkUnmask: column '{}' on collection '{}' has no mask declaration",
                        col, args.collection
                    ),
                    hint: Some(
                        "Apply `.mask({ kind, classification })` on the field via @zeroship/db"
                            .into(),
                    ),
                })?;
            classifications
                .entry(mask_meta.canonical_column.clone())
                .or_insert_with(|| mask_meta.classification.clone());
            let classification = classifications
                .get(&mask_meta.canonical_column)
                .expect("just inserted")
                .clone();
            let allowed = check_unmask_authorization(app_id, &args.actor, &classification)?;
            if !allowed {
                unauthorized.push((item.row_pk.clone(), mask_meta.canonical_column.clone()));
            }
            normalized_columns.push((col.clone(), mask_meta.canonical_column.clone()));
            audit_columns.push(mask_meta.canonical_column);
        }
        normalized_items.push((item.row_pk.clone(), normalized_columns));
        normalized_audit_items.push(BulkUnmaskItem {
            row_pk: item.row_pk.clone(),
            columns: audit_columns,
        });
    }
    let normalized_audit_args = BulkUnmaskArgs {
        collection: args.collection.clone(),
        items: normalized_audit_items,
        actor: args.actor.clone(),
        reason: args.reason.clone(),
    };

    // ---- Step 2 — atomic-fence enforcement. If ANY pair denied,
    // emit one audit row covering the whole call + refuse.
    if !unauthorized.is_empty() {
        write_audit_bulk_row(
            app_id,
            &normalized_audit_args,
            &classifications,
            "denied",
            Some(&unauthorized),
        )
        .await?;
        return Err(DbError::Coded {
            code: "bulk_unmask_partial_unauthorized".into(),
            message: format!(
                "bulkUnmask: {} (row, column) pair(s) not authorized; call refused atomically",
                unauthorized.len()
            ),
            hint: Some(
                "Drop the unauthorized columns or expand the mask policy via defineMaskPolicy()."
                    .into(),
            ),
        });
    }

    // ---- Step 3 — decrypt loop. Reuse the single-cell fetch helpers
    // so we don't duplicate the PG / SQLite arms. Bulk-of-one is
    // exactly one fetch.
    let mut out: BulkUnmaskResult = BulkUnmaskResult::default();
    for (row_pk, cols) in &normalized_items {
        if cols.is_empty() {
            continue;
        }
        let row_map = out.results.entry(row_pk.clone()).or_default();
        for (requested_col, canonical_col) in cols {
            let single_args = UnmaskFieldArgs {
                collection: args.collection.clone(),
                row_pk: row_pk.clone(),
                column: canonical_col.clone(),
                actor: args.actor.clone(),
                reason: args.reason.clone(),
            };
            // Read encryption / plaintext path identically to the
            // single-cell helper — we already checked auth, so call
            // the FETCH helpers directly (not `dispatch_unmask`,
            // which would re-audit per pair). This is the
            // "wrap-over-many" pattern the proposal describes.
            let plaintext = match lookup_encryption_meta(app_id, &args.collection, canonical_col)? {
                Some(enc_meta) => fetch_and_decrypt(app_id, &single_args, &enc_meta).await?,
                None => fetch_plaintext_parent(app_id, &single_args).await?,
            };
            row_map.insert(requested_col.clone(), plaintext);
        }
    }

    // ---- Step 4 — single audit row for the whole call on success.
    write_audit_bulk_row(app_id, &normalized_audit_args, &classifications, "granted", None).await?;

    Ok(out)
}

/// **P5.5 PR 7** — write the single audit row covering an entire
/// bulk-unmask call. Reuses `__zeroship_audit_unmask`; the row's
/// `column` carries a comma-joined column list, `row_pk` carries the
/// comma-joined row PK list, and `reason` is prefixed `[bulk_unmask]`
/// so operators can filter by dispatch shape.
///
/// `outcome` is either `"granted"` (every pair authorised + decrypted)
/// or `"denied"` (at least one pair denied; nothing decrypted). The
/// `unauthorized` slice carries the rejected pairs when the outcome is
/// `denied`; included in the reason text so audit-log readers see
/// exactly which pairs caused the refusal.
async fn write_audit_bulk_row(
    app_id: &str,
    args: &BulkUnmaskArgs,
    classifications: &std::collections::HashMap<String, String>,
    outcome: &str,
    unauthorized: Option<&[(String, String)]>,
) -> Result<(), DbError> {
    // Build the join-strings up-front: column list (unique, sorted for
    // stable audit-row diffing), row-pk list (in caller order — the
    // sequence captures the original bulk shape).
    let mut columns_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for item in &args.items {
        for col in &item.columns {
            columns_set.insert(col.clone());
        }
    }
    let columns_joined = columns_set.iter().cloned().collect::<Vec<_>>().join(",");
    let row_pks_joined = args
        .items
        .iter()
        .map(|i| i.row_pk.as_str())
        .collect::<Vec<_>>()
        .join(",");

    // Classification list: comma-joined unique values from the per-column
    // map. When the bulk involves columns of multiple classifications
    // (e.g. one `pii`, one `spi`), the audit row carries the union.
    let mut class_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for c in classifications.values() {
        class_set.insert(c.clone());
    }
    let classification_joined = class_set
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");

    let caller_reason = args.reason.clone().unwrap_or_default();
    let reason_text = match unauthorized {
        Some(pairs) => {
            let detail: Vec<String> = pairs
                .iter()
                .map(|(r, c)| format!("{r}/{c}"))
                .collect();
            format!(
                "[bulk_unmask] unauthorized=[{}] caller={caller_reason}",
                detail.join(",")
            )
        }
        None => format!("[bulk_unmask] caller={caller_reason}"),
    };

    let synthetic = UnmaskFieldArgs {
        collection: args.collection.clone(),
        row_pk: row_pks_joined,
        column: columns_joined,
        actor: args.actor.clone(),
        reason: Some(reason_text),
    };
    write_audit_unmask_row(app_id, &synthetic, &classification_joined, outcome).await
}

// ---------------------------------------------------------------------------
// Per-query unmask hint
// ---------------------------------------------------------------------------

/// **P5.5 PR 7** — pre-query authorization for the
/// `find({...}, { unmask: [...], actor })` hint.
///
/// Resolves every column in `unmask_columns` against the cached
/// schema, then walks every (column, classification) pair through
/// `MaskPolicy::allows`. If ANY column is denied the hint is REFUSED
/// entirely — we do NOT silently fall back to masked-only because
/// that would conceal the authorisation failure from the caller.
///
/// Called from `crud::dispatch_find` BEFORE
/// `build_find_with_schema` fires the SQL.
///
/// Returns `Ok(())` on full authorisation; on denial returns
/// `Err(DbError::Coded { code: "unmask_not_permitted", ... })`. The
/// denied path writes one `denied` audit row covering the whole
/// query.
pub async fn authorize_query_hint(
    app_id: &str,
    collection: &str,
    unmask_columns: &[String],
    actor: &Option<Value>,
    reason: &Option<String>,
) -> Result<(), DbError> {
    if unmask_columns.is_empty() {
        return Ok(());
    }

    ensure_mask_policy_cached(app_id).await?;

    let mut classifications: Vec<String> = Vec::with_capacity(unmask_columns.len());
    let mut unauthorized: Vec<String> = Vec::new();
    for col in unmask_columns {
        let mask_meta = lookup_mask_meta(app_id, collection, col)?
            .ok_or_else(|| DbError::ValidationFailed {
                code: "unmask_column_not_masked",
                message: format!(
                    "find: unmask hint refers to column '{col}' on '{collection}' which has no mask declaration"
                ),
                hint: Some(
                    "Drop the column from `opts.unmask` or apply `.mask({ kind, classification })`"
                        .into(),
                ),
            })?;
        let allowed = check_unmask_authorization(app_id, actor, &mask_meta.classification)?;
        if !allowed {
            unauthorized.push(col.clone());
        }
        classifications.push(mask_meta.classification);
    }

    if !unauthorized.is_empty() {
        write_audit_query_hint_row(
            app_id,
            collection,
            unmask_columns,
            &classifications,
            actor,
            reason,
            "denied",
            Some(&unauthorized),
        )
        .await?;
        return Err(DbError::Coded {
            code: "unmask_not_permitted".into(),
            message: format!(
                "find: actor not authorized to unmask {} column(s) via query hint",
                unauthorized.len()
            ),
            hint: Some(
                "Drop the unauthorized columns from `opts.unmask` or expand the mask policy."
                    .into(),
            ),
        });
    }

    // Granted audit row is deferred to AFTER the SELECT lands so we
    // don't leave a ghost row for a SQL failure. The orchestrator
    // calls `audit_query_hint_granted` after a successful find.
    Ok(())
}

/// **P5.5 PR 7** — write the audit row for a successful per-query
/// unmask hint. Called by the find dispatcher AFTER the SELECT lands.
/// Single row per query (NOT per row), so the audit-log volume scales
/// with query count not row count.
pub async fn audit_query_hint_granted(
    app_id: &str,
    collection: &str,
    unmask_columns: &[String],
    actor: &Option<Value>,
    reason: &Option<String>,
) -> Result<(), DbError> {
    if unmask_columns.is_empty() {
        return Ok(());
    }
    // Re-resolve classifications for the audit row. Cheap — the
    // schema lookup is a HashMap read.
    let mut classifications: Vec<String> = Vec::with_capacity(unmask_columns.len());
    for col in unmask_columns {
        let mask_meta = lookup_mask_meta(app_id, collection, col)?;
        let cls = mask_meta
            .map(|m| m.classification)
            .unwrap_or_else(|| "pii".to_string());
        classifications.push(cls);
    }
    write_audit_query_hint_row(
        app_id,
        collection,
        unmask_columns,
        &classifications,
        actor,
        reason,
        "granted",
        None,
    )
    .await
}

/// **P5.5 PR 7** — rewrite rows from a `find` result so the
/// `unmask`-listed columns carry plaintext instead of the
/// `__zsmask__`-wrapped sibling.
///
/// Per-query hint promotes plaintext for ONLY the listed columns;
/// non-listed masked columns keep their `MaskedValue<T>` wrapping (the
/// `apply_mask_wrap_on_read` pass already attached the sentinel before
/// this helper runs). On the wire we replace `row[col]` with the bare
/// decrypted string for each listed column.
///
/// `rows` is mutated in place. Each row's PK is read from `row["id"]`
/// (the implicit primary key; aligns with `wrap_row_on_read`'s
/// expectation).
pub async fn dispatch_unmask_for_query(
    app_id: &str,
    collection: &str,
    unmask_columns: &[String],
    rows: &mut [Value],
) -> Result<(), DbError> {
    if unmask_columns.is_empty() {
        return Ok(());
    }
    for row in rows.iter_mut() {
        let Some(row_pk) = row
            .get("id")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            })
        else {
            continue;
        };
        if row_pk.is_empty() {
            // No PK — can't fetch ciphertext. Leave the row's
            // `MaskedValue` wrapping in place; the caller's TypeScript
            // type narrows to plaintext only when row_pk is present.
            continue;
        }
        for col in unmask_columns {
            // Use the single-cell fetch helpers directly — auth was
            // already checked upstream via `authorize_query_hint`.
            let single_args = UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: row_pk.clone(),
                column: col.clone(),
                actor: None,
                reason: None,
            };
            let plaintext = match lookup_encryption_meta(app_id, collection, col)? {
                Some(enc_meta) => fetch_and_decrypt(app_id, &single_args, &enc_meta).await?,
                None => fetch_plaintext_parent(app_id, &single_args).await?,
            };
            if let Some(obj) = row.as_object_mut() {
                obj.insert(col.clone(), Value::String(plaintext));
            }
        }
    }
    Ok(())
}

/// Write the audit row for a per-query unmask hint dispatch. Reuses
/// the unmask audit table with a `[query_hint]` reason prefix and
/// comma-joined column / classification fields (same shape as the
/// bulk audit row writer).
#[allow(clippy::too_many_arguments)]
async fn write_audit_query_hint_row(
    app_id: &str,
    collection: &str,
    unmask_columns: &[String],
    classifications: &[String],
    actor: &Option<Value>,
    reason: &Option<String>,
    outcome: &str,
    unauthorized: Option<&[String]>,
) -> Result<(), DbError> {
    let columns_joined = unmask_columns.join(",");
    let class_set: std::collections::BTreeSet<String> =
        classifications.iter().cloned().collect();
    let class_joined = class_set.into_iter().collect::<Vec<_>>().join(",");
    let caller_reason = reason.clone().unwrap_or_default();
    let reason_text = match unauthorized {
        Some(cols) => format!(
            "[query_hint] unauthorized=[{}] caller={caller_reason}",
            cols.join(",")
        ),
        None => format!("[query_hint] caller={caller_reason}"),
    };
    let synthetic = UnmaskFieldArgs {
        collection: collection.to_string(),
        // Per-query hint isn't a per-row dispatch; row_pk slot carries
        // the literal "[query_hint]" marker so operators querying
        // `row_pk = '<id>'` don't accidentally include query-hint
        // audit rows.
        row_pk: "[query_hint]".to_string(),
        column: columns_joined,
        actor: actor.clone(),
        reason: Some(reason_text),
    };
    write_audit_unmask_row(app_id, &synthetic, &class_joined, outcome).await
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
    // DB-3: app JS cannot claim the reserved `auto` system actor.
    let actor = sanitize_app_actor(obj.get("actor").cloned().filter(|v| !v.is_null()));
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

// ---------------------------------------------------------------------------
// P5.5 PR 7 — V8 dispatch glue for `bulkUnmaskFields`
// ---------------------------------------------------------------------------

/// V8-facing dispatch helper for `zeroship.db.bulkUnmaskFields`.
///
/// Mirrors [`dispatch_unmask_field`]: parses the args eagerly so a
/// malformed shape surfaces synchronously, then spawns the bulk
/// dispatcher and resolves with `{ results: { <rowPk>: { <col>: <pt> } } }`
/// on success or rejects with the typed `OpError` on failure (most
/// commonly `bulk_unmask_partial_unauthorized`).
pub(crate) fn dispatch_bulk_unmask_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    args_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);

    let parsed = parse_bulk_args(&args_v);
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
        match dispatch_bulk_unmask(&app, args).await {
            Ok(result) => {
                // Wire shape: `{ results: { <rowPk>: { <col>: <plaintext> } } }`.
                // `BTreeMap` serialises as a JSON object with sorted
                // keys — deterministic for golden-snapshot tests.
                let mut obj = serde_json::Map::with_capacity(result.results.len());
                for (row_pk, cols) in result.results {
                    let mut col_obj = serde_json::Map::with_capacity(cols.len());
                    for (c, pt) in cols {
                        col_obj.insert(c, Value::String(pt));
                    }
                    obj.insert(row_pk, Value::Object(col_obj));
                }
                let payload = serde_json::json!({ "results": Value::Object(obj) });
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

/// Parse the JS-side `{ collection, items: [{ rowPk, columns }], actor?, reason? }`
/// shape into [`BulkUnmaskArgs`]. Refuses non-object args, missing
/// fields, items that aren't an array, items missing `rowPk` /
/// `columns`, and columns arrays containing non-strings — every error
/// surfaces a typed `ValidationFailed { code: "invalid_bulk_unmask_args" }`
/// so the SDK can branch on `.code` deterministically.
fn parse_bulk_args(v: &Value) -> Result<BulkUnmaskArgs, DbError> {
    let obj = v.as_object().ok_or_else(|| DbError::ValidationFailed {
        code: "invalid_bulk_unmask_args",
        message: "bulkUnmaskFields: args must be an object".into(),
        hint: Some(
            "pass `{ collection, items: [{ rowPk, columns }, ...], actor?, reason? }`"
                .into(),
        ),
    })?;
    let collection = require_string_with_code(
        obj,
        "collection",
        "invalid_bulk_unmask_args",
        "bulkUnmaskFields",
    )?;
    let items_v = obj
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| DbError::ValidationFailed {
            code: "invalid_bulk_unmask_args",
            message: "bulkUnmaskFields: `items` must be a non-empty array".into(),
            hint: None,
        })?;
    let mut items: Vec<BulkUnmaskItem> = Vec::with_capacity(items_v.len());
    for (i, item_v) in items_v.iter().enumerate() {
        let item_obj = item_v.as_object().ok_or_else(|| DbError::ValidationFailed {
            code: "invalid_bulk_unmask_args",
            message: format!("bulkUnmaskFields: items[{i}] must be an object"),
            hint: None,
        })?;
        let row_pk = item_obj
            .get("rowPk")
            .or_else(|| item_obj.get("row_pk"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| DbError::ValidationFailed {
                code: "invalid_bulk_unmask_args",
                message: format!("bulkUnmaskFields: items[{i}].rowPk must be a non-empty string"),
                hint: None,
            })?
            .to_string();
        if row_pk.is_empty() {
            return Err(DbError::ValidationFailed {
                code: "invalid_bulk_unmask_args",
                message: format!("bulkUnmaskFields: items[{i}].rowPk must be non-empty"),
                hint: None,
            });
        }
        let columns_v = item_obj.get("columns").and_then(|v| v.as_array()).ok_or_else(|| {
            DbError::ValidationFailed {
                code: "invalid_bulk_unmask_args",
                message: format!("bulkUnmaskFields: items[{i}].columns must be an array of strings"),
                hint: None,
            }
        })?;
        let mut columns: Vec<String> = Vec::with_capacity(columns_v.len());
        for (ci, col_v) in columns_v.iter().enumerate() {
            let s = col_v.as_str().ok_or_else(|| DbError::ValidationFailed {
                code: "invalid_bulk_unmask_args",
                message: format!(
                    "bulkUnmaskFields: items[{i}].columns[{ci}] must be a string"
                ),
                hint: None,
            })?;
            columns.push(s.to_string());
        }
        items.push(BulkUnmaskItem { row_pk, columns });
    }
    // DB-3: app JS cannot claim the reserved `auto` system actor.
    let actor = sanitize_app_actor(obj.get("actor").cloned().filter(|v| !v.is_null()));
    let reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(BulkUnmaskArgs {
        collection,
        items,
        actor,
        reason,
    })
}

fn require_string_with_code(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    code: &'static str,
    method: &str,
) -> Result<String, DbError> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::ValidationFailed {
            code,
            message: format!("{method}: '{key}' must be a string"),
            hint: None,
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_app_actor_strips_reserved_auto_db3() {
        // App JS claiming the privileged system actor is stripped to None, so
        // check_unmask_authorization's "unauthenticated → denied" arm applies —
        // an app handler can no longer unmask its PII via {actor:{kind:"auto"}}.
        assert_eq!(sanitize_app_actor(Some(json!({ "kind": "auto" }))), None);
        // A non-reserved, app-declared actor kind passes through unchanged.
        assert_eq!(
            sanitize_app_actor(Some(json!({ "kind": "support_agent" }))),
            Some(json!({ "kind": "support_agent" }))
        );
        // No actor / missing kind stay as-is (denied downstream regardless).
        assert_eq!(sanitize_app_actor(None), None);
        assert_eq!(sanitize_app_actor(Some(json!({}))), Some(json!({})));
    }

    #[test]
    fn stripped_auto_actor_is_denied_by_authorization_db3() {
        // The fix's effect: a sanitized app actor (the `auto` claim stripped to
        // None) hits check_unmask_authorization's "unauthenticated → denied"
        // arm — which returns before consulting any policy. Pre-fix the raw
        // {kind:"auto"} reached the no-policy fallback and was GRANTED.
        let sanitized = sanitize_app_actor(Some(json!({ "kind": "auto" })));
        assert_eq!(sanitized, None);
        assert_eq!(
            check_unmask_authorization("app_x", &sanitized, "pii").unwrap(),
            false,
            "sanitized (stripped-auto) app actor must be denied"
        );
    }

    // ---------------------------------------------------------------
    // PR 4 default-deny stub — exercised by passing an `app_id` that
    // has no policy cached. **P5.5 PR 5** kept the stub behaviour
    // intact for the no-policy fallthrough path; these tests pin that
    // fallthrough.
    //
    // Unit tests reach the per-isolate ISOLATE_CTX (which the
    // `check_unmask_authorization` body uses to look up the cached
    // policy). Each test uses a unique app_id so the global
    // thread-local cache state doesn't bleed between tests.
    // ---------------------------------------------------------------

    #[test]
    fn authz_stub_grants_auto_actor() {
        let actor = Some(json!({ "kind": "auto", "id": null }));
        assert!(check_unmask_authorization("authz_stub_grants_auto_1", &actor, "spi").unwrap());
        let actor = Some(json!({ "kind": "auto", "id": "system" }));
        assert!(check_unmask_authorization("authz_stub_grants_auto_2", &actor, "pii").unwrap());
    }

    #[test]
    fn authz_stub_denies_user_actor() {
        let actor = Some(json!({ "kind": "user", "id": "usr_xyz" }));
        assert!(!check_unmask_authorization("authz_stub_denies_user_1", &actor, "spi").unwrap());
        let actor = Some(json!({ "kind": "user", "id": "usr_xyz" }));
        assert!(!check_unmask_authorization("authz_stub_denies_user_2", &actor, "pii").unwrap());
    }

    #[test]
    fn authz_stub_denies_other_kinds() {
        for kind in ["operator", "ai-builder", "anonymous", "service", ""] {
            let actor = Some(json!({ "kind": kind }));
            let app_id = format!("authz_stub_denies_other_{kind}");
            assert!(
                !check_unmask_authorization(&app_id, &actor, "pii").unwrap(),
                "kind={kind} must be denied by the PR 4 stub"
            );
        }
    }

    #[test]
    fn authz_stub_denies_unauthenticated() {
        assert!(!check_unmask_authorization("authz_stub_unauth_1", &None, "pii").unwrap());
        // Empty object — no `kind` field — also denied.
        let actor = Some(json!({}));
        assert!(!check_unmask_authorization("authz_stub_unauth_2", &actor, "pii").unwrap());
        // Actor that isn't an object (e.g. JS passed a string) — denied.
        let actor = Some(json!("auto"));
        assert!(!check_unmask_authorization("authz_stub_unauth_3", &actor, "pii").unwrap());
    }

    // ---------------------------------------------------------------
    // P5.5 PR 5 — per-app policy lookup
    // ---------------------------------------------------------------

    /// Helper: install a [`MaskPolicy`] for `app_id` on the current
    /// isolate's context cache, then immediately remove it on drop.
    /// Keeps the thread-local cache hygiene clean across tests.
    struct PolicyGuard(String);
    impl PolicyGuard {
        fn install(
            app_id: &str,
            policy: crate::crud::mask_policy::MaskPolicy,
        ) -> Self {
            crate::context::with_mut(|c| {
                c.set_mask_policy_for_app(app_id, Some(policy));
            });
            Self(app_id.to_string())
        }
    }
    impl Drop for PolicyGuard {
        fn drop(&mut self) {
            crate::context::with_mut(|c| {
                c.set_mask_policy_for_app(&self.0, None);
            });
        }
    }

    #[test]
    fn pr5_policy_grants_role_with_classification() {
        use crate::crud::mask_policy::MaskPolicy;
        let app_id = "pr5_grants_role_classification";
        let policy = MaskPolicy::from_json(&json!({
            "user": ["public", "pii"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(json!({ "kind": "user", "id": "usr_x" }));
        assert!(check_unmask_authorization(app_id, &actor, "pii").unwrap());
        assert!(check_unmask_authorization(app_id, &actor, "public").unwrap());
        assert!(!check_unmask_authorization(app_id, &actor, "spi").unwrap());
    }

    #[test]
    fn pr5_policy_unknown_role_denied() {
        use crate::crud::mask_policy::MaskPolicy;
        let app_id = "pr5_unknown_role_denied";
        let policy = MaskPolicy::from_json(&json!({
            "user": ["public"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(json!({ "kind": "operator", "id": "op_1" }));
        assert!(!check_unmask_authorization(app_id, &actor, "public").unwrap());
    }

    #[test]
    fn pr5_auto_fallback_when_not_in_policy() {
        use crate::crud::mask_policy::MaskPolicy;
        let app_id = "pr5_auto_fallback";
        // Policy DOES list `user`, but NOT `auto` — the system actor
        // retains its uniform access via the fallback rule.
        let policy = MaskPolicy::from_json(&json!({
            "user": ["public"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(json!({ "kind": "auto" }));
        assert!(check_unmask_authorization(app_id, &actor, "pii").unwrap());
        assert!(check_unmask_authorization(app_id, &actor, "spi").unwrap());
        assert!(check_unmask_authorization(app_id, &actor, "internal").unwrap());
    }

    #[test]
    fn pr5_auto_explicit_restriction_honoured() {
        use crate::crud::mask_policy::MaskPolicy;
        let app_id = "pr5_auto_explicit_restriction";
        let policy = MaskPolicy::from_json(&json!({
            "auto": ["public"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(json!({ "kind": "auto" }));
        assert!(check_unmask_authorization(app_id, &actor, "public").unwrap());
        assert!(!check_unmask_authorization(app_id, &actor, "pii").unwrap());
        assert!(!check_unmask_authorization(app_id, &actor, "spi").unwrap());
    }

    #[test]
    fn lookup_mask_meta_accepts_field_name_alias_for_snake_case_schema() {
        let app_id = "lookup_mask_meta_alias_snake_case";
        crate::cache_schema_for_tests(
            app_id,
            "users",
            json!({
                "contact_email": {
                    "type": "string",
                    "mask": {
                        "kind": "email",
                        "classification": "pii"
                    }
                }
            }),
        );
        let meta = lookup_mask_meta(app_id, "users", "contactEmail")
            .unwrap()
            .expect("mask metadata");
        assert_eq!(meta.canonical_column, "contact_email");
        assert_eq!(meta.classification, "pii");
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

    // ---------------------------------------------------------------
    // P5.5 PR 7 — parse_bulk_args validation
    // ---------------------------------------------------------------

    #[test]
    fn parse_bulk_args_round_trip_well_formed() {
        let v = json!({
            "collection": "users",
            "items": [
                { "rowPk": "usr_01", "columns": ["ssn", "email"] },
                { "rowPk": "usr_02", "columns": ["ssn"] },
            ],
            "actor": { "kind": "user", "id": "usr_actor" },
            "reason": "support ticket #42",
        });
        let args = parse_bulk_args(&v).unwrap();
        assert_eq!(args.collection, "users");
        assert_eq!(args.items.len(), 2);
        assert_eq!(args.items[0].row_pk, "usr_01");
        assert_eq!(args.items[0].columns, vec!["ssn", "email"]);
        assert_eq!(args.items[1].row_pk, "usr_02");
        assert_eq!(args.items[1].columns, vec!["ssn"]);
        assert!(args.actor.is_some());
        assert_eq!(args.reason.as_deref(), Some("support ticket #42"));
    }

    #[test]
    fn parse_bulk_args_accepts_snake_case_row_pk() {
        // Mirror the single-cell `row_pk` accepted shape — bulk SDK
        // calls coming through `row_pk` (rather than `rowPk`) must
        // still parse.
        let v = json!({
            "collection": "users",
            "items": [{ "row_pk": "u1", "columns": ["ssn"] }],
        });
        let args = parse_bulk_args(&v).unwrap();
        assert_eq!(args.items[0].row_pk, "u1");
    }

    #[test]
    fn parse_bulk_args_rejects_non_object() {
        let v = json!("nope");
        let err = parse_bulk_args(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_bulk_unmask_args");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_bulk_args_rejects_missing_collection() {
        let v = json!({ "items": [{ "rowPk": "u1", "columns": ["ssn"] }] });
        let err = parse_bulk_args(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_bulk_unmask_args");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_bulk_args_rejects_non_array_items() {
        let v = json!({ "collection": "users", "items": "not-an-array" });
        let err = parse_bulk_args(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "invalid_bulk_unmask_args");
                assert!(message.contains("items"), "diagnostic: {message}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_bulk_args_rejects_empty_row_pk() {
        let v = json!({
            "collection": "users",
            "items": [{ "rowPk": "", "columns": ["ssn"] }],
        });
        let err = parse_bulk_args(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_bulk_unmask_args");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_bulk_args_rejects_non_string_column() {
        let v = json!({
            "collection": "users",
            "items": [{ "rowPk": "u1", "columns": [42] }],
        });
        let err = parse_bulk_args(&v).unwrap_err();
        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "invalid_bulk_unmask_args");
                assert!(message.contains("columns"), "diagnostic: {message}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_bulk_args_treats_null_actor_as_none() {
        let v = json!({
            "collection": "users",
            "items": [{ "rowPk": "u1", "columns": ["ssn"] }],
            "actor": null,
        });
        let args = parse_bulk_args(&v).unwrap();
        assert!(args.actor.is_none());
    }

    // ---------------------------------------------------------------
    // P5.5 PR 7 — dispatch_bulk_unmask atomic auth fence
    // ---------------------------------------------------------------

    #[test]
    fn bulk_unmask_empty_items_returns_empty_result() {
        let app_id = "bulk_unit_empty_app";
        let runtime = compio::runtime::Runtime::new().unwrap();
        let args = BulkUnmaskArgs {
            collection: "users".into(),
            items: vec![],
            actor: Some(json!({ "kind": "auto" })),
            reason: None,
        };
        let result = runtime.block_on(dispatch_bulk_unmask(app_id, args)).unwrap();
        assert!(result.results.is_empty());
    }

    #[test]
    fn bulk_unmask_unknown_column_returns_typed_error() {
        // No schema cached for this app → every column lookup fails
        // with `unmask_column_not_masked`. Pin the typed error code
        // so SDK callers can branch on .code.
        let app_id = "bulk_unit_unknown_column_app";
        let runtime = compio::runtime::Runtime::new().unwrap();
        let args = BulkUnmaskArgs {
            collection: "users".into(),
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["mystery".into()],
            }],
            actor: Some(json!({ "kind": "auto" })),
            reason: None,
        };
        let err = runtime.block_on(dispatch_bulk_unmask(app_id, args)).unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // P5.5 PR 7 — authorize_query_hint unit behaviour
    // ---------------------------------------------------------------

    #[test]
    fn query_hint_empty_columns_no_op() {
        let app_id = "qhint_unit_empty_app";
        let runtime = compio::runtime::Runtime::new().unwrap();
        let ok = runtime.block_on(authorize_query_hint(
            app_id,
            "users",
            &[],
            &Some(json!({ "kind": "user" })),
            &None,
        ));
        assert!(ok.is_ok());
    }

    #[test]
    fn query_hint_unknown_column_returns_typed_error() {
        // No schema cached → unknown column is the typed-error path.
        let app_id = "qhint_unit_unknown_app";
        let runtime = compio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(authorize_query_hint(
                app_id,
                "users",
                &["nonexistent".to_string()],
                &Some(json!({ "kind": "auto" })),
                &None,
            ))
            .unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_unmask_for_query_empty_columns_is_noop() {
        let app_id = "qhint_unit_empty_dispatch_app";
        let runtime = compio::runtime::Runtime::new().unwrap();
        let mut rows = vec![json!({ "id": "u1", "name": "alice" })];
        let original = rows.clone();
        runtime
            .block_on(dispatch_unmask_for_query(app_id, "users", &[], &mut rows))
            .unwrap();
        assert_eq!(rows, original, "empty unmask columns must be a no-op");
    }
}
