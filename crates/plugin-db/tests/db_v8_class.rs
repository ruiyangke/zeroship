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

use zeroship_plugin_db::v8_classes::collection::Collection;
use zeroship_plugin_db::v8_classes::db::{mint_db, Db};
use zeroship_runtime::init_v8;

#[test]
fn db_is_v8_class_instance() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

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

    // And the `name` getter returns the expected string.
    let name_key = v8::String::new(scope, "name").unwrap();
    let name_v = p1_obj.get(scope, name_key.into()).unwrap();
    let name_str = name_v.to_rust_string_lossy(scope);
    assert_eq!(name_str, "posts");
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

