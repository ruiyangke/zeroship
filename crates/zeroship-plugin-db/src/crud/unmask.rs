//! `unmask()` RPC + authorization + audit trail.
//!
//! The MaskedValue surface (`sdks/db/src/types.ts`) calls into this
//! module via the `zeroship.db.unmaskField` native op (registered as a
//! `#[v8_method]` on `Db`). The flow follows §6 of
//! `docs/archive/sensitive-field-masking.md`:
//!
//! 1. Look up the column's mask metadata (classification) from the
//!    cached schema. A column with no mask declaration cannot be
//!    unmasked — surface `unmask_column_not_masked`.
//! 2. Authorise via [`check_unmask_authorization`]: consult the app's
//!    per-app [`crate::crud::mask_policy::MaskPolicy`] (configured via `defineMaskPolicy()`) when
//!    one is cached; otherwise fall back to a strict default-deny rule
//!    where only the `auto` actor kind (system / migrations /
//!    background jobs) can unmask any classification. Denied attempts
//!    emit an audit row with `outcome = "denied"`.
//! 3. Look up the column's encryption metadata. If encrypted, SELECT
//!    the BYTEA / BLOB ciphertext, reconstruct the canonical AAD
//!    (Camp A — row_pk in AAD for Randomised, omitted for Deterministic),
//!    and decrypt through the backend's [`crate::backend::EncryptedColumn`]
//!    impl. If plaintext (mask-only, no encryption), SELECT the parent
//!    column directly.
//! 4. Emit a `granted`-outcome audit row to `__zeroship_audit_unmask`
//!    in the app's own schema.
//! 5. Return the plaintext.
//!
//! ## Audit-table location, and who creates it
//!
//! `__zeroship_audit_unmask` lives in the **per-app schema** (alongside
//! `__zeroship_schema_migrations`). App-scoped audit data should not
//! require platform-role access to query — operators query via the
//! per-app schema. A platform-wide `__zeroship_admin` schema was
//! proposed for it and refused; it is now deleted outright (see
//! `crate::auth`).
//!
//! **This module does not create it.** It did until 2026-08-28, from
//! `write_audit_unmask_row`, which meant eight DDL statements — one
//! `CREATE TABLE IF NOT EXISTS` and three `CREATE INDEX IF NOT EXISTS`
//! per dialect arm — on every `unmask()` dispatch, granted and denied
//! alike, on the privileged read path, issued by the process that runs
//! creator code. Schema change belongs to `zeroship-migrate`; the data
//! plane emits no DDL, and this was the last live site.
//!
//! The table now has the same lifecycle as the app's schema itself,
//! established by whichever migration apply host owns that dialect:
//!
//! | Dialect | Creator |
//! |---|---|
//! | Postgres | `zeroship_migrate_server::provisioning::provision_audit_unmask_table`, from the apply path |
//! | SQLite | `zeroship_migrate_sqlite::backend::audit_unmask_sql`, from the dev-tier `applyIrSqlite` host |
//!
//! Both are idempotent and run on every apply, so a redeploy is what
//! gives an app provisioned before the table existed its copy. Neither
//! runs in the worker.
//!
//! The two definitions are deliberately NOT shared. They cannot be: the
//! shapes differ by dialect (`BIGSERIAL` vs `INTEGER PRIMARY KEY`,
//! `TIMESTAMPTZ DEFAULT NOW()` vs `TEXT DEFAULT CURRENT_TIMESTAMP`), which
//! is the same split the engine's own journal tables already take —
//! `zeroship-migrate-postgres` and `zeroship-migrate-sqlite` each hold
//! their own `journal_sql.rs`.
//!
//! ## Default-deny authorization fallback
//!
//! [`check_unmask_authorization`] is deliberately strict when no policy
//! is configured: most real apps need `defineMaskPolicy()` to grant
//! access to non-system callers. Without a configured policy, only the
//! `auto` system actor can unmask.

use base64::Engine as _;
use serde_json::Value;

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;

// ---------------------------------------------------------------------------
// Args / result shape
// ---------------------------------------------------------------------------

/// Inputs to `dispatch_unmask`. Mirrors the `MaskedValue._meta` payload
/// the SDK ships through `zeroship.db.unmaskField({...})`. The `actor`
/// argument is the bare `Actor = Record<string, unknown>` shape; only
/// `actor.kind` and `actor.id` are inspected.
///
/// `pub` under `test-helpers` so `tests/sqlite_integration.rs` can drive
/// `dispatch_unmask` directly; production callers reach this through
/// the V8 dispatcher's `parse_args`.
#[derive(Debug, Clone, Default)]
pub struct UnmaskFieldArgs {
    pub collection: String,
    pub row_pk: String,
    pub column: String,
    /// Opaque actor descriptor. Only `actor.kind` and `actor.id` are
    /// inspected. `None` → unauthenticated caller (default-denied).
    pub actor: Option<Value>,
    /// Free-text rationale recorded on the audit row. Truncation is the
    /// caller's responsibility — long reasons are stored verbatim.
    pub reason: Option<String>,
    /// The actor claim [`sanitize_app_actor`] REFUSED, kept for the audit row
    /// and for nothing else.
    ///
    /// This is never consulted by [`check_unmask_authorization`] and never
    /// written to `actor_id` / `actor_role`. It exists because stripping the
    /// claim - which is correct - also erased the only evidence that anyone
    /// tried to make it: a forged `kind: "auto"` and a genuinely absent actor
    /// both arrived here as `actor: None` and audited identically.
    pub rejected_claim: Option<Value>,
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

/// Cached schema record for a single column. Pulled from the descriptor store
/// populated by native runtime boot.
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

/// Map the caller's column spelling onto the name the descriptor declares.
///
/// `schema` is the caller's already-resolved descriptor entry: every entry
/// point below resolves the collection ONCE through
/// [`crate::descriptor::collection_schema`], so an undeclared collection is
/// refused before any mask metadata is consulted. `None` here means the
/// declared entry has no such column under any of the three spellings.
fn resolve_schema_column(schema: &Value, column: &str) -> Option<String> {
    let obj = schema.as_object()?;
    if obj.contains_key(column) {
        return Some(column.to_string());
    }
    let snake = to_snake_case_alias(column);
    if snake != column && obj.contains_key(&snake) {
        return Some(snake);
    }
    let camel = to_camel_case_alias(column);
    if camel != column && camel != snake && obj.contains_key(&camel) {
        return Some(camel);
    }
    None
}

/// Walk the descriptor entry and return the mask metadata for `column`, or
/// `None` if the column has no `mask` block (or is opted out via
/// `kind: "none"`).
fn lookup_mask_meta(schema: &Value, column: &str) -> Option<ColumnMaskMeta> {
    let canonical_column = resolve_schema_column(schema, column)?;
    let obj = schema.as_object()?;
    let def = obj.get(&canonical_column)?;
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind = mask_meta
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    if kind == "none" {
        // Explicit opt-out — the parent column stays plaintext on read,
        // so unmask has nothing to do (and there's no `MaskedValue` for
        // the SDK to construct an `.unmask()` call from). Surface as
        // "not masked" so the caller sees the same error code they
        // would for a non-masked column.
        return None;
    }
    let classification = mask_meta
        .get("classification")
        .and_then(|v| v.as_str())
        .unwrap_or("pii")
        .to_string();
    Some(ColumnMaskMeta {
        canonical_column,
        classification,
    })
}

/// Encryption metadata for the target column (when present).
///
/// Carried no `#[allow(dead_code)]` justification that was true. The old one
/// said `key_id` and `wraps` are "only consumed under the `feature = "pg"` or
/// `feature = "sqlite"` arms" and that "a build with no backend feature never
/// reads them". There is no backend feature on this crate: both arms compile
/// into every binary and the url scheme picks one at runtime, so there is no
/// such build.
///
/// All three fields are read unconditionally - `mode` in `fetch_and_decrypt`'s
/// AAD branch and both decrypt calls, `key_id` in both `resolve_key` calls,
/// `wraps` in both `wrap_plaintext_per_wraps` calls - so the allow was
/// suppressing a warning that could not fire. Removed rather than reworded.
struct ColumnEncryptionMeta {
    mode: crate::backend::EncryptionMode,
    key_id: String,
    wraps: &'static str,
}

/// Walk the descriptor entry and return the encryption metadata for `column`,
/// or `None` if the column has no `encrypted` block (mask-only /
/// plaintext-storage case).
fn lookup_encryption_meta(
    schema: &Value,
    column: &str,
) -> Result<Option<ColumnEncryptionMeta>, DbError> {
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
    Ok(Some(ColumnEncryptionMeta {
        mode,
        key_id,
        wraps,
    }))
}

// ---------------------------------------------------------------------------
// Authorization (per-app policy lookup)
// ---------------------------------------------------------------------------

/// Authorization for the unmask path. Resolution rules:
///
/// 1. **Unauthenticated** (`actor = None` or `actor.kind` missing) →
///    deny. The denied path still writes an audit row.
///
/// 2. **Cached per-app policy present** → consult
///    [`crate::crud::mask_policy::MaskPolicy::allows`]. That helper
///    enforces the `auto`-actor fallback rule (system actor allowed
///    by default unless the policy explicitly restricts it).
///
/// 3. **No cached policy** → fall back to a strict default-deny rule
///    (`auto` allowed; everyone else denied). The real first-use load
///    happens inside [`dispatch_unmask`] before this helper runs —
///    that load is async, so cannot live here.
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
pub(crate) fn sanitize_app_actor(actor: Option<Value>) -> SanitizedActor {
    let kind = actor
        .as_ref()
        .and_then(|v| v.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or("");
    if RESERVED_SYSTEM_ACTOR_KINDS.contains(&kind) {
        SanitizedActor {
            actor: None,
            rejected_claim: actor,
        }
    } else {
        SanitizedActor {
            actor,
            rejected_claim: None,
        }
    }
}

/// What [`sanitize_app_actor`] decided: at most one of these is `Some`.
///
/// Two fields rather than one return value because the refused claim must be
/// recorded and must NEVER be usable as identity. Returning a bare
/// `Option<Value>` made those the same slot, so the only safe thing to do with a
/// refused claim was discard it - which erased the evidence that anyone tried.
/// A caller that wants identity reads [`Self::actor`]; the audit writer is the
/// only reader of [`Self::rejected_claim`].
#[derive(Debug, Clone, Default)]
pub(crate) struct SanitizedActor {
    /// The actor as it may be used for authorization. `None` when the caller
    /// sent none, AND when the claim was refused - those two are
    /// indistinguishable here on purpose, because they must be treated
    /// identically by every authorization decision.
    pub(crate) actor: Option<Value>,
    /// The claim that was refused, for the audit row only.
    pub(crate) rejected_claim: Option<Value>,
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
            // Default-deny fallback: only `auto` allowed when the app
            // has not declared a policy.
            Ok(kind == "auto")
        }
    }
}

/// Make sure the per-isolate policy cache has been consulted for
/// `app_id` before the auth check in [`dispatch_unmask`].
///
/// **On PG this is a no-op beyond the cache read.** The policy is
/// declared in the creator's source and installed at boot by
/// `crate::crud::mask_policy::dispatch_set_mask_policy`; there is no
/// durable store to fall back to, and an app that declared no policy
/// correctly lands on the default-deny rule below (`auto` only). The
/// PG arm used to `SELECT __zeroship_admin.get_mask_policy($1)` here,
/// which meant every unmask on an app with no cached policy failed with
/// `schema "__zeroship_admin" does not exist` rather than default-denying.
/// The routine and its store were deleted on 2026-08-27; see
/// `crate::crud::mask_policy`.
///
/// SQLite still keeps a sidecar file and is still re-read here. That
/// asymmetry is deliberate and flagged in the module header of
/// `crate::crud::mask_policy` - the sidecar is the arm that is out of
/// step, not this one.
///
/// A storage miss is a no-op (cache stays empty, the default-deny
/// fallback applies); a hit installs the loaded policy via
/// [`crate::context::ThreadDbContext::set_mask_policy_for_app`]. A
/// corrupt sidecar propagates as `DbError` so operators see the real
/// fault instead of a silent default-deny.
async fn ensure_mask_policy_cached(app_id: &str) -> Result<(), DbError> {
    if crate::context::with(|c| c.has_mask_policy(app_id)) {
        return Ok(());
    }
    let backend = match crate::context::with(|c| c.backend()) {
        Some(b) => b,
        None => return Ok(()), // backend not initialised — auth path's default-deny stub handles it
    };

    // ---- SQLite arm (the only durable store left) ----
    if let Some(sq) = backend.as_sqlite() {
        let loaded = crate::crud::mask_policy::load_sqlite(sq, app_id).await?;
        if let Some(p) = loaded {
            crate::context::with_mut(|c| c.set_mask_policy_for_app(app_id, Some(p)));
        }
    }
    Ok(())
}

/// Initialize the configured backend and bind the app file on SQLite before
/// an unmask path reaches its direct SQL helpers.
///
/// Ordinary CRUD gets the same binding from `exec`, but unmask fetches and
/// audit writes intentionally bypass that executor. The old schema bootstrap
/// call happened to initialize the backend and attach SQLite before either
/// path could run; lazy initialization must preserve that prerequisite without
/// restoring a per-collection boot RPC.
async fn ensure_unmask_backend(app_id: &str) -> Result<(), DbError> {
    let backend = crate::exec::ensure_backend_for_shared_sql().await?;
    if let Some(sqlite) = backend.as_sqlite() {
        sqlite.attach_app_file(app_id).await?;
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
    binding: &DbBinding,
    mut args: UnmaskFieldArgs,
) -> Result<UnmaskFieldResult, DbError> {
    let app_id = binding.app_id();
    // Step 0 — the descriptor entry. Resolved once for the whole dispatch: the
    // mask metadata, the column-spelling alias and the encryption metadata all
    // read it, and a collection this deploy does not declare is refused here
    // rather than reported as "column not masked".
    let schema = crate::descriptor::collection_schema(binding, &args.collection)?;
    // Step 1 — mask metadata lookup. A column with no mask declaration
    // (or `kind: "none"` opt-out) cannot be unmasked — there's no
    // `MaskedValue` for the SDK to dispatch from, and we don't want a
    // forged RPC to silently read plaintext through this path.
    let mask_meta =
        lookup_mask_meta(&schema, &args.column).ok_or_else(|| DbError::ValidationFailed {
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
    ensure_unmask_backend(app_id).await?;

    // Step 2 — authorization: load the per-app policy into the cache
    // (best-effort) THEN consult `check_unmask_authorization`, which
    // honours the cached policy or falls back to the default-deny
    // rule on a miss.
    ensure_mask_policy_cached(app_id).await?;
    let allowed = check_unmask_authorization(app_id, &args.actor, &mask_meta.classification)?;
    if !allowed {
        // Audit-then-refuse. The audit row carries `outcome = "denied"`
        // so operators see every attempted access — including the
        // `canUnmask()` probe path the SDK uses.
        write_audit_unmask_row(app_id, &args, &mask_meta.classification, "denied").await?;
        // The REFUSED unmask still wrote an audit row, and that row cost a
        // statement. Metering counts work performed, not permission granted.
        meter_audit_write(app_id);
        return Err(DbError::Coded {
            code: "unmask_not_permitted".into(),
            message: format!(
                "Actor not authorized to unmask classification '{}'",
                mask_meta.classification
            ),
            hint: Some(
                "Configure mask policy via defineMaskPolicy() in your app's bootstrap.".into(),
            ),
        });
    }

    // Step 3 — fetch + decrypt (or fetch-plaintext).
    let plaintext = match lookup_encryption_meta(&schema, &args.column)? {
        Some(enc_meta) => fetch_and_decrypt(app_id, &args, &enc_meta).await?,
        None => fetch_plaintext_parent(app_id, &args).await?,
    };
    // Both arms ran exactly one SELECT and both `?`, so reaching here means it
    // succeeded. Neither goes through `exec::run_sql`, so neither was billed
    // before 2026-09-01.
    crate::metrics::emit_db_metric(app_id, crate::metrics::DB_READS, 1);

    // Step 4 — audit the granted unmask. We do this AFTER the plaintext
    // is in hand so a SELECT failure / decrypt failure doesn't leave a
    // ghost "granted" row in the audit log (the failure surfaces a
    // typed error; the audit table reflects only completed unmasks).
    write_audit_unmask_row(app_id, &args, &mask_meta.classification, "granted").await?;
    meter_audit_write(app_id);

    Ok(UnmaskFieldResult { plaintext })
}

/// One audit-row append: one write op, one row.
///
/// Every unmask writes exactly one, granted or denied, and none of them goes
/// through `exec::exec_mutation`, so none was billed before 2026-09-01. Call
/// this only after the write's `?` has succeeded - metering is a success-arm
/// signal, and an audit row that failed to land must not be charged for.
fn meter_audit_write(app_id: &str) {
    crate::metrics::emit_db_metric(app_id, crate::metrics::DB_WRITES, 1);
    crate::metrics::emit_db_metric(app_id, crate::metrics::DB_ROWS_WRITTEN, 1);
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
        use crate::backend::EncryptedColumn as _;
        use crate::backend::pg_autocommit::ScalarRead;
        // The real value lives in the RAW column - the field's own column
        // holds the mask. This function and its SQLite twin are the only
        // readers of that column in the tree, and they sit behind
        // `check_unmask_authorization` and the `__zeroship_audit_unmask` row.
        let sql = format!(
            "SELECT \"{}\" FROM \"{}\".\"{}\" WHERE id = $1",
            crate::query::raw_column_name(&args.column),
            app_id,
            args.collection
        );
        // READ THE RAW SIBLING AS BYTES, NOT TEXT.
        //
        // The raw sibling of an ENCRYPTED column is BYTEA, and the funnel binds
        // every result in BINARY format (`libs/compio-postgres/src/query.rs:186`),
        // so there is no text rendering to parse.
        //
        // This asked for `Option<&str>` until 2026-09-01, and that is refused
        // outright rather than mis-parsed: `&str: FromSql::accepts` takes
        // VARCHAR/TEXT/BPCHAR/NAME/UNKNOWN plus citext/ltree and nothing else
        // (`libs/compio-postgres/vendor/postgres-types/src/lib.rs:729-742`), and
        // `Row::get_inner` consults `accepts` BEFORE decoding, even for NULL
        // (`libs/compio-postgres/src/row.rs:256`). So EVERY unmask of an
        // encrypted column on PostgreSQL failed, 100% of the time, with
        // `unmask: get column value: error deserializing column 0`. The comment
        // that used to sit here described `\xHHHH...` text arriving over the
        // text protocol - the legacy shape `decrypt_row_on_read` still keeps a
        // compatibility arm for, and one this call has never produced.
        //
        // The same reasoning is why this reads bytes through
        // `read_roled_scalar_bytes` rather than the JSON funnel the search
        // methods use: `pg_row_json::column_to_json` base64-encodes BYTEA, so
        // routing ciphertext through it would put the text round-trip back.
        //
        // Nothing caught it because every live PG unmask fixture declared a
        // masked but UNENCRYPTED column, and the SQLite twin below reads
        // `TypedCell::Blob` and was always correct. The regression test is
        // `unmask_encrypted_column_on_pg_reads_bytea_raw_sibling`.
        let bytes = match pg
            .read_roled_scalar_bytes(app_id, &sql, &[&args.row_pk])
            .await?
        {
            ScalarRead::NoRow => {
                return Err(DbError::ValidationFailed {
                    code: "unmask_not_found",
                    message: format!(
                        "row '{}' not found in '{}.{}'",
                        args.row_pk, app_id, args.collection
                    ),
                    hint: None,
                });
            }
            ScalarRead::Null => {
                return Err(DbError::ValidationFailed {
                    code: "unmask_value_null",
                    message: format!(
                        "column '{}' on row '{}' is NULL; nothing to unmask",
                        args.column, args.row_pk
                    ),
                    hint: None,
                });
            }
            ScalarRead::Value(bytes) => bytes,
        };
        let key = pg.resolve_key(app_id, &enc_meta.key_id).await?;
        let plaintext_bytes = pg.decrypt(&key, enc_meta.mode, &bytes, &aad)?;
        return wrap_plaintext_per_wraps(&plaintext_bytes, enc_meta.wraps);
    }

    // ---- SQLite arm ----
    if let Some(sq) = backend.as_encrypted_column_sqlite() {
        use crate::backend::sqlite::session::TypedCell;
        use crate::backend::{DialectBuilder as _, EncryptedColumn as _};
        let q_app = sq.quote_ident(app_id);
        let q_coll = sq.quote_ident(&args.collection);
        let q_col = sq.quote_ident(&crate::query::raw_column_name(&args.column));
        let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
        let handle = sq.autocommit_client();
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
        use crate::backend::pg_autocommit::ScalarRead;
        // The real value lives in the RAW column - the field's own column
        // holds the mask. This function and its SQLite twin are the only
        // readers of that column in the tree, and they sit behind
        // `check_unmask_authorization` and the `__zeroship_audit_unmask` row.
        //
        // Text, not bytes: this is the PLAINTEXT-storage path, so the raw
        // sibling is the column's own declared type. The encrypted path above
        // reads BYTEA and must use `read_roled_scalar_bytes`.
        let sql = format!(
            "SELECT \"{}\" FROM \"{}\".\"{}\" WHERE id = $1",
            crate::query::raw_column_name(&args.column),
            app_id,
            args.collection
        );
        return match pg
            .read_roled_scalar_text(app_id, &sql, &[&args.row_pk])
            .await?
        {
            ScalarRead::NoRow => Err(DbError::ValidationFailed {
                code: "unmask_not_found",
                message: format!(
                    "row '{}' not found in '{}.{}'",
                    args.row_pk, app_id, args.collection
                ),
                hint: None,
            }),
            ScalarRead::Null => Err(DbError::ValidationFailed {
                code: "unmask_value_null",
                message: format!(
                    "column '{}' on row '{}' is NULL; nothing to unmask",
                    args.column, args.row_pk
                ),
                hint: None,
            }),
            ScalarRead::Value(text) => Ok(text),
        };
    }

    // ---- SQLite arm ----
    if let Some(sq) = backend.as_sqlite() {
        use crate::backend::DialectBuilder as _;
        let q_app = sq.quote_ident(app_id);
        let q_coll = sq.quote_ident(&args.collection);
        let q_col = sq.quote_ident(&crate::query::raw_column_name(&args.column));
        let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
        let handle = sq.autocommit_client();
        let rows = handle.query_internal(&sql, &[args.row_pk.as_str()]).await?;
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
        let value =
            rows[0]
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

// `hex_to_bytes` and `nibble` lived here until 2026-09-01, carrying
// `#[allow(dead_code)]` and a doc line claiming they were "only reachable
// through `fetch_and_decrypt`'s PG arm". That arm asked BYTEA for a `&str` and
// so could never reach them - the driver refused the column before any hex
// existed to parse. Their only caller was their own unit test, which is the
// built-tested-unreferenced shape: a test proving a helper works says nothing
// about whether anything uses it. Reading the column as `&[u8]` removes the
// text detour entirely, so both are gone rather than re-homed.

// ---------------------------------------------------------------------------
// Audit row writer
// ---------------------------------------------------------------------------

/// Write one row into `<app>.__zeroship_audit_unmask`. Called from the
/// granted path on success AND the denied path on refusal — per
/// design Q-MASK-C "both granted and denied attempts logged".
///
/// The table is **per-app** (lives in the app's schema). Per-app
/// placement keeps audit data accessible to operators querying the
/// app's schema directly, without needing platform-role access.
///
/// # This function does not create the table, and that is the point
///
/// It used to. Every call ran `ensure_audit_unmask_table` first, which
/// issued `CREATE TABLE IF NOT EXISTS` plus three `CREATE INDEX IF NOT
/// EXISTS` on both dialects — eight DDL statements per `unmask()`
/// dispatch, granted and denied alike, on the privileged read path, from
/// the process that executes creator code. That was the last live DDL
/// the data plane emitted; schema change belongs to `zeroship-migrate`.
///
/// The creators are now the two migration apply hosts, which run before
/// the worker serves and do not execute creator code:
///
/// * Postgres — `zeroship_migrate_server::provisioning::provision_audit_unmask_table`,
///   called from the apply path BEFORE the runtime role's snapshot grants,
///   so the role reaches both the table and its `BIGSERIAL` sequence.
/// * SQLite — `zeroship_migrate_sqlite::backend::audit_unmask_sql`, called
///   by the dev-tier `applyIrSqlite` host after the envelopes deploy.
///
/// If an authority somehow has not run, this INSERT fails normally
/// ("relation does not exist" / "no such table") and the unmask refuses
/// with it. That is deliberate and it is the SAFE direction: an unmask
/// whose audit row cannot be written must not return plaintext, and the
/// granted-path caller sequences this before it hands the value back.
/// There is no create-on-demand fallback, because a fallback is a second
/// schema authority.
async fn write_audit_unmask_row(
    app_id: &str,
    args: &UnmaskFieldArgs,
    classification: &str,
    outcome: &str,
) -> Result<(), DbError> {
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
        let sql = format!(
            r#"INSERT INTO "{app_id}"."__zeroship_audit_unmask"
               (actor_id, actor_role, claimed_actor, collection, row_pk, "column",
                classification, reason, outcome)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
        );
        let actor_id_s: String = actor_id.unwrap_or_default();
        let actor_role_s: String = actor_role.unwrap_or_default();
        // The refused claim, serialised whole. Whole rather than picked apart
        // into id/kind because it is UNTRUSTED INPUT: an operator reading it is
        // reading what a handler SENT, and splitting it into the same shape as
        // the trusted columns is how the two get confused at a glance.
        let claimed_actor_s: String = args
            .rejected_claim
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let reason_s: String = args.reason.clone().unwrap_or_default();
        // text params: empty strings serve as NULL placeholders;
        // pg interprets `''` as TEXT, so we ROUTE truly-null fields
        // through `Option`-shaped params via NULLIF on the wire.
        // Simpler: just store empty strings as ''-typed text rows;
        // operators can `WHERE actor_id = ''` to filter. Trading
        // perfect NULL fidelity for codepath simplicity is fine
        // here — the audit table is operator-read-only.
        //
        // THROUGH THE FENCE, not around it. This used to call
        // `pool.query_text_params` on a bare checkout, which is a fresh pool
        // connection carrying the shared `zeroship_worker` login role and NO
        // `SET LOCAL ROLE` - the single ungated production path in this crate
        // that reached a tenant schema unfenced, while the two sibling readers
        // above (`fetch_and_decrypt`, `fetch_plaintext_parent`) took the same
        // pool and routed it through the roled funnel. Since
        // `runtime_dependents_sql` grants the runtime role `WITH INHERIT
        // FALSE`, the bare form no longer has the privilege and this INSERT
        // fails with `permission denied for table __zeroship_audit_unmask` -
        // which, because an unmask whose audit row cannot be written must not
        // return plaintext, would have failed every unmask rather than leaking
        // one. The funnel also brings the DB-1 statement/lock timeouts, which
        // the bare call never had.
        //
        // As of 2026-09-01 no production caller reaches a pool at all: this
        // file was the last one, and it now goes through `PostgresBackend`'s
        // roled entry points. `PgSqlExecutor::pool_handle` still EXISTS, so
        // the bare form is discouraged rather than impossible - its four
        // remaining callers are all in the `test-helpers`-gated
        // `crud::mask_drift`, and three of them issue DDL the per-app role is
        // not granted, so they cannot simply be routed through this fence.
        // Removing the accessor is tracked separately.
        pg.execute_roled(
            app_id,
            &sql,
            &[
                &actor_id_s,
                &actor_role_s,
                &claimed_actor_s,
                &args.collection,
                &args.row_pk,
                &args.column,
                classification,
                &reason_s,
                outcome,
            ],
        )
        .await?;
        return Ok(());
    }

    // ---- SQLite arm ----
    if let Some(sq) = backend.as_sqlite() {
        use crate::backend::{DialectBuilder as _, SqlExecutor as _};
        let q_app = sq.quote_ident(app_id);
        let sql = format!(
            r#"INSERT INTO {q_app}."__zeroship_audit_unmask"
               (actor_id, actor_role, claimed_actor, collection, row_pk, "column",
                classification, reason, outcome)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#
        );
        let actor_id_s: String = actor_id.unwrap_or_default();
        let actor_role_s: String = actor_role.unwrap_or_default();
        let claimed_actor_s: String = args
            .rejected_claim
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let reason_s: String = args.reason.clone().unwrap_or_default();
        sq.pool_exec(
            &sql,
            &[
                actor_id_s.as_str(),
                actor_role_s.as_str(),
                claimed_actor_s.as_str(),
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

// ===========================================================================
// Bulk unmask + per-query unmask hint
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

/// Args for the bulk unmask dispatcher.
///
/// `items[i].columns` is the list of column names to unmask on
/// `items[i].row_pk`. An item with an empty `columns` list is treated
/// as a no-op for that row.
#[derive(Debug, Clone)]
pub struct BulkUnmaskItem {
    pub row_pk: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BulkUnmaskArgs {
    pub collection: String,
    pub items: Vec<BulkUnmaskItem>,
    pub actor: Option<Value>,
    pub reason: Option<String>,
    /// The refused actor claim, for the audit row only. See
    /// [`UnmaskFieldArgs::rejected_claim`].
    pub rejected_claim: Option<Value>,
}

/// Result of a successful bulk unmask.
///
/// `results[row_pk][column]` carries the plaintext for every requested
/// pair. The shape mirrors the SDK's `Map<id, Record<col, plaintext>>`
/// so the JS caller materialises it directly.
#[derive(Debug, Clone, Default)]
pub struct BulkUnmaskResult {
    pub results: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

/// Public dispatch entry for `zeroship.db.bulkUnmaskFields`.
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
    binding: &DbBinding,
    args: BulkUnmaskArgs,
) -> Result<BulkUnmaskResult, DbError> {
    if args.items.is_empty() {
        return Ok(BulkUnmaskResult::default());
    }
    let app_id = binding.app_id();

    // ---- Step 0 — the descriptor entry, resolved once for every pair.
    let schema = crate::descriptor::collection_schema(binding, &args.collection)?;

    // Per-column classification cache so we don't re-walk the schema
    // map N×M times. `None` slot = column has no mask declaration →
    // typed error.
    let mut classifications: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut normalized_items: Vec<(String, Vec<(String, String)>)> =
        Vec::with_capacity(args.items.len());
    let mut normalized_audit_items: Vec<BulkUnmaskItem> = Vec::with_capacity(args.items.len());

    for item in &args.items {
        let mut normalized_columns: Vec<(String, String)> = Vec::with_capacity(item.columns.len());
        let mut audit_columns: Vec<String> = Vec::with_capacity(item.columns.len());
        for col in &item.columns {
            let mask_meta =
                lookup_mask_meta(&schema, col).ok_or_else(|| DbError::ValidationFailed {
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
        // Carried, not dropped: this value exists to reach the audit row, and
        // this struct is what the audit writer reads.
        rejected_claim: args.rejected_claim.clone(),
    };

    // ---- Step 1: only after every column is validated, initialize the
    // backend, load policy, and check authorization. Invalid descriptor input
    // keeps its typed validation error even when no database is configured.
    ensure_unmask_backend(app_id).await?;
    ensure_mask_policy_cached(app_id).await?;
    let mut unauthorized: Vec<(String, String)> = Vec::new(); // (row_pk, column)
    for (row_pk, columns) in &normalized_items {
        for (_, canonical_column) in columns {
            let classification = classifications
                .get(canonical_column)
                .expect("normalized columns always have a classification");
            if !check_unmask_authorization(app_id, &args.actor, classification)? {
                unauthorized.push((row_pk.clone(), canonical_column.clone()));
            }
        }
    }

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
        meter_audit_write(app_id);
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
                rejected_claim: args.rejected_claim.clone(),
            };
            // Read encryption / plaintext path identically to the
            // single-cell helper — we already checked auth, so call
            // the FETCH helpers directly (not `dispatch_unmask`,
            // which would re-audit per pair). This is the
            // "wrap-over-many" pattern the proposal describes.
            let plaintext = match lookup_encryption_meta(&schema, canonical_col)? {
                Some(enc_meta) => fetch_and_decrypt(app_id, &single_args, &enc_meta).await?,
                None => fetch_plaintext_parent(app_id, &single_args).await?,
            };
            // One SELECT per (row, column) pair. The bulk call writes a single
            // audit row for the whole request, but it reads once per cell, and
            // the read cost is what this counts.
            crate::metrics::emit_db_metric(app_id, crate::metrics::DB_READS, 1);
            row_map.insert(requested_col.clone(), plaintext);
        }
    }

    // ---- Step 4 — single audit row for the whole call on success.
    write_audit_bulk_row(
        app_id,
        &normalized_audit_args,
        &classifications,
        "granted",
        None,
    )
    .await?;
    meter_audit_write(app_id);

    Ok(out)
}

/// Write the single audit row covering an entire
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
    let classification_joined = class_set.iter().cloned().collect::<Vec<_>>().join(",");

    let caller_reason = args.reason.clone().unwrap_or_default();
    let reason_text = match unauthorized {
        Some(pairs) => {
            let detail: Vec<String> = pairs.iter().map(|(r, c)| format!("{r}/{c}")).collect();
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
        rejected_claim: args.rejected_claim.clone(),
    };
    write_audit_unmask_row(app_id, &synthetic, &classification_joined, outcome).await
}

// ---------------------------------------------------------------------------
// Per-query unmask hint
// ---------------------------------------------------------------------------

/// Pre-query authorization for the
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
    binding: &DbBinding,
    collection: &str,
    unmask_columns: &[String],
    actor: &Option<Value>,
    // The refused claim, for the audit row only - see
    // `SanitizedActor::rejected_claim`. Callers that never sanitise (tests
    // building args in Rust) pass `None`.
    rejected_claim: Option<&Value>,
    reason: &Option<String>,
) -> Result<(), DbError> {
    if unmask_columns.is_empty() {
        return Ok(());
    }
    let app_id = binding.app_id();

    let schema = crate::descriptor::collection_schema(binding, collection)?;

    let mut classifications: Vec<String> = Vec::with_capacity(unmask_columns.len());
    for col in unmask_columns {
        let mask_meta = lookup_mask_meta(&schema, col)
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
        classifications.push(mask_meta.classification);
    }

    ensure_unmask_backend(app_id).await?;
    ensure_mask_policy_cached(app_id).await?;
    let mut unauthorized: Vec<String> = Vec::new();
    for (column, classification) in unmask_columns.iter().zip(&classifications) {
        if !check_unmask_authorization(app_id, actor, classification)? {
            unauthorized.push(column.clone());
        }
    }

    if !unauthorized.is_empty() {
        write_audit_query_hint_row(
            app_id,
            collection,
            unmask_columns,
            &classifications,
            actor,
            rejected_claim,
            reason,
            "denied",
            Some(&unauthorized),
        )
        .await?;
        meter_audit_write(app_id);
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

/// Write the audit row for a successful per-query
/// unmask hint. Called by the find dispatcher AFTER the SELECT lands.
/// Single row per query (NOT per row), so the audit-log volume scales
/// with query count not row count.
pub async fn audit_query_hint_granted(
    binding: &DbBinding,
    collection: &str,
    unmask_columns: &[String],
    actor: &Option<Value>,
    // See `authorize_query_hint`. A granted hint normally carries no refused
    // claim, but the parameter exists so the two audit arms cannot drift.
    rejected_claim: Option<&Value>,
    reason: &Option<String>,
) -> Result<(), DbError> {
    if unmask_columns.is_empty() {
        return Ok(());
    }
    let app_id = binding.app_id();
    // Re-resolve classifications for the audit row. Cheap — the descriptor
    // lookup is a HashMap read.
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    ensure_unmask_backend(app_id).await?;
    let mut classifications: Vec<String> = Vec::with_capacity(unmask_columns.len());
    for col in unmask_columns {
        let cls = lookup_mask_meta(&schema, col)
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
        rejected_claim,
        reason,
        "granted",
        None,
    )
    .await?;
    meter_audit_write(app_id);
    Ok(())
}

/// Rewrite rows from a `find` result so the
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
    binding: &DbBinding,
    collection: &str,
    unmask_columns: &[String],
    rows: &mut [Value],
) -> Result<(), DbError> {
    if unmask_columns.is_empty() {
        return Ok(());
    }
    let app_id = binding.app_id();
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    ensure_unmask_backend(app_id).await?;
    for row in rows.iter_mut() {
        let Some(row_pk) = row.get("id").map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        }) else {
            continue;
        };
        if row_pk.is_empty() {
            // No PK — can't fetch ciphertext. Leave the row's
            // `MaskedValue` wrapping in place; the caller's TypeScript
            // type narrows to plaintext only when row_pk is present.
            continue;
        }
        for col in unmask_columns {
            // Resolve the caller's spelling to the DECLARED one before reading,
            // the way both siblings do: `dispatch_unmask` assigns
            // `args.column = mask_meta.canonical_column`, and
            // `dispatch_bulk_unmask` carries it through to its fetch.
            //
            // This path used to skip it, and `authorize_query_hint` keeps only
            // the classification, so the caller's spelling reached
            // `raw_column_name` - a bare prefix with no normalisation. A hint of
            // `contactEmail` against a descriptor declaring `contact_email`
            // therefore AUTHORISED correctly (`resolve_schema_column` is
            // alias-tolerant) and then read `__zs_raw__contactEmail`, which no
            // migration creates. It failed closed, but three siblings behaving
            // two ways at one boundary is how the next divergence gets in.
            let canonical = lookup_mask_meta(&schema, col)
                .map_or_else(|| col.clone(), |meta| meta.canonical_column);
            // Use the single-cell fetch helpers directly — auth was
            // already checked upstream via `authorize_query_hint`.
            let single_args = UnmaskFieldArgs {
                collection: collection.to_string(),
                row_pk: row_pk.clone(),
                column: canonical.clone(),
                actor: None,
                reason: None,
                // A fetch helper, not an audited dispatch: authorization already
                // happened upstream in `authorize_query_hint`, which is also
                // where any refused claim was recorded.
                rejected_claim: None,
            };
            let plaintext = match lookup_encryption_meta(&schema, &canonical)? {
                Some(enc_meta) => fetch_and_decrypt(app_id, &single_args, &enc_meta).await?,
                None => fetch_plaintext_parent(app_id, &single_args).await?,
            };
            // One SELECT per (row, column) pair. The bulk call writes a single
            // audit row for the whole request, but it reads once per cell, and
            // the read cost is what this counts.
            crate::metrics::emit_db_metric(app_id, crate::metrics::DB_READS, 1);
            if let Some(obj) = row.as_object_mut() {
                // Under the DECLARED name, because that is the key the row
                // already carries: the SELECT projects descriptor keys, so
                // inserting under the caller's spelling would leave the masked
                // value in place and add a second, differently-spelled field.
                obj.insert(canonical.clone(), Value::String(plaintext));
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
    // The claim `sanitize_app_actor` refused, if any. A separate parameter
    // rather than a field of `actor` because it must never be reachable from
    // anything that decides authorization.
    rejected_claim: Option<&Value>,
    reason: &Option<String>,
    outcome: &str,
    unauthorized: Option<&[String]>,
) -> Result<(), DbError> {
    let columns_joined = unmask_columns.join(",");
    let class_set: std::collections::BTreeSet<String> = classifications.iter().cloned().collect();
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
        rejected_claim: rejected_claim.cloned(),
    };
    write_audit_unmask_row(app_id, &synthetic, &class_joined, outcome).await
}

// ---------------------------------------------------------------------------
// V8 dispatch glue
// ---------------------------------------------------------------------------

use zeroship_runtime::state::ResolveValue;

/// V8-facing dispatch helper. Returns the unresolved Promise; the
/// `dispatch_unmask` body runs as a spawned op and resolves with
/// `{ plaintext }` on success or rejects with the typed `OpError`.
///
/// Called from `v8_classes::db::Db::unmask_field` (the `#[v8_method]`
/// wrapping this entry point).
pub(crate) fn dispatch_unmask_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: &DbBinding,
    args_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);

    // Parse the args eagerly so a malformed shape surfaces a typed
    // error synchronously rather than racing the spawn.
    let parsed = parse_args(&args_v);
    let binding = binding.clone();

    // The parse error folds into `settle`'s error arm via `?`; it made the same
    // `reject_op` call the hand-rolled arm here did.
    state.borrow_mut().spawned_ops.push(Box::pin(super::settle(
        resolver,
        request_id,
        async move { dispatch_unmask(&binding, parsed?).await },
        |result| {
            // Wire shape: `{ plaintext: <string> }`. The SDK reads
            // `result.plaintext` directly; for `wraps = bytes` the
            // SDK base64-decodes on its side.
            ResolveValue::Json(serde_json::json!({ "plaintext": result.plaintext }).to_string())
        },
    )));

    promise
}

// ---------------------------------------------------------------------------
// The DB-3 boundary, cfg-forked so an integration target can reach it
// ---------------------------------------------------------------------------
//
// `parse_args` and `parse_bulk_args` are the two places where an `actor`
// supplied by APP JS meets `sanitize_app_actor`. That call IS the DB-3 fix, and
// while both parsers were private `fn` no integration target could drive them:
// every live-PG test in `tests/mask_flip.rs` built `UnmaskFieldArgs` /
// `BulkUnmaskArgs` in Rust and so entered BELOW the fence, leaving the fence
// itself covered only by in-module units that call `sanitize_app_actor`
// directly with no database behind it. A fence no integration test can reach is
// a fence nobody re-checks.
//
// Same shape as `backend` / `binding` / `crud` in `lib.rs`: `pub` under the
// test gate, `pub(crate)` in release, so the shipped surface is unchanged. The
// forked item is the thin visibility wrapper; the body below it is shared by
// both arms, so the two builds cannot drift.
#[cfg(any(test, feature = "test-helpers"))]
pub fn parse_args(v: &Value) -> Result<UnmaskFieldArgs, DbError> {
    parse_args_inner(v)
}

#[cfg(not(any(test, feature = "test-helpers")))]
pub(crate) fn parse_args(v: &Value) -> Result<UnmaskFieldArgs, DbError> {
    parse_args_inner(v)
}

/// Decode the JSON args coming from V8 into [`UnmaskFieldArgs`]. The
/// V8 boundary already converted the JS object to `serde_json::Value`
/// via `read_json_arg`; we just pluck the typed fields.
fn parse_args_inner(v: &Value) -> Result<UnmaskFieldArgs, DbError> {
    let obj = v.as_object().ok_or_else(|| DbError::ValidationFailed {
        code: "invalid_unmask_args",
        message: "unmaskField: args must be an object".into(),
        hint: Some("pass `{ collection, row_pk, column, actor?, reason? }`".into()),
    })?;
    let collection = require_string(obj, "collection")?;
    let row_pk = require_string(obj, "row_pk")?;
    let column = require_string(obj, "column")?;
    if row_pk.is_empty() {
        return Err(DbError::ValidationFailed {
            code: "invalid_unmask_args",
            message: "unmaskField: row_pk must be a non-empty string".into(),
            hint: Some("MaskedValue rows without an `id` cannot be unmasked".into()),
        });
    }
    // DB-3: app JS cannot claim the reserved `auto` system actor.
    let sanitized = sanitize_app_actor(obj.get("actor").cloned().filter(|v| !v.is_null()));
    let reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(UnmaskFieldArgs {
        collection,
        row_pk,
        column,
        actor: sanitized.actor,
        reason,
        rejected_claim: sanitized.rejected_claim,
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
// V8 dispatch glue for `bulkUnmaskFields`
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
    binding: &DbBinding,
    args_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);

    let parsed = parse_bulk_args(&args_v);
    let binding = binding.clone();

    state.borrow_mut().spawned_ops.push(Box::pin(super::settle(
        resolver,
        request_id,
        async move { dispatch_bulk_unmask(&binding, parsed?).await },
        |result| {
            // Wire shape: `{ results: { <rowPk>: { <col>: <plaintext> } } }`.
            // `BTreeMap` serialises as a JSON object with sorted
            // keys — deterministic for golden-snapshot tests. The reshaping is
            // JS-wire lowering, so it belongs on this side of the boundary.
            let mut obj = serde_json::Map::with_capacity(result.results.len());
            for (row_pk, cols) in result.results {
                let mut col_obj = serde_json::Map::with_capacity(cols.len());
                for (c, pt) in cols {
                    col_obj.insert(c, Value::String(pt));
                }
                obj.insert(row_pk, Value::Object(col_obj));
            }
            ResolveValue::Json(serde_json::json!({ "results": Value::Object(obj) }).to_string())
        },
    )));

    promise
}

// The bulk half of the DB-3 boundary. Visibility is forked for the reason
// spelled out above `parse_args`.
#[cfg(any(test, feature = "test-helpers"))]
pub fn parse_bulk_args(v: &Value) -> Result<BulkUnmaskArgs, DbError> {
    parse_bulk_args_inner(v)
}

#[cfg(not(any(test, feature = "test-helpers")))]
pub(crate) fn parse_bulk_args(v: &Value) -> Result<BulkUnmaskArgs, DbError> {
    parse_bulk_args_inner(v)
}

/// Parse the JS-side `{ collection, items: [{ rowPk, columns }], actor?, reason? }`
/// shape into [`BulkUnmaskArgs`]. Refuses non-object args, missing
/// fields, items that aren't an array, items missing `rowPk` /
/// `columns`, and columns arrays containing non-strings — every error
/// surfaces a typed `ValidationFailed { code: "invalid_bulk_unmask_args" }`
/// so the SDK can branch on `.code` deterministically.
fn parse_bulk_args_inner(v: &Value) -> Result<BulkUnmaskArgs, DbError> {
    let obj = v.as_object().ok_or_else(|| DbError::ValidationFailed {
        code: "invalid_bulk_unmask_args",
        message: "bulkUnmaskFields: args must be an object".into(),
        hint: Some(
            "pass `{ collection, items: [{ rowPk, columns }, ...], actor?, reason? }`".into(),
        ),
    })?;
    let collection = require_string_with_code(
        obj,
        "collection",
        "invalid_bulk_unmask_args",
        "bulkUnmaskFields",
    )?;
    let items_v =
        obj.get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| DbError::ValidationFailed {
                code: "invalid_bulk_unmask_args",
                message: "bulkUnmaskFields: `items` must be a non-empty array".into(),
                hint: None,
            })?;
    let mut items: Vec<BulkUnmaskItem> = Vec::with_capacity(items_v.len());
    for (i, item_v) in items_v.iter().enumerate() {
        let item_obj = item_v
            .as_object()
            .ok_or_else(|| DbError::ValidationFailed {
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
        let columns_v = item_obj
            .get("columns")
            .and_then(|v| v.as_array())
            .ok_or_else(|| DbError::ValidationFailed {
                code: "invalid_bulk_unmask_args",
                message: format!(
                    "bulkUnmaskFields: items[{i}].columns must be an array of strings"
                ),
                hint: None,
            })?;
        let mut columns: Vec<String> = Vec::with_capacity(columns_v.len());
        for (ci, col_v) in columns_v.iter().enumerate() {
            let s = col_v.as_str().ok_or_else(|| DbError::ValidationFailed {
                code: "invalid_bulk_unmask_args",
                message: format!("bulkUnmaskFields: items[{i}].columns[{ci}] must be a string"),
                hint: None,
            })?;
            columns.push(s.to_string());
        }
        items.push(BulkUnmaskItem { row_pk, columns });
    }
    // DB-3: app JS cannot claim the reserved `auto` system actor. The refused
    // claim rides along to the audit row so the attempt is recorded.
    let sanitized = sanitize_app_actor(obj.get("actor").cloned().filter(|v| !v.is_null()));
    let reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(BulkUnmaskArgs {
        collection,
        items,
        actor: sanitized.actor,
        reason,
        rejected_claim: sanitized.rejected_claim,
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
        let forged = sanitize_app_actor(Some(json!({ "kind": "auto" })));
        assert_eq!(forged.actor, None);
        // ...and the refused claim is RETAINED, so the audit row can record
        // that someone tried. Dropping it made a forged claim indistinguishable
        // from an absent actor in `__zeroship_audit_unmask`.
        assert_eq!(forged.rejected_claim, Some(json!({ "kind": "auto" })));

        // A non-reserved, app-declared actor kind passes through unchanged, and
        // produces no rejected claim.
        let ordinary = sanitize_app_actor(Some(json!({ "kind": "support_agent" })));
        assert_eq!(ordinary.actor, Some(json!({ "kind": "support_agent" })));
        assert_eq!(ordinary.rejected_claim, None);

        // No actor / missing kind stay as-is (denied downstream regardless),
        // and neither is a refused CLAIM - nothing was claimed.
        assert_eq!(sanitize_app_actor(None).actor, None);
        assert_eq!(sanitize_app_actor(None).rejected_claim, None);
        assert_eq!(
            sanitize_app_actor(Some(json!({}))).actor,
            Some(json!({}))
        );
        assert_eq!(sanitize_app_actor(Some(json!({}))).rejected_claim, None);
    }

    #[test]
    fn stripped_auto_actor_is_denied_by_authorization_db3() {
        // The fix's effect: a sanitized app actor (the `auto` claim stripped to
        // None) hits check_unmask_authorization's "unauthenticated → denied"
        // arm — which returns before consulting any policy. Pre-fix the raw
        // {kind:"auto"} reached the no-policy fallback and was GRANTED.
        let sanitized = sanitize_app_actor(Some(json!({ "kind": "auto" })));
        assert_eq!(sanitized.actor, None);
        assert!(
            !check_unmask_authorization("app_x", &sanitized.actor, "pii").unwrap(),
            "sanitized (stripped-auto) app actor must be denied"
        );
        // And the retained claim must not reach the authorization decision:
        // the checker takes `sanitized.actor`, never the whole struct.
        assert!(sanitized.rejected_claim.is_some());
    }

    // ---------------------------------------------------------------
    // Default-deny fallback — exercised by passing an `app_id` that
    // has no policy cached; these tests pin that fallthrough
    // behaviour.
    //
    // Unit tests reach the per-isolate THREAD_DB_CTX (which the
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
    // Per-app policy lookup
    // ---------------------------------------------------------------

    /// Helper: install a [`crate::crud::mask_policy::MaskPolicy`] for `app_id` on the current
    /// isolate's context cache, then immediately remove it on drop.
    /// Keeps the thread-local cache hygiene clean across tests.
    struct PolicyGuard(String);
    impl PolicyGuard {
        fn install(app_id: &str, policy: crate::crud::mask_policy::MaskPolicy) -> Self {
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
        let schema = json!({
            "contact_email": {
                "type": "string",
                "mask": {
                    "kind": "email",
                    "classification": "pii"
                }
            }
        });
        let meta = lookup_mask_meta(&schema, "contactEmail").expect("mask metadata");
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
            DbError::ValidationFailed {
                code: "invalid_unmask_args",
                ..
            }
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
        // 3.14 is test input data asserted against the literal string
        // "3.14" below — not a stand-in for pi, so it must stay exactly
        // this value rather than become `std::f64::consts::PI`.
        #[allow(clippy::approx_constant)]
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

    // ---------------------------------------------------------------
    // parse_bulk_args validation
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
    // dispatch_bulk_unmask atomic auth fence
    // ---------------------------------------------------------------

    #[test]
    fn bulk_unmask_empty_items_returns_empty_result() {
        let binding = DbBinding::cold_start("bulk_unit_empty_app");
        let runtime = compio::runtime::Runtime::new().unwrap();
        let args = BulkUnmaskArgs {
            collection: "users".into(),
            items: vec![],
            actor: Some(json!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let result = runtime
            .block_on(dispatch_bulk_unmask(&binding, args))
            .unwrap();
        assert!(result.results.is_empty());
    }

    #[test]
    fn bulk_unmask_unknown_column_returns_typed_error() {
        // The collection IS declared; the requested column is not one of its
        // fields. That is the `unmask_column_not_masked` case, and it must stay
        // distinguishable from `collection_not_declared` (see the sibling test
        // below) — an SDK caller branches on `.code`.
        //
        // This used to reach the same code by declaring NOTHING, which the
        // descriptor now refuses outright; asserting it that way would have
        // stopped measuring the column check the moment the refusal landed.
        let app_id = "bulk_unit_unknown_column_app";
        crate::cache_schema_for_tests(
            app_id,
            "users",
            json!({ "ssn": { "type": "string", "mask": { "kind": "last4" } } }),
        );
        let binding = DbBinding::cold_start(app_id);
        let runtime = compio::runtime::Runtime::new().unwrap();
        let args = BulkUnmaskArgs {
            collection: "users".into(),
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["mystery".into()],
            }],
            actor: Some(json!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let err = runtime
            .block_on(dispatch_bulk_unmask(&binding, args))
            .unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    }

    #[test]
    fn bulk_unmask_undeclared_collection_returns_typed_error() {
        // The counterpart to the test above: nothing declared at all is a
        // DIFFERENT refusal, and conflating the two would let a bulk unmask
        // against a collection this deploy cannot serve read as "no such
        // masked column".
        crate::reset_context_for_tests();
        let binding = DbBinding::cold_start("bulk_unit_undeclared_app");
        let runtime = compio::runtime::Runtime::new().unwrap();
        let args = BulkUnmaskArgs {
            collection: "users".into(),
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["ssn".into()],
            }],
            actor: Some(json!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let err = runtime
            .block_on(dispatch_bulk_unmask(&binding, args))
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("collection_not_declared"),
            "expected the descriptor refusal, got {err:?}",
        );
    }

    // ---------------------------------------------------------------
    // authorize_query_hint unit behaviour
    // ---------------------------------------------------------------

    #[test]
    fn query_hint_empty_columns_no_op() {
        let binding = DbBinding::cold_start("qhint_unit_empty_app");
        let runtime = compio::runtime::Runtime::new().unwrap();
        let ok = runtime.block_on(authorize_query_hint(
            &binding,
            "users",
            &[],
            &Some(json!({ "kind": "user" })),
            None,
            &None,
        ));
        assert!(ok.is_ok());
    }

    #[test]
    fn query_hint_unknown_column_returns_typed_error() {
        // Declared collection, undeclared column — the typed-error path. See
        // `bulk_unmask_unknown_column_returns_typed_error` for why this now
        // installs a schema instead of relying on an empty store.
        let app_id = "qhint_unit_unknown_app";
        crate::cache_schema_for_tests(
            app_id,
            "users",
            json!({ "ssn": { "type": "string", "mask": { "kind": "last4" } } }),
        );
        let binding = DbBinding::cold_start(app_id);
        let runtime = compio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(authorize_query_hint(
                &binding,
                "users",
                &["nonexistent".to_string()],
                &Some(json!({ "kind": "auto" })),
                None,
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
        let binding = DbBinding::cold_start("qhint_unit_empty_dispatch_app");
        let runtime = compio::runtime::Runtime::new().unwrap();
        let mut rows = vec![json!({ "id": "u1", "name": "alice" })];
        let original = rows.clone();
        runtime
            .block_on(dispatch_unmask_for_query(&binding, "users", &[], &mut rows))
            .unwrap();
        assert_eq!(rows, original, "empty unmask columns must be a no-op");
    }
}
