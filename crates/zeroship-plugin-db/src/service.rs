//! `DbService` - process-wide ownership of the `env.db` primitive.
//!
//! One [`DbService`] is constructed at worker / CLI composition, BEFORE any V8
//! isolate exists, and every isolate on every worker thread is served from it.
//! It owns the five things the runtime-db-binding design (SC-5) says cannot
//! live per runtime or per thread:
//!
//! | Owned | Why it cannot live per runtime |
//! | --- | --- |
//! | Validated configuration | Backend selection happens ONCE. Nothing downstream re-parses the URL. |
//! | The plugin prototype | `build_runtime` clones an `Arc` instead of minting a plugin set. |
//! | The stable thread-resource key | Current and deploy-pinned isolates on one OS thread resolve the SAME resources. |
//! | The process-wide live-metadata cache | Its values are immutable plain data; sharing is the point. |
//! | The neutral operator-lifecycle handle | Deprovision uses the service's configuration and one shared operator pool instead of re-parsing the URL and building a pool per deletion. |
//!
//! # The operator pool exists, and SC-5's arm said it would not
//!
//! SC-5 words the deprovision clause as "performs NO second URL parse and opens
//! NO second pool". The first half holds exactly. **The second does not: this
//! module opens a dedicated [`OPERATOR_POOL_SIZE`]-connection pool** and shares
//! it across deletions. The clause was written against the alternative of
//! re-deriving everything per deleted app; sharing one pool is the better of the
//! two, but it is not zero, and the arm that measures it asserts `1`, not `0`.
//! The claim is amended here rather than left to be read off an assertion that
//! contradicts it.
//!
//! What that costs an operator, and what bounds it, is in
//! `docs/runbooks/docker-compose.md` under "Postgres connections a worker
//! holds". The short version: the pool is opened lazily on the first deletion a
//! process reconciles and released by [`close_operator_pools`] once the poller's
//! pending set drains, so a worker holds these connections while it is
//! reconciling deletions and none between. It has no `ThreadDbContext` pool to
//! borrow instead - the version poller runs on a thread that hosts no isolate.
//!
//! # `Send + Sync`, and never a driver connection
//!
//! [`DbService`] crosses worker-thread boundaries, so it is `Send + Sync` -
//! which is *itself* the proof that it holds no driver connection.
//! `compio_postgres::Pool` is `!Send` by construction (`Cell`/`RefCell`, no
//! atomics), so a `Send` struct cannot transitively own one, by value or behind
//! an `Rc`. Opening still happens on the owning thread and yields a
//! thread-bound `Rc`; nothing became `Send` to satisfy service storage.
//!
//! # What a `DbResourceKey` is for
//!
//! It is the identity of one *database's* resources, minted once from the
//! validated configuration. Two places index by it today - the process-wide
//! metadata cache and the per-thread operator pool - and both need an identity
//! that is stable across isolates and equal for two isolates of the same app at
//! different deploys. It is a digest, not the URL, because it reaches `Debug`
//! output and logs and a DSN carries a password.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use compio_postgres::Pool;
use sha2::{Digest, Sha256};

use crate::error::DbError;
use crate::{backend_for_url, BackendUrl, DbPlugin};

/// Connections the operator-lifecycle pool keeps for maintenance work.
///
/// Two, matching what the per-deletion pool used before this pool became
/// shared. It is a maintenance path, not a data path: its work is catalog
/// reads and `pg_drop_replication_slot`.
///
/// **All of them are real backends for as long as the pool is installed.**
/// `Pool::connect(url, 2)` lowers the driver's default `min_idle` of 2 to
/// `min(2, max_size)` = 2 and opens that many upfront, and idle eviction only
/// closes connections while the idle count EXCEEDS `min_idle` - so neither
/// `idle_timeout` nor an absent housekeeper ever takes this pool below two.
/// Read `OPERATOR_POOL_SIZE` as "backends held", not "ceiling occasionally
/// reached". [`close_operator_pools`] is what takes it back to zero.
const OPERATOR_POOL_SIZE: usize = 2;

thread_local! {
    /// Database-URL parses performed ON THIS THREAD.
    ///
    /// The design forbids a second URL parse once configuration is validated,
    /// and "does not re-parse" is not a property `grep` can rule on - it is a
    /// claim about what runs. This counter is the instrument the acceptance
    /// arms read. It is a single non-atomic increment on a cold path
    /// (composition, and lazy backend init), so it is always compiled rather
    /// than `cfg`-gated: a counter that only exists under `cfg(test)` measures
    /// a different binary from the one that ships.
    ///
    /// **Per thread, not per process, and that is not a compromise.** Every
    /// claim it exists to rule on is about one call path - "*this* deprovision
    /// re-parsed nothing" - and a process-wide counter cannot answer that in a
    /// test binary, where other tests run concurrently on other threads and
    /// move it underneath the assertion. Cross-thread properties (one plugin
    /// prototype, one cache entry) are ruled on by pointer identity instead,
    /// which is the right instrument for them.
    static URL_PARSES: Cell<u64> = const { Cell::new(0) };

    /// Operator-lifecycle pools opened ON THIS THREAD.
    ///
    /// Same rationale as [`URL_PARSES`], and the pools are thread-bound
    /// anyway - an `Rc<Pool>` never leaves the thread that opened it, so a
    /// per-thread count is the complete count for that pool map.
    static OPERATOR_POOLS_OPENED: Cell<u64> = const { Cell::new(0) };

    /// Data-plane backends opened ON THIS THREAD - a Postgres pool or a SQLite
    /// backend handle installed by `init_pool_async`.
    ///
    /// The instrument for "`build_runtime` opens no pool". That claim is about
    /// a call that must NOT happen, so a test can only rule on it by counting;
    /// a passing runtime build proves nothing on its own, and inferring it from
    /// "the fixture DSN is unreachable, so a connect would have failed" is an
    /// argument about the fixture rather than a measurement of the code.
    ///
    /// **What it does NOT count.** It counts the DATA-PLANE backend install,
    /// not every connection the crate opens. The operator-lifecycle pool
    /// ([`OPERATOR_POOLS`]) has its own counter, and `wal_consumer`'s
    /// replication connection has none. Both are outside what this counter is
    /// for. Read a zero as "no backend was installed", never as "no socket was
    /// opened".
    ///
    /// It said `acquire_dedicated_client` "calls `compio_postgres::connect`
    /// straight through for each explicit transaction" and named moving it onto
    /// a pooled checkout as a later step. That move has landed:
    /// `PostgresBackend::acquire_dedicated_client` is `self.pool.get_owned()`,
    /// so an explicit transaction borrows from the data-plane pool and opens no
    /// socket of its own.
    static BACKENDS_OPENED: Cell<u64> = const { Cell::new(0) };

    /// This thread's operator pools, one per [`DbResourceKey`].
    ///
    /// `Rc<Pool>` - thread-bound by construction, exactly like the data plane's
    /// pool. The map is what makes ONE pool serve a whole run of deletions: the
    /// operator path used to build a fresh two-connection pool per
    /// deprovisioned app, paying two connects, two authentications and two TLS
    /// handshakes each time.
    ///
    /// **Installed lazily, and NOT for the life of the process.** An entry
    /// appears on the first deletion this thread reconciles and is removed by
    /// [`close_operator_pools`], which the worker's version poller calls once
    /// its pending set drains. Without that call the entry would be permanent -
    /// `OPERATOR_POOLS` is a `thread_local!` with no eviction, so a worker
    /// container would hold [`OPERATOR_POOL_SIZE`] extra Postgres backends from
    /// its first app deletion until the process exited, against whatever
    /// `max_connections` the cluster is sized for.
    static OPERATOR_POOLS: RefCell<HashMap<DbResourceKey, Rc<Pool>>> =
        RefCell::new(HashMap::new());
}

/// Database-URL parses on this thread. See [`URL_PARSES`].
#[doc(hidden)]
#[must_use]
pub fn url_parse_count() -> u64 {
    URL_PARSES.with(Cell::get)
}

/// Operator-lifecycle pools opened on this thread. See
/// [`OPERATOR_POOLS_OPENED`].
#[doc(hidden)]
#[must_use]
pub fn operator_pool_open_count() -> u64 {
    OPERATOR_POOLS_OPENED.with(Cell::get)
}

/// Data-plane backends opened on this thread. See [`BACKENDS_OPENED`].
#[doc(hidden)]
#[must_use]
pub fn backend_open_count() -> u64 {
    BACKENDS_OPENED.with(Cell::get)
}

/// Record one data-plane backend open. Called by `init_pool_async` on both
/// arms, so the count covers a Postgres pool and a SQLite handle alike.
pub(crate) fn note_backend_open() {
    BACKENDS_OPENED.with(|opened| opened.set(opened.get() + 1));
}

/// Parse and classify a database URL, recording the parse.
///
/// Every backend selection in the crate goes through here so [`URL_PARSES`]
/// is a complete count rather than a sample.
pub(crate) fn select_backend(url: &str) -> Result<BackendUrl, DbError> {
    URL_PARSES.with(|parses| parses.set(parses.get() + 1));
    backend_for_url(url)
}

/// Stable identity of one database's resources.
///
/// A digest of the validated URL: equal configurations are the same resource,
/// different ones are never confused, and neither `Debug` nor a log line can
/// leak the DSN's password.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct DbResourceKey([u8; 32]);

impl DbResourceKey {
    /// The identity of a thread with no database configured.
    ///
    /// A context in this state cannot introspect anything - live metadata is
    /// read through a pool it does not have - so the only entries that can ever
    /// land under it are a test harness's own.
    pub(crate) const UNBOUND: Self = Self([0u8; 32]);

    /// Derive the key for one database URL.
    ///
    /// Domain-separated so the digest cannot collide with any other sha256 the
    /// crate computes over the same bytes.
    #[must_use]
    pub fn for_url(url: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"zeroship.db.resource.v1\0");
        hasher.update(url.as_bytes());
        Self(hasher.finalize().into())
    }
}

impl std::fmt::Debug for DbResourceKey {
    /// Print a short digest prefix, never the URL. A DSN carries a password and
    /// this value reaches `Debug` output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == Self::UNBOUND {
            return f.write_str("DbResourceKey(unbound)");
        }
        write!(
            f,
            "DbResourceKey({:02x}{:02x}{:02x}{:02x})",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

/// Everything the platform must decide about `env.db` before an isolate exists.
#[derive(Clone)]
pub struct DbServiceConfig {
    /// The database URL. Parsed exactly once, by [`DbService::new`].
    pub url: String,
    /// Stable across every isolate in one worker process and distinct across
    /// worker containers. Used to derive the process's per-app CDC slot.
    pub worker_id: String,
    /// The process-wide usage meter. `None` in meter-less test harnesses.
    pub meter: Option<Arc<zeroship_metering::Meter>>,
}

impl std::fmt::Debug for DbServiceConfig {
    /// Hand-written for the same reason [`DbResourceKey`] is a digest: the DSN
    /// carries a password and this struct is `pub`, so any caller may render it.
    /// A derived `Debug` here would have made the module doc above - "it is a
    /// digest, not the URL, because it reaches `Debug` output" - false about the
    /// value the URL actually lives in.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbServiceConfig")
            .field("url", &"<redacted>")
            .field("worker_id", &self.worker_id)
            .field("meter", &self.meter.as_ref().map(|_| "<meter>"))
            .finish()
    }
}

/// The process-wide owner of the `env.db` primitive's configuration.
pub struct DbService {
    url: String,
    backend: BackendUrl,
    resource_key: DbResourceKey,
    plugin: Arc<DbPlugin>,
}

// The service crosses worker-thread boundaries. This is also the mechanical
// proof that it holds no driver connection: `Pool` is `!Send`, so it cannot
// appear anywhere in these fields.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DbService>();
};

impl std::fmt::Debug for DbService {
    /// No URL - it carries a password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbService")
            .field("backend", &self.backend)
            .field("resource", &self.resource_key)
            .finish_non_exhaustive()
    }
}

impl DbService {
    /// Validate configuration and mint the process's `env.db` service.
    ///
    /// This is the ONLY place production code selects a backend from a URL.
    /// A malformed or unsupported URL fails HERE, at composition, rather than
    /// inside the first database operation an app happens to run.
    pub fn new(config: DbServiceConfig) -> Result<Arc<Self>, DbError> {
        let backend = select_backend(&config.url)?;
        // Parse the operator charter HERE, for the same reason the backend is
        // selected here: a malformed authority fails at composition rather than
        // inside the first write an app happens to run.
        let system_shape_charter = crate::system_shape_charter::load()?;
        let resource_key = DbResourceKey::for_url(&config.url);
        let plugin = Arc::new(DbPlugin::new(
            config.url.clone(),
            config.worker_id,
            config.meter,
            resource_key,
            backend.clone(),
            system_shape_charter,
        ));
        Ok(Arc::new(Self {
            url: config.url,
            backend,
            resource_key,
            plugin,
        }))
    }

    /// The plugin prototype every runtime clones.
    ///
    /// `build_runtime` calls this and gets an `Arc` clone. It does not mint a
    /// plugin, and it performs no backend selection: both already happened, in
    /// [`Self::new`], once for the process.
    #[must_use]
    pub fn plugin(&self) -> Arc<DbPlugin> {
        Arc::clone(&self.plugin)
    }

    /// The stable resource key isolates resolve their thread resources under.
    #[must_use]
    pub fn resource_key(&self) -> DbResourceKey {
        self.resource_key
    }

    /// The validated database URL.
    ///
    /// `Debug` hides this deliberately - it carries a password - but the
    /// accessor exists because the workflow journal path still opens its own
    /// connections from a URL. Those callers are not this contract's to move;
    /// what this contract removes is the *second backend selection* and the
    /// per-deletion pool, not every remaining URL consumer.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The validated backend selection. Never re-derived from the URL.
    pub(crate) fn backend(&self) -> &BackendUrl {
        &self.backend
    }

    /// The neutral operator-lifecycle handle.
    ///
    /// "Neutral" in the design's sense: it holds no connection of its own. It
    /// borrows the service's validated configuration and resolves the calling
    /// thread's shared operator pool when it actually needs one.
    #[must_use]
    pub fn lifecycle(&self) -> DbLifecycle<'_> {
        DbLifecycle { service: self }
    }
}

/// Operator lifecycle operations against one [`DbService`]'s database.
///
/// Obtained from [`DbService::lifecycle`]. Deliberately a borrow: the handle is
/// a view onto validated configuration, not a resource.
#[derive(Debug, Clone, Copy)]
pub struct DbLifecycle<'a> {
    service: &'a DbService,
}

impl DbLifecycle<'_> {
    /// Tear down all CDC state for a deleted app without requiring a live V8
    /// isolate.
    ///
    /// The worker's process-wide version poller calls this after an app
    /// disappears from the control-plane registry. It closes local
    /// subscriptions, stops this process's consumer, then drops every worker
    /// slot. Publication membership remains owned by the migration service. The
    /// Postgres teardown is idempotent so every worker container may observe
    /// the same deletion safely.
    ///
    /// The backend selection is the service's, decided once at composition, and
    /// the pool is this thread's shared operator pool. Neither is derived per
    /// deletion. That pool is not free and is not zero; see the module header.
    ///
    pub async fn deprovision_app(&self, app_id: &str) -> Result<(), DbError> {
        crate::cdc_lifecycle::shutdown_app(app_id).await;
        crate::broker::drop_app(Some(app_id));
        match self.service.backend() {
            BackendUrl::Sqlite { .. } => Ok(()),
            BackendUrl::Postgres => {
                let pool = self.operator_pool().await?;
                crate::replication::drop_worker_slots(&pool, app_id).await
            }
        }
    }

    /// This thread's operator pool for the service's database, opening it on
    /// first use.
    ///
    /// The borrow is released before every await point, and a pool that lost
    /// the race to publish is discarded in favour of the published one - so
    /// concurrent callers on one thread converge on a single pool rather than
    /// leaving whichever finished last installed.
    pub(crate) async fn operator_pool(&self) -> Result<Rc<Pool>, DbError> {
        let key = self.service.resource_key;
        if let Some(pool) = OPERATOR_POOLS.with(|pools| pools.borrow().get(&key).cloned()) {
            return Ok(pool);
        }

        let opened = Rc::new(
            Pool::connect(&self.service.url, OPERATOR_POOL_SIZE)
                .await
                .map_err(|error| DbError::Transient {
                    message: format!("db operator connection failed: {error}"),
                })?,
        );
        OPERATOR_POOLS_OPENED.with(|count| count.set(count.get() + 1));

        // The loser is carried OUT of the `with` before being dropped. Dropping
        // a `Pool` only asks its detached driver tasks to shut down; the
        // `Terminate` write and socket drop still have to be driven, so the drop
        // has to happen where this async fn's runtime can still run them (see
        // `tests/integration.rs`'s `release_pg`). Inside the closure it would
        // also run while the `RefCell` is mutably borrowed. Unreachable today -
        // only the version poller deprovisions, sequentially - but the doc
        // above advertises the race as handled, so the handling is written to
        // be correct rather than to be unreached.
        let (installed, lost_the_race) = OPERATOR_POOLS.with(|pools| {
            match pools.borrow_mut().entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    (entry.get().clone(), Some(opened))
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    (Rc::clone(entry.insert(opened)), None)
                }
            }
        });
        drop(lost_the_race);

        Ok(installed)
    }
}

/// Release this thread's operator pools.
///
/// They own live Postgres backends - [`OPERATOR_POOL_SIZE`] of them per
/// installed pool, none of which idle eviction can reclaim - and nothing else
/// removes an entry from [`OPERATOR_POOLS`]. Without this call a worker
/// container holds them from its first app deletion until the process exits.
///
/// **Always compiled, and called in production.** The worker's version poller
/// calls it once its pending-deprovision set drains, so the connections live
/// for a reconcile batch rather than for the process; the test suites call the
/// same function for the same reason, through `reset_context_for_tests`. It was
/// a `#[cfg(test)]` helper first, which is what made "the pool is never closed"
/// true of every shipped binary while looking handled in the tests.
///
/// Call it where a runtime can still drive the connections' shutdown: dropping
/// a `Pool` asks its detached driver tasks to close, it does not wait for them.
pub fn close_operator_pools() {
    OPERATOR_POOLS.with(|pools| pools.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(url: &str) -> DbServiceConfig {
        DbServiceConfig {
            url: url.to_string(),
            worker_id: "service-test-worker".to_string(),
            meter: None,
        }
    }

    /// Composition parses the URL exactly once, and nothing the service hands
    /// out afterwards parses it again.
    ///
    /// The plugin prototype, the resource key and the backend selection are all
    /// read here; a re-parse anywhere in that surface moves the counter.
    #[test]
    fn the_service_parses_its_url_once_and_never_again() {
        let before = url_parse_count();
        let service = DbService::new(config("sqlite::memory:")).expect("service");
        let after_construction = url_parse_count();
        assert_eq!(
            after_construction - before,
            1,
            "composition must parse the URL exactly once",
        );

        let _plugin = service.plugin();
        let _key = service.resource_key();
        let _lifecycle = service.lifecycle();
        assert_eq!(
            url_parse_count(),
            after_construction,
            "nothing the service hands out may re-parse the URL",
        );
    }

    /// A bad URL fails at composition, not inside an app's first query.
    #[test]
    fn an_unsupported_scheme_fails_at_composition() {
        let error = DbService::new(config("mysql://localhost/dev")).unwrap_err();
        assert!(
            matches!(
                error,
                DbError::Configuration {
                    code: "unsupported_database_url_scheme",
                    ..
                }
            ),
            "expected a configuration error, got {error:?}",
        );
    }

    /// Every caller gets the same prototype object, so a runtime clones rather
    /// than mints.
    ///
    /// **What this arm can and cannot fail, recorded because the assertion
    /// reads stronger than it is.** `Arc::ptr_eq` over two `Arc::clone`s of one
    /// field is true by construction; the only edit that turns it red is
    /// [`DbService::plugin`] minting a fresh `DbPlugin` per call. It does NOT
    /// rule on two runtimes, two isolates or two threads sharing one prototype -
    /// nothing here builds a second runtime. That is
    /// `zeroship_worker::cache::tests::db_plugin_prototype_is_one_object_across_worker_threads`,
    /// which is the arm that fails on the pre-service code.
    #[test]
    fn the_plugin_prototype_is_one_object() {
        let service = DbService::new(config("sqlite::memory:")).expect("service");
        assert!(
            Arc::ptr_eq(&service.plugin(), &service.plugin()),
            "the prototype must be cloned, not minted per call",
        );
    }

    /// Equal configuration is the same resource; different configuration is
    /// not. The control matters: a key that collapsed distinct databases would
    /// let them share a metadata cache and an operator pool.
    #[test]
    fn the_resource_key_follows_the_validated_configuration() {
        let one = DbService::new(config("postgres://host-a/db")).expect("service");
        let same = DbService::new(config("postgres://host-a/db")).expect("service");
        let other = DbService::new(config("postgres://host-b/db")).expect("service");

        assert_eq!(one.resource_key(), same.resource_key());
        assert_ne!(one.resource_key(), other.resource_key());
        assert_ne!(one.resource_key(), DbResourceKey::UNBOUND);
    }

    /// The key's `Debug` must not carry the DSN - it holds a password.
    #[test]
    fn the_resource_key_debug_hides_the_url() {
        let key = DbResourceKey::for_url("postgres://postgres:hunter2@host/db");
        let rendered = format!("{key:?}");
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("host"),
            "DbResourceKey Debug leaked the DSN: {rendered}",
        );
        assert_eq!(
            format!("{:?}", DbResourceKey::UNBOUND),
            "DbResourceKey(unbound)",
        );
    }

    /// The config struct's `Debug` must not carry the DSN - it holds a
    /// password, and the struct is `pub` so any caller may render it.
    ///
    /// The derived `Debug` this replaces printed `url: "postgres://postgres:
    /// hunter2@host/db"` verbatim. No call site formatted it, which is why the
    /// leak was latent rather than observed; a `pub` field is reachable by
    /// definition, so latency is not a defence.
    #[test]
    fn the_service_config_debug_hides_the_dsn() {
        let rendered = format!("{:?}", config("postgres://postgres:hunter2@host/db"));
        assert!(
            !rendered.contains("hunter2"),
            "DbServiceConfig Debug leaked the DSN password: {rendered}",
        );
        assert!(
            rendered.contains("service-test-worker"),
            "the redaction must not blind the fields that are safe to print: {rendered}",
        );
    }

    /// A SQLite deprovision needs no pool and, crucially, no second URL parse.
    ///
    /// The pre-change free function took a `&str` and re-ran `backend_for_url`
    /// on it, so this delta was 1. Mutating `deprovision_app` back to
    /// `select_backend(&self.service.url)` turns this assertion red.
    #[compio::test]
    async fn deprovisioning_on_sqlite_re_parses_nothing() {
        let service = DbService::new(config("sqlite::memory:")).expect("service");
        let parses_before = url_parse_count();
        let pools_before = operator_pool_open_count();

        service
            .lifecycle()
            .deprovision_app("app_sqlite_deprovision")
            .await
            .expect("sqlite deprovision is a no-op");

        assert_eq!(
            url_parse_count(),
            parses_before,
            "deprovision must use the service's validated backend selection",
        );
        assert_eq!(
            operator_pool_open_count(),
            pools_before,
            "the SQLite arm must not open an operator pool",
        );
    }
}
