//! `Transaction` — native V8 wrapper for an open DB transaction.
//!
//! Returned by `env.db.beginTransaction(isolationLevel?)`. The wrapper
//! owns the lifecycle of the per-isolate [`crate::TX_CONN`] thread-local
//! for as long as the transaction is active; `.commit()` / `.rollback()`
//! drain TX_CONN and run the matching SQL, and a `v8::Weak` guaranteed
//! finalizer reclaims the wrapper's `Box<Transaction>` on GC.
//!
//! ## Why a v8_class
//!
//! Before this wrapper landed, a user who `await`-ed
//! `env.db.beginTransaction()` and then dropped the result without
//! calling `commitTransaction()` / `rollbackTransaction()` leaked the
//! Postgres connection until isolate teardown — the TX_CONN
//! thread-local pinned the [`compio_postgres::Client`] indefinitely.
//! The wrapper's Weak finalizer fixes that: when V8 collects the
//! wrapper, the finalizer drops the `Box<Transaction>`, our `Drop` impl
//! takes the still-active `Client` out of TX_CONN, and Postgres
//! observes the connection close and auto-rollbacks server-side.
//!
//! ## JS surface
//!
//! ```ts
//! const tx = await env.db.beginTransaction("read committed");
//! await tx.collection("todos").create({ ... });
//! await tx.commit();                 // explicit
//! // (or) await tx.rollback();
//! // (or) tx goes out of scope    → GC finalizer rollbacks
//! ```
//!
//! `.collection(name)` returns a [`super::collection::Collection`]
//! v8_class instance bound to a *fake-Db parent that this Transaction
//! impersonates* — Collection's forwarders read methods like `findOne`
//! / `insert` off the parent object and call them with `[name,
//! ...args]`. The Transaction wrapper carries the 27 flat callbacks as
//! own properties (mirrored from the real Db at mint time), so each
//! forwarded call reads the underlying Db callback through this
//! wrapper and dispatches it. Because TX_CONN is set while the
//! transaction is active, every CRUD callback routes through the
//! transaction connection via `run_sql` automatically.
//!
//! ## Coexistence with the legacy flat API
//!
//! The flat callbacks `commitTransaction` / `rollbackTransaction` stay
//! registered for back-compat with the current `@zeroship/db` SDK,
//! which still does `await native.commitTransaction()` after the user
//! handler runs. To keep both surfaces correct under the same TX_CONN
//! slot, we track ownership with [`TX_TOKEN`]: every begin (wrapper or
//! flat) bumps the token, and any commit/rollback (wrapper or flat or
//! GC) checks `self.token == TX_TOKEN` before acting. After the legacy
//! `commitTransaction` consumes TX_CONN, the wrapper's own
//! `commit`/`rollback`/finalizer sees the mismatch and short-circuits.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};

use compio_postgres::Client;
use zeroship_runtime::state::OpError;
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
    /// before taking action — if the legacy flat callback already
    /// consumed TX_CONN, our token won't match and we no-op.
    pub(crate) token: Cell<u64>,
    /// True once the wrapper has been committed or rolled back. Further
    /// `.commit()` / `.rollback()` calls reject; `.collection()` /
    /// CRUD forwarders reject as well.
    pub(crate) settled: Cell<bool>,
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
        let current = crate::TX_TOKEN.with(Cell::get);
        if current != token {
            return;
        }
        // We are the live owner. Take and drop the Client; Postgres
        // auto-rollbacks at connection close. Also clear TX_TOKEN so
        // run_sql / a future begin sees a clean slate.
        let client: Option<Client> = crate::TX_CONN.with(|c| c.borrow_mut().take());
        crate::TX_TOKEN.with(|t| t.set(0));
        drop(client);
    }
}

// ---------------------------------------------------------------------------
// Transaction IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Transaction {
    /// Placeholder constructor for the macro. Real instances come from
    /// [`mint_transaction`] via `env.db.beginTransaction(...)`. Calling
    /// `new Transaction()` from JS produces a wrapper whose `token` is
    /// 0; every method short-circuits ("already settled") because no
    /// BEGIN ever ran for this instance.
    #[v8_constructor]
    fn new() -> Transaction {
        Transaction {
            token: Cell::new(0),
            settled: Cell::new(false),
            collection_cache: RefCell::new(std::collections::HashMap::new()),
        }
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
        wrapper: v8::Local<v8::Object>,
        name: String,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if name.is_empty() {
            return Err(OpError::type_error(
                "tx.collection: name must be a non-empty string",
            ));
        }
        if self.settled.get() {
            return Err(OpError::error(
                "tx.collection: transaction already committed or rolled back",
            ));
        }
        if let Some(existing) = self.collection_cache.borrow().get(&name) {
            return Ok(v8::Local::new(scope, existing));
        }
        let parent_global = v8::Global::new(scope, wrapper);
        let obj = mint_collection(scope, name.clone(), parent_global)?;
        let global = v8::Global::new(scope, obj);
        self.collection_cache.borrow_mut().insert(name, global);
        Ok(obj)
    }

    /// `tx.commit(): Promise<void>` — run COMMIT against the
    /// transaction connection, clear [`crate::TX_CONN`], and mark this
    /// wrapper settled.
    ///
    /// If the underlying transaction has already been settled (via this
    /// method, `.rollback()`, or the legacy `commitTransaction`
    /// callback), this rejects with "transaction already settled".
    #[v8_async_method]
    async fn commit(&self) -> Result<(), OpError> {
        end(self, "COMMIT").await
    }

    /// `tx.rollback(): Promise<void>` — symmetric to [`Self::commit`]
    /// but issues ROLLBACK instead.
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
/// Defensive against races with the legacy flat callbacks: if our
/// `token` doesn't match the current TX_TOKEN by the time we run, the
/// transaction has already been settled by someone else — we reject
/// with a clear "already settled" message.
async fn end(this: &Transaction, cmd: &str) -> Result<(), OpError> {
    let token = this.token.get();
    if token == 0 || this.settled.get() {
        return Err(OpError::error(
            "tx: transaction already committed or rolled back",
        ));
    }
    let current = crate::TX_TOKEN.with(Cell::get);
    if current != token {
        this.settled.set(true);
        return Err(OpError::error(
            "tx: transaction already committed or rolled back",
        ));
    }

    // Take the Client out — same drain pattern as the legacy
    // `exec_end`. We hold it through the cmd execution and drop it at
    // the end of this scope; that lets the spawned Connection task
    // observe the closed sender and tear the conn down cleanly.
    let client_opt = crate::TX_CONN.with(|c| c.borrow_mut().take());
    let Some(client) = client_opt else {
        // Belt and suspenders: TX_CONN was already cleared. Mark
        // settled and surface the same error shape.
        this.settled.set(true);
        crate::TX_TOKEN.with(|t| t.set(0));
        return Err(OpError::error(
            "tx: no active transaction connection",
        ));
    };

    // Clear ownership BEFORE awaiting so a concurrent finalizer (or a
    // legacy flat callback firing in parallel — unlikely but cheap to
    // defend) can observe "settled" and no-op.
    crate::TX_TOKEN.with(|t| t.set(0));
    this.settled.set(true);

    let result = client
        .execute(cmd, &[])
        .await
        .map_err(|e| OpError::error(format!("tx: {cmd} failed: {e}")));
    // Client dropped here either way.
    drop(client);

    result.map(|_| ())
}

// ---------------------------------------------------------------------------
// mint_transaction — build the wrapper at begin-time
// ---------------------------------------------------------------------------

/// Mint a Transaction v8_class instance and stamp `token` into both
/// the wrapper state and [`crate::TX_TOKEN`].
///
/// Caller invariant: TX_CONN has just been set by a successful BEGIN
/// and no other Transaction wrapper is alive for the same TX_CONN —
/// this is enforced by the "nested transactions not supported" check
/// in [`crate::callbacks::begin_transaction`]'s async path.
///
/// The caller must layer the 27 flat callbacks (`findOne`, `insert`,
/// …) as own properties on the returned object — otherwise
/// `tx.collection("x").find(...)` has nothing to dispatch to. This is
/// done by [`install_flat_callbacks_on`].
pub fn mint_transaction<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    token: u64,
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

/// Copy the parent Db's flat CRUD callbacks onto a freshly minted
/// Transaction wrapper as own properties.
///
/// The Transaction's `.collection(name)` returns a Collection whose
/// CRUD forwarders read methods like `findOne`, `insert` off the
/// parent object (the Transaction wrapper here) and call them with
/// `[name, ...args]`. So the wrapper must carry those callbacks as
/// own properties — same shape the real `env.db` instance does via
/// `NativeRegistrar`.
///
/// We mirror the subset that participate in TX_CONN-driven dispatch
/// (the CRUD ops + `aggregate` / `count` / `distinct`). Subscription
/// ops are deliberately excluded — the broker isn't transactional.
pub fn install_flat_callbacks_on(
    scope: &mut v8::PinScope<'_, '_>,
    tx_obj: v8::Local<v8::Object>,
    env_db: v8::Local<v8::Object>,
) {
    const METHODS: &[&str] = &[
        "findOne", "find", "insert", "insertMany", "updateOne", "updateMany",
        "deleteOne", "deleteMany", "upsert", "count", "distinct", "aggregate",
    ];
    for m in METHODS {
        let key = v8::String::new(scope, m).unwrap();
        if let Some(v) = env_db.get(scope, key.into()) {
            tx_obj.set(scope, key.into(), v);
        }
    }
}
