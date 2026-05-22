//! Auto-tx wrappers — defense-in-depth around `query()` / `mutation()`
//! handlers.
//!
//! `__zsBeginAutoTx(kindStr): Promise<number>`
//!   Resolves with a numeric token the JS shim hands back to
//!   `__zsEndAutoTx(token, success)`.
//!
//!     token = 0  → no auto-tx opened (kind is not "query"/"mutation",
//!                  or a user-driven `db.transaction(...)` is already
//!                  active, or the DB plugin is configured with a
//!                  never-dialed dummy URL — capability gate fired
//!                  before we got here).
//!     token = 1  → auto-tx opened successfully; commit/rollback owed.
//!
//! `__zsEndAutoTx(token, success): Promise<void>`
//!   Token 0 → resolved promise, no-op. Token 1 → COMMIT on success,
//!   ROLLBACK on failure. Errors during commit/rollback are surfaced
//!   verbatim to JS; the SSR shim still re-throws the underlying
//!   handler error so callers don't see commit failures mask handler
//!   errors.
//!
//! Picks isolation level by kind:
//!   query    → BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY
//!   mutation → BEGIN ISOLATION LEVEL <override or READ COMMITTED>
//!              READ WRITE
//!
//! The mutation default is READ COMMITTED — same as Postgres's default
//! for explicit BEGIN. Apps that need write-skew protection bump to
//! `serializable` per-mutation via the wrapper config; apps that need
//! consistent re-reads inside the handler bump to `repeatable read`.
//! Stronger isolation costs throughput (SSI bookkeeping, more 40001
//! retries) and is opt-in by design.
//!
//! `action`, `stream`, `subscription` and unknown kinds are not wrapped:
//!   actions can hold open external IO, streams/subscriptions are
//!   long-lived; both would starve the connection pool. The capability
//!   gate (B3 runtime layer) is the primary enforcement; auto-tx is a
//!   second line of defense at the Postgres level.

use zeroship_runtime::state::{OpResult, SharedState};

use crate::error::DbError;
use crate::exec::{clear_pending_emits, drain_pending_emits_on_commit};
use crate::v8_bridge::{get_i64_arg, get_string_arg, setup_promise};

/// `globalThis.__zsBeginAutoTx(kindStr): Promise<number>` — see module
/// comment above.
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
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_auto_begin(kind.as_deref(), isolation.as_deref()).await {
            Ok(token) => OpResult::Completed {
                op_id,
                value: token.to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed {
                op_id,
                error: e.into_string(),
                request_id,
            },
        }
    }));

    rv.set(promise.into());
}

/// `globalThis.__zsEndAutoTx(token, success): Promise<void>` — see
/// module comment above.
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
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_auto_end(token, success).await {
            Ok(()) => OpResult::Completed {
                op_id,
                value: "null".to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed {
                op_id,
                error: e.into_string(),
                request_id,
            },
        }
    }));

    rv.set(promise.into());
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
    // holds TX_CONN — that would be a nested-tx attempt the user
    // already opted out of. Returning 0 keeps the auto-end callback
    // off the user's tx entirely.
    let has_tx = crate::TX_CONN.with(|tx| tx.borrow().is_some());
    if has_tx {
        return Ok(0);
    }

    // Open a dedicated connection (same pattern as user-driven
    // `begin_transaction`). compio-postgres splits the connection into
    // (Client, Connection); spawn the run loop on a detached task, hold
    // the Client in TX_CONN.
    let url = crate::context::with(|c| c.db_url())
        .ok_or_else(|| DbError::config("not_configured", "db: not configured"))?;
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .map_err(|e| DbError::Transient {
            message: format!("db: auto-tx connect failed: {e}"),
        })?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("db: auto-tx connection task error: {e}");
        }
    })
    .detach();

    client
        .execute(&sql, &[])
        .await
        .map_err(|e| DbError::from_pg(&e))?;

    crate::TX_CONN.with(|tx| {
        tx.borrow_mut().replace(client);
    });
    crate::AUTO_TX_OWNED.with(|f| f.set(true));
    clear_pending_emits();
    Ok(1)
}

async fn exec_auto_end(token: i64, success: bool) -> Result<(), DbError> {
    // No-op tokens (unwrapped kinds) — nothing to commit.
    if token == 0 {
        return Ok(());
    }

    // Defensive ownership check: if AUTO_TX_OWNED is false the tx is
    // either gone or owned by user code. Either way leave it alone.
    let owned = crate::AUTO_TX_OWNED.with(|f| f.get());
    if !owned {
        return Ok(());
    }

    // Take the client. We MUST clear AUTO_TX_OWNED before any await so
    // a re-entrant call (shouldn't happen — V8 is single-threaded —
    // but cheap insurance) doesn't see stale ownership.
    let client = crate::TX_CONN.with(|tx| tx.borrow_mut().take());
    crate::AUTO_TX_OWNED.with(|f| f.set(false));

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
