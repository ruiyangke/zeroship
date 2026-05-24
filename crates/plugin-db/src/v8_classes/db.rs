//! `Db` — the `#[v8_class]` instance backing `env.db`.
//!
//! `DbPlugin::build_instance` returns a `Db` instance from this
//! module; the `NativeRegistrar` then attaches the Db-scoped entry
//! points (`registerModel`, `transaction`, the `replication*`
//! ops) on top. `Migrations` is reached via the `migrations` getter
//! on this class, not via flat callbacks.
//!
//! ## What this class adds
//!
//! - `collection(name)` — `#[v8_method]` returning a `Collection`
//!   v8_class instance for the given collection name. Cached by
//!   `name`: subsequent calls for the same `name` return the same
//!   `Collection` JS object (`env.db.collection("users") ===
//!   env.db.collection("users")` holds).
//!
//! Per-collection CRUD lives on the `Collection` wrapper, not here —
//! every `find` / `insert` / `update` / `delete` etc. is a
//! `#[v8_method]` on `Collection` that calls into the
//! `crate::crud::dispatch_*` helpers.
//!
//! ## Why a v8_class
//!
//! The instance carries per-isolate state (`app_id` + the collection
//! cache); the brand check that ships with `#[v8_class]` gives a
//! free `instanceof`-style guard for any receiver-shape checks the
//! runtime needs.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

use crate::crud::dispatch_set_mask_policy_field;
use crate::orchestrator::register_model::register_model_dispatch;
use crate::orchestrator::transaction::transaction_dispatch;
use crate::replication_ops::start_replication_consumer_dispatch;
use crate::v8_bridge::{read_json_arg, v8_value_to_serde_json};
use crate::v8_classes::collection::mint_collection;

// ---------------------------------------------------------------------------
// Db state
// ---------------------------------------------------------------------------

/// Owned state for the `env.db` v8_class instance.
///
/// Field 0 of the wrapper holds a `Box<Db>` (this struct). The Weak
/// finalizer registered by `mint_db` drops the Box on GC. There are
/// no native resources to release in `Drop` — `app_id` is a String and
/// the `collection_cache` holds `v8::Global<v8::Object>` handles whose
/// own Weak counterparts (registered by `mint_collection`) reclaim
/// the wrapped Collection state.
pub struct Db {
    /// The app_id this Db belongs to. Captured at instance-build time
    /// from `SharedState.env_vars["APP_ID"]`. Never mutated after mint,
    /// so a plain `String` (not `RefCell`) — borrowed by `&self.app_id`
    /// on the Collection's forwarded dispatch.
    pub(crate) app_id: String,
    /// Cache of `(collection_name -> Collection JS wrapper)`. Populated
    /// on the first `.collection(name)` call for each name; subsequent
    /// calls return the same Global so identity holds:
    /// `env.db.collection("users") === env.db.collection("users")`.
    pub(crate) collection_cache: RefCell<HashMap<String, v8::Global<v8::Object>>>,
    /// Cache of the `Migrations` namespace wrapper minted on first
    /// access of `env.db.migrations`. Stable identity so
    /// `env.db.migrations === env.db.migrations` holds.
    pub(crate) migrations_obj: RefCell<Option<v8::Global<v8::Object>>>,
    /// Cache of the `Replication` namespace wrapper minted on first
    /// access of `env.db.replication`. Stable identity so
    /// `env.db.replication === env.db.replication` holds.
    pub(crate) replication_obj: RefCell<Option<v8::Global<v8::Object>>>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("app_id", &self.app_id)
            .field("collection_cache_len", &self.collection_cache.borrow().len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Db IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Db {
    /// `new Db()` from JS rejects with `TypeError("illegal
    /// constructor")` — real instances are minted via [`mint_db`] from
    /// `DbPlugin::build_instance`, which stamps the live `app_id` onto
    /// the wrapper. A user-constructed Db would have an empty app_id
    /// and every method would silently target a non-existent
    /// `"".* ` schema.
    #[v8_constructor]
    fn new() -> Result<Db, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `db.collection(name)` — returns a [`Collection`] v8_class
    /// instance bound to this Db and the given collection name.
    ///
    /// Cached by `name`: subsequent calls with the same `name` return
    /// the same JS object (so `db.collection("users") ===
    /// db.collection("users")` holds). The cache lives for the
    /// lifetime of the Db wrapper; entries are dropped when the Db
    /// wrapper is GC'd (the `v8::Global` handles in the cache are
    /// dropped with the Box).
    #[v8_method]
    fn collection<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        name: String,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if name.is_empty() {
            return Err(OpError::type_error(
                "db.collection: name must be a non-empty string",
            ));
        }
        use std::collections::hash_map::Entry;
        let mut cache = self.collection_cache.borrow_mut();
        match cache.entry(name) {
            Entry::Occupied(o) => Ok(v8::Local::new(scope, o.get())),
            Entry::Vacant(v) => {
                let obj = mint_collection(scope, v.key().clone(), self.app_id.clone())?;
                v.insert(v8::Global::new(scope, obj));
                Ok(obj)
            }
        }
    }

    /// `db.registerModel(collection, schema, indexes?)` — DDL
    /// orchestrator entry. Idempotent: returns a resolved promise on
    /// second call for the same (app_id, collection). `indexes` is the
    /// array of named multi-column indexes declared on the schema via
    /// `schema(...).index(name, fields)`; each materialises as
    /// `CREATE INDEX CONCURRENTLY IF NOT EXISTS "<collection>__<name>"`.
    #[v8_method]
    #[v8_name = "registerModel"]
    fn register_model<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        collection: String,
        schema: v8::Local<v8::Value>,
        indexes: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if collection.is_empty() {
            return Err(OpError::type_error(
                "db.registerModel: collection must be a non-empty string",
            ));
        }
        let schema_v = read_json_arg(scope, Some(schema));
        let indexes_v = if indexes.is_null_or_undefined() {
            serde_json::Value::Array(Vec::new())
        } else {
            read_json_arg(scope, Some(indexes))
        };
        Ok(register_model_dispatch(
            scope,
            &self.app_id,
            &collection,
            schema_v,
            indexes_v,
        )
        .into())
    }

    /// `db.transaction(asyncFn, opts?)` — run `asyncFn` inside a
    /// transaction (P9 PR 3).
    ///
    /// This is the native orchestrator behind the creator-facing
    /// `await env.db.transaction(async tx => { ... })`. `asyncFn` is
    /// called with a collections-only `tx` view
    /// ([`super::transaction::mint_tx_view`]); the returned promise
    /// resolves with the callback's result on **commit** (callback
    /// resolved) and rejects with the callback's error on **rollback**
    /// (callback threw / rejected). There is no `tx.commit()` /
    /// `tx.rollback()` — abort by throwing.
    ///
    /// A `transaction()` call made while a transaction is already active
    /// for this isolate (an enclosing `transaction()` or the auto-tx
    /// wrapper) opens a `SAVEPOINT` instead of a fresh `BEGIN`; the inner
    /// callback's failure rolls back only to that savepoint. See
    /// [`crate::orchestrator::transaction`] for the full state machine.
    ///
    /// `opts` is `{ isolationLevel?: "readCommitted" | "repeatableRead"
    /// | "serializable" }` (honoured only on the outermost `BEGIN`; a
    /// `SAVEPOINT` inherits the enclosing transaction's isolation).
    #[v8_method]
    #[v8_name = "transaction"]
    fn transaction<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        callback: v8::Local<v8::Value>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        // First arg must be a callable.
        let user_fn: v8::Local<v8::Function> = callback.try_into().map_err(|_| {
            let got = js_type_name(callback);
            OpError::type_error(format!(
                "db.transaction: first argument must be a function, got {got}"
            ))
        })?;

        let isolation = if opts.is_null_or_undefined() {
            None
        } else {
            if !opts.is_object() {
                let got = js_type_name(opts);
                return Err(OpError::type_error(format!(
                    "db.transaction: opts must be an object, got {got}"
                )));
            }
            let parsed = v8_value_to_serde_json(scope, opts);
            let raw = parsed
                .as_object()
                .and_then(|o| o.get("isolationLevel"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            match raw {
                Some(s) => Some(normalize_isolation_level(&s)?),
                None => None,
            }
        };
        Ok(transaction_dispatch(scope, user_fn, isolation, self.app_id.clone()).into())
    }

    /// `db.startReplicationConsumer(opts?)` — provisions the per-app
    /// publication + slot, then spawns the supervised WAL consumer.
    /// Idempotent.
    ///
    /// Always scoped to `self.app_id` (the id stamped on this `Db`
    /// wrapper at mint time, sourced from the isolate's `APP_ID` env
    /// var). `opts` is accepted but currently unused — reserved for
    /// forward compatibility. Any app-id-shaped value is intentionally
    /// ignored: the v8_class runs in the tenant isolate, so honouring
    /// a JS-supplied app_id here would let App A spawn a WAL consumer
    /// against App B's stream. Operator-shaped provisioning is a
    /// control-plane concern. Resolution policy lives in
    /// `resolve_consumer_app_id` (out-of-method so it's unit-testable
    /// without V8 plumbing — see the `consumer_app_id_*` tests).
    #[v8_method]
    #[v8_name = "startReplicationConsumer"]
    fn start_replication_consumer<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let app_id = resolve_consumer_app_id(&self.app_id, scope, opts);
        start_replication_consumer_dispatch(scope, app_id).into()
    }

    /// `db.setMaskPolicy(policy)` — **P5.5 PR 5**. Persist the per-app
    /// mask policy declared via `defineMaskPolicy()` and refresh the
    /// in-process cache write-through. The flushed policy is the
    /// authoritative source for the unmask authorization path
    /// (`crud::unmask::check_unmask_authorization`); without it, the
    /// PR 4 default-deny stub applies (`auto` actor allowed; everyone
    /// else denied).
    ///
    /// `policy` is the canonical wire shape produced by
    /// `defineMaskPolicy()`: `{ "<role>": ["<classification>", ...], … }`.
    /// Validated structurally + against the six-classification
    /// taxonomy in [`crate::crud::mask_policy::MaskPolicy::from_json`].
    ///
    /// Resolves with `{}` on success; rejects with the typed
    /// `invalid_mask_policy_shape` / `invalid_mask_classification` /
    /// `backend_unsupported` codes on failure.
    #[v8_method]
    #[v8_name = "setMaskPolicy"]
    fn set_mask_policy<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        policy: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let policy_v = read_json_arg(scope, Some(policy));
        Ok(dispatch_set_mask_policy_field(scope, &self.app_id, policy_v).into())
    }

    // **P9 PR 2** — `db.unmaskField` and `db.bulkUnmaskFields` were
    // removed. The unmask round-trip is now collection-scoped:
    // `Collection.unmaskField(rowPk, col, opts)` and
    // `Collection.bulkUnmask(items, opts)` (the collection name is
    // inherited from the receiver, so it can't be spoofed by an
    // args-shape mismatch). `MaskedValue.unmask(...)` dispatches
    // natively from the v8_class instance using its own bound `_meta`.

    /// `db.replication` — returns the [`super::replication::Replication`]
    /// namespace wrapper exposing `setup` / `watchdog` / `dropAbandoned`
    /// scoped to this app. Cached on first access.
    #[v8_getter]
    fn replication<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if let Some(existing) = self.replication_obj.borrow().as_ref() {
            return Ok(v8::Local::new(scope, existing));
        }
        let obj = super::replication::mint_replication(scope, &self.app_id)?;
        let global = v8::Global::new(scope, obj);
        *self.replication_obj.borrow_mut() = Some(global);
        Ok(obj)
    }

    /// `db.migrations` — returns the [`super::migrations::Migrations`]
    /// namespace wrapper. Cached on first access: subsequent reads of
    /// `env.db.migrations` return the same JS object.
    ///
    /// Exposed as a `#[v8_getter]` (not `#[v8_method]`) so callers
    /// access it as a property — `env.db.migrations.start(spec)` — and
    /// JS identity holds across reads (`env.db.migrations ===
    /// env.db.migrations`). The Db instance owns the cached Global, so
    /// we don't need WebIDL `[SameObject]` macro support; manual
    /// caching on `migrations_obj` is sufficient and lets us pass the
    /// owned `app_id` into `mint_migrations`.
    #[v8_getter]
    fn migrations<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if let Some(existing) = self.migrations_obj.borrow().as_ref() {
            return Ok(v8::Local::new(scope, existing));
        }
        let obj = super::migrations::mint_migrations(scope, &self.app_id)?;
        let global = v8::Global::new(scope, obj);
        *self.migrations_obj.borrow_mut() = Some(global);
        Ok(obj)
    }
}

/// Cheap JS-side type label for error messages. Matches the labels
/// `typeof` would surface so users can correlate with what they
/// passed.
fn js_type_name(v: v8::Local<v8::Value>) -> &'static str {
    if v.is_string() { "string" }
    else if v.is_number() { "number" }
    else if v.is_boolean() { "boolean" }
    else if v.is_function() { "function" }
    else if v.is_array() { "array" }
    else if v.is_null() { "null" }
    else if v.is_undefined() { "undefined" }
    else { "value" }
}

/// Resolve the app_id used by `Db::start_replication_consumer` for
/// dispatch.
///
/// Returns `stamped` verbatim, ignoring any JS-supplied `opts`. Lifted
/// out of the `#[v8_method]` body (a) so the security-critical
/// resolution policy is unit-testable without V8 plumbing and (b) so
/// a future contributor restoring caller-controlled overrides has to
/// delete this helper (and its tests) — making the regression visible
/// in review.
///
/// The `_scope` and `_opts` parameters mirror the v8_method signature
/// so future forward-compat fields can be plumbed through without
/// touching the security-critical app-id path.
#[inline]
fn resolve_consumer_app_id(
    stamped: &str,
    _scope: &mut v8::PinScope<'_, '_>,
    _opts: v8::Local<v8::Value>,
) -> String {
    // INVARIANT: never derive the app_id from `_opts`. The v8_class
    // executes inside the tenant isolate; any caller-supplied override
    // is a cross-app hijack vector (App A spawning a WAL consumer on
    // App B's stream). See method doc-comment.
    stamped.to_string()
}

/// Normalise a JS-supplied isolation-level identifier into the SQL
/// form Postgres expects. Accepts both camelCase (`"readCommitted"`)
/// and the literal SQL string (`"read committed"`). Rejects anything
/// else with a `TypeError`.
fn normalize_isolation_level(raw: &str) -> Result<String, OpError> {
    let trimmed = raw.trim();
    let sql = match trimmed {
        "readUncommitted" | "read uncommitted" | "READ UNCOMMITTED" => "READ UNCOMMITTED",
        "readCommitted" | "read committed" | "READ COMMITTED" => "READ COMMITTED",
        "repeatableRead" | "repeatable read" | "REPEATABLE READ" => "REPEATABLE READ",
        "serializable" | "SERIALIZABLE" => "SERIALIZABLE",
        _ => {
            return Err(OpError::type_error(format!(
                "db.transaction: unknown isolationLevel '{raw}' \
                 (expected readCommitted | repeatableRead | serializable)"
            )));
        }
    };
    Ok(sql.to_string())
}

// ---------------------------------------------------------------------------
// mint_db — build a `Db` wrapper for a given app_id
// ---------------------------------------------------------------------------

/// Mint a `Db` v8_class instance with state stamped from `app_id`.
///
/// Called from `DbPlugin::build_instance` once per V8 isolate during
/// `build_env_object`. The returned object becomes the `env.db`
/// namespace value; the runtime then layers the Db-scoped entry
/// points (registerModel, transaction, …) on top via the
/// `NativeRegistrar` returned by `DbPlugin::register`.
pub fn mint_db<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = Db::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = Db {
        app_id: app_id.to_string(),
        collection_cache: RefCell::new(HashMap::new()),
        migrations_obj: RefCell::new(None),
        replication_obj: RefCell::new(None),
    };
    let boxed: Box<Db> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<Db>`; the
    // finalizer closure casts back to the same type and drops the Box
    // exactly once when V8 reclaims the wrapper. There are no native
    // resources to release; the Collection cache holds
    // `v8::Global<v8::Object>` handles that are dropped together with
    // the Box, and each Collection's own Weak finalizer reclaims its
    // state.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Db));
        }),
    );
    std::mem::forget(weak);

    Some(obj)
}

#[cfg(test)]
mod tests {
    //! Regression guards for the cross-app replication-consumer
    //! hijack fix (security review r2, 2026-05-22). Prior to the fix,
    //! `Db::start_replication_consumer` accepted a string `opts`
    //! argument and used it as an app-id override, letting App A
    //! spawn a WAL consumer reading any victim app's WAL stream. The
    //! fix routes app-id resolution through
    //! [`super::resolve_consumer_app_id`], which always returns the
    //! mint-time `self.app_id`.
    //!
    //! Tests construct a V8 scope (cheap — no isolate snapshots, no
    //! contexts beyond the bare minimum) and feed varied `opts` shapes
    //! through `resolve_consumer_app_id`. The assertion is the same
    //! across every shape: the resolved id equals the stamped id,
    //! never the override.
    #![allow(unsafe_code)]

    use super::resolve_consumer_app_id;
    use zeroship_runtime::init_v8;

    #[test]
    fn consumer_app_id_ignores_string_override() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // The legacy bug: `opts` typed as a JS string. App A passes
        // "victim_app" expecting the dispatch to target it.
        let victim = v8::String::new(scope, "victim_app").unwrap();
        let resolved = resolve_consumer_app_id("app_a", scope, victim.into());
        assert_eq!(resolved, "app_a", "string override leaked through");
    }

    #[test]
    fn consumer_app_id_ignores_object_override() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Object-shaped opts ({appId: "victim_app"}) — symmetric with
        // the `Replication::setup` exploit shape. Must not leak.
        let obj = v8::Object::new(scope);
        let key = v8::String::new(scope, "appId").unwrap();
        let val = v8::String::new(scope, "victim_app").unwrap();
        obj.set(scope, key.into(), val.into());
        let resolved = resolve_consumer_app_id("app_a", scope, obj.into());
        assert_eq!(resolved, "app_a", "object override leaked through");
    }

    #[test]
    fn consumer_app_id_undefined_opts_uses_stamped() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let undef = v8::undefined(scope);
        let resolved = resolve_consumer_app_id("app_a", scope, undef.into());
        assert_eq!(resolved, "app_a");

        let null = v8::null(scope);
        let resolved = resolve_consumer_app_id("app_a", scope, null.into());
        assert_eq!(resolved, "app_a");
    }
}
