//! Auto-tx wrappers — defense-in-depth around `query()` / `mutation()`
//! handlers.
//!
//! `__zsBeginAutoTx(kindStr)` resolves with a numeric token the JS shim
//! hands back to `__zsEndAutoTx(token, success)`.
//!
//! Token semantics: `0` means no auto-tx opened (kind is not
//! `"query"`/`"mutation"`, or a user-driven `db.transaction(...)` is
//! already active, or the DB plugin is configured with a never-dialed
//! dummy URL — capability gate fired before we got here). `1` means
//! auto-tx opened successfully; commit/rollback owed.
//!
//! `__zsEndAutoTx(token, success)`: token `0` → resolved promise, no-op.
//! Token `1` → COMMIT on success, ROLLBACK on failure. Errors during
//! commit/rollback are surfaced verbatim to JS; the SSR shim still
//! re-throws the underlying handler error so callers don't see commit
//! failures mask handler errors.
//!
//! Isolation level by kind:
//! - `query`    → `BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY`
//! - `mutation` → `BEGIN ISOLATION LEVEL {override or READ COMMITTED} READ WRITE`
//!
//! The mutation default is READ COMMITTED — same as Postgres's default
//! for explicit BEGIN. Apps that need write-skew protection bump to
//! `serializable` per-mutation via the wrapper config; apps that need
//! consistent re-reads inside the handler bump to `repeatable read`.
//! Stronger isolation costs throughput (SSI bookkeeping, more 40001
//! retries) and is opt-in by design.
//!
//! `action`, `stream`, `subscription` and unknown kinds are not wrapped:
//! actions can hold open external IO, streams/subscriptions are
//! long-lived; both would starve the connection pool. The capability
//! gate (B3 runtime layer) is the primary enforcement; auto-tx is a
//! second line of defense at the Postgres level.

use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

use crate::backend::SqlExecutor;
use crate::error::DbError;
use crate::exec::{clear_pending_emits, drain_pending_emits_on_commit};
use crate::v8_bridge::{get_i64_arg, get_string_arg, setup_js_promise};

/// `globalThis.__zsBeginAutoTx(kindStr)` — see module comment above.
pub fn auto_begin_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let kind = get_string_arg(scope, &args, 0);
    // Optional second arg: per-mutation isolation override
    // ("read committed" | "repeatable read" | "serializable"). Empty /
    // missing → use the per-kind default in `auto_tx_begin_sql`.
    let isolation = get_string_arg(scope, &args, 1);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Route via the typed `OpResult::JsValue` channel so the COMMIT-
        // time error path preserves `DbError.code` / `.hint` (mirrors
        // `orchestrator::transaction::begin_transaction_dispatch`). The
        // legacy `OpResult::Failed { error: String }` rail flattened the
        // typed `DbError` and stripped the SDK's retry-by-code signal —
        // the exact site retry-by-code matters most.
        let value = begin_to_resolve_value(exec_auto_begin(kind.as_deref(), isolation.as_deref()).await);
        OpResult::JsValue {
            resolver,
            value,
            request_id,
        }
    }));

    rv.set(promise.into());
}

/// `globalThis.__zsEndAutoTx(token, success)` — see module comment above.
pub fn auto_end_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let token = get_i64_arg(scope, &args, 0).unwrap_or(0);
    // Second arg is a boolean. `is_true()` covers the literal `true`;
    // we deliberately do NOT treat truthy non-booleans as success — the
    // SSR shim always passes a real boolean and any other shape is a
    // bug we want surfaced as a rollback (defense in depth).
    let success = args.length() >= 2 && args.get(1).is_true();
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // COMMIT-time `DbError` (e.g. 40001 serialization, 55P03 lock
        // contention, class 08 transient) must reach JS with `.code` /
        // `.hint` intact so the SDK can branch on `err.code` for
        // retry-vs-bail decisions. See module note in `error.rs`.
        let value = end_to_resolve_value(exec_auto_end(token, success).await);
        OpResult::JsValue {
            resolver,
            value,
            request_id,
        }
    }));

    rv.set(promise.into());
}

/// Convert an `exec_auto_begin` result into the `ResolveValue` that
/// settles the promise handed to JS. Mirrors the
/// `orchestrator::transaction::begin_transaction_dispatch` pattern —
/// success carries the u32 token, failure carries the *typed* `OpError`
/// (code + hint preserved) instead of a flat string. Extracted as a
/// standalone fn so tests can exercise the conversion without a V8
/// scope.
fn begin_to_resolve_value(result: Result<u32, DbError>) -> ResolveValue {
    match result {
        Ok(token) => ResolveValue::U32(token),
        Err(e) => ResolveValue::RejectError(e.to_op_error()),
    }
}

/// Convert an `exec_auto_end` result into the `ResolveValue` that
/// settles the promise handed to JS. The success path resolves with
/// `undefined` (the JS dispatcher ignores the value — it only cares
/// whether the promise rejected); the failure path carries the *typed*
/// `OpError` so the SDK can branch on `err.code` at the
/// COMMIT/ROLLBACK boundary. See module note + `error.rs` for the
/// retry-by-code rationale.
fn end_to_resolve_value(result: Result<(), DbError>) -> ResolveValue {
    match result {
        Ok(()) => ResolveValue::Undefined,
        Err(e) => ResolveValue::RejectError(e.to_op_error()),
    }
}

/// Per-kind BEGIN SQL. Returns `None` for kinds we don't wrap. For
/// mutations, `isolation` overrides the default READ COMMITTED.
fn auto_tx_begin_sql(kind: Option<&str>, isolation: Option<&str>) -> Option<String> {
    match kind {
        Some("query") => Some("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY".to_string()),
        Some("mutation") => {
            let level = normalize_isolation(isolation).unwrap_or("READ COMMITTED");
            Some(format!("BEGIN ISOLATION LEVEL {level} READ WRITE"))
        }
        _ => None,
    }
}

/// Case-fold + whitespace-collapse a user-supplied isolation string to
/// one of Postgres' four accepted values. Unknown / empty → `None`
/// (caller falls back to the default).
fn normalize_isolation(s: Option<&str>) -> Option<&'static str> {
    let raw = s?.trim();
    if raw.is_empty() {
        return None;
    }
    let upper: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    match upper.as_str() {
        "READ COMMITTED" => Some("READ COMMITTED"),
        "REPEATABLE READ" => Some("REPEATABLE READ"),
        "SERIALIZABLE" => Some("SERIALIZABLE"),
        // READ UNCOMMITTED is accepted by Postgres but silently upgrades
        // to READ COMMITTED — treat as alias.
        "READ UNCOMMITTED" => Some("READ COMMITTED"),
        _ => None,
    }
}

async fn exec_auto_begin(kind: Option<&str>, isolation: Option<&str>) -> Result<u32, DbError> {
    // Skip if this kind isn't wrapped (action / stream / subscription /
    // unknown). Token 0 → end is a no-op.
    let Some(sql) = auto_tx_begin_sql(kind, isolation) else {
        return Ok(0);
    };

    // Don't wrap when a user-driven `db.transaction(...)` already
    // holds the tx-conn slot — that would be a nested-tx attempt the
    // user already opted out of. Returning 0 keeps the auto-end
    // callback off the user's tx entirely.
    let has_tx = crate::context::with(|c| c.has_tx());
    if has_tx {
        return Ok(0);
    }

    // Open a dedicated connection (same pattern as user-driven
    // `begin_transaction`).
    //
    // **Post-P0 mop-up (I-R12-1)**: routed through
    // [`SqlExecutor::acquire_dedicated_client`] so the
    // `compio_postgres::connect` + `connection.run()` spawn lives in
    // exactly one place (the PG impl in `backend/postgres.rs`).
    // Operator-facing prefix ("auto-tx connect failed") preserved.
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: not configured"))?;
    let pg = backend.as_postgres().ok_or_else(|| DbError::Configuration {
        code: "backend_unsupported",
        message: "db: auto-tx requires the Postgres backend".to_string(),
        hint: None,
    })?;
    let client = pg.acquire_dedicated_client().await.map_err(|e| match e {
        DbError::Transient { message } => DbError::Transient {
            message: format!("db: auto-tx connect failed: {message}"),
        },
        other => other,
    })?;

    client
        .execute(&sql, &[])
        .await
        .map_err(|e| DbError::from_pg(&e))?;

    crate::context::with_mut(|c| {
        let _previous = c.install_tx_client(client);
        debug_assert!(
            _previous.is_none(),
            "exec_auto_begin: tx_conn slot already occupied"
        );
        c.set_auto_tx_owned(true);
    });
    clear_pending_emits();
    Ok(1)
}

async fn exec_auto_end(token: i64, success: bool) -> Result<(), DbError> {
    // No-op tokens (unwrapped kinds) — nothing to commit.
    if token == 0 {
        return Ok(());
    }

    // Defensive ownership check: if `auto_tx_owned` is false the tx
    // is either gone or owned by user code. Either way leave it alone.
    let owned = crate::context::with(|c| c.auto_tx_owned());
    if !owned {
        return Ok(());
    }

    // Take the client. We MUST clear `auto_tx_owned` before any await
    // so a re-entrant call (shouldn't happen — V8 is single-threaded
    // — but cheap insurance) doesn't see stale ownership.
    let client = crate::context::with_mut(|c| {
        let client = c.take_tx_client();
        c.set_auto_tx_owned(false);
        client
    });

    let Some(client) = client else {
        // Ownership flag said yes, conn slot is empty: state is
        // inconsistent. Treat as already-ended.
        return Ok(());
    };

    let cmd = if success { "COMMIT" } else { "ROLLBACK" };
    let result = client.execute(cmd, &[]).await;
    drop(client); // explicit — terminates the spawned connection task.

    // Settle the deferred broker queue. On a successful COMMIT, fire
    // every event we'd have published mid-tx; on ROLLBACK (or COMMIT
    // failure) drop them silently so subscribers never see writes
    // Postgres just undid.
    if success && result.is_ok() {
        drain_pending_emits_on_commit();
    } else {
        clear_pending_emits();
    }

    // `from_pg` walks the source chain so deferred-FK / unique
    // violations surface with their SQLSTATE-classified code instead
    // of a bare "db error".
    result.map(|_| ()).map_err(|e| DbError::from_pg(&e))
}

/// Install `__zsBeginAutoTx` / `__zsEndAutoTx` on `globalThis`. Called
/// once during plugin `register()` via `NativeRegistrar::add_setup`.
pub fn install_auto_tx_globals(scope: &mut v8::PinScope<'_, '_>) {
    let global = scope.get_current_context().global(scope);
    {
        let f = v8::Function::new(scope, auto_begin_transaction).unwrap();
        let key = v8::String::new(scope, "__zsBeginAutoTx").unwrap();
        global.set(scope, key.into(), f.into());
    }
    {
        let f = v8::Function::new(scope, auto_end_transaction).unwrap();
        let key = v8::String::new(scope, "__zsEndAutoTx").unwrap();
        global.set(scope, key.into(), f.into());
    }
}

#[cfg(test)]
mod tests {
    //! Code-preservation tests for the auto-tx COMMIT-time error path.
    //!
    //! Pre-fix: both sites flattened typed `DbError` into
    //! `OpResult::Failed { error: e.into_string() }`, stripping `.code` /
    //! `.hint`. The SDK can no longer branch on `err.code === "transient"`
    //! / `"lock_not_available"` to decide whether to retry a serialized
    //! mutation, so application-level retry loops degenerate to
    //! `if (e.message.includes(...))` substring sniffing.
    //!
    //! These tests pin the canonical `to_op_error()` pathway for the two
    //! variants the SDK most needs to branch on at the auto-tx boundary
    //! (`Transient` — connection drops, class 08; `LockContention` —
    //! 55P03 from `SELECT … FOR UPDATE NOWAIT`).
    use super::*;
    use zeroship_runtime::state::OpErrorKind;

    /// Extract `(code, hint)` from a `ResolveValue::RejectError` produced
    /// by the conversion helpers. Panics on any other shape — the
    /// auto-tx error path must always materialise a `CodedError` (every
    /// `DbError` variant maps to one via `to_op_error()`, including the
    /// catch-all `Internal` which stamps `"internal"`).
    fn reject_code_hint(rv: ResolveValue) -> (String, Option<String>) {
        match rv {
            ResolveValue::RejectError(op_err) => match op_err.kind {
                OpErrorKind::CodedError { code, hint } => (code, hint),
                other => panic!("expected CodedError, got {other:?}"),
            },
            _ => panic!("expected ResolveValue::RejectError"),
        }
    }

    #[test]
    fn auto_begin_transient_error_preserves_code() {
        // `Transient` is the SQL class 08 / connection-drop bucket — the
        // SDK retries by `err.code === "transient"`. Pre-fix the wire
        // payload carried only `err.message` (the connect/socket text).
        let rv = begin_to_resolve_value(Err(DbError::Transient {
            message: "db: auto-tx connect failed: connection refused".into(),
        }));
        let (code, hint) = reject_code_hint(rv);
        assert_eq!(code, "transient", "wire code must equal 'transient'");
        // Transient variants must carry the human-facing retry hint so
        // the SDK can surface it verbatim — verified centrally in
        // `error::tests::retryable_variants_carry_hint`; re-asserted
        // here so a hint regression at the auto-tx boundary doesn't
        // sneak past the conversion path.
        assert!(
            hint.is_some(),
            "transient errors must carry a retry hint"
        );
    }

    #[test]
    fn auto_end_lock_contention_preserves_code() {
        // `LockContention` is Postgres 55P03 — emitted by COMMIT-time
        // deferred-constraint paths or SELECT…FOR UPDATE NOWAIT.
        // SDK branches on `err.code === "lock_not_available"` to back
        // off vs. abort.
        let rv = end_to_resolve_value(Err(DbError::LockContention {
            message: "db: could not obtain lock on row in relation users".into(),
        }));
        let (code, _hint) = reject_code_hint(rv);
        assert_eq!(
            code, "lock_not_available",
            "wire code must equal 'lock_not_available'"
        );
    }

    /// Sanity guard: the success paths must NOT route through the
    /// `RejectError` arm. A regression that swapped Ok/Err arms would
    /// silently turn every successful COMMIT into a rejection — the
    /// SDK would observe phantom failures.
    #[test]
    fn auto_begin_ok_resolves_with_token() {
        let rv = begin_to_resolve_value(Ok(1));
        match rv {
            ResolveValue::U32(t) => assert_eq!(t, 1),
            _ => panic!("expected ResolveValue::U32 for Ok(1)"),
        }
    }

    #[test]
    fn auto_end_ok_resolves_with_undefined() {
        let rv = end_to_resolve_value(Ok(()));
        match rv {
            ResolveValue::Undefined => {}
            _ => panic!("expected ResolveValue::Undefined for Ok(())"),
        }
    }
}
