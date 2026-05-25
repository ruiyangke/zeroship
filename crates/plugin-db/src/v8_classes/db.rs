//! `Db` — the `#[v8_class]` instance backing `env.db`.
//!
//! `DbPlugin::build_instance` returns a `Db` instance from this module.
//!
//! ## Creator-facing surface (`env.db.*`)
//!
//! - `collection(name)` — `#[v8_method]` returning a `Collection`
//!   v8_class instance for the given collection name. Cached by
//!   `name`: subsequent calls for the same `name` return the same
//!   `Collection` JS object (`env.db.collection("users") ===
//!   env.db.collection("users")` holds).
//! - `transaction(fn, opts?)` — the native transaction orchestrator
//!   (P9 PR 3).
//!
//! Per-collection CRUD lives on the `Collection` wrapper, not here —
//! every `find` / `insert` / `update` / `delete` etc. is a
//! `#[v8_method]` on `Collection` that calls into the
//! `crate::crud::dispatch_*` helpers.
//!
//! ## Platform-internal surface (P9 PR 4 — behind `__platform`)
//!
//! `registerModel`, `setMaskPolicy`, `startReplicationConsumer`, and the
//! `migrations` / `replication` namespaces moved off `env.db` to the
//! [`super::db_platform::DbPlatform`] capability handle. That handle is
//! set on this `Db` object under the `ZS_PLATFORM` private symbol in
//! [`mint_db`] and reached only via `@zeroship/bootstrap`'s
//! runtime-entry (§8). The string `env.db.__platform` is actively
//! refused by the [`Db::platform_trap`] getter
//! (`platform_internal_only`).
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

use crate::transaction::transaction_dispatch;
use crate::v8_bridge::v8_value_to_serde_json;
use crate::v8_classes::collection::mint_collection;
use crate::v8_classes::db_platform::mint_db_platform;

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
    // **P9 PR 4** — the `migrations` / `replication` namespace caches
    // moved to `DbPlatform` (they're reached via `__platform.migrations`
    // / `__platform.replication`, not `env.db.*`). The `DbPlatform`
    // instance itself is stashed on this wrapper under the `ZS_PLATFORM`
    // private symbol (set in `mint_db`), not as a struct field — its
    // own Weak finalizer reclaims it.
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

    // **P9 PR 4** — `db.registerModel` moved to
    // `DbPlatform::register_model` (reached via `__platform`, not
    // `env.db`). The `register_model_dispatch` pipeline is unchanged.

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
    /// for this isolate (an enclosing `transaction()`) opens a
    /// `SAVEPOINT` instead of a fresh `BEGIN`; the inner callback's
    /// failure rolls back only to that savepoint. See
    /// [`crate::transaction`] for the full state machine.
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

    // **P9 PR 4** — `db.startReplicationConsumer`, `db.setMaskPolicy`,
    // and the `db.migrations` / `db.replication` getters moved to
    // `DbPlatform` (reached via the `__platform` capability handle, not
    // `env.db`). Their dispatch pipelines (`start_replication_consumer_
    // dispatch`, `dispatch_set_mask_policy_field`, `mint_migrations`,
    // `mint_replication`) are unchanged — only the JS carrier relocated.
    //
    // **P9 PR 2** — `db.unmaskField` / `db.bulkUnmaskFields` had already
    // moved to `Collection.unmaskField` / `.bulkUnmask` +
    // `MaskedValue.unmask`.

    /// `env.db.__platform` (string access) — **actively refused** (P9
    /// §8). The real `DbPlatform` capability handle lives under the
    /// `ZS_PLATFORM` private symbol, not under any string-named
    /// property, so a creator reading `env.db.__platform` hits this trap
    /// and gets a typed `platform_internal_only` error rather than the
    /// handle (or a silent `undefined`). Defense-in-depth: even if a
    /// future code path accidentally planted a string `__platform`
    /// property, this getter shadows it. The legitimate reader
    /// (`@zeroship/bootstrap`'s `runtime-entry`) never uses the string
    /// name — it resolves the handle through `globalThis.__zsDbPlatform`,
    /// which reads the private slot in Rust.
    #[v8_getter]
    #[v8_name = "__platform"]
    fn platform_trap<'s>(
        &self,
        _scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        tracing::error!(
            target: "zeroship_db",
            app_id = %self.app_id,
            "env.db.__platform string access denied (platform_internal_only) — \
             the platform capability handle is private-symbol-only"
        );
        Err(crate::error::DbError::AccessDenied {
            code: "platform_internal_only",
        }
        .to_op_error())
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

/// Resolve the app_id used by `startReplicationConsumer` for dispatch.
///
/// Returns `stamped` verbatim, ignoring any JS-supplied `opts`. Lifted
/// out of the `#[v8_method]` body (a) so the security-critical
/// resolution policy is unit-testable without V8 plumbing and (b) so
/// a future contributor restoring caller-controlled overrides has to
/// delete this helper (and its tests) — making the regression visible
/// in review.
///
/// **P9 PR 4** — `startReplicationConsumer` moved to
/// [`super::db_platform::DbPlatform`]; this helper stays here (its
/// `consumer_app_id_*` unit tests live in this module) and is called by
/// `DbPlatform::start_replication_consumer` via
/// `super::db::resolve_consumer_app_id`.
///
/// The `_scope` and `_opts` parameters mirror the v8_method signature
/// so future forward-compat fields can be plumbed through without
/// touching the security-critical app-id path.
#[inline]
pub(crate) fn resolve_consumer_app_id(
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
/// namespace value.
///
/// **P9 PR 4** — before returning, this also mints a [`DbPlatform`]
/// capability handle scoped to the same `app_id` and stashes it on the
/// `Db` object under the `ZS_PLATFORM` private symbol (§8). The handle
/// holds the platform-internal callables (`registerModel`,
/// `setMaskPolicy`, `startReplicationConsumer`, `migrations`,
/// `replication`); it is unreachable from creator JS (a `v8::Private`
/// slot is invisible to every JS reflection path and cannot be keyed
/// from JS) and is read only by Rust and the bootstrap runtime-entry
/// resolver (`globalThis.__zsDbPlatform`).
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

    // **P9 PR 4** — mint the platform capability handle and stash it on
    // the Db object under the `ZS_PLATFORM` private symbol. A failure to
    // mint the handle is non-fatal: the Db is still usable for the public
    // `collection` / `transaction` surface; `__platform` resolution
    // simply yields `undefined` and `installSchema` falls back to its
    // platform-handle-absent path (skip registerModel, used by RPC-only /
    // fetch-only apps and dev runs without a DB URL).
    if let Some(plat) = mint_db_platform(scope, app_id) {
        let priv_sym = zeroship_runtime::core::init::zs_platform_private(scope);
        // `set_private` returns `Option<bool>` (None only on context
        // teardown — impossible here, we just minted the object). The
        // private slot is the sole capability carrier; if it somehow
        // failed, `__zsDbPlatform` returns undefined and installSchema
        // takes its handle-absent path.
        let _ = obj.set_private(scope, priv_sym, plat.into());
    }

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
