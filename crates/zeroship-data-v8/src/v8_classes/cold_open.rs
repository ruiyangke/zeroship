//! The cold-open witness for the five V8 sites that resolve a backend.
//!
//! # What was unbound, and what already was not
//!
//! `tx_scope::ensure_backend`'s own cold arm has had a witness since the engine
//! cut: [`crate::tx_scope::tests::set_mask_policy_installs_through_an_adapter_opened_cold_backend`]
//! asserts an empty isolate, calls `ensure_backend` and asserts one is
//! installed. Deleting the `init_pool_async` arm fails it. Three more of the
//! same shape live in `tests/sqlite_integration.rs`
//! (`cold_*_open_comes_from_ensure_backend_not_the_fixture`).
//!
//! Every one of those calls the resolver ITSELF. None of them rules on the
//! question this module exists for: **do the five production V8 sites still
//! CHOOSE the resolver?** Swap
//! `crate::tx_scope::{ensure_backend, bind_route}` at those five lines for a
//! plain `crate::context::with(|c| c.backend())` read and all four of the
//! witnesses above stay green, because none of them is downstream of a
//! dispatcher. `installSchema` would fail `not_configured` on every fresh
//! isolate and nothing in the tree would say so.
//!
//! # The five sites
//!
//! | site | reached here through |
//! | --- | --- |
//! | `dispatch::dispatch_unmask_field` | `collection.unmaskField(pk, col)` |
//! | `dispatch::dispatch_bulk_unmask_field` | `collection.bulkUnmask(items)` |
//! | `dispatch::dispatch_set_mask_policy_field` | `__platform.setMaskPolicy(p)` |
//! | `masked_value::MaskedValue::dispatch_unmask_single` | `mv.unmask()` |
//! | `masked_value::MaskedValue::dispatch_unmask_multi` | `mv.unmask([col])` |
//!
//! A sixth arm drives `collection.find()`, the carrier of the per-query unmask
//! hint - the third family the masking tests name, whose backend is resolved by
//! `dispatch::dispatch_find` rather than by any of the five.
//!
//! Each arm enters through the JS method a creator or the bootstrap actually
//! calls, not through the `pub(crate)` dispatcher, so the `#[v8_class]` glue is
//! on the path too. That is the only reason this module is IN the crate rather
//! than an integration target: `mint_collection`, `mint_db_platform` and
//! `mint_masked_value` are `pub(crate)`, and promoting them to `pub` to let a
//! test reach them would widen the shipped surface to buy a test seam.
//!
//! # What each arm asserts, and why it is not the settled value
//!
//! The isolate starts with a configured SQLite url and NO backend. The dispatch
//! is driven, its spawned op is drained and awaited off the V8 stack, and then:
//!
//!   1. the isolate must now HAVE a backend - the dispatch opened one; and
//!   2. if the op rejected, the code must not be `not_configured` /
//!      `lazy_init_failed`, the two answers a plain context read produces on a
//!      cold isolate.
//!
//! What the op ultimately resolves to is deliberately NOT asserted. These
//! fixtures cache no descriptor and create no table, so most arms reject with a
//! schema or SQL error - which is fine, because that rejection is downstream of
//! the open and therefore proves it. Seeding a full row fixture would bind the
//! same property through more moving parts, and the engine-level behaviour it
//! would re-check is already covered by `tests/sqlite_integration.rs`.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use zeroship_data_orm::binding::DbBinding;
use zeroship_runtime::state::{OpErrorKind, OpResult, ResolveValue};
use zeroship_runtime::{RuntimeState, SharedState, init_v8};

macro_rules! cold_isolate {
    (let $scope:ident, let $state:ident) => {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let $scope = &mut v8::ContextScope::new(handle_scope, context);
        let $state: SharedState =
            Rc::new(RefCell::new(RuntimeState::new(HashMap::new(), None, None)));
        $scope.set_slot($state.clone());
    };
}

/// Leave the thread with a configured SQLite url and no open backend - the
/// state every fresh isolate is in before its first op.
///
/// Returns the `TempDir`; keep it bound for the whole test or the database file
/// is unlinked while the backend still has it open.
fn cold_sqlite_isolate() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    crate::reset_context_for_tests();
    let url = format!("sqlite:{}", dir.path().join("cold.sqlite").display());
    crate::set_db_url_for_tests(&url);
    assert!(
        crate::context::with(|c| c.backend()).is_none(),
        "precondition: the fixture must leave the isolate with no backend, or \
         the arm below rules on nothing"
    );
    dir
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime build")
        .block_on(f)
}

/// Drain the ops the dispatch queued and settle them off the V8 stack.
///
/// This is what the runtime pump does with `spawned_ops`, minus the part that
/// needs a live scope: the futures resolve to an `OpResult` without touching
/// V8, and only the pump's `resolve`/`reject` step would.
fn settle_pushed_ops(state: &SharedState) -> Vec<ResolveValue> {
    let ops: Vec<_> = state.borrow_mut().spawned_ops.drain(..).collect();
    run(async move {
        let mut settled = Vec::with_capacity(ops.len());
        for op in ops {
            match op.await {
                OpResult::JsValue { value, .. } => settled.push(value),
                _ => panic!("a db dispatch must settle through OpResult::JsValue"),
            }
        }
        settled
    })
}

/// The `e.code` a rejection carries, or `None` when the op resolved.
fn rejection_code(value: &ResolveValue) -> Option<String> {
    match value {
        ResolveValue::RejectError(err) => match &err.kind {
            OpErrorKind::CodedError { code, .. } => Some(code.clone()),
            other => Some(format!("{other:?}")),
        },
        _ => None,
    }
}

/// The whole assertion, applied identically to every arm.
///
/// Call it with the dispatch already driven and its op still queued.
fn assert_the_dispatch_opened_the_backend(site: &str, state: &SharedState) {
    let settled = settle_pushed_ops(state);
    assert_eq!(
        settled.len(),
        1,
        "{site}: the JS method must queue exactly one spawned op"
    );

    assert!(
        crate::context::with(|c| c.backend()).is_some(),
        "{site}: the dispatch left the isolate with no backend, so it never \
         called tx_scope::ensure_backend. A plain context read answers \
         not_configured on every fresh isolate, which is what installSchema \
         boots into",
    );

    if let Some(code) = rejection_code(&settled[0]) {
        assert!(
            code != "not_configured" && code != "lazy_init_failed",
            "{site}: rejected with `{code}`, the answer a cold isolate gives \
             when nothing opened a backend for it",
        );
    }
}

fn js_string<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::Value> {
    v8::String::new(scope, s).expect("string alloc").into()
}

fn js_json<'s>(scope: &mut v8::PinScope<'s, '_>, src: &str) -> v8::Local<'s, v8::Value> {
    let source = v8::String::new(scope, src).expect("json source alloc");
    v8::json::parse(scope, source).expect("json parse")
}

/// Call `receiver[name](args...)` and refuse a synchronous throw.
///
/// A throw means the arm never reached the dispatch, so it would assert on an
/// empty op queue and report the wrong defect.
fn call_js_method<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    receiver: v8::Local<'s, v8::Object>,
    name: &str,
    args: &[v8::Local<'s, v8::Value>],
) {
    let key = v8::String::new(scope, name).expect("method name alloc");
    let method_v = receiver
        .get(scope, key.into())
        .unwrap_or_else(|| panic!("`{name}` is absent from the wrapper"));
    let method: v8::Local<v8::Function> = method_v
        .try_into()
        .unwrap_or_else(|_| panic!("`{name}` is not a function"));
    assert!(
        method.call(scope, receiver.into(), args).is_some(),
        "`{name}` threw synchronously; the dispatch was never reached"
    );
}

fn cold_collection<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    name: &str,
) -> v8::Local<'s, v8::Object> {
    super::collection::mint_collection(scope, name.to_string(), DbBinding::cold_start(app_id))
        .expect("mint_collection")
}

/// `dispatch_unmask_field`, entered at `collection.unmaskField(pk, column)`.
#[test]
fn a_single_unmask_dispatch_opens_the_cold_isolates_backend() {
    let _dir = cold_sqlite_isolate();
    cold_isolate!(let scope, let state);

    let collection = cold_collection(scope, "app_cold_unmask", "users");
    let row_pk = js_string(scope, "usr_01");
    let column = js_string(scope, "ssn");
    let opts = js_json(scope, "{}");
    call_js_method(scope, collection, "unmaskField", &[row_pk, column, opts]);

    assert_the_dispatch_opened_the_backend("dispatch_unmask_field", &state);
    crate::reset_context_for_tests();
}

/// `dispatch_bulk_unmask_field`, entered at `collection.bulkUnmask(items)`.
#[test]
fn a_bulk_unmask_dispatch_opens_the_cold_isolates_backend() {
    let _dir = cold_sqlite_isolate();
    cold_isolate!(let scope, let state);

    let collection = cold_collection(scope, "app_cold_bulk", "users");
    let items = js_json(scope, r#"[{ "rowPk": "usr_01", "columns": ["ssn"] }]"#);
    let opts = js_json(scope, "{}");
    call_js_method(scope, collection, "bulkUnmask", &[items, opts]);

    assert_the_dispatch_opened_the_backend("dispatch_bulk_unmask_field", &state);
    crate::reset_context_for_tests();
}

/// `dispatch_set_mask_policy_field`, entered at `__platform.setMaskPolicy(p)`.
///
/// This is the site `tx_scope::ensure_backend`'s rustdoc calls load-bearing:
/// `installSchema` fires it at boot, typically before any other op has opened
/// the backend, so it is the one that would fail on EVERY fresh isolate.
#[test]
fn a_set_mask_policy_dispatch_opens_the_cold_isolates_backend() {
    let _dir = cold_sqlite_isolate();
    cold_isolate!(let scope, let state);

    let platform =
        super::db_platform::mint_db_platform(scope, DbBinding::cold_start("app_cold_policy"))
            .expect("mint_db_platform");
    let policy = js_json(scope, r#"{ "support": ["spi"] }"#);
    call_js_method(scope, platform, "setMaskPolicy", &[policy]);

    assert_the_dispatch_opened_the_backend("dispatch_set_mask_policy_field", &state);

    // This arm CAN rule on the outcome, and does: a policy install needs
    // nothing but a backend, so the cold dispatch has to succeed outright.
    assert!(
        crate::crud::mask_policy::cache_get("app_cold_policy")
            .is_some_and(|policy| policy.allows("support", "spi")),
        "the cold setMaskPolicy must have installed and cached the policy"
    );
    crate::reset_context_for_tests();
}

/// `MaskedValue::dispatch_unmask_single`, entered at `mv.unmask()`.
#[test]
fn a_masked_value_unmask_opens_the_cold_isolates_backend() {
    let _dir = cold_sqlite_isolate();
    cold_isolate!(let scope, let state);

    let masked = super::masked_value::mint_masked_value(
        scope,
        DbBinding::cold_start("app_cold_mv"),
        "users".to_string(),
        "usr_01".to_string(),
        "ssn".to_string(),
        "spi".to_string(),
        "***-**-6789".to_string(),
    )
    .expect("mint_masked_value");
    let opts = js_json(scope, "{}");
    call_js_method(scope, masked, "unmask", &[opts]);

    assert_the_dispatch_opened_the_backend("MaskedValue::dispatch_unmask_single", &state);
    crate::reset_context_for_tests();
}

/// `MaskedValue::dispatch_unmask_multi`, entered at `mv.unmask([column])`.
///
/// The array in `arg0` is what selects the multi-column arm; the single-column
/// arm above passes an object.
#[test]
fn a_masked_value_multi_column_unmask_opens_the_cold_isolates_backend() {
    let _dir = cold_sqlite_isolate();
    cold_isolate!(let scope, let state);

    let masked = super::masked_value::mint_masked_value(
        scope,
        DbBinding::cold_start("app_cold_mv_multi"),
        "users".to_string(),
        "usr_01".to_string(),
        "ssn".to_string(),
        "spi".to_string(),
        "***-**-6789".to_string(),
    )
    .expect("mint_masked_value");
    let columns = js_json(scope, r#"["ssn"]"#);
    let opts = js_json(scope, "{}");
    call_js_method(scope, masked, "unmask", &[columns, opts]);

    assert_the_dispatch_opened_the_backend("MaskedValue::dispatch_unmask_multi", &state);
    crate::reset_context_for_tests();
}

/// `dispatch_find`, the carrier of the per-query unmask hint.
///
/// The hint is an option on `find`, so its backend comes from the find
/// dispatch rather than from any of the five sites above - which is why the
/// masking suite's third cold family needs its own arm here.
#[test]
fn a_query_hint_carrying_find_opens_the_cold_isolates_backend() {
    let _dir = cold_sqlite_isolate();
    let app_id = "app_cold_qhint";
    crate::cache_schema_for_tests(
        app_id,
        "users",
        zeroship_data_sql::value!({
            "id":  { "type": "string" },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" },
            },
        }),
    );
    cold_isolate!(let scope, let state);

    let collection = cold_collection(scope, app_id, "users");
    let filter = js_json(scope, "{}");
    let opts = js_json(scope, r#"{ "unmask": { "columns": ["ssn"] } }"#);
    call_js_method(scope, collection, "find", &[filter, opts]);

    assert_the_dispatch_opened_the_backend("dispatch_find", &state);
    crate::reset_context_for_tests();
}
