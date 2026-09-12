//! V8 database operations open a cold backend through the adapter resolver.
//! Startup mask-policy installation is independent of database configuration.
//!
//! Each test calls the native JS method and settles its queued operation, so
//! the V8 wrapper and dispatcher are both exercised.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use zeroship_data_orm::binding::DbBinding;
use zeroship_runtime::state::{OpErrorKind, OpResult, ResolveValue};
use zeroship_runtime::{init_v8, RuntimeState, SharedState};

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
    crate::tests::fixtures::reset_context();
    let url = format!("sqlite:{}", dir.path().join("cold.sqlite").display());
    crate::tests::fixtures::set_database_url(&url);
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
    super::collection::mint_collection(
        scope,
        name.to_string(),
        crate::tests::fixtures::binding(app_id),
    )
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
    crate::tests::fixtures::reset_context();
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
    crate::tests::fixtures::reset_context();
}

/// Startup policy installation neither opens a backend nor touches its files.
#[test]
fn mask_policy_install_does_not_open_a_database() {
    let dir = cold_sqlite_isolate();
    cold_isolate!(let scope, let state);
    let binding = crate::tests::fixtures::binding("app_cold_policy");
    let platform = super::db_platform::mint_db_platform(scope, binding.clone()).unwrap();
    let policy = js_json(scope, r#"{ "support": ["spi"] }"#);
    call_js_method(scope, platform, "setMaskPolicy", &[policy]);
    let settled = settle_pushed_ops(&state);
    assert_eq!(settled.len(), 1);
    assert!(rejection_code(&settled[0]).is_none());
    assert!(crate::context::with(|context| context.backend()).is_none());
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    zeroship_data_orm::protection::mask_policy::install_mask_policy(
        &binding,
        zeroship_data_orm::value!({ "support": ["spi"] }),
    )
    .expect("V8 installed this policy");

    let changed = js_json(scope, r#"{ "support": ["pii"] }"#);
    call_js_method(scope, platform, "setMaskPolicy", &[changed]);
    let settled = settle_pushed_ops(&state);
    assert_eq!(
        rejection_code(&settled[0]).as_deref(),
        Some("mask_policy_immutable")
    );
    let next = DbBinding::new(binding.app_id(), "next_deploy", binding.schema().clone());
    let next_platform = super::db_platform::mint_db_platform(scope, next.clone()).unwrap();
    call_js_method(scope, next_platform, "setMaskPolicy", &[changed]);
    let settled = settle_pushed_ops(&state);
    assert!(rejection_code(&settled[0]).is_none());
    zeroship_data_orm::protection::mask_policy::install_mask_policy(
        &next,
        zeroship_data_orm::value!({ "support": ["pii"] }),
    )
    .expect("V8 installed this policy");
    zeroship_data_orm::protection::mask_policy::install_mask_policy(
        &binding,
        zeroship_data_orm::value!({ "support": ["spi"] }),
    )
    .expect("V8 installed this policy");
    assert!(
        zeroship_data_orm::protection::mask_policy::install_mask_policy(
            &binding,
            zeroship_data_orm::value!({ "support": ["pii"] })
        )
        .is_err(),
        "the pinned policy remains immutable"
    );
    assert!(crate::context::with(|context| context.backend()).is_none());
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    crate::tests::fixtures::reset_context();
}

/// `MaskedValue::dispatch_unmask_single`, entered at `mv.unmask()`.
#[test]
fn a_masked_value_unmask_opens_the_cold_isolates_backend() {
    let _dir = cold_sqlite_isolate();
    cold_isolate!(let scope, let state);

    let masked = super::masked_value::mint_masked_value(
        scope,
        crate::tests::fixtures::binding("app_cold_mv"),
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
    crate::tests::fixtures::reset_context();
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
        crate::tests::fixtures::binding("app_cold_mv_multi"),
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
    crate::tests::fixtures::reset_context();
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
    crate::tests::fixtures::install_cold_schema(
        app_id,
        "users",
        zeroship_data_orm::value!({
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
    let opts = js_json(scope, r#"{ "unmask": ["ssn"] }"#);
    call_js_method(scope, collection, "find", &[filter, opts]);

    assert_the_dispatch_opened_the_backend("dispatch_find", &state);
    crate::tests::fixtures::reset_context();
}

#[test]
fn collection_dispatch_uses_the_injected_orm_factory() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use zeroship_data_orm::{
        backend::BackendHandle,
        connection::{BackendFactory, ConnectionFactory},
        encryption::ProjectKeySource,
        error::DbError,
    };
    struct HostFactory(Arc<AtomicUsize>);
    impl BackendFactory for HostFactory {
        fn sql_registration(&self) -> zeroship_data_orm::sql::registration::SqlRegistration {
            zeroship_data_orm::sql::registration::SqlRegistration::sqlite()
        }
        fn connect(
            &self,
            _: ProjectKeySource,
        ) -> futures::future::LocalBoxFuture<'_, Result<BackendHandle, DbError>> {
            Box::pin(async {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(DbError::config(
                    "host_factory_called",
                    "host refused the fixture connection",
                ))
            })
        }
    }
    crate::tests::fixtures::reset_context();
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = ConnectionFactory::new("adapter_injection", HostFactory(calls.clone()));
    let service = crate::service::DbService::new(crate::service::DbServiceConfig {
        project_keys: crate::tests::fixtures::project_keys(),
        connection: factory.clone(),
        cdc_relay: None,
        meter: None,
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    crate::context::with_mut(|context| context.install_connection(service.connection().clone()));
    cold_isolate!(let scope, let state);
    crate::tests::fixtures::install_cold_schema(
        "app_injected",
        "items",
        zeroship_data_orm::value!({
            "id": { "type": "id" }, "name": { "type": "string" }
        }),
    );
    let collection = cold_collection(scope, "app_injected", "items");
    let filter = js_json(scope, "{}");
    call_js_method(scope, collection, "find", &[filter]);
    let values = settle_pushed_ops(&state);
    assert_eq!(values.len(), 1);
    assert_eq!(
        rejection_code(&values[0]).as_deref(),
        Some("host_factory_called")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    crate::tests::fixtures::reset_context();
}
