//! Preserve ORM transaction scope through V8 promise continuations.
//!
//! The shared continuation-preserved Map follows AsyncLocalStorage's
//! clone-before-write convention. Our symbol holds the app, session generation
//! and savepoint frame. Promise reactions retain that identity after settlement;
//! the ORM then refuses them instead of borrowing a replacement transaction.
//! Unrelated async branches retain their original Map and route to the pool.

use zeroship_data_orm::error::DbError;
use zeroship_data_orm::transaction::scope::TransactionScope;

const SCOPE_SYMBOL_KEY: &str = "zeroship.plugin-db.txScope";

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
    let generation = v8::Local::<v8::BigInt>::try_from(value.get_index(scope, 1)?).ok()?;
    let frame = v8::Local::<v8::BigInt>::try_from(value.get_index(scope, 2)?).ok()?;
    Some(TransactionScope::observed(
        app,
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
    let app = v8::String::new(scope, transaction.app_id())?;
    let generation = v8::BigInt::new_from_u64(scope, transaction.generation());
    let frame = v8::BigInt::new_from_u64(scope, transaction.frame());
    let value = v8::Array::new_with_elements(scope, &[app.into(), generation.into(), frame.into()]);
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

/// Which SQL dialect this thread's statements must be written in.
///
/// **The one place the dialect question is asked**, and it is here because the
/// answer is ADAPTER state: [`crate::context::ThreadDbContext::sql_dialect`]
/// owns both inputs (an open backend, else the service's selection). The engine
/// used to ask it directly from `crud::current_sql_dialect`, which was the last
/// ENGINE-to-ADAPTER edge on `tests/lib/tier_direction_census.sh`.
///
/// It answers WITHOUT an open backend, which is what makes the stamp below
/// possible at all - the eager `plan_*` half runs before anything is opened.
/// Pinned by `tx_route`'s
/// `a_configured_sqlite_dialect_is_captured_without_an_open_backend`.
pub(crate) fn configured_dialect() -> crate::compile::SqlDialect {
    crate::context::with(|c| c.sql_dialect())
}

/// Relay configuration resolved from this isolate's database service.
pub(crate) fn cdc_relay() -> Option<zeroship_data_orm::cdc::relay::RelayConfig> {
    crate::context::with(|c| c.cdc_relay())
}

/// Read the transaction frame out of V8 and freeze a [`TxRoute`] from it.
///
/// This is the whole of the V8 half of routing, and it lives here because this
/// module is where the context map is. [`TxRoute::capture`] still owns the
/// comparison that decides the route - it takes the observation, not the scope -
/// so the SEC-1 property is stated once, in the type that carries it, and
/// `tx_route.rs` names no `v8::` type.
///
/// Call this from a dispatch prologue while `scope` is live. The answer is only
/// correct at the dispatch boundary: the runtime's continuation slot rotates on
/// the next pump turn.
///
/// **WHY A CACHED DIALECT CANNOT GO STALE.** The dialect is stamped once, here,
/// and every statement of this dispatch - the ones the sync prelude plans and
/// the ones the async body builds after `bind_route` - is written in it. That is
/// sound because the only thing that changes a thread's configured dialect is
/// `ThreadDbContext::set_resource`, which the service calls at REQUEST
/// ADMISSION, never inside a dispatch; capture and bind are both inside one
/// dispatch. Re-reading it in the async body would be the weaker choice, not the
/// safer one: it could answer a different dialect than the prelude planned
/// against, which is precisely the split this stamp closes.
///
/// **It takes the binding, not an app id, because the route carries BOTH
/// identities.** The tenant decides the transaction frame, the lane key, the
/// SQLite ATTACH alias and the metering subject; the schema decides how tables
/// are qualified and which PostgreSQL role the session narrows to. The binding
/// is the one place both were resolved together.
pub(crate) fn capture_route(
    scope: &mut v8::PinScope<'_, '_>,
    binding: &zeroship_data_orm::binding::DbBinding,
) -> crate::tx_route::CapturedRoute {
    crate::tx_route::CapturedRoute::capture(
        current_tx_scope(scope).as_ref(),
        binding.app_id(),
        binding.schema().clone(),
        configured_dialect(),
    )
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
    Ok(captured.bind(ensure_backend().await?))
}

#[cfg(test)]
mod tests {
    //! ## The capture arms
    //!
    //! ESTABLISHED: `capture_route`'s answer tracks the async-scope marker, and
    //! it is NOT the ambient `has_tx_for` answer - `capture` says "in the
    //! transaction" at a moment when `has_tx_for` says "no transaction parked",
    //! so the two discriminators are provably different functions. Reverting
    //! `CapturedRoute::capture` to the pre-fix
    //! `tx_lanes::with(|l| l.has_tx_for(app_id))` fails three of the four.
    //!
    //! NOT ESTABLISHED, stated rather than implied:
    //!   - that every `dispatch_*` actually calls `capture`. Nothing at runtime
    //!     can check that; it is enforced by the TYPE (the exec entry points
    //!     take `&TxRoute`, and `TxRoute` has no other production constructor)
    //!     and end to end by `examples/db-todos/tests/database.test.ts` (`cxPlain`).
    //!   - the OTHER direction of the #254 defect - an app with a transaction
    //!     genuinely PARKED in the per-isolate slot while an unrelated dispatch
    //!     runs. Reaching that state needs a real `Session` (a live
    //!     Postgres `Client` or SQLite session handle), which these tests
    //!     deliberately do not open. It is covered by `cxPlain` on both tiers.
    //!   - anything about which CONNECTION the exec path then picks.
    //!
    //! **These five arms lived in the engine's `tx_route.rs`** and moved here
    //! with the data-engine cut: every name they drive except `tx_lanes` is this
    //! crate's, and the engine may not see `v8`, `zeroship_runtime` or the
    //! per-isolate context at all.

    use zeroship_runtime::init_v8;

    macro_rules! in_scope {
        (let $scope:ident) => {
            init_v8();
            let mut isolate = v8::Isolate::new(v8::CreateParams::default());
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let $scope = &mut v8::ContextScope::new(handle_scope, context);
        };
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    /// The dialect is knowable COLD, and the capture is where that is proven.
    ///
    /// This lived in `crud/mod.rs` as
    /// `configured_sqlite_dialect_does_not_require_an_open_backend`, pinned on
    /// the engine's own `current_sql_dialect()` - the function that read the
    /// context from an ENGINE file. It is here rather than deleted because the
    /// property it pins is what permits the stamp at all: nine `plan_*`
    /// functions build SQL in the synchronous prelude, so if the dialect needed
    /// an open backend the whole design would be unavailable.
    /// The fixture binding: app id and schema are the same string here, which
    /// is what production still mints. Spelled once so the tests below read the
    /// route`s two identities off ONE source, as `mint_db` does.
    fn app_a_binding() -> zeroship_data_orm::binding::DbBinding {
        zeroship_data_orm::binding::DbBinding::new(
            "app_a",
            zeroship_data_orm::binding::COLD_START_DEPLOY_TOKEN,
            zeroship_data_sql::SchemaName::new("app_a").expect("fixture schema name"),
        )
    }

    #[test]
    fn a_configured_sqlite_dialect_is_captured_without_an_open_backend() {
        in_scope!(let scope);
        crate::tests::fixtures::reset_context();
        crate::tests::fixtures::set_database_url("sqlite:route-test.sqlite");
        assert!(
            crate::context::with(|context| context.backend()).is_none(),
            "precondition: nothing has opened a backend on this thread"
        );
        assert_eq!(
            super::capture_route(scope, &app_a_binding()).dialect(),
            crate::compile::SqlDialect::Sqlite
        );
        crate::tests::fixtures::reset_context();
    }

    #[test]
    fn top_level_dispatch_routes_to_the_pool() {
        in_scope!(let scope);
        let route = super::capture_route(scope, &app_a_binding());
        assert!(!route.in_tx(), "no transaction scope entered");
        assert_eq!(route.app_id(), "app_a");
    }

    #[test]
    fn dispatch_inside_the_callback_routes_to_the_transaction() {
        in_scope!(let scope);
        let prev = super::enter(
            scope,
            &super::TransactionScope::observed("app_a".to_owned(), 1, 1),
        );
        assert!(super::capture_route(scope, &app_a_binding()).in_tx());
        super::leave(scope, prev);
        assert!(
            !super::capture_route(scope, &app_a_binding()).in_tx(),
            "leaving the scope must stop routing to the tx"
        );
    }

    #[test]
    fn a_co_resident_apps_transaction_scope_does_not_capture_this_app() {
        in_scope!(let scope);
        let prev = super::enter(
            scope,
            &super::TransactionScope::observed("app_other".to_owned(), 2, 1),
        );
        assert!(
            !super::capture_route(scope, &app_a_binding()).in_tx(),
            "SEC-1: app_a must not join app_other's transaction"
        );
        let other = zeroship_data_orm::binding::DbBinding::new(
            "app_other",
            zeroship_data_orm::binding::COLD_START_DEPLOY_TOKEN,
            zeroship_data_sql::SchemaName::new("app_other").expect("fixture schema name"),
        );
        assert!(super::capture_route(scope, &other).in_tx());
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
            &super::TransactionScope::observed("app_a".to_owned(), 1, 1),
        );
        let ambient = zeroship_data_orm::transaction::is_active("app_a");
        let captured = super::capture_route(scope, &app_a_binding()).in_tx();
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
            assert_eq!(backend.dialect(), crate::compile::SqlDialect::Sqlite);
            assert!(crate::context::with(|context| context.backend()).is_some());
        });
        crate::tests::fixtures::reset_context();
    }
}
