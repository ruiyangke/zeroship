//! `Db` — the `#[v8_class]` instance backing `env.db`.
//!
//! `DbPlugin::build_instance` returns a `Db` instance from this
//! module; the `NativeRegistrar` then attaches the Db-scoped entry
//! points (`registerModel`, `beginTransaction`, `openSubscription`,
//! the `replication*` ops) on top. `Migrations` is reached via the
//! `migrations` getter on this class, not via flat callbacks.
//!
//! ## What this class adds
//!
//! - `collection(name)` — `#[v8_method]` returning a [`Collection`]
//!   v8_class instance for the given collection name. Cached by
//!   `name`: subsequent calls for the same `name` return the same
//!   `Collection` JS object (`env.db.collection("users") ===
//!   env.db.collection("users")` holds).
//!
//! Per-collection CRUD lives on the `Collection` wrapper, not here —
//! every `find` / `insert` / `update` / `delete` etc. is a
//! `#[v8_method]` on `Collection` that calls into the
//! `callbacks::dispatch_*` helpers.
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

use crate::callbacks;
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
    /// from `SharedState.env_vars["APP_ID"]`. Avoids a per-callback
    /// slot lookup on the Collection's forwarded dispatch.
    pub(crate) app_id: RefCell<String>,
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
            .field("app_id", &self.app_id.borrow())
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
        // Fast path: existing entry in the cache.
        if let Some(existing) = self.collection_cache.borrow().get(&name) {
            return Ok(v8::Local::new(scope, existing));
        }

        // Slow path: mint a new Collection and stash a Global in the
        // cache so the next call hits the fast path.
        let app_id = self.app_id.borrow().clone();
        let obj = mint_collection(scope, name.clone(), app_id)?;
        let global = v8::Global::new(scope, obj);
        self.collection_cache
            .borrow_mut()
            .insert(name, global);
        Ok(obj)
    }

    /// `db.registerModel(collection, schema)` — DDL orchestrator
    /// entry. Idempotent: returns a resolved promise on second call
    /// for the same (app_id, collection).
    #[v8_method]
    #[v8_name = "registerModel"]
    fn register_model<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        collection: String,
        schema: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if collection.is_empty() {
            return Err(OpError::type_error(
                "db.registerModel: collection must be a non-empty string",
            ));
        }
        let schema_v = callbacks::read_json_arg(scope, Some(schema));
        let app_id = self.app_id.borrow().clone();
        Ok(callbacks::register_model_dispatch(scope, &app_id, &collection, schema_v).into())
    }

    /// `db.beginTransaction(opts?)` — open a transaction.
    ///
    /// `opts` is `{ isolationLevel?: "readCommitted" | "repeatableRead"
    /// | "serializable" }`. Returns a [`super::transaction::Transaction`]
    /// wrapper whose `.commit()` / `.rollback()` are explicit; Drop
    /// auto-rollbacks on GC.
    #[v8_method]
    #[v8_name = "beginTransaction"]
    fn begin_transaction<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let isolation = if opts.is_null_or_undefined() {
            None
        } else {
            let parsed = callbacks::v8_value_to_serde_json(scope, opts);
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
        Ok(callbacks::begin_transaction_dispatch(scope, isolation).into())
    }

    /// `db.openSubscription(collection)` — mint a [`super::subscription::Subscription`]
    /// wrapper for the given collection name.
    #[v8_method]
    #[v8_name = "openSubscription"]
    fn open_subscription<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        collection: String,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if collection.is_empty() {
            return Err(OpError::type_error(
                "db.openSubscription: collection must be a non-empty string",
            ));
        }
        let app_id = self.app_id.borrow().clone();
        let obj = super::subscription::mint_subscription(scope, &app_id, &collection)?;
        Ok(obj.into())
    }

    /// `db.startReplicationConsumer(opts?)` — provisions the per-app
    /// publication + slot, then spawns the supervised WAL consumer.
    /// Idempotent.
    #[v8_method]
    #[v8_name = "startReplicationConsumer"]
    fn start_replication_consumer<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let app_id_override = if opts.is_null_or_undefined() {
            None
        } else if opts.is_string() {
            Some(opts.to_rust_string_lossy(scope))
        } else {
            None
        };
        let app_id = app_id_override.unwrap_or_else(|| self.app_id.borrow().clone());
        callbacks::start_replication_consumer_dispatch(scope, app_id).into()
    }

    /// `db.replication` — returns the [`super::replication::Replication`]
    /// namespace wrapper exposing the operator-facing `setup` /
    /// `watchdog` / `dropAbandoned` ops. Cached on first access.
    #[v8_getter]
    fn replication<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if let Some(existing) = self.replication_obj.borrow().as_ref() {
            return Ok(v8::Local::new(scope, existing));
        }
        let app_id = self.app_id.borrow().clone();
        let obj = super::replication::mint_replication(scope, &app_id)?;
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
        let app_id = self.app_id.borrow().clone();
        let obj = super::migrations::mint_migrations(scope, &app_id)?;
        let global = v8::Global::new(scope, obj);
        *self.migrations_obj.borrow_mut() = Some(global);
        Ok(obj)
    }
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
                "db.beginTransaction: unknown isolationLevel '{raw}' \
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
/// points (registerModel, beginTransaction, openSubscription, …) on
/// top via the `NativeRegistrar` returned by `DbPlugin::register`.
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
        app_id: RefCell::new(app_id.to_string()),
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
