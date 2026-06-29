//! Stage 2 — Db + Collection v8_class tests.
//!
//! Verifies the runtime-side wiring of `env.db` as a `Db` v8_class
//! instance and the `Collection` v8_class returned by
//! `Db.collection(name)`. None of these tests touch Postgres — they
//! exercise the V8 surface in isolation:
//!
//! 1. `db_is_v8_class_instance` — the wrapper returned by `mint_db` is
//!    a `Db` v8_class instance (verified via prototype chain).
//! 2. `db_collection_caches_by_name` — `db.collection("users")`
//!    returns the same JS object on repeated calls.
//! 3. `collection_brand_check_rejects_non_collection` — calling a
//!    `Collection` method with a non-Collection receiver throws a
//!    TypeError ("Illegal invocation") via the macro's brand check.
//! 4. `db_brand_check_rejects_non_db` — symmetric check on the Db
//!    class's `collection()` method.

#![allow(unsafe_code)]

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use zeroship_plugin_db::v8_classes::collection::Collection;
use zeroship_plugin_db::v8_classes::db::{mint_db, Db};
use zeroship_runtime::{init_v8, RuntimeState, SharedState};

fn install_runtime_state(scope: &mut v8::PinScope<'_, '_>) {
    let state: SharedState = Rc::new(RefCell::new(RuntimeState::new(HashMap::new(), None, None)));
    scope.set_slot(state);
}

#[test]
fn db_is_v8_class_instance() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");

    // Walk the prototype chain: db.__proto__ must equal Db.prototype.
    let class_tmpl = Db::install(scope);
    let class_fn = class_tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let class_proto = class_fn.get(scope, proto_key.into()).unwrap();

    let db_proto = db.get_prototype(scope).unwrap();
    assert!(
        db_proto.strict_equals(class_proto),
        "env.db should have Db.prototype as its [[Prototype]] — it's not a Db v8_class instance"
    );

    // Sanity: the wrapper has internal field 0 set to an External
    // (the boxed Db state). Without it the brand check would reject
    // any method call.
    let ext = db.get_internal_field(scope, 0).expect("missing internal field 0");
    let _: v8::Local<v8::External> = ext
        .try_into()
        .expect("internal field 0 is not an External — Db state never installed");
}

#[test]
fn db_collection_caches_by_name() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    let collection_key = v8::String::new(scope, "collection").unwrap();
    let collection_fn_v = db.get(scope, collection_key.into()).expect("collection prop");
    let collection_fn: v8::Local<v8::Function> = collection_fn_v
        .try_into()
        .expect("collection is not a function");

    let users_name = v8::String::new(scope, "users").unwrap();
    let c1 = collection_fn
        .call(scope, db.into(), &[users_name.into()])
        .expect("collection(\"users\") returned None");
    let c2 = collection_fn
        .call(scope, db.into(), &[users_name.into()])
        .expect("collection(\"users\") second call returned None");

    assert!(
        c1.strict_equals(c2),
        "db.collection(\"users\") must return the same JS object on repeated calls (identity caching)"
    );

    let posts_name = v8::String::new(scope, "posts").unwrap();
    let p1 = collection_fn
        .call(scope, db.into(), &[posts_name.into()])
        .expect("collection(\"posts\")");

    assert!(
        !c1.strict_equals(p1),
        "different names must produce different Collection instances"
    );

    // Verify Collection is also a v8_class instance.
    let p1_obj: v8::Local<v8::Object> = p1.try_into().expect("collection result is not an Object");
    let coll_class_tmpl = Collection::install(scope);
    let coll_class_fn = coll_class_tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let coll_proto = coll_class_fn.get(scope, proto_key.into()).unwrap();
    let p1_proto = p1_obj.get_prototype(scope).unwrap();
    assert!(
        p1_proto.strict_equals(coll_proto),
        "Collection instance does not have Collection.prototype as [[Prototype]]"
    );
}

#[test]
fn collection_brand_check_rejects_non_collection() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    // Install Collection to materialise the prototype + brand-check
    // function. We then yank the `find` method off the prototype and
    // try to call it with a plain {} receiver.
    let coll_class_tmpl = Collection::install(scope);
    let coll_class_fn = coll_class_tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let coll_proto_v = coll_class_fn.get(scope, proto_key.into()).unwrap();
    let coll_proto: v8::Local<v8::Object> = coll_proto_v.try_into().unwrap();

    let find_key = v8::String::new(scope, "find").unwrap();
    let find_v = coll_proto.get(scope, find_key.into()).unwrap();
    let find_fn: v8::Local<v8::Function> = find_v.try_into().expect("find is not a function");

    // Call with a plain {} receiver — should throw "Illegal invocation".
    let plain = v8::Object::new(scope);
    let exc_str = {
        v8::tc_scope!(let tc, scope);
        let result = find_fn.call(tc, plain.into(), &[]);
        assert!(result.is_none(), "expected the call to throw");
        assert!(tc.has_caught(), "expected a pending exception");
        let exc = tc.exception().unwrap();
        exc.to_rust_string_lossy(tc)
    };
    assert!(
        exc_str.contains("Illegal invocation"),
        "expected 'Illegal invocation' TypeError, got: {exc_str}"
    );
}

#[test]
fn db_brand_check_rejects_non_db() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    // Yank `collection` off Db.prototype and try to call it with a
    // plain {} receiver.
    let db_class_tmpl = Db::install(scope);
    let db_class_fn = db_class_tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let db_proto_v = db_class_fn.get(scope, proto_key.into()).unwrap();
    let db_proto: v8::Local<v8::Object> = db_proto_v.try_into().unwrap();

    let collection_key = v8::String::new(scope, "collection").unwrap();
    let collection_v = db_proto.get(scope, collection_key.into()).unwrap();
    let collection_fn: v8::Local<v8::Function> = collection_v
        .try_into()
        .expect("collection is not a function");

    let plain = v8::Object::new(scope);
    let users = v8::String::new(scope, "users").unwrap();
    let exc_str = {
        v8::tc_scope!(let tc, scope);
        let result = collection_fn.call(tc, plain.into(), &[users.into()]);
        assert!(result.is_none(), "expected the call to throw");
        assert!(tc.has_caught(), "expected a pending exception");
        let exc = tc.exception().unwrap();
        exc.to_rust_string_lossy(tc)
    };
    assert!(
        exc_str.contains("Illegal invocation"),
        "expected 'Illegal invocation' TypeError, got: {exc_str}"
    );
}


// ---------------------------------------------------------------------------
// P9 PR 3 — native Db.transaction surface
// ---------------------------------------------------------------------------

/// `db.transaction` is a native method (function) on the `Db` v8 surface.
/// (Previously the tx entry point on `Db` was `beginTransaction`; the
/// creator-facing `transaction` was a bootstrap-installed JS function.
/// P9 PR 3 makes `transaction` the native method.)
#[test]
fn db_transaction_is_a_native_method() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    let key = v8::String::new(scope, "transaction").unwrap();
    let v = db.get(scope, key.into()).expect("transaction prop");
    assert!(
        v.is_function(),
        "env.db.transaction must be a native function (the orchestrator entry point)"
    );
}

/// `db.beginTransaction` is GONE — not on the instance, not up the
/// prototype chain. The native primitive was deleted entirely in P9 PR 3
/// (not hidden behind `__platform`).
#[test]
fn db_begin_transaction_is_not_exposed() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    let key = v8::String::new(scope, "beginTransaction").unwrap();
    // `get` walks the whole prototype chain, so this also proves it isn't
    // on Db.prototype.
    let v = db.get(scope, key.into()).expect("get beginTransaction");
    assert!(
        v.is_undefined(),
        "env.db.beginTransaction must be undefined — the native primitive was deleted in P9 PR 3"
    );
}

// ---------------------------------------------------------------------------
// P9 PR 4 — `__platform` capability handle fence tests (§8)
// ---------------------------------------------------------------------------
//
// The platform-internal callables (`registerModel`, `setMaskPolicy`,
// `startReplicationConsumer`, `migrations`, `replication`) moved off the
// `Db` v8_class to a `DbPlatform` handle stashed under a V8 private
// symbol. These tests prove the fence holds: creator JS cannot reach the
// handle through any reflection path, the moved names are gone from
// `env.db`, the kept public methods stay, and string `__platform` access
// is actively refused.

/// Helper: read the `ZS_PLATFORM` private symbol the runtime mints, the
/// same way `mint_db` and the runtime resolver derive it (interned by
/// name). The fence tests use it to assert the handle IS in the private
/// slot (bootstrap-reachable) while being invisible to JS reflection.
fn platform_private<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Private> {
    zeroship_runtime::core::init::zs_platform_private(scope)
}

/// Run `expr` (a JS expression with `db` bound to the minted Db) and
/// return the result as a Rust string via `String(result)`. Used to call
/// `Object.getOwnPropertyNames` / `Reflect.ownKeys` / `JSON.stringify`
/// over the Db object and assert on the textual result.
fn eval_with_db(scope: &mut v8::PinScope, db: v8::Local<v8::Object>, src: &str) -> String {
    // Bind `db` on the global so the compiled script can see it.
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "db").unwrap();
    global.set(scope, key.into(), db.into());
    let code = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, code, None).expect("compile");
    let result = script.run(scope).expect("run");
    result.to_rust_string_lossy(scope)
}

/// The five platform-internal members are GONE from `env.db` — not on
/// the instance, not up the prototype chain (`get` walks the chain).
#[test]
fn db_platform_internals_removed_from_env_db() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    for name in [
        "registerModel",
        "setMaskPolicy",
        "startReplicationConsumer",
        "migrations",
        "replication",
    ] {
        let key = v8::String::new(scope, name).unwrap();
        let v = db.get(scope, key.into()).expect("get");
        assert!(
            v.is_undefined(),
            "env.db.{name} must be undefined — it moved to the __platform handle in P9 PR 4"
        );
    }
}

/// The public surface stays: `collection` and `transaction` are still
/// callable native methods on `env.db`.
#[test]
fn db_public_surface_retained() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    for name in ["collection", "transaction"] {
        let key = v8::String::new(scope, name).unwrap();
        let v = db.get(scope, key.into()).expect("get");
        assert!(
            v.is_function(),
            "env.db.{name} must stay a public native method after P9 PR 4"
        );
    }
}

/// `env.db.__platform` (string access) is actively REFUSED — the getter
/// trap throws a `platform_internal_only` error (not `undefined`, not the
/// real handle).
#[test]
fn db_platform_string_access_is_denied() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    let key = v8::String::new(scope, "__platform").unwrap();

    let (exc_msg, exc_code) = {
        v8::tc_scope!(let tc, scope);
        let result = db.get(tc, key.into());
        // The getter throws — `get` returns None with a pending exception.
        assert!(result.is_none(), "env.db.__platform must throw, not return a value");
        assert!(tc.has_caught(), "expected a pending exception from the __platform trap");
        let exc = tc.exception().unwrap();
        let msg = exc.to_rust_string_lossy(tc);
        // The thrown Error carries `.code === "platform_internal_only"`.
        let code = exc
            .to_object(tc)
            .and_then(|o| {
                let code_key = v8::String::new(tc, "code").unwrap();
                o.get(tc, code_key.into())
            })
            .map(|c| c.to_rust_string_lossy(tc))
            .unwrap_or_default();
        (msg, code)
    };
    assert_eq!(
        exc_code, "platform_internal_only",
        "the __platform trap must stamp code=platform_internal_only; msg={exc_msg}"
    );
}

/// `env.db["__platform"]` (computed string access) — same refusal as
/// dotted access. Creator code can't slip past the trap with bracket
/// notation.
#[test]
fn db_platform_computed_string_access_is_denied() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    let threw = {
        v8::tc_scope!(let tc, scope);
        let r = eval_try(tc, db, r#"db["__platform"]"#);
        r.is_err()
    };
    assert!(threw, "env.db[\"__platform\"] must throw (the getter trap)");
}

/// Reflection cannot surface the platform handle: neither the string
/// `__platform` nor any symbol key appears in `getOwnPropertyNames`,
/// `getOwnPropertySymbols`, or `Reflect.ownKeys` of `env.db`. The handle
/// lives in a `v8::Private` slot — invisible to all JS reflection.
#[test]
fn db_platform_invisible_to_reflection() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");

    // Own string property names — must NOT contain "__platform".
    let names = eval_with_db(
        scope,
        db,
        "JSON.stringify(Object.getOwnPropertyNames(db))",
    );
    assert!(
        !names.contains("__platform"),
        "Object.getOwnPropertyNames(env.db) must not surface __platform; got {names}"
    );

    // Own symbol count — the private-symbol slot is NOT a JS Symbol, so
    // it must not appear. (Db installs Symbol.toStringTag on the
    // prototype, not as an own symbol of the instance, so the instance's
    // own-symbol set is empty.)
    let sym_count = eval_with_db(
        scope,
        db,
        "String(Object.getOwnPropertySymbols(db).length)",
    );
    assert_eq!(
        sym_count, "0",
        "env.db must expose no own symbols (the platform slot is a v8::Private, not a Symbol)"
    );

    // Reflect.ownKeys — union of string + symbol own keys. No __platform.
    let own_keys = eval_with_db(
        scope,
        db,
        "Reflect.ownKeys(db).map(String).join(',')",
    );
    assert!(
        !own_keys.contains("__platform"),
        "Reflect.ownKeys(env.db) must not surface __platform; got {own_keys}"
    );

    // JSON.stringify — the platform handle must not serialize. (Db has no
    // enumerable own data props, so this is "{}" or similar; assert only
    // that __platform is absent.)
    let json = eval_with_db(scope, db, "JSON.stringify(db) || 'undefined'");
    assert!(
        !json.contains("__platform"),
        "JSON.stringify(env.db) must not surface __platform; got {json}"
    );
}

/// Bootstrap-can-reach: the `DbPlatform` handle IS present in the private
/// slot under `ZS_PLATFORM`, and exposes the moved callables
/// (`registerModel` is a function on it). This is the path the runtime's
/// `__zsDbPlatform` resolver reads (via `get_private`).
#[test]
fn db_platform_handle_reachable_via_private_symbol() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");
    let priv_sym = platform_private(scope);
    let handle = db
        .get_private(scope, priv_sym)
        .expect("get_private returned None");
    assert!(
        !handle.is_undefined() && handle.is_object(),
        "the DbPlatform handle must live in the ZS_PLATFORM private slot"
    );
    let handle_obj: v8::Local<v8::Object> = handle.try_into().unwrap();

    // The handle carries the moved platform callables.
    for name in ["registerModel", "setMaskPolicy", "startReplicationConsumer"] {
        let key = v8::String::new(scope, name).unwrap();
        let v = handle_obj.get(scope, key.into()).expect("get");
        assert!(
            v.is_function(),
            "__platform.{name} must be a function on the capability handle"
        );
    }
    // And the namespace getters resolve to objects.
    for name in ["migrations", "replication"] {
        let key = v8::String::new(scope, name).unwrap();
        let v = handle_obj.get(scope, key.into()).expect("get");
        assert!(
            v.is_object(),
            "__platform.{name} must resolve to a namespace object"
        );
    }
}

/// A creator cannot reconstruct the private symbol: a string-named or
/// `Symbol.for(...)` key with the same description does NOT read the
/// private slot. Even knowing the name `"zeroship::db::__platform#capability"`
/// is useless from JS — `v8::Private` is a distinct key space.
#[test]
fn db_platform_private_slot_unreachable_by_named_symbol() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_runtime_state(scope);

    let db = mint_db(scope, "test_app").expect("mint_db");

    // A JS `Symbol.for("...")` with the private's description reads
    // nothing — `db[Symbol.for(name)]` is undefined.
    let via_symbol_for = eval_with_db(
        scope,
        db,
        "String(db[Symbol.for('zeroship::db::__platform#capability')])",
    );
    assert_eq!(
        via_symbol_for, "undefined",
        "a Symbol.for() with the private's description must not read the private slot"
    );
}

/// Run a JS expression that may throw; returns Ok(value-as-string) or
/// Err(()) when an exception was thrown. Used by the bracket-access
/// fence test.
fn eval_try(
    tc: &mut v8::PinScope,
    db: v8::Local<v8::Object>,
    src: &str,
) -> Result<String, ()> {
    let global = tc.get_current_context().global(tc);
    let key = v8::String::new(tc, "db").unwrap();
    global.set(tc, key.into(), db.into());
    let code = v8::String::new(tc, src).unwrap();
    let Some(script) = v8::Script::compile(tc, code, None) else {
        return Err(());
    };
    match script.run(tc) {
        Some(v) => Ok(v.to_rust_string_lossy(tc)),
        None => Err(()),
    }
}
