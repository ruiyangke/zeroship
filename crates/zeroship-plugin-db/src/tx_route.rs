//! [`TxRoute`] — the tx-vs-pool routing decision for ONE CRUD dispatch,
//! captured synchronously at the V8 frame that started it.
//!
//! ## The defect this type exists to make unrepresentable
//!
//! `crate::exec` used to decide "transaction connection or pool?" at SQL
//! time, from ambient state:
//!
//! ```text
//! crate::tx_lanes::with(|l| l.has_tx_for(app_id))   // "does this app have a tx open RIGHT NOW"
//! ```
//!
//! That is a *temporal* test standing in for a *structural* one, and the
//! two come apart the moment an ordinary write overlaps someone else's
//! transaction. They overlap routinely: a worker OS thread multiplexes
//! many requests over one isolate and hands control to another dispatch at
//! every `.await`, and `pnpm dev` is a single isolate by construction
//! (`zeroship serve --workers=1`). Measured on BOTH tiers by
//! `tests/e2e_dev_vs_deployed_db.sh` (`cxPlain`):
//!
//! ```text
//! request A   db.transaction(async tx => { insert; await …; throw })
//! request B                     db.todos.insert(…)   // NO transaction anywhere
//! ```
//!
//! B's plain insert read `has_tx_for == true`, was routed onto **A's**
//! transaction connection, reported `inserted: true` — and A's `ROLLBACK`
//! then destroyed it (`bAfter: 0`). This is the same root cause as the
//! `transaction()`-nesting defect fixed in 00188d788, in a different
//! consumer: there it decided BEGIN-vs-SAVEPOINT, here it decides
//! tx-conn-vs-pool. The default write path is the worse of the two,
//! because most creator writes are not inside a transaction at all.
//!
//! ## The discriminator, and why it must be captured at dispatch
//!
//! Whether an op belongs to a transaction is a property of its **async
//! context**, not of the app's wall-clock state — see [`crate::tx_scope`],
//! which reads V8's continuation-preserved embedder data (the slot
//! `AsyncLocalStorage` uses). `crate::tx_scope::current_tx_app` answers it,
//! but it needs a `&mut v8::PinScope`, and `crate::exec::run_sql` runs
//! inside a spawned future long after the V8 frame returned. So the answer
//! is read in the `dispatch_*` prelude — the last place `scope` is live —
//! frozen into a `TxRoute`, and moved into the spawned async block.
//!
//! ## Why a missed dispatch site cannot compile
//!
//! `TxRoute` has exactly ONE production constructor, [`TxRoute::capture`],
//! and it takes the OBSERVED transaction frame - not an `app_id`, and not a
//! `bool`. There is no `From<&str>`, no `Default`, no `new(app_id)`, and the
//! fields are private, so a `TxRoute` cannot be conjured from an `app_id`.
//! Every exec entry point (`exec_query`, `exec_count`,
//! `exec_mutation`, `exec_mutation_with_emit`) takes `&TxRoute` instead of
//! `app_id: &str`. A new dispatcher that forgets to capture therefore has
//! nothing to pass and fails to compile — it cannot silently fall through
//! to the pool, which would be a WORSE defect than the one being fixed
//! (a transactional write leaking out of its transaction).
//!
//! The one non-`scope` constructor, `CapturedRoute::pool_for_tests`, is
//! `#[cfg(any(test, feature = "test-helpers"))]`: the `test-helpers`
//! feature is declared in this crate's `[features]` and is enabled only by
//! its own `[[test]]` targets, never by a binary that ships. It yields a
//! `CapturedRoute`, so even a test still has to `bind` a backend to reach
//! the type the exec helpers take.

use crate::backend::BackendHandle;

/// The routing decision, frozen at the dispatch frame and not yet bound to a
/// backend.
///
/// **This exists because capture is SYNC and resolution is ASYNC.** The
/// decision has to be taken while the V8 scope is live; opening the backend
/// may have to connect, which cannot happen there. Splitting the two states
/// into two types is what keeps that gap from becoming an `Option` - see
/// [`TxRoute`].
#[derive(Debug)]
pub struct CapturedRoute {
    app_id: String,
    /// `true` iff this dispatch is lexically-and-asynchronously inside a
    /// `db.transaction(fn)` callback **for this same app**.
    in_tx: bool,
}

/// Where one CRUD dispatch's SQL must go, decided at the dispatch frame and
/// bound to the backend it will run on.
///
/// Carries the `app_id` too, so the exec helpers take a single argument
/// and cannot be handed a route captured for one app alongside another
/// app's id.
///
/// Deliberately NOT `Clone`/`Copy`: a route is minted for one dispatch and
/// moved into that dispatch's future. Deliberately NOT `Default` and with
/// no `From<&str>` — see the module docs.
///
/// **There is no `TxRoute::capture` and no public constructor.**
/// [`CapturedRoute::bind`] is the only way to obtain one, so a dispatcher that
/// forgets to capture and a dispatcher that captures but forgets to bind are
/// BOTH compile errors. Carrying `Option<BackendHandle>` on a single type
/// would have demoted the second one to a runtime `None`, surfacing as
/// `not_configured` - the same error a genuinely unconfigured plugin returns -
/// which is precisely the silent-fallthrough class the module docs above say
/// this type exists to prevent.
#[derive(Debug)]
pub struct TxRoute {
    app_id: String,
    in_tx: bool,
    backend: BackendHandle,
}

impl CapturedRoute {
    /// Freeze the routing decision for a dispatch.
    ///
    /// The ONLY production constructor. `current_tx_app` is whichever app
    /// owns the transaction frame this dispatch started inside, as observed
    /// at the dispatch boundary - `None` at top level.
    ///
    /// SEC-1 is structural here rather than incidental: a co-resident app's
    /// callback plants ITS app_id in the continuation slot, so the comparison
    /// below fails and this app routes to its own pool connection.
    ///
    /// **It takes the OBSERVATION, not the V8 scope, and that is the point.**
    /// Until 2026-08-31 this read `crate::tx_scope::current_tx_app(scope)`
    /// itself, which made a routing decision - engine vocabulary - depend on
    /// the adapter that reads V8's context map. The comparison is the security
    /// property and stays here; only the READING of the context moved out, to
    /// [`crate::tx_scope::capture_route`], which is the one place a V8 scope is
    /// in hand. This module now names no `v8::` type.
    ///
    /// Deliberately not a `bool` parameter: a caller cannot assert "I am in a
    /// transaction", only report which app the frame belongs to. The
    /// comparison that turns that into a route is not the caller's to make.
    pub(crate) fn capture(current_tx_app: Option<&str>, app_id: &str) -> Self {
        let in_tx = current_tx_app == Some(app_id);
        Self {
            app_id: app_id.to_string(),
            in_tx,
        }
    }

    /// The app whose schema/role/metering this dispatch runs under.
    pub(crate) fn app_id(&self) -> &str {
        &self.app_id
    }

    /// The decision itself, before a backend is attached.
    ///
    /// Readable here because the SEC-1 comparison happens in [`Self::capture`],
    /// so this is the value that comparison produced - the tests below assert
    /// on it directly rather than having to bind a backend first.
    pub(crate) fn in_tx(&self) -> bool {
        self.in_tx
    }

    /// Bind the frozen decision to the backend its SQL will run on.
    ///
    /// Consuming, and the ONLY way to build a [`TxRoute`]. The adapter calls
    /// it once per dispatch from the async body, because that is the first
    /// point at which a backend can be opened; see
    /// [`crate::tx_scope::bind_route`].
    pub(crate) fn bind(self, backend: BackendHandle) -> TxRoute {
        TxRoute {
            app_id: self.app_id,
            in_tx: self.in_tx,
            backend,
        }
    }

    /// **Test-only**: a decision known to be outside any transaction.
    ///
    /// For test harnesses that drive the exec helpers directly, with no
    /// V8 isolate to capture from. Gated so it cannot appear in a shipped
    /// binary; see the module docs. Still has to be `bind`-ed.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn pool_for_tests(app_id: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            in_tx: false,
        }
    }

    /// **Test-only**: a decision that claims the app's open transaction.
    ///
    /// Pairs with `crate::install_tx_marker_for_tests`, which parks a real
    /// connection in the per-isolate slot. Gated like [`Self::pool_for_tests`].
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn tx_for_tests(app_id: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            in_tx: true,
        }
    }
}

impl TxRoute {
    /// The app whose schema/role/metering this dispatch runs under.
    pub(crate) fn app_id(&self) -> &str {
        &self.app_id
    }

    /// The backend this dispatch's SQL runs on.
    ///
    /// Bound at [`CapturedRoute::bind`], so it is the handle the adapter
    /// resolved for THIS dispatch rather than whatever the thread's context
    /// holds by the time the statement finally runs.
    pub(crate) fn backend(&self) -> &BackendHandle {
        &self.backend
    }

    /// `true` when this dispatch's SQL must run on the app's open
    /// transaction connection.
    ///
    /// A `true` here is a claim about the CALL, not about the app, so the
    /// transaction it names may not be reachable when the SQL finally runs.
    /// The exec path REFUSES in both such cases rather than quietly
    /// autocommitting on a pooled connection — `transaction_scope_expired`
    /// when the enclosing transaction has already settled (a continuation
    /// that outlived its `transaction()`), `transaction_connection_busy`
    /// when it is still open but another op holds its one connection.
    /// Falling back to the pool would let a write the creator wrote inside
    /// a transaction commit unilaterally, which is the mirror image of the
    /// defect this type fixes.
    ///
    /// This used to buy nothing on the dev tier: SQLite ran the whole app on
    /// one connection, so a correctly pool-routed write still executed inside
    /// whatever transaction that connection was holding, and died with its
    /// `ROLLBACK`. SC-2 Decision 1 retired that on 2026-08-27 - the actor now
    /// keeps a shared `op_conn` and a transaction connection **per app**, so
    /// both tiers have somewhere else to send it. The
    /// `docs/reference/sqlite-divergences.md` row is marked retired in the same
    /// change.
    ///
    /// What SQLite still cannot give, and no number of connections would: an
    /// autocommit *write* contends for the single writer lock an open
    /// transaction holds, and waits out `busy_timeout` before reporting lock
    /// contention. Reads are unaffected.
    pub(crate) fn in_tx(&self) -> bool {
        self.in_tx
    }

    /// Promote this already-captured dispatch onto an internal transaction.
    ///
    /// This is deliberately a consuming conversion rather than another
    /// constructor: the app identity and the original async-scope decision
    /// still have to come from [`Self::capture`]. Bulk write fan-out uses it
    /// only after opening either a top-level transaction or a savepoint, so
    /// every statement and its deferred broker event share that frame.
    pub(crate) fn into_internal_transaction(mut self) -> Self {
        self.in_tx = true;
        self
    }
}

#[cfg(test)]
mod tests {
    //! What these establish, and what they do NOT.
    //!
    //! ESTABLISHED: `capture`'s answer tracks the async-scope marker, and
    //! it is NOT the ambient `has_tx_for` answer — `capture` says "in the
    //! transaction" at a moment when `has_tx_for` says "no transaction
    //! parked", so the two discriminators are provably different
    //! functions. Reverting `capture` to the pre-fix
    //! `crate::tx_lanes::with(|l| l.has_tx_for(app_id))` fails three of the four
    //! tests below.
    //!
    //! NOT ESTABLISHED, stated rather than implied:
    //!   - that every `dispatch_*` actually calls `capture`. Nothing at
    //!     runtime can check that; it is enforced by the TYPE (the exec
    //!     entry points take `&TxRoute`, and `TxRoute` has no other
    //!     production constructor) and end to end by
    //!     `tests/e2e_dev_vs_deployed_db.sh` (`cxPlain`).
    //!   - the OTHER direction of the #254 defect — an app with a
    //!     transaction genuinely PARKED in the per-isolate slot while an
    //!     unrelated dispatch runs. Reaching that state needs a real
    //!     `TxConnection` (a live Postgres `Client` or SQLite session
    //!     handle), which these tests deliberately do not open. It is
    //!     covered by `cxPlain` on both tiers instead.
    //!   - anything about which CONNECTION the exec path then picks.

    use super::*;
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

    #[test]
    fn top_level_dispatch_routes_to_the_pool() {
        in_scope!(let scope);
        let route = crate::tx_scope::capture_route(scope, "app_a");
        assert!(!route.in_tx(), "no transaction scope entered");
        assert_eq!(route.app_id(), "app_a");
    }

    #[test]
    fn dispatch_inside_the_callback_routes_to_the_transaction() {
        in_scope!(let scope);
        let prev = crate::tx_scope::enter(scope, "app_a");
        assert!(crate::tx_scope::capture_route(scope, "app_a").in_tx());
        crate::tx_scope::leave(scope, prev);
        assert!(
            !crate::tx_scope::capture_route(scope, "app_a").in_tx(),
            "leaving the scope must stop routing to the tx"
        );
    }

    #[test]
    fn a_co_resident_apps_transaction_scope_does_not_capture_this_app() {
        in_scope!(let scope);
        let prev = crate::tx_scope::enter(scope, "app_other");
        assert!(
            !crate::tx_scope::capture_route(scope, "app_a").in_tx(),
            "SEC-1: app_a must not join app_other's transaction"
        );
        assert!(crate::tx_scope::capture_route(scope, "app_other").in_tx());
        crate::tx_scope::leave(scope, prev);
    }

    /// The one-variable control that separates the two discriminators.
    ///
    /// Inside the scope, `capture` answers "transaction" while the
    /// per-isolate slot this app-id would be looked up in is EMPTY, so
    /// `has_tx_for` answers "no transaction". A `capture` implemented on
    /// the pre-fix ambient test cannot produce this pair.
    #[test]
    fn capture_is_not_the_ambient_has_tx_for_answer() {
        in_scope!(let scope);
        let prev = crate::tx_scope::enter(scope, "app_a");
        let ambient = crate::tx_lanes::with(|l| l.has_tx_for("app_a"));
        let captured = crate::tx_scope::capture_route(scope, "app_a").in_tx();
        crate::tx_scope::leave(scope, prev);
        assert!(!ambient, "precondition: no transaction is parked for app_a");
        assert!(
            captured,
            "capture must read the async scope, not the parked-tx slot"
        );
    }
}
