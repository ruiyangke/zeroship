//! Process-wide composition of the V8 database adapter.
//! Validated configuration, the plugin prototype and metadata identity are shared
//! across isolates. App teardown stops local subscriptions; the relay owns slots.

use std::cell::Cell;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::{backend_for_url, BackendUrl, DbPlugin};
use zeroship_data_orm::error::DbError;

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

    /// Data-plane backends opened ON THIS THREAD - a Postgres pool or a SQLite
    /// backend handle installed by `init_pool_async`.
    ///
    /// The instrument for "`build_runtime` opens no pool". That claim is about
    /// a call that must NOT happen, so a test can only rule on it by counting;
    /// a passing runtime build proves nothing on its own, and inferring it from
    /// "the fixture DSN is unreachable, so a connect would have failed" is an
    /// argument about the fixture rather than a measurement of the code.
    ///
    static BACKENDS_OPENED: Cell<u64> = const { Cell::new(0) };

}

/// Database-URL parses on this thread. See [`URL_PARSES`].
#[doc(hidden)]
#[must_use]
pub fn url_parse_count() -> u64 {
    URL_PARSES.with(Cell::get)
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
    /// Authenticated relay transport for PostgreSQL subscriptions.
    pub cdc_relay: Option<zeroship_data_orm::cdc::relay::RelayConfig>,
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
            .field("cdc_relay", &self.cdc_relay)
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
            config.cdc_relay,
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

    /// Local subscription lifecycle, independent of database connections.
    #[must_use]
    pub fn lifecycle(&self) -> DbLifecycle {
        DbLifecycle
    }
}

/// Stops local subscription delivery for a deleted app.
#[derive(Debug, Clone, Copy)]
pub struct DbLifecycle;

impl DbLifecycle {
    /// Close this process's subscriptions. The relay releases its source when
    /// the last connected worker leaves; publications remain migration-owned.
    pub async fn deprovision_app(&self, app_id: &str) -> Result<(), DbError> {
        zeroship_data_orm::cdc::lifecycle::shutdown_app(app_id).await;
        crate::broker::drop_app(app_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn postgres_app_teardown_needs_no_database_connection() {
        let service = DbService::new(config("postgres://unused:unused@127.0.0.1:1/unused"))
            .expect("validate configuration without connecting");
        let app = "local-teardown-test";
        let subscription = crate::broker::subscribe(app, "items");
        let lease = zeroship_data_orm::cdc::lifecycle::acquire(app);
        service.lifecycle().deprovision_app(app).await.unwrap();
        assert!(subscription.is_closed());
        drop(lease);
    }

    fn config(url: &str) -> DbServiceConfig {
        DbServiceConfig {
            url: url.to_string(),
            cdc_relay: None,
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
        let service = DbService::new(config("sqlite:service-test.sqlite")).expect("service");
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
        let service = DbService::new(config("sqlite:service-test.sqlite")).expect("service");
        assert!(
            Arc::ptr_eq(&service.plugin(), &service.plugin()),
            "the prototype must be cloned, not minted per call",
        );
    }

    /// Equal configuration is the same resource; different configuration is
    /// not. The control matters: a key that collapsed distinct databases would
    /// let them share metadata belonging to different databases.
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
            rendered.contains("cdc_relay"),
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
        let service = DbService::new(config("sqlite:service-test.sqlite")).expect("service");
        let parses_before = url_parse_count();
        let backends_before = backend_open_count();

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
            backend_open_count(),
            backends_before,
            "local teardown must not open a database backend",
        );
    }
}
