//! Preserve ORM transaction scope through V8 promise continuations.
//!
//! The shared continuation-preserved Map follows AsyncLocalStorage's
//! clone-before-write convention. Our symbol holds the app, session generation
//! and savepoint frame. Promise reactions retain that identity after settlement;
//! the ORM then refuses them instead of borrowing a replacement transaction.
//! Unrelated async branches retain their original Map and route to the pool.

use zeroship_data_orm::error::DbError;
use zeroship_data_orm::transaction::scope::TransactionScope;

const SCOPE_SYMBOL_KEY: &str = "zeroship.data-v8.txScope";

fn scope_symbol<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Symbol>> {
    let key = v8::String::new(scope, SCOPE_SYMBOL_KEY)?;
    Some(v8::Symbol::for_key(scope, key))
}

/// Read the continuation-preserved context `Map`, if one is installed.
fn read_context_map<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Map>> {
    let val = scope.get_continuation_preserved_embedder_data();
    if val.is_undefined() || val.is_null() {
        return None;
    }
    v8::Local::<v8::Map>::try_from(val).ok()
}

/// Clone a JS `Map` by walking its flattened `[k0, v0, k1, v1, …]` array.
/// Mirrors `als::clone_map`; duplicated rather than shared because that
/// helper is `pub(crate)` inside the runtime crate.
fn clone_context_map<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    src: v8::Local<v8::Map>,
) -> v8::Local<'s, v8::Map> {
    let dst = v8::Map::new(scope);
    let arr = src.as_array(scope);
    let len = arr.length();
    let mut i: u32 = 0;
    while i + 1 < len {
        let (Some(k), Some(v)) = (arr.get_index(scope, i), arr.get_index(scope, i + 1)) else {
            return dst;
        };
        dst.set(scope, k, v);
        i += 2;
    }
    dst
}

/// Decode the transaction scope inherited by this async continuation.
pub(crate) fn current_tx_scope(scope: &mut v8::PinScope<'_, '_>) -> Option<TransactionScope> {
    let map = read_context_map(scope)?;
    let sym = scope_symbol(scope)?;
    let value = map.get(scope, sym.into())?;
    let value = v8::Local::<v8::Array>::try_from(value).ok()?;
    let app = value.get_index(scope, 0)?.to_rust_string_lossy(scope);
    let database = value.get_index(scope, 1)?.to_rust_string_lossy(scope);
    let generation = v8::Local::<v8::BigInt>::try_from(value.get_index(scope, 2)?).ok()?;
    let frame = v8::Local::<v8::BigInt>::try_from(value.get_index(scope, 3)?).ok()?;
    // A database half this build cannot parse decodes to nothing rather than to
    // a platform route: widening a creator scope into one that narrows to no
    // role is the direction that must not be available.
    let route = zeroship_data_orm::binding::DbRoute::decoded(app, &database)?;
    Some(TransactionScope::observed(
        route,
        generation.u64_value().0,
        frame.u64_value().0,
    ))
}

/// Plant a captured transaction identity. Returns the previous
/// slot value, which the caller MUST hand to [`leave`] on every exit path
/// so a sibling branch is not left inside a scope it never entered.
pub(crate) fn enter(
    scope: &mut v8::PinScope<'_, '_>,
    transaction: &TransactionScope,
) -> Option<v8::Global<v8::Value>> {
    let sym = scope_symbol(scope)?;
    let app = v8::String::new(scope, transaction.route().app_id())?;
    let database = v8::String::new(scope, transaction.route().database_text())?;
    let generation = v8::BigInt::new_from_u64(scope, transaction.generation());
    let frame = v8::BigInt::new_from_u64(scope, transaction.frame());
    let value = v8::Array::new_with_elements(
        scope,
        &[app.into(), database.into(), generation.into(), frame.into()],
    );
    let prev = scope.get_continuation_preserved_embedder_data();
    let prev_global = v8::Global::new(scope, prev);

    // Clone-then-extend: see the module docs. A sibling async branch that
    // captured the current map keeps seeing the map it captured.
    let next = match read_context_map(scope) {
        Some(m) => clone_context_map(scope, m),
        None => v8::Map::new(scope),
    };
    next.set(scope, sym.into(), value.into());
    scope.set_continuation_preserved_embedder_data(next.into());
    Some(prev_global)
}

/// Restore the slot value [`enter`] displaced.
pub(crate) fn leave(scope: &mut v8::PinScope<'_, '_>, prev: Option<v8::Global<v8::Value>>) {
    let Some(prev) = prev else { return };
    let local = v8::Local::new(scope, prev);
    scope.set_continuation_preserved_embedder_data(local);
}

/// Capture this thread's compiler, codecs, and support without opening a backend.
pub(crate) fn configured_sql_registration() -> zeroship_data_orm::sql::registration::SqlRegistration
{
    crate::context::with(|c| c.sql_registration())
}

/// Relay configuration resolved from this isolate's database service.
pub(crate) fn cdc_relay() -> Option<zeroship_data_orm::cdc::relay::RelayConfig> {
    crate::context::with(|c| c.cdc_relay())
}

/// Read the transaction frame out of V8 and freeze a [`TxRoute`] from it.
///
/// This is the whole of the V8 half of routing, and it lives here because this
/// module is where the context map is. [`crate::tx_route::CapturedRoute::capture`] owns the
/// comparison that decides the route - it takes the observation, not the scope -
/// so the SEC-1 property is stated once, in the type that carries it, and
/// `tx_route.rs` names no `v8::` type.
///
/// Call this from a dispatch prologue while `scope` is live. The answer is only
/// correct at the dispatch boundary: the runtime's continuation slot rotates on
/// the next pump turn.
///
/// Compiler, codecs, and effective support are stamped together. Backend binding
/// verifies that identity after an asynchronous open.
///
/// **It takes the binding, not an app id, because the route carries BOTH
/// identities.** The tenant decides the transaction frame, the lane key, the
/// SQLite ATTACH alias and the app the usage sink attributes to; the schema
/// decides how tables are qualified and which PostgreSQL role the session
/// narrows to. The binding is the one place both were resolved together.
///
/// Every creator dispatch reaches the ORM through here, so this is where the
/// service's meter becomes the route's usage sink. A metered binding whose app
/// id cannot be attributed is refused with `invalid_meter_app_id`.
pub(crate) fn capture_route(
    scope: &mut v8::PinScope<'_, '_>,
    binding: &zeroship_data_orm::binding::DbBinding,
) -> Result<crate::tx_route::CapturedRoute, DbError> {
    let connection = crate::context::with(|context| context.connection_identity())
        .ok_or_else(|| DbError::config("not_configured", "db: no connection is installed"))?;
    let usage = crate::usage::sink_for(binding)?;
    Ok(crate::tx_route::CapturedRoute::capture(
        current_tx_scope(scope).as_ref(),
        binding,
        configured_sql_registration(),
        connection,
        usage,
    ))
}

/// Resolve the registered ORM connection and open it lazily.
/// The captured handle remains bound to its original configuration while the
/// future waits, even if another isolate replaces the thread's registration.
pub async fn ensure_backend() -> Result<crate::backend::BackendHandle, DbError> {
    let connection = crate::context::with(|context| context.connection())
        .ok_or_else(|| DbError::config("not_configured", "db: no connection is installed"))?;
    connection
        .ensure(crate::context::isolate_key_source())
        .await
}

/// Bind a captured routing decision to the backend its SQL will run on.
///
/// The one place the two halves meet. Call it from the dispatch's async body:
/// `capture_route` must run while the V8 scope is live, and this must run
/// where it can `await`.
pub(crate) async fn bind_route(
    captured: crate::tx_route::CapturedRoute,
) -> Result<crate::tx_route::TxRoute, DbError> {
    captured.bind(ensure_backend().await?)
}

#[cfg(test)]
mod tests {
    //! Route capture follows the continuation scope. Database-backed tests cover
    //! the connection selected after that captured route is bound.

    use zeroship_runtime::init_v8;

    macro_rules! in_scope {
        (let $scope:ident) => {
            init_v8();
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let $scope = &mut v8::ContextScope::new(handle_scope, context);
            crate::tests::fixtures::reset_context();
            crate::tests::fixtures::set_database_url("postgres://route-capture.invalid/db");
        };
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    /// The fixture binding. Spelled once so the tests below read the route's
    /// two identities off ONE source, as `mint_db` does.
    fn app_a_binding() -> zeroship_data_orm::binding::DbBinding {
        crate::tests::fixtures::harness_binding("app_a")
    }

    #[test]
    fn a_configured_sqlite_registration_is_captured_without_an_open_backend() {
        in_scope!(let scope);
        crate::tests::fixtures::reset_context();
        crate::tests::fixtures::set_database_url("sqlite:route-test.sqlite");
        assert!(
            crate::context::with(|context| context.backend()).is_none(),
            "precondition: nothing has opened a backend on this thread"
        );
        assert_eq!(
            super::capture_route(scope, &app_a_binding())
                .unwrap()
                .sql_registration()
                .family(),
            zeroship_data_orm::sql::registration::SQLITE_FAMILY
        );
        crate::tests::fixtures::reset_context();
    }

    #[test]
    fn top_level_dispatch_routes_to_the_pool() {
        in_scope!(let scope);
        let route = super::capture_route(scope, &app_a_binding()).unwrap();
        assert!(!route.in_tx(), "no transaction scope entered");
        assert_eq!(route.app_id(), "app_a");
    }

    #[test]
    fn dispatch_inside_the_callback_routes_to_the_transaction() {
        in_scope!(let scope);
        let prev = super::enter(
            scope,
            &super::TransactionScope::observed(
                crate::tests::fixtures::harness_route("app_a"),
                1,
                1,
            ),
        );
        assert!(
            super::capture_route(scope, &app_a_binding())
                .unwrap()
                .in_tx()
        );
        super::leave(scope, prev);
        assert!(
            !super::capture_route(scope, &app_a_binding())
                .unwrap()
                .in_tx(),
            "leaving the scope must stop routing to the tx"
        );
    }

    #[test]
    fn a_co_resident_apps_transaction_scope_does_not_capture_this_app() {
        in_scope!(let scope);
        let prev = super::enter(
            scope,
            &super::TransactionScope::observed(
                crate::tests::fixtures::harness_route("app_other"),
                2,
                1,
            ),
        );
        assert!(
            !super::capture_route(scope, &app_a_binding())
                .unwrap()
                .in_tx(),
            "SEC-1: app_a must not join app_other's transaction"
        );
        let other = crate::tests::fixtures::harness_binding("app_other");
        assert!(super::capture_route(scope, &other).unwrap().in_tx());
        super::leave(scope, prev);
    }

    /// The one-variable control that separates the two discriminators.
    ///
    /// Inside the scope, `capture` answers "transaction" while the per-isolate
    /// slot this app-id would be looked up in is EMPTY, so `has_tx_for` answers
    /// "no transaction". A `capture` implemented on the pre-fix ambient test
    /// cannot produce this pair.
    #[test]
    fn capture_is_not_the_ambient_has_tx_for_answer() {
        in_scope!(let scope);
        let prev = super::enter(
            scope,
            &super::TransactionScope::observed(
                crate::tests::fixtures::harness_route("app_a"),
                1,
                1,
            ),
        );
        let ambient = zeroship_data_orm::transaction::is_active(&crate::tests::fixtures::harness_route("app_a"));
        let captured = super::capture_route(scope, &app_a_binding())
            .unwrap()
            .in_tx();
        super::leave(scope, prev);
        assert!(!ambient, "precondition: no transaction is parked for app_a");
        assert!(
            captured,
            "capture must read the async scope, not the parked-tx slot"
        );
    }

    /// The first database operation must still initialize a cold backend.
    #[test]
    fn ensure_backend_opens_a_cold_sqlite_context() {
        crate::tests::fixtures::reset_context();
        let dir = tempfile::tempdir().expect("create tempdir");
        crate::tests::fixtures::set_database_url(&format!(
            "sqlite:{}",
            dir.path().join("cold.sqlite").display()
        ));
        assert!(crate::context::with(|context| context.backend()).is_none());
        run(async {
            let backend = super::ensure_backend().await.expect("open cold backend");
            assert_eq!(
                backend.sql_registration().family(),
                zeroship_data_orm::sql::registration::SQLITE_FAMILY
            );
            assert!(crate::context::with(|context| context.backend()).is_some());
        });
        crate::tests::fixtures::reset_context();
    }
}
