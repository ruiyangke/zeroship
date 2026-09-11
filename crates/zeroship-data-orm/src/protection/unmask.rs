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
//!    per-app [`crate::protection::mask_policy::MaskPolicy`] (configured via `defineMaskPolicy()`) when
//!    one is cached; otherwise fall back to a strict default-deny rule
//!    where only the `auto` actor kind (system / migrations /
//!    background jobs) can unmask any classification. Denied attempts
//!    emit an audit row with `outcome = "denied"`.
//! 3. Look up the column's encryption metadata. If encrypted, SELECT
//!    the BYTEA / BLOB ciphertext, reconstruct the canonical AAD
//!    including the row primary key,
//!    and decrypt through [`crate::encryption::aead`] with a key from the
//!    backend handle's [`crate::encryption::KeyStore`]. If plaintext
//!    (mask-only, no encryption), read the descriptor's raw storage column.
//! 4. Emit a `granted`-outcome audit row to `__zeroship_audit_unmask`
//!    in the app's own schema.
//! 5. Return the plaintext.
//!
//! ## Audit-table location, and who creates it
//!
//! `__zeroship_audit_unmask` lives in the **per-app schema** (alongside
//! `__zeroship_schema_migrations`). App-scoped audit data should not
//! require platform-role access to query — operators query via the
//! per-app schema. A platform-wide system schema was proposed for it
//! and refused; that schema is now deleted outright (see
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

use crate::encryption::plaintext::PlaintextType;
use zeroship_data_sql::value::Value;

use crate::backend::{BackendHandle, ScalarRead};
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

// ---------------------------------------------------------------------------
// Args / result shape
// ---------------------------------------------------------------------------

/// Inputs to `dispatch_unmask`. Mirrors the `MaskedValue._meta` payload
/// the SDK ships through `zeroship.db.unmaskField({...})`. The `actor`
/// argument is the bare `Actor = Record<string, unknown>` shape; only
/// `actor.kind` and `actor.id` are inspected.
///
/// `pub` under `test-helpers` so `crates/zeroship-data-v8/tests/sqlite_integration.rs` can drive
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

/// Native plaintext returned after authorization and audit.
#[derive(Debug, Clone)]
pub struct UnmaskFieldResult {
    pub plaintext: Value,
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
    let mask = zeroship_data_sql::descriptors::effective_mask(def)?;
    let classification = mask.classification.to_string();
    Some(ColumnMaskMeta {
        canonical_column,
        classification,
    })
}

/// The physical column holding `canonical_column`'s REAL value, as the
/// DESCRIPTOR names it.
///
/// The two fetch helpers below are the only readers of that column in the tree.
/// They formatted the name themselves until this existed, which was coherent
/// only while the write side did too: `protection::mask_pass` now places the value
/// under the name `zeroship_data_sql::compile::declared_raw_column` resolves, so a
/// SELECT that kept its own `format!` would miss every row a renamed column
/// stored - and on SQLite it would MISS QUIETLY, because a double-quoted
/// identifier that matches no column is taken as a string literal and the
/// "plaintext" that comes back is the column name.
///
/// Resolved by the two dispatchers rather than inside the fetchers so the
/// descriptor is consulted once per dispatch, beside the mask and encryption
/// lookups that already read it, and so a fetch helper cannot be called without
/// one.
///
/// # Errors
///
/// The refusal `declared_raw_column` raises for a descriptor naming a column
/// creator code could reach. Also [`DbError::internal`] if the column has no
/// mask, which is unreachable: every caller has already been through
/// [`lookup_mask_meta`], and that is the same test.
fn resolve_raw_column(schema: &Value, canonical_column: &str) -> Result<String, DbError> {
    let def = schema.get(canonical_column).ok_or_else(|| {
        DbError::internal(format!("unmask: column '{canonical_column}' vanished"))
    })?;
    crate::compile::declared_raw_column(canonical_column, def)?.ok_or_else(|| {
        DbError::internal(format!(
            "unmask: column '{canonical_column}' has no raw column but passed the mask lookup"
        ))
    })
}

/// Select the field's plaintext decoder, or leave mask-only storage alone.
fn lookup_encryption_meta(schema: &Value, column: &str) -> Result<Option<PlaintextType>, DbError> {
    schema
        .get(column)
        .map(PlaintextType::from_field)
        .transpose()
        .map(Option::flatten)
}

// ---------------------------------------------------------------------------
// Authorization (per-app policy lookup)
// ---------------------------------------------------------------------------

/// Actor `kind`s reserved for genuine platform/system callers (migration,
/// backfill, drift). The default-deny stub and the `auto`-fallback rule grant
/// these broad access; app JS must never be able to claim one.
pub const RESERVED_SYSTEM_ACTOR_KINDS: &[&str] = &["auto"];

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
pub fn sanitize_app_actor(actor: Option<Value>) -> SanitizedActor {
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
pub struct SanitizedActor {
    /// The actor as it may be used for authorization. `None` when the caller
    /// sent none, AND when the claim was refused - those two are
    /// indistinguishable here on purpose, because they must be treated
    /// identically by every authorization decision.
    pub actor: Option<Value>,
    /// The claim that was refused, for the audit row only.
    pub rejected_claim: Option<Value>,
}

/// Check the app-at-deploy startup policy without database or filesystem I/O.
pub fn check_unmask_authorization(
    binding: &DbBinding,
    actor: &Option<Value>,
    classification: &str,
) -> Result<bool, DbError> {
    let Some(actor_obj) = actor.as_ref().and_then(|v| v.as_object()) else {
        return Ok(false); // unauthenticated → denied
    };
    let kind = actor_obj.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    Ok(super::mask_policy::allows(binding, kind, classification))
}

/// Bind the app file on SQLite before an unmask path reaches its direct SQL
/// helpers.
///
/// Ordinary CRUD gets the same binding from `exec`, but unmask fetches and
/// audit writes intentionally bypass that executor. The old schema bootstrap
/// call happened to initialize the backend and attach SQLite before either
/// path could run; lazy initialization must preserve that prerequisite without
/// restoring a per-collection boot RPC.
///
/// **The backend is a PARAMETER, not something this function resolves.** It
/// resolved one itself - through `exec::ensure_backend_for_shared_sql`, which
/// read `crate::context` and called `crate::init_pool_async` - until
/// 2026-09-03. Both are ADAPTER state, and this file is ENGINE, so that call
/// was the one dependency direction the crate split forbids. The funnel now
/// lives at `crate::tx_scope::ensure_backend` and every entry point in this
/// module receives what it resolved: a routed one takes it off its `TxRoute`,
/// an unrouted one is handed the value the V8 dispatcher already opened. What
/// is left here is the half that is genuinely about unmask - the per-app
/// preparation - and it stays because these paths bypass `exec`.
async fn prepare_unmask_backend(backend: &BackendHandle, app_id: &str) -> Result<(), DbError> {
    // Asked, not downcast. What "ready for this app" means is the backend's
    // business - SQLite must attach the app file, PostgreSQL needs nothing -
    // and this path only needs it to have happened.
    backend.prepare_for_app(app_id).await
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
/// in the adapter tier's `dispatch_unmask_field` is the only crate-internal caller.
///
/// `route` is supplied by the caller because this function is ENGINE and the
/// funnel that opens a backend is ADAPTER state; the V8 dispatcher captures the
/// route at its own frame and hands the value down. See
/// `prepare_unmask_backend`.
///
/// **It is a `TxRoute` and not a bare `BackendHandle`.** A `MaskedValue.unmask()`
/// issued inside a `db.transaction(fn)` callback has to read the ciphertext on
/// that transaction's connection - a pooled read cannot see a row the same
/// transaction has just written. The audit row deliberately does NOT follow the
/// lane; see [`write_audit_unmask_row`].
pub async fn dispatch_unmask(
    route: &crate::tx_route::TxRoute,
    binding: &DbBinding,
    mut args: UnmaskFieldArgs,
) -> Result<UnmaskFieldResult, DbError> {
    let app_id = binding.app_id();
    // SCHEMA: the audit table is reached through it on PostgreSQL. `app_id`
    // above stays the TENANT - metering subject, SQLite ATTACH alias, key salt.
    let db_schema = binding.schema();
    let backend = route.backend();
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
    prepare_unmask_backend(backend, app_id).await?;

    // Authorize against the immutable declaration for this app and deploy.
    let allowed = check_unmask_authorization(binding, &args.actor, &mask_meta.classification)?;
    if !allowed {
        // Audit-then-refuse. The audit row carries `outcome = "denied"`
        // so operators see every attempted access — including the
        // `canUnmask()` probe path the SDK uses.
        write_audit_unmask_row(
            backend,
            db_schema,
            app_id,
            &args,
            &mask_meta.classification,
            "denied",
        )
        .await?;
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
                "Edit defineMaskPolicy() in the app source and redeploy to change permissions."
                    .into(),
            ),
        });
    }

    // Step 3 — fetch + decrypt (or fetch-plaintext). The raw column is
    // resolved from the same descriptor entry Step 0 pinned, so this SELECT
    // and the write that placed the value name one column.
    let raw_column = resolve_raw_column(&schema, &args.column)?;
    let plaintext = match lookup_encryption_meta(&schema, &args.column)? {
        Some(enc_meta) => fetch_and_decrypt(&raw_column, route, &args, &enc_meta).await?,
        None => fetch_plaintext_raw(&raw_column, route, &args).await?,
    };
    // Both arms ran exactly one SELECT and both `?`, so reaching here means it
    // succeeded. Neither goes through `exec::run_sql`, so neither was billed
    // before 2026-09-01.
    crate::metrics::emit_db_metric(app_id, crate::metrics::DB_READS, 1);

    // Step 4 — audit the granted unmask. We do this AFTER the plaintext
    // is in hand so a SELECT failure / decrypt failure doesn't leave a
    // ghost "granted" row in the audit log (the failure surfaces a
    // typed error; the audit table reflects only completed unmasks).
    write_audit_unmask_row(
        backend,
        db_schema,
        app_id,
        &args,
        &mask_meta.classification,
        "granted",
    )
    .await?;
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
/// `(collection, row_pk)`, reconstruct the row-bound AAD,
/// and decrypt through `encryption::aead`. The SELECT is
/// still per-arm because the SQL differs; the decrypt is not, and stopped
/// being so when `EncryptedColumn` was deleted on 2026-09-02.
/// Returns the plaintext as a native string, byte buffer, or number according
/// to the declared wrapped type.
#[allow(unused_variables)]
async fn fetch_and_decrypt(
    raw_column: &str,
    route: &crate::tx_route::TxRoute,
    args: &UnmaskFieldArgs,
    enc_meta: &PlaintextType,
) -> Result<Value, DbError> {
    // The prepared route arrives as an argument. This re-resolved a BACKEND
    // through the funnel until 2026-09-03, which was idempotent but pointless:
    // every reaching path runs `prepare_unmask_backend` first, so the second
    // lookup could only ever return what the caller already held. Taking it as
    // a parameter makes that ordering a data dependency rather than a comment.
    //
    // It became a ROUTE the same day, because a handle answers "which backend"
    // and this read also has to answer "which connection" - see
    // `crate::backend_handle::read_raw_column_bytes`.
    let app_id = route.app_id();

    let aad = crate::encryption::aad::canonical_aad(
        app_id,
        &args.collection,
        &args.column,
        args.row_pk.as_bytes(),
    );

    // ---- read the ciphertext ----
    //
    // The vendor split moved into `BackendHandle::read_raw_column_bytes` on
    // 2026-09-02 (#119). Everything below the read - key resolution, AEAD,
    // plaintext decoding - was already vendor-neutral and was duplicated
    // VERBATIM in the two arms this replaces.
    {
        // The real value lives in the RAW column - the field's own column
        // holds the mask. This read and its plaintext sibling are the only
        // readers of that column in the tree, and both sit behind
        // `check_unmask_authorization` and the `__zeroship_audit_unmask` row.
        //
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
        // Ciphertext is read as bytes on the captured transaction route.
        let bytes = match crate::backend_handle::read_raw_column_bytes(
            route,
            &args.collection,
            raw_column,
            &args.row_pk,
        )
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
        // Key sourcing and AEAD are vendor-neutral; only the read above was
        // not, and it now dispatches inside `crate::backend_handle`.
        let key = route
            .backend()
            .key_store()
            .resolve(app_id)
            .await?;
        let plaintext_bytes =
            zeroize::Zeroizing::new(crate::encryption::aead::decrypt(&key, &bytes, &aad)?);
        enc_meta.decode(&plaintext_bytes)
    }
}

/// Read mask-only plaintext from the runtime descriptor's `storage.rawColumn`.
async fn fetch_plaintext_raw(
    raw_column: &str,
    route: &crate::tx_route::TxRoute,
    args: &UnmaskFieldArgs,
) -> Result<Value, DbError> {
    // The prepared route arrives as an argument, for the reason spelled out
    // on `fetch_and_decrypt`.
    let app_id = route.app_id();

    // The real value lives in the RAW column - the field's own column holds
    // the mask. This read and its encrypted sibling are the only readers of
    // that column in the tree, and both sit behind
    // `check_unmask_authorization` and the `__zeroship_audit_unmask` row.
    //
    // Text, not bytes: this is the PLAINTEXT-storage path, so the raw sibling
    // is the column's own declared type. The encrypted path above reads bytes.
    //
    // The vendor split moved into `backend_handle::read_raw_column_value` on
    // 2026-09-02 (#119); the two error codes below are engine-tier policy and
    // stay here, which is why the backend returns a tri-state rather than
    // minting them itself.
    match crate::backend_handle::read_raw_column_value(
        route,
        &args.collection,
        raw_column,
        &args.row_pk,
    )
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
///
/// # This write is deliberately NOT on the caller's transaction lane
///
/// It takes a `BackendHandle` while the raw-column READ beside it takes a
/// `TxRoute`, and the asymmetry is the decision, not an oversight. `append_unmask_audit`
/// goes through the autocommit funnel on both vendors, so an audit row written
/// inside `db.transaction(fn)` COMMITS even when that transaction rolls back.
/// That is the behaviour we want in both directions:
///
/// * a DENIED row records an attempt that really happened. Letting the
///   attempt's own transaction erase it would hand any caller a one-line way to
///   try and leave no trace.
/// * a GRANTED row records that plaintext left the database. It did leave;
///   rolling the surrounding work back does not un-read it.
///
/// The audit table is append-only and operator-read-only, so nothing about the
/// creator's transaction is inconsistent afterwards - it holds no foreign key
/// into the rows the transaction touched, only their ids as text.
///
/// **This interacts with the DB-3 strip.** `sanitize_app_actor` zeroes the
/// actor on exactly the rows most likely to be denied, so a surviving denied
/// row can carry `actor_id = ''` and `actor_role = ''`. That is why
/// `claimed_actor` exists: the refused claim is preserved verbatim in its own
/// UNTRUSTED column rather than being erased with the identity it failed to
/// establish. A denied row with an empty actor and a populated `claimed_actor`
/// is an impersonation attempt; one with both empty is an anonymous call.
///
/// Bound by `plugin-db/tests/unmask_tx_lane.rs`, whose control writes an
/// ordinary row in the same transaction and asserts the ROLLBACK destroys THAT
/// and not this.
async fn write_audit_unmask_row(
    backend: &BackendHandle,
    // SCHEMA: where the audit table lives on PostgreSQL, and what the runtime
    // role the INSERT runs under is derived from.
    db_schema: &zeroship_data_sql::SchemaName,
    // TENANT: the SQLite ATTACH alias the same table is reached through on the
    // dev tier, and the metering subject.
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

    // The refused claim, serialised whole. Whole rather than picked apart into
    // id/kind because it is UNTRUSTED INPUT: an operator reading it is reading
    // what a handler SENT, and splitting it into the same shape as the trusted
    // columns is how the two get confused at a glance.
    let claimed_actor_s: String = args
        .rejected_claim
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let actor_id_s: String = actor_id.unwrap_or_default();
    let actor_role_s: String = actor_role.unwrap_or_default();
    let reason_s: String = args.reason.clone().unwrap_or_default();

    // The two INSERTs this replaces differed only in placeholder syntax and
    // identifier quoting, and the six lines above were written out twice - once
    // per vendor - behind an `as_postgres()` / `as_sqlite()` downcast. Both
    // statements now live in `BackendHandle::append_unmask_audit`, which also
    // carries the note about routing PostgreSQL through the role fence rather
    // than a bare pool checkout.
    backend
        .append_unmask_audit(
            db_schema,
            app_id,
            &crate::backend::UnmaskAuditRow {
                actor_id: &actor_id_s,
                actor_role: &actor_role_s,
                claimed_actor: &claimed_actor_s,
                collection: &args.collection,
                row_pk: &args.row_pk,
                column: &args.column,
                classification,
                reason: &reason_s,
                outcome,
            },
        )
        .await
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
    pub results: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Value>>,
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
///
/// `route` is supplied by the caller for the reason `prepare_unmask_backend`
/// gives: opening a backend reads ADAPTER state, and this is ENGINE. The V8
/// glue captures the route before it enters here. It is a route rather than a
/// bare handle for the reason [`dispatch_unmask`] gives.
pub async fn dispatch_bulk_unmask(
    route: &crate::tx_route::TxRoute,
    binding: &DbBinding,
    args: BulkUnmaskArgs,
) -> Result<BulkUnmaskResult, DbError> {
    if args.items.is_empty() {
        return Ok(BulkUnmaskResult::default());
    }
    let app_id = binding.app_id();
    // SCHEMA: the audit table is reached through it on PostgreSQL. `app_id`
    // above stays the TENANT - metering subject, SQLite ATTACH alias, key salt.
    let db_schema = binding.schema();
    let backend = route.backend();

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

    // ---- Step 1: only after every column is validated, prepare the
    // backend and check the startup policy. Invalid descriptor input
    // keeps its typed validation error rather than being reported as a
    // database fault.
    //
    // That ordering is now local to THIS function. It used to hold for the
    // whole op, because the backend was opened here; the caller opens it now,
    // so on an isolate with no database at all the V8 dispatch surfaces the
    // configuration error first. Nothing in production reaches that state -
    // the isolate always has a URL, and the dev tier opens SQLite lazily - and
    // the alternative is handing the engine an `Option` it would have to
    // unwrap at a statement.
    prepare_unmask_backend(backend, app_id).await?;
    let mut unauthorized: Vec<(String, String)> = Vec::new(); // (row_pk, column)
    for (row_pk, columns) in &normalized_items {
        for (_, canonical_column) in columns {
            let classification = classifications
                .get(canonical_column)
                .expect("normalized columns always have a classification");
            if !check_unmask_authorization(binding, &args.actor, classification)? {
                unauthorized.push((row_pk.clone(), canonical_column.clone()));
            }
        }
    }

    // ---- Step 2 — atomic-fence enforcement. If ANY pair denied,
    // emit one audit row covering the whole call + refuse.
    if !unauthorized.is_empty() {
        write_audit_bulk_row(
            backend,
            db_schema,
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
                "Drop the unauthorized columns or edit defineMaskPolicy() and redeploy.".into(),
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
            let raw_column = resolve_raw_column(&schema, canonical_col)?;
            let plaintext = match lookup_encryption_meta(&schema, canonical_col)? {
                Some(enc_meta) => {
                    fetch_and_decrypt(&raw_column, route, &single_args, &enc_meta).await?
                }
                None => fetch_plaintext_raw(&raw_column, route, &single_args).await?,
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
        backend,
        db_schema,
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
    backend: &BackendHandle,
    db_schema: &zeroship_data_sql::SchemaName,
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
    write_audit_unmask_row(
        backend,
        db_schema,
        app_id,
        &synthetic,
        &classification_joined,
        outcome,
    )
    .await
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
///
/// `backend` comes off the find's own [`crate::tx_route::TxRoute`] rather than
/// being resolved here: `run_find` already holds the handle its SELECT will run
/// on, and the fence, the SELECT and the audit row all have to be the same
/// backend. Resolving a second one here would also put this ENGINE file's hands
/// on ADAPTER state - see `prepare_unmask_backend`.
pub async fn authorize_query_hint(
    backend: &BackendHandle,
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
    // SCHEMA: the audit table is reached through it on PostgreSQL. `app_id`
    // above stays the TENANT - metering subject, SQLite ATTACH alias, key salt.
    let db_schema = binding.schema();

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

    prepare_unmask_backend(backend, app_id).await?;
    let mut unauthorized: Vec<String> = Vec::new();
    for (column, classification) in unmask_columns.iter().zip(&classifications) {
        if !check_unmask_authorization(binding, actor, classification)? {
            unauthorized.push(column.clone());
        }
    }

    if !unauthorized.is_empty() {
        write_audit_query_hint_row(
            backend,
            db_schema,
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
                "Drop the unauthorized columns from `opts.unmask` or edit defineMaskPolicy() and redeploy."
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
///
/// Takes the same `backend` its `authorize_query_hint` half took, for the same
/// reason: the granted row must land on the connection the find ran on, and
/// `run_find` is the frame that holds it.
pub async fn audit_query_hint_granted(
    backend: &BackendHandle,
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
    // SCHEMA: the audit table is reached through it on PostgreSQL. `app_id`
    // above stays the TENANT - metering subject, SQLite ATTACH alias, key salt.
    let db_schema = binding.schema();
    // Re-resolve classifications for the audit row. Cheap — the descriptor
    // lookup is a HashMap read.
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    prepare_unmask_backend(backend, app_id).await?;
    let mut classifications: Vec<String> = Vec::with_capacity(unmask_columns.len());
    for col in unmask_columns {
        let cls = lookup_mask_meta(&schema, col)
            .map(|m| m.classification)
            .unwrap_or_else(|| "pii".to_string());
        classifications.push(cls);
    }
    write_audit_query_hint_row(
        backend,
        db_schema,
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
///
/// `route` arrives from `crate::crud::read_pipeline::apply`, which is the stage
/// this promotion belongs to and which takes it off the read's own dispatch.
/// The stage resolved a backend through the engine funnel until 2026-09-03; the
/// handle it found was the route's anyway, so the parameter costs nothing and
/// stops an ENGINE file reading ADAPTER state. See `prepare_unmask_backend`.
///
/// **The whole route, not the handle it carries.** These SELECTs unmask the
/// rows `exec_query` just returned, and `exec_query` honours `route.in_tx()`.
/// Reading only the handle sent them to the autocommit lane, so a
/// `find({ unmask })` inside `db.transaction(fn)` failed `unmask_not_found`
/// over a row the same transaction had inserted - the rows were there, the
/// connection sent to fetch their plaintext was not the one that could see
/// them. Bound by `plugin-db/tests/unmask_tx_lane.rs`.
pub async fn dispatch_unmask_for_query(
    route: &crate::tx_route::TxRoute,
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
    prepare_unmask_backend(route.backend(), app_id).await?;
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
            //
            // The `None` arm no longer falls back to the caller's spelling.
            // That fallback fed an unmasked column's own name to the raw-column
            // read, and the read is not merely wrong there - on SQLite it is
            // SILENT, because a double-quoted identifier matching no column is
            // taken as a string literal, so the "plaintext" that came back was
            // the column NAME. `authorize_query_hint` already refuses an
            // unmasked column with `unmask_column_not_masked` before this
            // function runs (`crud/mod.rs:828`), so this arm is unreachable and
            // says so rather than reading something.
            let canonical = lookup_mask_meta(&schema, col)
                .map(|meta| meta.canonical_column)
                .ok_or_else(|| {
                    DbError::internal(format!(
                        "unmask hint reached the fetch for unmasked column '{col}' on \
                         '{collection}'; authorize_query_hint must refuse it first"
                    ))
                })?;
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
            let raw_column = resolve_raw_column(&schema, &canonical)?;
            let plaintext = match lookup_encryption_meta(&schema, &canonical)? {
                Some(enc_meta) => {
                    fetch_and_decrypt(&raw_column, route, &single_args, &enc_meta).await?
                }
                None => fetch_plaintext_raw(&raw_column, route, &single_args).await?,
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
                obj.insert(canonical.clone(), plaintext);
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
    backend: &BackendHandle,
    db_schema: &zeroship_data_sql::SchemaName,
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
    write_audit_unmask_row(
        backend,
        db_schema,
        app_id,
        &synthetic,
        &class_joined,
        outcome,
    )
    .await
}

// ---------------------------------------------------------------------------
// The DB-3 boundary
// ---------------------------------------------------------------------------
//
// `parse_args` and `parse_bulk_args` are the two places where an `actor`
// supplied by APP JS meets `sanitize_app_actor`. That call IS the DB-3 fix, and
// while both parsers were private `fn` no integration target could drive them:
// every live-PG test in `crates/zeroship-data-v8/tests/mask_flip.rs` built
// `UnmaskFieldArgs` / `BulkUnmaskArgs` in Rust and so entered BELOW the fence,
// leaving the fence itself covered only by in-module units that call
// `sanitize_app_actor` directly with no database behind it. A fence no
// integration test can reach is a fence nobody re-checks.
//
// Both are plainly `pub`: `zeroship_data_v8::v8_classes::dispatch` calls them
// on the production path, so this is the shipped surface, not a test affordance.

/// Decode the JSON args coming from V8 into [`UnmaskFieldArgs`]. The
/// V8 boundary already converted the JS object to `zeroship_data_sql::value::Value`
/// via `read_json_arg`; we just pluck the typed fields.
pub fn parse_args(v: &Value) -> Result<UnmaskFieldArgs, DbError> {
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

fn require_string(
    obj: &zeroship_data_sql::value::Map<String, Value>,
    key: &str,
) -> Result<String, DbError> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| DbError::ValidationFailed {
            code: "invalid_unmask_args",
            message: format!("unmaskField: '{key}' must be a string"),
            hint: None,
        })
        .map(str::to_string)
}

// The bulk half of the DB-3 boundary. Same reasoning as `parse_args` above.

/// Parse the JS-side `{ collection, items: [{ rowPk, columns }], actor?, reason? }`
/// shape into [`BulkUnmaskArgs`]. Refuses non-object args, missing
/// fields, items that aren't an array, items missing `rowPk` /
/// `columns`, and columns arrays containing non-strings — every error
/// surfaces a typed `ValidationFailed { code: "invalid_bulk_unmask_args" }`
/// so the SDK can branch on `.code` deterministically.
pub fn parse_bulk_args(v: &Value) -> Result<BulkUnmaskArgs, DbError> {
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
    obj: &zeroship_data_sql::value::Map<String, Value>,
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
    use zeroship_data_sql::value;

    // Every dispatch unit below refuses in the descriptor / validation
    // prologue, or returns on an empty input, BEFORE `prepare_unmask_backend`
    // and before any SQL - but the entry points take the backend as a
    // parameter now, so each still has to hand one over. See the helper's own
    // doc for why that is the right trade.
    use crate::test_support::{unit_backend, unit_route};

    #[test]
    fn sanitize_app_actor_strips_reserved_auto_db3() {
        // App JS claiming the privileged system actor is stripped to None, so
        // check_unmask_authorization's "unauthenticated → denied" arm applies —
        // an app handler can no longer unmask its PII via {actor:{kind:"auto"}}.
        let forged = sanitize_app_actor(Some(value!({ "kind": "auto" })));
        assert_eq!(forged.actor, None);
        // ...and the refused claim is RETAINED, so the audit row can record
        // that someone tried. Dropping it made a forged claim indistinguishable
        // from an absent actor in `__zeroship_audit_unmask`.
        assert_eq!(forged.rejected_claim, Some(value!({ "kind": "auto" })));

        // A non-reserved, app-declared actor kind passes through unchanged, and
        // produces no rejected claim.
        let ordinary = sanitize_app_actor(Some(value!({ "kind": "support_agent" })));
        assert_eq!(ordinary.actor, Some(value!({ "kind": "support_agent" })));
        assert_eq!(ordinary.rejected_claim, None);

        // No actor / missing kind stay as-is (denied downstream regardless),
        // and neither is a refused CLAIM - nothing was claimed.
        assert_eq!(sanitize_app_actor(None).actor, None);
        assert_eq!(sanitize_app_actor(None).rejected_claim, None);
        assert_eq!(sanitize_app_actor(Some(value!({}))).actor, Some(value!({})));
        assert_eq!(sanitize_app_actor(Some(value!({}))).rejected_claim, None);
    }

    #[test]
    fn stripped_auto_actor_is_denied_by_authorization_db3() {
        // The fix's effect: a sanitized app actor (the `auto` claim stripped to
        // None) hits check_unmask_authorization's "unauthenticated → denied"
        // arm — which returns before consulting any policy. Pre-fix the raw
        // {kind:"auto"} reached the no-policy fallback and was GRANTED.
        let sanitized = sanitize_app_actor(Some(value!({ "kind": "auto" })));
        assert_eq!(sanitized.actor, None);
        assert!(
            !check_unmask_authorization(&DbBinding::cold_start("app_x"), &sanitized.actor, "pii")
                .unwrap(),
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
        let actor = Some(value!({ "kind": "auto", "id": null }));
        assert!(
            check_unmask_authorization(
                &DbBinding::cold_start("authz_stub_grants_auto_1"),
                &actor,
                "spi"
            )
            .unwrap()
        );
        let actor = Some(value!({ "kind": "auto", "id": "system" }));
        assert!(
            check_unmask_authorization(
                &DbBinding::cold_start("authz_stub_grants_auto_2"),
                &actor,
                "pii"
            )
            .unwrap()
        );
    }

    #[test]
    fn authz_stub_denies_user_actor() {
        let actor = Some(value!({ "kind": "user", "id": "usr_xyz" }));
        assert!(
            !check_unmask_authorization(
                &DbBinding::cold_start("authz_stub_denies_user_1"),
                &actor,
                "spi"
            )
            .unwrap()
        );
        let actor = Some(value!({ "kind": "user", "id": "usr_xyz" }));
        assert!(
            !check_unmask_authorization(
                &DbBinding::cold_start("authz_stub_denies_user_2"),
                &actor,
                "pii"
            )
            .unwrap()
        );
    }

    #[test]
    fn authz_stub_denies_other_kinds() {
        for kind in ["operator", "ai-builder", "anonymous", "service", ""] {
            let actor = Some(value!({ "kind": kind }));
            let app_id = format!("authz_stub_denies_other_{kind}");
            assert!(
                !check_unmask_authorization(&DbBinding::cold_start(&app_id), &actor, "pii")
                    .unwrap(),
                "kind={kind} must be denied by the PR 4 stub"
            );
        }
    }

    #[test]
    fn authz_stub_denies_unauthenticated() {
        assert!(
            !check_unmask_authorization(
                &DbBinding::cold_start("authz_stub_unauth_1"),
                &None,
                "pii"
            )
            .unwrap()
        );
        // Empty object — no `kind` field — also denied.
        let actor = Some(value!({}));
        assert!(
            !check_unmask_authorization(
                &DbBinding::cold_start("authz_stub_unauth_2"),
                &actor,
                "pii"
            )
            .unwrap()
        );
        // Actor that isn't an object (e.g. JS passed a string) — denied.
        let actor = Some(value!("auto"));
        assert!(
            !check_unmask_authorization(
                &DbBinding::cold_start("authz_stub_unauth_3"),
                &actor,
                "pii"
            )
            .unwrap()
        );
    }

    // ---------------------------------------------------------------
    // Per-app policy lookup
    // ---------------------------------------------------------------

    /// Helper: install a [`crate::protection::mask_policy::MaskPolicy`] for `app_id` on the current
    /// isolate's context cache, then immediately remove it on drop.
    /// Keeps the thread-local cache hygiene clean across tests.
    struct PolicyGuard(String);
    impl PolicyGuard {
        fn install(app_id: &str, policy: crate::protection::mask_policy::MaskPolicy) -> Self {
            crate::protection::mask_policy::cache_put(&DbBinding::cold_start(app_id), Some(policy));
            Self(app_id.to_string())
        }
    }
    impl Drop for PolicyGuard {
        fn drop(&mut self) {
            crate::protection::mask_policy::cache_put(&DbBinding::cold_start(&self.0), None);
        }
    }

    #[test]
    fn pr5_policy_grants_role_with_classification() {
        use crate::protection::mask_policy::MaskPolicy;
        let app_id = "pr5_grants_role_classification";
        let policy = MaskPolicy::from_json(&value!({
            "user": ["public", "pii"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(value!({ "kind": "user", "id": "usr_x" }));
        assert!(check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "pii").unwrap());
        assert!(
            check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "public").unwrap()
        );
        assert!(
            !check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "spi").unwrap()
        );
    }

    #[test]
    fn pr5_policy_unknown_role_denied() {
        use crate::protection::mask_policy::MaskPolicy;
        let app_id = "pr5_unknown_role_denied";
        let policy = MaskPolicy::from_json(&value!({
            "user": ["public"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(value!({ "kind": "operator", "id": "op_1" }));
        assert!(
            !check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "public").unwrap()
        );
    }

    #[test]
    fn pr5_auto_fallback_when_not_in_policy() {
        use crate::protection::mask_policy::MaskPolicy;
        let app_id = "pr5_auto_fallback";
        // Policy DOES list `user`, but NOT `auto` — the system actor
        // retains its uniform access via the fallback rule.
        let policy = MaskPolicy::from_json(&value!({
            "user": ["public"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(value!({ "kind": "auto" }));
        assert!(check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "pii").unwrap());
        assert!(check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "spi").unwrap());
        assert!(
            check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "internal").unwrap()
        );
    }

    #[test]
    fn pr5_auto_explicit_restriction_honoured() {
        use crate::protection::mask_policy::MaskPolicy;
        let app_id = "pr5_auto_explicit_restriction";
        let policy = MaskPolicy::from_json(&value!({
            "auto": ["public"],
        }))
        .unwrap();
        let _g = PolicyGuard::install(app_id, policy);

        let actor = Some(value!({ "kind": "auto" }));
        assert!(
            check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "public").unwrap()
        );
        assert!(
            !check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "pii").unwrap()
        );
        assert!(
            !check_unmask_authorization(&DbBinding::cold_start(app_id), &actor, "spi").unwrap()
        );
    }

    #[test]
    fn lookup_mask_meta_accepts_field_name_alias_for_snake_case_schema() {
        let schema = value!({
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
        let v = value!({
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
        let v = value!({ "collection": "users", "row_pk": "x" });
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
        let v = value!({
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
        let v = value!({
            "collection": "users",
            "row_pk": "u1",
            "column": "ssn",
            "actor": null,
        });
        let args = parse_args(&v).unwrap();
        assert!(args.actor.is_none(), "JSON null actor must become None");
    }

    // ---------------------------------------------------------------
    // parse_bulk_args validation
    // ---------------------------------------------------------------

    #[test]
    fn parse_bulk_args_round_trip_well_formed() {
        let v = value!({
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
        let v = value!({
            "collection": "users",
            "items": [{ "row_pk": "u1", "columns": ["ssn"] }],
        });
        let args = parse_bulk_args(&v).unwrap();
        assert_eq!(args.items[0].row_pk, "u1");
    }

    #[test]
    fn parse_bulk_args_rejects_non_object() {
        let v = value!("nope");
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
        let v = value!({ "items": [{ "rowPk": "u1", "columns": ["ssn"] }] });
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
        let v = value!({ "collection": "users", "items": "not-an-array" });
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
        let v = value!({
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
        let v = value!({
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
        let v = value!({
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
            actor: Some(value!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let result = runtime
            .block_on(async {
                let (route, _dir) = unit_route(binding.app_id());
                dispatch_bulk_unmask(&route, &binding, args).await
            })
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
            value!({ "ssn": { "type": "string", "mask": { "kind": "last4" } } }),
        );
        let binding = DbBinding::cold_start(app_id);
        let runtime = compio::runtime::Runtime::new().unwrap();
        let args = BulkUnmaskArgs {
            collection: "users".into(),
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["mystery".into()],
            }],
            actor: Some(value!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let err = runtime
            .block_on(async {
                let (route, _dir) = unit_route(binding.app_id());
                dispatch_bulk_unmask(&route, &binding, args).await
            })
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
        crate::reset_engine_for_tests();
        let binding = DbBinding::cold_start("bulk_unit_undeclared_app");
        let runtime = compio::runtime::Runtime::new().unwrap();
        let args = BulkUnmaskArgs {
            collection: "users".into(),
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["ssn".into()],
            }],
            actor: Some(value!({ "kind": "auto" })),
            reason: None,
            rejected_claim: None,
        };
        let err = runtime
            .block_on(async {
                let (route, _dir) = unit_route(binding.app_id());
                dispatch_bulk_unmask(&route, &binding, args).await
            })
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
        let ok = runtime.block_on(async {
            let (backend, _dir) = unit_backend();
            authorize_query_hint(
                &backend,
                &binding,
                "users",
                &[],
                &Some(value!({ "kind": "user" })),
                None,
                &None,
            )
            .await
        });
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
            value!({ "ssn": { "type": "string", "mask": { "kind": "last4" } } }),
        );
        let binding = DbBinding::cold_start(app_id);
        let runtime = compio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(async {
                let (backend, _dir) = unit_backend();
                authorize_query_hint(
                    &backend,
                    &binding,
                    "users",
                    &["nonexistent".to_string()],
                    &Some(value!({ "kind": "auto" })),
                    None,
                    &None,
                )
                .await
            })
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
        let mut rows = vec![value!({ "id": "u1", "name": "alice" })];
        let original = rows.clone();
        runtime
            .block_on(async {
                let (route, _dir) = unit_route(binding.app_id());
                dispatch_unmask_for_query(&route, &binding, "users", &[], &mut rows).await
            })
            .unwrap();
        assert_eq!(rows, original, "empty unmask columns must be a no-op");
    }
}
