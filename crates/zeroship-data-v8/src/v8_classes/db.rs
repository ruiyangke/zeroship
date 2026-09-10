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
//! - `transaction(fn, opts?)` — the native transaction orchestrator.
//!
//! Per-collection CRUD lives on the `Collection` wrapper, not here —
//! every `find` / `insert` / `update` / `delete` etc. is a
//! `#[v8_method]` on `Collection` that calls into the
//! `crate::crud::dispatch_*` helpers.
//!
//! ## Platform-internal surface (behind `__platform`)
//!
//! `setMaskPolicy` and the `replication` namespace live off `env.db` on the
//! [`super::db_platform::DbPlatform`] capability handle. That handle is
//! set on this `Db` object under the `ZS_PLATFORM` private symbol in
//! [`mint_db`] and reached only via `@zeroship/bootstrap`'s
//! runtime-entry (§8). The string `env.db.__platform` is actively
//! refused by the [`Db::platform_trap`] getter
//! (`platform_internal_only`).
//!
//! ## Why a v8_class
//!
//! The instance carries per-isolate state (its immutable [`DbBinding`] + the collection
//! cache); the brand check that ships with `#[v8_class]` gives a
//! free `instanceof`-style guard for any receiver-shape checks the
//! runtime needs.

#![allow(unsafe_code)]

use crate::op_error::ToOpError;
use std::cell::RefCell;
use std::collections::HashMap;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

use crate::v8_bridge::decode_native;
use crate::v8_classes::collection::mint_collection;
use crate::v8_classes::db_platform::mint_db_platform;
use crate::v8_classes::transaction::transaction_dispatch;
use zeroship_data_orm::binding::{COLD_START_DEPLOY_TOKEN, DbBinding};
use zeroship_data_orm::error::IsolationLevel;

// ---------------------------------------------------------------------------
// Db state
// ---------------------------------------------------------------------------

/// Owned state for the `env.db` v8_class instance.
///
/// Field 0 of the wrapper holds a `Box<Db>` (this struct). The Weak
/// finalizer registered by `mint_db` drops the Box on GC. There are
/// no native resources to release in `Drop` — `binding` is owned and
/// the `collection_cache` holds `v8::Global<v8::Object>` handles whose
/// own Weak counterparts (registered by `mint_collection`) reclaim
/// the wrapped Collection state.
pub struct Db {
    /// Immutable app-at-deploy identity captured from the active isolate when
    /// this wrapper is minted. Every Collection and async CRUD continuation
    /// receives a clone, so another isolate on this thread cannot redirect it.
    pub(crate) binding: DbBinding,
    /// Cache of `(collection_name -> Collection JS wrapper)`. Populated
    /// on the first `.collection(name)` call for each name; subsequent
    /// calls return the same Global so identity holds:
    /// `env.db.collection("users") === env.db.collection("users")`.
    pub(crate) collection_cache: RefCell<HashMap<String, v8::Global<v8::Object>>>,
    // The `migrations` / `replication` namespace caches live on
    // `DbPlatform` (they're reached via `__platform.migrations`
    // / `__platform.replication`, not `env.db.*`). The `DbPlatform`
    // instance itself is stashed on this wrapper under the `ZS_PLATFORM`
    // private symbol (set in `mint_db`), not as a struct field — its
    // own Weak finalizer reclaims it.
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("binding", &self.binding)
            .field(
                "collection_cache_len",
                &self.collection_cache.borrow().len(),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Db IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
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

    /// `db.collection(name)` — returns a [`crate::v8_classes::collection::Collection`] v8_class
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
                let obj = mint_collection(scope, v.key().clone(), self.binding.clone())?;
                v.insert(v8::Global::new(scope, obj));
                Ok(obj)
            }
        }
    }

    /// `db.transaction(asyncFn, opts?)` — run `asyncFn` inside a
    /// transaction.
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
            let parsed = decode_native(scope, opts).map_err(|e| {
                OpError::type_error(format!("db.transaction: opts could not be decoded ({e:?})"))
            })?;
            let raw = parsed
                .as_object()
                .and_then(|o| o.get("isolationLevel"))
                .and_then(zeroship_data_sql::value::Value::as_str)
                .map(str::to_string);
            match raw {
                Some(s) => Some(normalize_isolation_level(&s)?),
                None => None,
            }
        };
        Ok(transaction_dispatch(scope, user_fn, isolation, self.binding.clone()).into())
    }

    // `db.setMaskPolicy` and the
    // `db.replication` getter moved to `DbPlatform` (reached via the
    // `__platform` capability handle, not `env.db`). Their dispatch
    // pipelines (`dispatch_set_mask_policy_field`, `mint_replication`) are
    // unchanged — only the JS carrier relocated.
    //
    // `db.unmaskField` / `db.bulkUnmaskFields` moved to
    // `Collection.unmaskField` / `.bulkUnmask` + `MaskedValue.unmask`.

    /// `env.db.__platform` (string access) — **actively refused**. The
    /// real `DbPlatform` capability handle lives under the
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
            app_id = %self.binding.app_id(),
            "env.db.__platform string access denied (platform_internal_only) — \
             the platform capability handle is private-symbol-only"
        );
        Err(zeroship_data_orm::error::DbError::AccessDenied {
            code: "platform_internal_only",
        }
        .to_op_error())
    }
}

/// Cheap JS-side type label for error messages. Matches the labels
/// `typeof` would surface so users can correlate with what they
/// passed.
fn js_type_name(v: v8::Local<v8::Value>) -> &'static str {
    if v.is_string() {
        "string"
    } else if v.is_number() {
        "number"
    } else if v.is_boolean() {
        "boolean"
    } else if v.is_function() {
        "function"
    } else if v.is_array() {
        "array"
    } else if v.is_null() {
        "null"
    } else if v.is_undefined() {
        "undefined"
    } else {
        "value"
    }
}

/// Resolve a JS-supplied isolation-level identifier to the closed set.
///
/// Accepts both camelCase (`"readCommitted"`) and the literal SQL string
/// (`"read committed"`). Rejects anything else with a `TypeError`.
///
/// **This returns a variant, not a normalised string, and that is the point.**
/// It used to hand back `"READ COMMITTED"` for the engine to re-validate
/// against its own list - two lists, accepting different things (only this one
/// takes camelCase), agreeing only because this one always ran first. A caller
/// that reached the engine directly with the creator's spelling would have been
/// refused by the second list for a level the first accepts. Past this
/// function the value cannot be a typo.
fn normalize_isolation_level(raw: &str) -> Result<IsolationLevel, OpError> {
    let trimmed = raw.trim();
    let level = match trimmed {
        "readUncommitted" | "read uncommitted" | "READ UNCOMMITTED" => {
            IsolationLevel::ReadUncommitted
        }
        "readCommitted" | "read committed" | "READ COMMITTED" => IsolationLevel::ReadCommitted,
        "repeatableRead" | "repeatable read" | "REPEATABLE READ" => IsolationLevel::RepeatableRead,
        "serializable" | "SERIALIZABLE" => IsolationLevel::Serializable,
        _ => {
            // Name every level the match above accepts. This listed three
            // while the first arm admitted `readUncommitted`, so the one
            // artefact a creator reads at the moment they get it wrong omitted
            // a value that would have worked (task #245).
            //
            // Spelled camelCase here, which is NOT a statement that camelCase
            // is canonical: each level also accepts its SQL-spaced and
            // uppercase forms, and which vocabulary the platform publishes is
            // still open. Listing four instead of three is correct under any
            // of those choices.
            return Err(OpError::type_error(format!(
                "db.transaction: unknown isolationLevel '{raw}' \
                 (expected readUncommitted | readCommitted | repeatableRead | serializable)"
            )));
        }
    };
    Ok(level)
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
/// Returns `None` when the identity cannot be resolved - including when
/// `app_id` is not a legal physical schema name, which
/// [`binding_for_isolate`] refuses.
///
/// Before returning, this also mints a [`crate::v8_classes::db_platform::DbPlatform`]
/// capability handle scoped to the same `app_id` and stashes it on the
/// `Db` object under the `ZS_PLATFORM` private symbol. The handle
/// holds the platform-internal callables (`setMaskPolicy`, `migrations`,
/// `replication`); it is unreachable from creator JS (a `v8::Private`
/// slot is invisible to every JS reflection path and cannot be keyed
/// from JS) and is read only by Rust and the bootstrap runtime-entry
/// resolver (`globalThis.__zsDbPlatform`).
pub fn mint_db<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
) -> Option<v8::Local<'s, v8::Object>> {
    // No binding, no `env.db`. An app id that is not a legal schema name has no
    // schema to reach, so the namespace is refused here rather than handed back
    // as an object whose every operation fails.
    let binding = binding_for_isolate(scope, app_id)?;

    let class_tmpl = Db::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;

    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);

    let state = Db {
        binding: binding.clone(),
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

    // Mint the platform capability handle and stash it on
    // the Db object under the `ZS_PLATFORM` private symbol. A failure to
    // mint the handle is non-fatal: the Db is still usable for the public
    // `collection` / `transaction` surface; `__platform` resolution simply
    // yields `undefined`, so optional platform-only boot work is skipped.
    if let Some(plat) = mint_db_platform(scope, binding) {
        let priv_sym = zeroship_runtime::core::init::zs_platform_private(scope);
        // `set_private` returns `Option<bool>` (None only on context
        // teardown — impossible here, we just minted the object). The
        // private slot is the sole capability carrier; if it somehow
        // failed, `__zsDbPlatform` returns undefined.
        let _ = obj.set_private(scope, priv_sym, plat.into());
    }

    Some(obj)
}

/// Resolve the immutable app-at-deploy identity for the active isolate.
/// Native descriptor binding and the `Db` wrapper both call this helper so
/// cache keys cannot drift from the receivers that later read them.
///
/// **This is the DATA PLANE's one app-id-to-schema derivation.** Every other
/// data-plane consumer takes the [`zeroship_data_sql::SchemaName`] off the
/// binding instead of deriving its own.
///
/// It is NOT the only one in the tree, and changing it alone does not complete
/// the flip. Two other sites derive or assume the same equality:
///
/// - `zeroship_migrate_server::apply::apply_ir_documents` holds the migration
///   service's own derivation, and says so in its own comment.
/// - The SQLite ATTACH alias is still minted from the TENANT
///   (`attach_app_file(binding.app_id())`) while every query builder qualifies
///   with the SCHEMA. The alias occupies SQLite's schema-name position, so a
///   schema that stops being the app id has to move the alias with it or every
///   SQLite statement names an alias nothing attached.
///
/// Returns `None` when `app_id` is not a legal schema name. That refusal used
/// to be deferred: the binding was minted unconditionally and every operation
/// failed one at a time inside the query builder, so an isolate could hold a
/// live `env.db` whose every call was doomed with nothing said at mint time.
pub(crate) fn binding_for_isolate(
    scope: &mut v8::PinScope<'_, '_>,
    app_id: &str,
) -> Option<DbBinding> {
    let schema = zeroship_data_sql::SchemaName::new(app_id).ok()?;
    // The worker injects `deploy_hash` as `ZEROSHIP_DEPLOY_ID`; pinned workflow
    // runtimes carry the hash they were started on. Absent in dev/raw-JS
    // harnesses means the historical `cold_start` token.
    let state = crate::v8_bridge::runtime_state(scope);
    let deploy_token = state
        .borrow()
        .env_vars
        .get("ZEROSHIP_DEPLOY_ID")
        .cloned()
        .unwrap_or_else(|| COLD_START_DEPLOY_TOKEN.to_string());
    Some(DbBinding::new(app_id, deploy_token, schema))
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)]

    use std::collections::HashMap;

    use zeroship_data_sql::value;
    use zeroship_data_sql::value::Value;
    use zeroship_runtime::Runtime;

    use super::normalize_isolation_level;
    use crate::v8_classes::collection::Collection;
    use zeroship_data_orm::binding::DbBinding;

    fn runtime_for_deploy(app_id: &str, deploy_token: &str) -> Runtime {
        Runtime::builder()
            .env_vars(HashMap::from([
                ("APP_ID".to_string(), app_id.to_string()),
                ("ZEROSHIP_DEPLOY_ID".to_string(), deploy_token.to_string()),
            ]))
            .build()
    }

    fn mint_collection_binding(
        runtime: &Runtime,
        app_id: &str,
        collection: &str,
    ) -> v8::Global<v8::Object> {
        runtime.with_scope(|scope| {
            let db = super::mint_db(scope, app_id).expect("mint Db binding");
            let collection_key = v8::String::new(scope, "collection").unwrap();
            let collection_fn: v8::Local<v8::Function> = db
                .get(scope, collection_key.into())
                .expect("Db.collection property")
                .try_into()
                .expect("Db.collection function");
            let name = v8::String::new(scope, collection).unwrap();
            let value = collection_fn
                .call(scope, db.into(), &[name.into()])
                .expect("mint Collection binding");
            let object: v8::Local<v8::Object> =
                value.try_into().expect("Collection binding object");
            v8::Global::new(scope, object)
        })
    }

    fn resolve_collection_binding(
        runtime: &Runtime,
        collection: &v8::Global<v8::Object>,
    ) -> (String, Option<Value>) {
        runtime.with_scope(|scope| {
            let object = v8::Local::new(scope, collection);
            let external: v8::Local<v8::External> = object
                .get_internal_field(scope, 0)
                .expect("Collection internal field")
                .try_into()
                .expect("Collection native state");
            // SAFETY: `mint_collection` stores a `Box<Collection>` in internal
            // field 0 and the Global above keeps the wrapper (and therefore the
            // Box) live for this read in its owning isolate.
            let binding = unsafe { &*(external.value() as *const Collection) };
            binding.resolved_runtime_schema_for_tests()
        })
    }

    /// A worker thread can hold a deploy-pinned isolate and the current isolate
    /// of ONE app at the same time. Their schema metadata must not be shared:
    /// each `Collection` receiver has to resolve the descriptor entry its own
    /// deploy installed, and see nothing at all for a collection the other
    /// deploy declared.
    ///
    /// The property is unchanged; what carries it is not. It used to be pinned
    /// on the deploy-keyed live-metadata cache, which is deleted. It is now
    /// pinned on the descriptor store — `cache_schema` in, `collection_schema`
    /// out — which is the only schema authority the data plane has left, so
    /// this is the same isolation guarantee measured one layer closer to the
    /// reads that depend on it.
    #[test]
    fn co_resident_deploy_bindings_keep_tokens_and_schema_entries_isolated() {
        const APP: &str = "app_same";
        const COLLECTION: &str = "secrets";
        const PINNED: &str = "deploy_pinned";
        const CURRENT: &str = "deploy_current";

        // The descriptor store is PER-THREAD, so replacing this thread's
        // context is the whole isolation this fixture needs — it cannot reach
        // a concurrently-running test on another thread.
        crate::reset_context_for_tests();

        let pinned_runtime = runtime_for_deploy(APP, PINNED);
        let pinned_collection = mint_collection_binding(&pinned_runtime, APP, COLLECTION);
        let pinned_binding = DbBinding::new(
            APP,
            PINNED,
            zeroship_data_sql::SchemaName::new(APP).unwrap(),
        );
        crate::cache_schema_for_deploy_for_tests(
            &pinned_binding,
            COLLECTION,
            value!({ "marker": { "type": "string" } }),
        );
        pinned_runtime.exit_isolate();

        // Before the current deploy installs anything, its own receiver must
        // resolve NOTHING for the collection the pinned deploy declared.
        assert_eq!(
            resolve_collection_binding(&pinned_runtime, &pinned_collection),
            (
                PINNED.to_string(),
                Some(value!({ "marker": { "type": "string" } }))
            ),
            "the pinned deploy must resolve the entry it installed",
        );

        let current_runtime = runtime_for_deploy(APP, CURRENT);
        let current_collection = mint_collection_binding(&current_runtime, APP, COLLECTION);
        assert_eq!(
            resolve_collection_binding(&current_runtime, &current_collection),
            (CURRENT.to_string(), None),
            "the current deploy must not read the pinned deploy's descriptor entry",
        );

        let current_binding = DbBinding::new(
            APP,
            CURRENT,
            zeroship_data_sql::SchemaName::new(APP).unwrap(),
        );
        crate::cache_schema_for_deploy_for_tests(
            &current_binding,
            COLLECTION,
            value!({ "other": { "type": "string" } }),
        );
        current_runtime.exit_isolate();

        assert_eq!(
            resolve_collection_binding(&pinned_runtime, &pinned_collection),
            (
                PINNED.to_string(),
                Some(value!({ "marker": { "type": "string" } }))
            ),
            "installing the current deploy redirected the pinned binding",
        );
        assert_eq!(
            resolve_collection_binding(&current_runtime, &current_collection),
            (
                CURRENT.to_string(),
                Some(value!({ "other": { "type": "string" } }))
            ),
            "the current binding must retain its own deploy token and descriptor entry",
        );
    }

    /// The rejection message is the one artefact a creator reads at the moment
    /// they get the spelling wrong, so it must not omit a value the very same
    /// function accepts. It did: the match admitted `readUncommitted` while the
    /// message advertised only three levels (task #245).
    ///
    /// The four names are asserted ACCEPTED first, in this same test, so the
    /// list the message is checked against is not an independent copy that can
    /// drift from the match arms on its own.
    ///
    /// WHAT THIS DOES NOT CATCH: a fifth level added to the match without being
    /// added here. Nothing enumerates match arms at runtime, so that case needs
    /// the author to update this list. It catches the reverse -- the message
    /// falling behind a level this test already knows is accepted.
    #[test]
    fn rejection_message_names_every_accepted_level() {
        let levels = [
            "readUncommitted",
            "readCommitted",
            "repeatableRead",
            "serializable",
        ];

        for spelling in levels {
            assert!(
                normalize_isolation_level(spelling).is_ok(),
                "{spelling} is expected to be an accepted isolation level"
            );
        }

        let err = normalize_isolation_level("bogus").expect_err("bogus must be rejected");
        let msg = err.message;
        for level in levels {
            assert!(
                msg.contains(level),
                "rejection message omits the accepted level {level}: {msg}"
            );
        }
    }

    /// Minting must refuse an app id that is not a legal schema name.
    ///
    /// `binding_for_isolate` used to stamp the app id into a `DbBinding` with no
    /// validation at all, so a name `zeroship_data_sql::compile::validate_schema`
    /// rejected survived the mint. The refusal surfaced only LATER and only PER
    /// OPERATION, inside the query builder, as `QueryError::InvalidCollection`
    /// -- so an isolate could hold a live `env.db` whose every operation was
    /// doomed, and nothing said so at the moment the binding was created.
    ///
    /// That deferral is what made the app-id/schema-name conflation invisible.
    /// One string is today both the tenant identity and the physical schema
    /// name; the mint is the one place that can refuse a value which is illegal
    /// as a SCHEMA while perfectly legal as a TENANT id. Validating at the mint
    /// is what forces the two meanings apart.
    ///
    /// **THE DEFERRED REFUSAL IS NOW UNREACHABLE, WHICH IS THE POINT.** The
    /// builders take a [`zeroship_data_sql::SchemaName`], so the control below
    /// asserts the refusal where it now lives - at construction - rather than at
    /// a per-operation `build_find_with_schema` that can no longer be handed an
    /// illegal name.
    ///
    /// WHAT THIS DOES NOT CATCH: it pins the refusal, not the channel. If the
    /// mint later reports the refusal by some route other than `None`, this test
    /// needs rewriting rather than deleting.
    #[test]
    fn mint_refuses_an_app_id_that_is_not_a_legal_schema_name() {
        // A double quote is the character that makes `quote_ident`'s escaping
        // load-bearing, so it is the one whose acceptance matters most.
        const ILLEGAL: &str = "app\"; DROP SCHEMA public; --";

        // CONTROL, differing in one variable: the fixture really is a name the
        // shared validator rejects, so the arm below is not asserting against an
        // arbitrary string.
        let refused = zeroship_data_sql::SchemaName::new(ILLEGAL);
        assert!(
            matches!(
                refused,
                Err(zeroship_data_sql::compile::QueryError::InvalidCollection(_))
            ),
            "control: the fixture must be a name validate_schema rejects, got {refused:?}"
        );

        // THE PROPERTY: the mint refuses it, rather than handing back a binding
        // that only fails one operation at a time.
        crate::reset_context_for_tests();
        let runtime = runtime_for_deploy(ILLEGAL, "deploy_mint_refusal");
        let minted = runtime.with_scope(|scope| super::mint_db(scope, ILLEGAL).is_some());
        runtime.exit_isolate();

        assert!(
            !minted,
            "mint_db handed back an env.db binding for an app id that is not a legal \
             schema name; the refusal is still deferred to per-operation query building"
        );
    }
}
