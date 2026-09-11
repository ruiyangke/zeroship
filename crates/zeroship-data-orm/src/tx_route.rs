//! Per-dispatch SQL routing, captured before asynchronous backend resolution.
//!
//! A transaction belongs to a callback's async context. App identity alone
//! cannot distinguish an orphan from work in a replacement transaction. The
//! captured scope therefore carries the session generation and savepoint frame.
//! Execution validates both before touching the lane. Unrelated callbacks use
//! the pool even while the app has a transaction open.
//!
//! Capture and bind are separate because hosts observe async context
//! synchronously, while opening a connection can yield. The route also carries
//! the physical schema and dialect used by query preparation.

use crate::backend::BackendHandle;
use crate::transaction::scope::TransactionScope;
use crate::compile::SqlDialect;
use zeroship_data_sql::SchemaName;

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
    /// The PHYSICAL SCHEMA this dispatch qualifies its tables with and derives
    /// its PostgreSQL role from. Separate from `app_id`, which is the TENANT:
    /// the transaction-lane key, the SQLite ATTACH alias, the metering subject
    /// and the CDC stamp. They hold the same characters today; carrying them
    /// apart is what forces each consumer to say which one it means.
    schema: SchemaName,
    /// `true` iff this dispatch is lexically-and-asynchronously inside a
    /// `db.transaction(fn)` callback **for this same app**.
    in_tx: bool,
    scope: Option<TransactionScope>,
    /// Which SQL dialect this dispatch's statements must be written in.
    ///
    /// **A configuration fact, not a connection fact**, which is why it can
    /// ride here at all: the eager `plan_*` half runs in the V8 prelude,
    /// BEFORE [`Self::bind`] has opened anything, and still has to emit SQL
    /// text. `crate::tx_scope::capture_route` reads it once, from the one
    /// place that can answer it cold, and stamps it here.
    ///
    /// Carrying it on the route rather than passing it beside one is what
    /// makes "planned Postgres, executed on SQLite" unrepresentable instead
    /// of merely avoided: the plan and the connection come from ONE capture.
    dialect: SqlDialect,
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
    schema: SchemaName,
    in_tx: bool,
    scope: Option<TransactionScope>,
    backend: BackendHandle,
    dialect: SqlDialect,
}

impl CapturedRoute {
    /// Freeze the host's observed async scope for this dispatch. A scope for
    /// another app does not confer access to this app's transaction.
    pub fn capture(
        current_scope: Option<&TransactionScope>,
        app_id: &str,
        schema: SchemaName,
        dialect: SqlDialect,
    ) -> Self {
        // SEC-1 compares TENANT against TENANT. The schema rides along; it is
        // never the admission key, because two apps sharing one database would
        // share a schema and must still not share a transaction frame.
        let scope = current_scope.filter(|scope| scope.app_id() == app_id).cloned();
        let in_tx = scope.is_some();
        Self {
            app_id: app_id.to_string(),
            schema,
            in_tx,
            scope,
            dialect,
        }
    }

    /// The physical schema this dispatch qualifies its tables with.
    pub fn schema(&self) -> &SchemaName {
        &self.schema
    }

    /// The app whose schema/role/metering this dispatch runs under.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// The decision itself, before a backend is attached.
    ///
    /// Readable here because the SEC-1 comparison happens in [`Self::capture`],
    /// so this is the value that comparison produced - the tests below assert
    /// on it directly rather than having to bind a backend first.
    pub fn in_tx(&self) -> bool {
        self.in_tx
    }

    /// The dialect the eager `plan_*` half must write its SQL in.
    ///
    /// Readable BEFORE [`Self::bind`], which is the whole reason the dialect is
    /// stamped at capture rather than derived from the backend: nine `plan_*`
    /// functions build SQL in the synchronous V8 prelude, where no backend
    /// exists yet.
    ///
    /// It cannot go stale between here and the statement running. The only
    /// thing that changes a thread's configured dialect is
    /// `ThreadDbContext::set_resource`, which runs at request admission - never
    /// inside a dispatch - and capture and bind are both inside ONE dispatch.
    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    /// Bind the frozen decision to the backend its SQL will run on.
    ///
    /// Consuming, and the ONLY way to build a [`TxRoute`]. The adapter calls
    /// it once per dispatch from the async body, because that is the first
    /// point at which a backend can be opened; see
    /// the adapter tier's `tx_scope::bind_route`.
    pub fn bind(self, backend: BackendHandle) -> TxRoute {
        TxRoute {
            app_id: self.app_id,
            schema: self.schema,
            in_tx: self.in_tx,
            scope: self.scope,
            backend,
            dialect: self.dialect,
        }
    }

    /// **Test-only**: a decision known to be outside any transaction.
    ///
    /// For test harnesses that drive the exec helpers directly, with no
    /// V8 isolate to capture from. Gated so it cannot appear in a shipped
    /// binary; see the module docs. Still has to be `bind`-ed.
    ///
    /// **THE DIALECT IS A PARAMETER, and it was `SqlDialect::Postgres`
    /// unconditionally until 2026-09-03.** A capture without a V8 frame has no
    /// adapter to ask, and this module may not read `crate::context` to find
    /// out - but "cannot derive it" is a reason to make the caller state it,
    /// not a licence to guess. The old constant was wrong on every SQLite
    /// harness in the tree and was contained by nothing reading it, which is
    /// not containment but luck: `crud/mod.rs` alone spells `route.dialect()`
    /// 34 times, counted 2026-09-03 - 17 of them on a `&CapturedRoute` in the
    /// `plan_*` half, 17 on a `&TxRoute` in the `run_*` half - so the day a
    /// SQLite fixture reached one it would have planned Postgres SQL and
    /// blamed the builder.
    ///
    /// Callers that already hold the backend should not spell the answer at
    /// all: `crate::exec::ambient_route_for_tests` derives it from the handle
    /// it is given.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn pool_for_tests(app_id: &str, dialect: SqlDialect) -> Self {
        Self {
            app_id: app_id.to_string(),
            schema: SchemaName::new(app_id).expect("test app ids are legal schema names"),
            in_tx: false,
            scope: None,
            dialect,
        }
    }

    /// **Test-only**: a decision that claims the app's open transaction.
    ///
    /// Pairs with `crate::install_tx_marker_for_tests`, which parks a real
    /// connection in the per-isolate slot. Gated like [`Self::pool_for_tests`],
    /// and taking the dialect as a parameter for the same reason.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn tx_for_tests(app_id: &str, dialect: SqlDialect) -> Self {
        Self {
            app_id: app_id.to_string(),
            schema: SchemaName::new(app_id).expect("test app ids are legal schema names"),
            in_tx: true,
            scope: TransactionScope::current(app_id).ok(),
            dialect,
        }
    }
}

impl TxRoute {
    /// Validate the captured callback before admitting work to its lane.
    pub(crate) fn check_scope(&self) -> Result<(), crate::error::DbError> {
        self.scope.as_ref().map_or(Ok(()), TransactionScope::check)
    }

    /// The TENANT this dispatch runs for: the transaction-lane key, the SQLite
    /// ATTACH alias, the metering subject, the CDC stamp.
    ///
    /// NOT the schema. Use [`Self::schema`] to qualify a table or to derive the
    /// PostgreSQL runtime role.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// The PHYSICAL SCHEMA this dispatch qualifies its tables with, and the one
    /// the per-app PostgreSQL role is derived from.
    pub fn schema(&self) -> &SchemaName {
        &self.schema
    }

    /// The backend this dispatch's SQL runs on.
    ///
    /// Bound at [`CapturedRoute::bind`], so it is the handle the adapter
    /// resolved for THIS dispatch rather than whatever the thread's context
    /// holds by the time the statement finally runs.
    pub fn backend(&self) -> &BackendHandle {
        &self.backend
    }

    /// The dialect this dispatch's statements are written in.
    ///
    /// The value [`CapturedRoute::capture`] stamped, carried through
    /// [`CapturedRoute::bind`] unchanged. Deliberately NOT re-derived from
    /// [`Self::backend`]: matching on the handle's variants is the
    /// vendor-enum read the engine was taken off in the first place, and it
    /// would let the async half answer a different dialect than the eager half
    /// planned against.
    pub fn dialect(&self) -> SqlDialect {
        self.dialect
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
    pub fn in_tx(&self) -> bool {
        self.in_tx
    }

    /// Promote this already-captured dispatch onto an internal transaction.
    ///
    /// This is deliberately a consuming conversion rather than another
    /// constructor: the app identity and the original async-scope decision
    /// still have to come from [`CapturedRoute::capture`]. Bulk write fan-out uses it
    /// only after opening either a top-level transaction or a savepoint, so
    /// every statement and its deferred broker event share that frame.
    pub fn into_internal_transaction(mut self) -> Result<Self, crate::error::DbError> {
        self.in_tx = true;
        self.scope = Some(TransactionScope::current(&self.app_id)?);
        Ok(self)
    }
}

// THE `#[cfg(test)] mod tests` THAT SAT HERE MOVED to `zeroship-data-v8`'s
// `tx_scope.rs` with the data-engine cut, unchanged in substance.
//
// All five arms drive `tx_scope::capture_route` inside a live `v8::PinScope`,
// and three of the four names they use - `tx_scope`, `context`,
// `set_db_url_for_tests`, plus `v8` and `zeroship_runtime::init_v8` - are the
// adapter's. What they establish is a property OF the capture, not of the value
// it produces: that `capture` reads the async-scope marker and is provably not
// the ambient `tx_lanes::has_tx_for` answer. Reverting `capture` to the pre-fix
// ambient test still fails three of them, in their new home.
