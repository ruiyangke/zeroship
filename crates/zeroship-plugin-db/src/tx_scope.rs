//! Async-scoped "am I inside a transaction callback" marker.
//!
//! ## Why this exists
//!
//! `env.db.transaction(fn)` has to decide, at call time, whether it is
//! opening a **new** transaction (`BEGIN`) or **nesting** inside one that
//! is already open (`SAVEPOINT`). Until 2026-08-10 that decision read
//! [`crate::context::ThreadDbContext::has_tx_for`] — "does this app
//! currently have a transaction open on this isolate?".
//!
//! That is a *temporal* test standing in for a *structural* one, and the
//! two come apart the moment two transactions for one app overlap in
//! time. They do overlap: a worker OS thread multiplexes many requests
//! over one isolate and hands control to another dispatch at every
//! `.await`, and `pnpm dev` is a single isolate by construction
//! (`zeroship serve --workers=1`). Measured on both tiers by
//! `tests/e2e_dev_vs_deployed_db.sh`:
//!
//! ```text
//! request A   db.transaction(async tx => { insert; await …; throw })
//! request B                     db.transaction(async tx => { insert })  // resolves "committed"
//! ```
//!
//! B read `has_tx_for == true`, opened a SAVEPOINT on **A's** connection,
//! reported success — and A's `ROLLBACK` then destroyed B's row. Two
//! unrelated end users' work, entangled, with the loser told it had
//! committed.
//!
//! ## The discriminator
//!
//! Nesting is a property of the **call's async context**, not of the
//! app's wall-clock state: a `transaction()` call nests exactly when it
//! runs inside the enclosing callback's continuation chain. V8 v147 has
//! the primitive for that — `Isolate::SetContinuationPreservedEmbedderData`,
//! the same slot `node:async_hooks`' `AsyncLocalStorage` uses (see
//! `crates/runtime/src/node/async_hooks/als.rs`). The slot holds a JS
//! `Map`, and V8 carries it across every async hop, restoring it when a
//! promise reaction runs.
//!
//! So: [`enter`] plants `app_id` under our own registry Symbol for the
//! duration of the synchronous `user_fn.call(...)` frame, every
//! continuation that branches off inside the callback inherits it, and
//! [`current_tx_app`] reads it back. A concurrent dispatch's continuations
//! branched off *before* that frame and therefore see nothing — which is
//! the whole point.
//!
//! ## Interop with `AsyncLocalStorage`
//!
//! The slot is shared with ALS, so this module obeys the same convention:
//! the value is a `v8::Map`, entries are keyed by Symbol, and [`enter`]
//! CLONES the map before adding its entry (mutating in place would leak
//! the entry into sibling async branches that captured the map by
//! reference). Our key comes from the global symbol registry rather than
//! a per-instance Symbol, because there is exactly one transaction scope
//! per isolate and it must be readable from a different call site than
//! the one that wrote it.

/// Global-registry key for the transaction-scope entry in the shared
/// continuation-preserved `Map`. Namespaced so it cannot collide with a
/// creator's own `Symbol.for(...)` key.
use zeroship_data_core::error::DbError;

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

/// The `app_id` whose `transaction()` callback the CURRENT async context
/// is executing inside, or `None` at top level.
///
/// `None` is the answer for a dispatch that merely *overlaps* another
/// app-level transaction in time, which is exactly the case the old
/// `has_tx_for` test got wrong.
pub(crate) fn current_tx_app(scope: &mut v8::PinScope<'_, '_>) -> Option<String> {
    let map = read_context_map(scope)?;
    let sym = scope_symbol(scope)?;
    let value = map.get(scope, sym.into())?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    Some(value.to_rust_string_lossy(scope))
}

/// Plant `app_id` as the current transaction scope. Returns the previous
/// slot value, which the caller MUST hand to [`leave`] on every exit path
/// so a sibling branch is not left inside a scope it never entered.
pub(crate) fn enter(
    scope: &mut v8::PinScope<'_, '_>,
    app_id: &str,
) -> Option<v8::Global<v8::Value>> {
    let sym = scope_symbol(scope)?;
    let value = v8::String::new(scope, app_id)?;
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
pub(crate) fn configured_dialect() -> crate::query::SqlDialect {
    crate::context::with(|c| c.sql_dialect())
}

/// The worker-process identity CDC slot names are built from, if this process
/// was composed with one.
///
/// Beside [`configured_dialect`] for the same reason: it is a question only the
/// per-isolate context can answer, asked by a tier that may not read it.
/// `cdc_lifecycle.rs` used to ask `crate::context` directly, which was the last
/// CDC-to-ADAPTER edge on `tests/lib/tier_direction_census.sh`.
///
/// It answers `Option`, and the refusal is deliberately NOT here. A missing
/// identity is only an error on the one path that mints a slot name, and the
/// message and hint for it belong beside that path - see
/// `cdc_lifecycle::start_on_current_isolate`. Raising it here would fail
/// `Subscription.next()` on an already-running consumer, which needs no
/// identity at all.
pub(crate) fn cdc_worker_id() -> Option<String> {
    crate::context::with(|c| c.cdc_worker_id())
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
pub(crate) fn capture_route(
    scope: &mut v8::PinScope<'_, '_>,
    app_id: &str,
) -> crate::tx_route::CapturedRoute {
    crate::tx_route::CapturedRoute::capture(
        current_tx_app(scope).as_deref(),
        app_id,
        configured_dialect(),
    )
}

/// Open this thread's backend if it is cold, and hand it back.
///
/// **This is the funnel, and it lives here because the state it reads is the
/// adapter's.** It was `exec::ensure_backend_for_shared_sql` until 2026-09-03,
/// which put an ENGINE file's hands on `crate::context` and `init_pool_async`,
/// the one edge direction the crate split forbids. Nothing about the body
/// changed; only its address did, so the engine now receives a backend instead
/// of fetching one.
///
/// **The cold-init half is load-bearing, not incidental.** Mask-policy
/// installation runs at boot, before any creator code, and is the call that
/// warms a cold isolate; deleting the `init_pool_async` arm and keeping only
/// the read would make `installSchema`'s `setMaskPolicy` fail
/// `not_configured` on every fresh isolate, which on the SQLite dev tier is
/// every boot.
///
/// **`pub`, not `pub(crate)`, and the shipped surface is unchanged ONLY
/// BECAUSE OF A CONDITION THIS DOC USED TO LEAVE UNSTATED.** The condition is
/// `lib.rs:353-356`, which declares this module as a two-arm pair:
///
/// ```text
/// #[cfg(not(feature = "test-helpers"))] pub(crate) mod tx_scope;
/// #[cfg(feature = "test-helpers")]      pub       mod tx_scope;
/// ```
///
/// A `pub fn` inside a `pub(crate) mod` has crate-only EFFECTIVE visibility,
/// so in a release build this symbol is unreachable from outside. Delete the
/// `not(...)` arm, or re-declare the module `pub` unconditionally in whatever
/// crate ends up holding it, and `ensure_backend` silently becomes public API
/// with no compile error anywhere. The old wording asserted the conclusion; the
/// point of this paragraph is that the conclusion has a premise, and the
/// premise is one line in another file.
///
/// **The crate split does not remove that cap, and a reviewer's worry that it
/// would is refuted by the proposal.** `docs/proposals/2026-08-31-data-crate-shape.md:50`
/// keeps `zeroship-plugin-db` as "THIN. The worker/runtime plugin ADAPTER
/// ONLY", and `tests/lib/tier_direction_census.sh` tiers `tx_scope.rs` ADAPTER.
/// The engine is cut OUT to `data-engine`; this file does not move, so the
/// two-arm declaration above moves with neither.
///
/// **Why there is no `ensure_backend_for_tests` wrapper**, the idiom
/// `tx_route.rs` uses for `pool_for_tests` / `tx_for_tests`: that idiom fits a
/// test-only CONSTRUCTOR, which has no production twin, so gating it genuinely
/// removes a capability from the shipped build. `ensure_backend` is the
/// opposite: eleven production call sites, counted 2026-09-03 - nine in
/// `v8_classes/` (`dispatch.rs` 5, `masked_value.rs` 2, `replication.rs` 1,
/// `transaction.rs` 1) and two in `lib.rs` - plus [`bind_route`] below. Every
/// one of those files is ADAPTER, so they stay in this crate when `data-engine`
/// is cut out, and the symbol has to remain reachable from all of them.
/// (`transaction/probe.rs` also calls it and is NOT in that count: its module
/// is declared `#[cfg(any(test, feature = "test-helpers"))]` at
/// `transaction/mod.rs:145`, so it is in no production build.) A gated wrapper
/// would therefore hide nothing that is not already hidden; it would only
/// rename five calls in
/// `tests/{integration,sqlite_integration,mask_flip}.rs`. Naming the condition
/// is the fix that does work; a wrapper would be ceremony that reads as one.
///
/// The integration targets need the `pub` arm because they drive the engine's
/// unmask entry points directly, and those take the backend as a parameter now.
/// It is the same call the V8 dispatcher makes on their behalf in production.
pub async fn ensure_backend() -> Result<crate::backend::BackendHandle, DbError> {
    if crate::context::with(|c| c.backend().is_none()) {
        crate::init_pool_async()
            .await
            .map_err(|e| DbError::config("lazy_init_failed", format!("db: lazy init failed: {e}")))?;
    }

    crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized".to_string()))
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
    //!     and end to end by `tests/e2e_dev_vs_deployed_db.sh` (`cxPlain`).
    //!   - the OTHER direction of the #254 defect - an app with a transaction
    //!     genuinely PARKED in the per-isolate slot while an unrelated dispatch
    //!     runs. Reaching that state needs a real `TxConnection` (a live
    //!     Postgres `Client` or SQLite session handle), which these tests
    //!     deliberately do not open. It is covered by `cxPlain` on both tiers.
    //!   - anything about which CONNECTION the exec path then picks.
    //!
    //! **These five arms lived in the engine's `tx_route.rs`** and moved here
    //! with the data-engine cut: every name they drive except `tx_lanes` is this
    //! crate's, and the engine may not see `v8`, `zeroship_runtime` or the
    //! per-isolate context at all.

    use serde_json::json;
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
    #[test]
    fn a_configured_sqlite_dialect_is_captured_without_an_open_backend() {
        in_scope!(let scope);
        crate::reset_context_for_tests();
        crate::set_db_url_for_tests("sqlite::memory:");
        assert!(
            crate::context::with(|context| context.backend()).is_none(),
            "precondition: nothing has opened a backend on this thread"
        );
        assert_eq!(
            super::capture_route(scope, "app_a").dialect(),
            crate::query::SqlDialect::Sqlite
        );
        crate::reset_context_for_tests();
    }

    #[test]
    fn top_level_dispatch_routes_to_the_pool() {
        in_scope!(let scope);
        let route = super::capture_route(scope, "app_a");
        assert!(!route.in_tx(), "no transaction scope entered");
        assert_eq!(route.app_id(), "app_a");
    }

    #[test]
    fn dispatch_inside_the_callback_routes_to_the_transaction() {
        in_scope!(let scope);
        let prev = super::enter(scope, "app_a");
        assert!(super::capture_route(scope, "app_a").in_tx());
        super::leave(scope, prev);
        assert!(
            !super::capture_route(scope, "app_a").in_tx(),
            "leaving the scope must stop routing to the tx"
        );
    }

    #[test]
    fn a_co_resident_apps_transaction_scope_does_not_capture_this_app() {
        in_scope!(let scope);
        let prev = super::enter(scope, "app_other");
        assert!(
            !super::capture_route(scope, "app_a").in_tx(),
            "SEC-1: app_a must not join app_other's transaction"
        );
        assert!(super::capture_route(scope, "app_other").in_tx());
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
        let prev = super::enter(scope, "app_a");
        let ambient = crate::tx_lanes::with(|l| l.has_tx_for("app_a"));
        let captured = super::capture_route(scope, "app_a").in_tx();
        super::leave(scope, prev);
        assert!(!ambient, "precondition: no transaction is parked for app_a");
        assert!(
            captured,
            "capture must read the async scope, not the parked-tx slot"
        );
    }

    /// The cold-isolate path, driven in the order production drives it.
    ///
    /// **This arm lived in the engine's `crud/mask_policy.rs` and moved here
    /// with the data-engine cut**, because three of the four things it names
    /// belong to this crate: `set_db_url_for_tests`, `context::with` and
    /// [`ensure_backend`]. Only its last line is the engine's. That split is the
    /// point - it witnesses the boot sequence at the seam between the two tiers.
    ///
    /// The assertion that matters is unchanged in substance: if the resolve step
    /// ever loses its `init_pool_async` arm, `ensure_backend` returns
    /// `not_configured` here and the install never happens - the failure
    /// `installSchema` would hit on every boot of the SQLite dev tier.
    #[test]
    fn set_mask_policy_installs_through_an_adapter_opened_cold_backend() {
        crate::reset_context_for_tests();
        let dir = tempfile::tempdir().expect("create tempdir");
        let url = format!("sqlite:{}", dir.path().join("cold.sqlite").display());
        crate::set_db_url_for_tests(&url);
        assert!(crate::context::with(|context| context.backend()).is_none());

        run(async {
            // The adapter half of the dispatcher: resolve, warming the cold
            // isolate on the way.
            let backend = super::ensure_backend()
                .await
                .expect("the adapter funnel must open a cold backend");
            assert!(
                backend.as_sqlite().is_some(),
                "the configured url is sqlite:, so the opened backend must be too"
            );
            assert!(
                crate::context::with(|context| context.backend()).is_some(),
                "resolving must leave the backend installed on this isolate"
            );

            // The engine half: it receives the handle rather than fetching one.
            crate::crud::mask_policy::dispatch_set_mask_policy(
                &backend,
                "app_cold_policy",
                json!({ "support": ["spi"] }),
            )
            .await
            .expect("policy install must succeed on the handed-down backend");
        });

        let policy = crate::crud::mask_policy::cache_get("app_cold_policy")
            .expect("policy must be cached after the cold install");
        assert!(policy.allows("support", "spi"));
        crate::reset_context_for_tests();
    }
}
