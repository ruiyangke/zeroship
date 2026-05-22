//! `Transaction` — native V8 wrapper for an open DB transaction.
//!
//! Returned by `env.db.beginTransaction(isolationLevel?)`. The wrapper
//! owns the per-isolate [`crate::TX_CONN`] for the lifetime of the
//! transaction; `.commit()` / `.rollback()` drain TX_CONN and run the
//! matching SQL, and a `v8::Weak` guaranteed finalizer reclaims the
//! wrapper's `Box<Transaction>` on GC. If user code drops the wrapper
//! without explicit `.commit()` / `.rollback()`, the `Drop` impl
//! takes the still-active `Client` out of TX_CONN, and Postgres
//! observes the connection close and auto-rollbacks server-side.
//!
//! ## JS surface
//!
//! ```ts
//! const tx = await env.db.beginTransaction("read committed");
//! await tx.collection("todos").insert({ ... });
//! await tx.commit();              // explicit
//! // (or) await tx.rollback();
//! // (or) tx goes out of scope    → GC finalizer rollbacks
//! ```
//!
//! `.collection(name)` returns a [`super::collection::Collection`]
//! v8_class instance bound to this Transaction. Because TX_CONN is
//! set while the transaction is active, every CRUD method on that
//! Collection (which calls `crate::crud::dispatch_*` → `run_sql`)
//! automatically routes through the transaction connection.
//!
//! ## Ownership token
//!
//! TX_CONN is a single per-isolate slot; an explicit `.commit()` and
//! the wrapper's `Drop` finalizer can race (commit succeeds, GC
//! finalizer wakes up afterwards). To make the race safe, every
//! successful BEGIN bumps [`TX_TOKEN`] and stamps the same value
//! onto the wrapper. Commit / rollback / Drop all check
//! `self.token == TX_TOKEN` before touching the connection — once
//! one path settles the tx and clears TX_TOKEN, the others no-op.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};

use zeroship_runtime::state::OpError;

// The transaction wrapper owns a backend session client borrowed
// out of the per-isolate context's tx slot. The concrete type is
// `<PostgresBackend as Backend>::Client` (= `compio_postgres::Client`
// today) but consumer files name it through the type alias so the
// `compio_postgres` crate stays scoped to `backend/postgres.rs` and
// `context.rs`.
type Client = <crate::backend::PostgresBackend as crate::backend::Backend>::Client;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_async_method, v8_constructor, v8_method};

use crate::v8_classes::collection::mint_collection;

// ---------------------------------------------------------------------------
// Transaction state
// ---------------------------------------------------------------------------

/// Owned state for one JS `Transaction` instance.
///
/// Field 0 of the wrapper holds a `Box<Transaction>` (this struct).
/// The Weak finalizer registered by [`mint_transaction`] reclaims the
/// Box on GC and our `Drop` impl auto-rollbacks if this wrapper still
/// owns the active transaction.
pub struct Transaction {
    /// Ownership token stamped into [`crate::TX_TOKEN`] at successful
    /// BEGIN. Commit / rollback / GC compare to the current TX_TOKEN
    /// before taking action — once one path settles the tx and clears
    /// TX_TOKEN, the others see the mismatch and no-op (so an
    /// explicit `.commit()` followed by GC doesn't run COMMIT twice).
    pub(crate) token: Cell<u64>,
    /// True once the wrapper has been committed or rolled back.
    /// Further `.commit()` / `.rollback()` calls resolve void; new
    /// `.collection()` calls reject (the tx connection is gone).
    pub(crate) settled: Cell<bool>,
    /// app_id inherited from the parent `Db` wrapper at mint time. The
    /// canonical source for `tx.collection(name)`-minted Collections so
    /// they bind to the same app as the Db that began the transaction
    /// (rather than re-reading the runtime slot, which can drift if a
    /// nested execution context replaces the SharedState).
    pub(crate) app_id: String,
    /// Cache of `(collection_name -> Collection JS wrapper)`. Same shape
    /// as `Db::collection_cache` so identity holds across calls:
    /// `tx.collection("users") === tx.collection("users")`.
    pub(crate) collection_cache: RefCell<std::collections::HashMap<String, v8::Global<v8::Object>>>,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("token", &self.token.get())
            .field("settled", &self.settled.get())
            .field("cache_len", &self.collection_cache.borrow().len())
            .finish()
    }
}

impl Drop for Transaction {
    /// GC-time auto-rollback.
    ///
    /// Fires from the Weak finalizer that [`mint_transaction`] registers
    /// on every instance: when V8 collects the wrapper, the finalizer
    /// drops the Box, which runs this.
    ///
    /// If our token still matches the current TX_TOKEN, this wrapper is
    /// the live owner of TX_CONN — take the Client out and drop it.
    /// Dropping the Client closes the per-tx connection (the spawned
    /// `Connection` run-loop observes the closed sender and sends
    /// Terminate), and Postgres rolls the open transaction back
    /// server-side.
    ///
    /// If our token doesn't match — either the user committed via
    /// `.commit()` / `commitTransaction()`, rolled back, or the
    /// auto-tx wrapper repurposed TX_CONN — we leave TX_CONN alone.
    fn drop(&mut self) {
        let token = self.token.get();
        if token == 0 || self.settled.get() {
            return;
        }
        let current = crate::context::with(|c| c.tx_token());
        if current != token {
            return;
        }
        // We are the live owner. Take and drop the Client; Postgres
        // auto-rollbacks at connection close. Also clear `tx_token` so
        // run_sql / a future begin sees a clean slate.
        let client: Option<Client> = crate::context::with_mut(|c| {
            let client = c.take_tx_client();
            c.set_tx_token(0);
            client
        });
        drop(client);
        // GC-driven implicit rollback — drop any queued broker events
        // so subscribers never observe the now-aborted writes.
        crate::exec::clear_pending_emits();
    }
}

// ---------------------------------------------------------------------------
// Transaction IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Transaction {
    /// `new Transaction()` from JS rejects — real instances come from
    /// [`mint_transaction`] via `env.db.beginTransaction(...)`, which
    /// stamps the ownership token onto the wrapper only after a
    /// successful BEGIN.
    #[v8_constructor]
    fn new() -> Result<Transaction, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `tx.collection(name)` — returns a [`super::collection::Collection`]
    /// instance whose CRUD methods forward through this Transaction
    /// wrapper. Cached by `name`; identity holds across calls.
    ///
    /// Because each CRUD callback consults [`crate::TX_CONN`] via
    /// `run_sql`, any operation through this collection automatically
    /// participates in the open transaction.
    #[v8_method]
    fn collection<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        name: String,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if name.is_empty() {
            return Err(OpError::type_error(
                "tx.collection: name must be a non-empty string",
            ));
        }
        if self.settled.get() {
            return Err(crate::error::DbError::validation(
                "tx_settled",
                "tx.collection: transaction already committed or rolled back",
            )
            .to_op_error());
        }
        if let Some(existing) = self.collection_cache.borrow().get(&name) {
            return Ok(v8::Local::new(scope, existing));
        }
        let obj = mint_collection(scope, name.clone(), self.app_id.clone())?;
        let global = v8::Global::new(scope, obj);
        self.collection_cache.borrow_mut().insert(name, global);
        Ok(obj)
    }

    /// `tx.commit(): Promise<void>` — run COMMIT against the
    /// transaction connection, clear [`crate::TX_CONN`], and mark this
    /// wrapper settled. Idempotent: a second `commit()` / `rollback()`
    /// resolves void instead of rejecting, and the GC finalizer no-
    /// ops once a settle path has run.
    #[v8_async_method]
    async fn commit(&self) -> Result<(), OpError> {
        end(self, "COMMIT").await
    }

    /// `tx.rollback(): Promise<void>` — symmetric to [`Self::commit`]
    /// but issues ROLLBACK instead. Idempotent.
    #[v8_async_method]
    async fn rollback(&self) -> Result<(), OpError> {
        end(self, "ROLLBACK").await
    }
}

// ---------------------------------------------------------------------------
// Shared commit/rollback implementation
// ---------------------------------------------------------------------------

/// Run `cmd` (`"COMMIT"` or `"ROLLBACK"`) against the transaction's
/// connection, then clear [`crate::TX_CONN`] / [`crate::TX_TOKEN`] and
/// mark the wrapper settled.
///
/// Idempotent: if our `token` is zero, the wrapper is already settled,
/// or `TX_TOKEN` has been claimed by another path (Drop finalizer,
/// concurrent commit), `end` returns `Ok(())` without re-running the
/// SQL. Postgres errors during the live commit path are surfaced
/// verbatim.
async fn end(this: &Transaction, cmd: &str) -> Result<(), OpError> {
    let token = this.token.get();
    if token == 0 || this.settled.get() {
        return Ok(());
    }
    let current = crate::context::with(|c| c.tx_token());
    if current != token {
        this.settled.set(true);
        return Ok(());
    }

    // Take the Client out. We hold it through the cmd execution and
    // drop it at the end of this scope; that lets the spawned
    // Connection task observe the closed sender and tear the conn
    // down cleanly.
    let client_opt = crate::context::with_mut(|c| c.take_tx_client());
    let Some(client) = client_opt else {
        // tx_conn already cleared by another path — treat as already
        // settled rather than a fresh failure.
        this.settled.set(true);
        crate::context::with_mut(|c| c.set_tx_token(0));
        return Ok(());
    };

    // Clear ownership BEFORE awaiting so a concurrent finalizer (the
    // wrapper getting GC'd while the await is in flight) observes
    // "settled" and no-ops instead of running ROLLBACK on a connection
    // we already have in hand.
    crate::context::with_mut(|c| c.set_tx_token(0));
    this.settled.set(true);

    let result = client
        .execute(cmd, &[])
        .await
        .map_err(|e| crate::error::DbError::from_pg(&e).to_op_error());
    // Client dropped here either way.
    drop(client);

    // Settle the deferred broker queue (Gap B). On a successful COMMIT
    // fire every event we'd have published mid-tx; on ROLLBACK or
    // COMMIT failure drop the queue so subscribers never see writes
    // Postgres just undid.
    if cmd == "COMMIT" && result.is_ok() {
        crate::exec::drain_pending_emits_on_commit();
    } else {
        crate::exec::clear_pending_emits();
    }

    result.map(|_| ())
}

// ---------------------------------------------------------------------------
// mint_transaction — build the wrapper at begin-time
// ---------------------------------------------------------------------------

/// Mint a Transaction v8_class instance and stamp `token` into the
/// wrapper state. The matching `TX_TOKEN` write happens in
/// [`crate::orchestrator::transaction::begin_transaction_dispatch`] only after the async BEGIN
/// succeeds — so a failed BEGIN leaves the wrapper with a token that
/// never matches, and its `Drop` is a no-op when V8 eventually
/// collects it.
///
/// Caller invariant: TX_CONN has just been set by a successful BEGIN
/// and no other Transaction wrapper is alive for the same TX_CONN —
/// enforced by the "nested transactions not supported" check in
/// [`crate::orchestrator::transaction::begin_transaction_dispatch`]'s async path.
pub fn mint_transaction<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    token: u64,
    app_id: String,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let class_tmpl = Transaction::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Transaction instance allocation failed"))?;

    // Set the prototype so .commit / .rollback / .collection resolve.
    let class_fn = class_tmpl
        .get_function(scope)
        .ok_or_else(|| OpError::type_error("Transaction template missing function"))?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn
        .get(scope, proto_key.into())
        .ok_or_else(|| OpError::type_error("Transaction prototype missing"))?;
    obj.set_prototype(scope, proto_v);

    let state = Transaction {
        token: Cell::new(token),
        settled: Cell::new(false),
        app_id,
        collection_cache: RefCell::new(std::collections::HashMap::new()),
    };
    let boxed: Box<Transaction> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<Transaction>`;
    // the finalizer closure casts back to the same type and drops the
    // Box exactly once when V8 reclaims the wrapper. The `Drop` impl
    // above checks the current TX_TOKEN and auto-rollbacks if we still
    // own the open transaction.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Transaction));
        }),
    );
    std::mem::forget(weak);

    Ok(obj)
}
