//! Explicit transaction lifecycle — `db.beginTransaction(opts?)`.
//!
//! V8 is single-threaded per isolate, so only one transaction can be
//! active at a time. The transaction connection lives in the per-isolate
//! context (`IsolateDbContext::tx_conn`). All CRUD callbacks
//! (`crate::exec::run_sql`) use that slot when it's set, falling
//! through to the pool otherwise.
//!
//! The dedicated `Transaction` v8_class
//! ([`crate::v8_classes::transaction`]) is minted *synchronously* by
//! [`begin_transaction_dispatch`] (we need a V8 scope for the
//! allocation). The wrapper carries a token minted by
//! `IsolateDbContext::next_tx_token`; commit/rollback/Drop all fence on
//! the token so no two paths settle the same transaction.

use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::backend::SqlExecutor;
use crate::error::DbError;
use crate::exec::clear_pending_emits;
use crate::v8_bridge::runtime_state;

/// `zeroship.db.beginTransaction(isolationLevel?)` → `Promise<Transaction>`
///
/// Opens a dedicated connection, runs BEGIN, stores it in the
/// per-isolate context's `tx_conn` slot, and resolves with a fresh
/// [`crate::v8_classes::transaction::Transaction`] v8_class instance.
/// The wrapper's Weak finalizer auto-rollbacks if the handle is dropped
/// without `.commit()` / `.rollback()` — closes the connection-leak
/// footgun the pre-wrapper API had.
///
/// All subsequent CRUD ops use the transaction connection until
/// the wrapper's `.commit()` or `.rollback()` runs (or the wrapper's
/// `Drop` finalizer auto-rollbacks on GC).
///
/// The Transaction wrapper is minted *synchronously* before the BEGIN
/// future runs (we need a V8 scope to allocate it). On BEGIN success
/// the future stamps the wrapper's pre-allocated `token` onto the
/// per-isolate context's `tx_token` slot and resolves the promise with
/// the wrapper; on failure the wrapper is left with a token that never
/// matches the live slot, so its `Drop` is a no-op when V8 eventually
/// collects it.
pub fn begin_transaction_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    isolation_level: Option<String>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // Allocate the ownership token + mint the wrapper synchronously.
    // We need a scope to allocate the V8 object; the spawned future
    // doesn't have one. On BEGIN success the future stamps
    // `IsolateDbContext::tx_token` with this same token; on failure
    // `tx_token` stays 0 so the wrapper's Drop sees
    // `current(0) != token` and no-ops.
    let token = crate::next_tx_token();
    // Clone before the move into `mint_transaction` so the spawned BEGIN
    // future can apply `SET LOCAL ROLE "<per-app role>"` (§17.5).
    let app_id_for_begin = app_id.clone();
    let tx_obj = match crate::v8_classes::transaction::mint_transaction(scope, token, app_id) {
        Ok(obj) => obj,
        Err(e) => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let msg = v8::String::new(scope, &e.message).unwrap();
            let exc = v8::Exception::error(scope, msg);
            resolver.reject(scope, exc);
            return promise;
        }
    };

    let tx_obj_as_value: v8::Local<v8::Value> = tx_obj.into();
    let tx_global: v8::Global<v8::Value> = v8::Global::new(scope, tx_obj_as_value);

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_begin(isolation_level.as_deref(), &app_id_for_begin).await {
            Ok(()) => {
                // Stamp ownership now that the transaction-conn slot
                // holds the client — the wrapper's commit / rollback /
                // Drop all gate on this matching the wrapper's `token`.
                crate::context::with_mut(|c| c.set_tx_token(token));
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::JsGlobal(tx_global),
                    request_id,
                }
            }
            Err(e) => {
                // Wrapper's `token` never matches `tx_token`(=0), so when
                // V8 collects the wrapper (no JS reference survives a
                // rejected await) the Drop is a no-op.
                drop(tx_global);
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                }
            }
        }
    }));

    promise
}

/// Allowed isolation levels (uppercased for validation).
const VALID_ISOLATION_LEVELS: &[&str] = &[
    "READ UNCOMMITTED",
    "READ COMMITTED",
    "REPEATABLE READ",
    "SERIALIZABLE",
];

async fn exec_begin(isolation_level: Option<&str>, app_id: &str) -> Result<(), DbError> {
    // Check: no nested transactions
    let has_tx = crate::context::with(|c| c.has_tx());
    if has_tx {
        return Err(DbError::validation(
            "tx_already_active",
            "db: transaction already active (nested transactions not supported)",
        ));
    }

    // Build BEGIN statement with optional isolation level
    let begin_sql = match isolation_level {
        Some(level) => {
            let upper = level.to_uppercase();
            if !VALID_ISOLATION_LEVELS.contains(&upper.as_str()) {
                return Err(DbError::validation(
                    "invalid_isolation_level",
                    format!(
                        "db: invalid isolation level: {level}. Must be one of: read uncommitted, read committed, repeatable read, serializable"
                    ),
                ));
            }
            format!("BEGIN ISOLATION LEVEL {upper}")
        }
        None => "BEGIN".to_string(),
    };

    // Open a dedicated connection (not from pool — we need to hold it).
    //
    // **Post-P0 mop-up (I-R12-1)**: routed through
    // [`SqlExecutor::acquire_dedicated_client`] so the
    // `compio_postgres::connect` + `connection.run()` spawn lives in
    // exactly one place (the PG impl in `backend/postgres.rs`).
    // Previously this site open-coded the connect; now the inline
    // `connect(&url, NoTls)` is gone and the error rail is
    // single-prefixed at the backend (`"db: backend connect failed:
    // <e>"`) instead of double-prefixed through a `tx connect failed`
    // re-wrap (code-critique R15-3).
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: not configured"))?;
    let pg = backend
        .as_postgres()
        .ok_or_else(|| DbError::backend_unsupported("beginTransaction"))?;
    // **Post-P0 mop-up (code-critique R15-3)**: `acquire_dedicated_client`
    // already prefixes its `Transient` errors with `"db: backend connect
    // failed: <e>"` (see `backend/postgres.rs::acquire_dedicated_client`).
    // The previous shape here added a second `"db: tx connect failed: "`
    // prefix on top, producing the double-prefixed body
    // `"db: tx connect failed: db: backend connect failed: <e>"` the SDK
    // saw. We now `?`-propagate the inner error verbatim — single prefix,
    // same `Transient` variant, same wire `.code = "transient"`.
    let client = pg.acquire_dedicated_client().await?;

    client
        .execute(&begin_sql, &[])
        .await
        .map_err(|e| DbError::from_pg(&e))?;

    // §17.5 — constrain client SQL to the per-app role for the lifetime
    // of this transaction. `SET LOCAL ROLE` auto-reverts at COMMIT /
    // ROLLBACK, so the dedicated tx connection never leaks the role.
    // Production-only (`hardening`): the per-app role is provisioned at
    // `register_model` time on the same gate. Without the feature, client
    // SQL runs under the platform login role exactly as before.
    super::apply_per_app_role(&client, app_id).await?;

    crate::context::with_mut(|c| {
        let _previous = c.install_tx_client(client);
        debug_assert!(_previous.is_none(), "exec_begin: tx_conn slot already occupied");
    });
    // Defensive: any residue from a prior tx that didn't drain cleanly
    // (shouldn't happen — every settle path clears) must NOT leak into
    // the new tx's drain. Drop without firing.
    clear_pending_emits();
    Ok(())
}
