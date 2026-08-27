//! The PROCESS-WIDE live-schema metadata cache.
//!
//! Entries are facts read from the live catalog for one
//! `(resource, app, deploy token, collection)` identity. Every component of
//! that identity is immutable for the life of the entry: a redeploy mints a new
//! deploy token, and a different database is a different [`DbResourceKey`]. So
//! an entry can never be *invalidated*, only accumulated - which is exactly why
//! a per-thread owner is the wrong one. An n-thread worker previously held n
//! copies of the same immutable facts and paid n whole-catalog reads to build
//! them.
//!
//! # Why the resource key is part of the identity
//!
//! The per-thread map this replaces was keyed by `(DbBinding, collection)`
//! alone, which was sound only because a thread had exactly one database. A
//! process-wide map does not get that for free: two [`crate::service::DbService`]s
//! over different URLs would otherwise alias one another's facts under the same
//! app/deploy/collection triple. The key is therefore qualified by the
//! service's [`DbResourceKey`].
//!
//! # What this module deliberately does NOT own
//!
//! The **singleflight** that collapses concurrent cold misses stays per thread
//! (`ThreadDbContext::poll_schema_introspection`). The parent design chooses
//! per-thread singleflight and lists a process-wide one under rejected
//! alternatives: it would need a cross-thread wake path to save at most
//! `n_threads` catalog walks per epoch bump. Two threads racing a cold miss
//! therefore both walk the catalog; what they must not end up with is two cache
//! entries or two distinct fact objects.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::Value;

use crate::binding::DbBinding;
use crate::service::DbResourceKey;

/// Identity of one cached live-metadata fact.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct LiveMetadataKey {
    resource: DbResourceKey,
    binding: DbBinding,
    collection: String,
}

impl LiveMetadataKey {
    pub(crate) fn new(resource: DbResourceKey, binding: &DbBinding, collection: &str) -> Self {
        Self {
            resource,
            binding: binding.clone(),
            collection: collection.to_string(),
        }
    }
}

/// One collection's introspected facts, or `None` when the catalog read proved
/// the collection absent (the negative cache).
pub(crate) type CachedFacts = Option<Arc<Value>>;

/// The process-wide cache [`crate::service::DbService`] owns.
///
/// `Send + Sync`; holds plain immutable data behind an `RwLock` and never a
/// driver handle.
#[derive(Debug, Default)]
pub struct LiveMetadataCache {
    entries: RwLock<HashMap<LiveMetadataKey, CachedFacts>>,
}

/// The one instance for this process.
///
/// [`crate::service::DbService::new`] adopts it, and
/// [`crate::context::ThreadDbContext`] starts from it, so a thread that never
/// had a service registered still reads and writes the same map rather than a
/// private one. Both are the same object by construction; there is no arm where
/// they differ and therefore no "which cache am I on" question to get wrong.
static PROCESS_WIDE: OnceLock<Arc<LiveMetadataCache>> = OnceLock::new();

/// Handle to this process's live-metadata cache.
pub(crate) fn process_wide() -> Arc<LiveMetadataCache> {
    Arc::clone(PROCESS_WIDE.get_or_init(|| Arc::new(LiveMetadataCache::default())))
}

impl LiveMetadataCache {
    /// Read one identity's facts.
    ///
    /// `None` - never introspected, the caller must read the catalog.
    /// `Some(None)` - introspected, the collection is absent.
    /// `Some(Some(facts))` - introspected, here are the facts.
    ///
    /// The `Arc` is handed out rather than the `Value` cloned, so every reader
    /// of one identity - on any thread - observes the same allocation.
    pub(crate) fn get(&self, key: &LiveMetadataKey) -> Option<CachedFacts> {
        self.entries
            .read()
            .expect("live metadata cache poisoned")
            .get(key)
            .cloned()
    }

    /// True when an entry exists, INCLUDING a cached-absent one. Cheaper than
    /// [`Self::get`] when admission accounting only needs presence.
    pub(crate) fn contains(&self, key: &LiveMetadataKey) -> bool {
        self.entries
            .read()
            .expect("live metadata cache poisoned")
            .contains_key(key)
    }

    /// Record one identity's facts. `facts = None` records absence.
    pub(crate) fn insert(&self, key: LiveMetadataKey, facts: CachedFacts) {
        self.entries
            .write()
            .expect("live metadata cache poisoned")
            .insert(key, facts);
    }

    /// Number of cached identities. Test observability only.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .expect("live metadata cache poisoned")
            .len()
    }

    /// True when nothing is cached. Present because clippy requires it beside
    /// [`Self::len`].
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every entry.
    ///
    /// The per-thread map this replaces was cleared whenever a test replaced
    /// its `ThreadDbContext`. A process-wide map outlives that, so the reset
    /// has to be explicit; [`crate::reset_context_for_tests`] calls this.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn clear(&self) {
        self.entries
            .write()
            .expect("live metadata cache poisoned")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts(marker: &str) -> CachedFacts {
        Some(Arc::new(json!({ "marker": marker })))
    }

    /// The cache is shared across OS threads, and readers on both threads
    /// resolve the SAME allocation.
    ///
    /// This is the arm the per-thread `HashMap` could not pass: a writer on one
    /// thread was invisible to a reader on another. `Arc::ptr_eq` is the right
    /// instrument here precisely because the facts are process-wide and behind
    /// an `Arc` - it rules on object identity, which counting reads cannot.
    ///
    /// Two OS THREADS, not two isolates on one thread. A same-thread arm passes
    /// identically on a wrong implementation that keeps one cache per thread,
    /// because the context it would live in is a `thread_local!` either way.
    #[test]
    fn facts_written_on_one_thread_are_the_same_allocation_on_another() {
        let resource = DbResourceKey::for_url("postgres://live-metadata-cross-thread/db");
        let binding = DbBinding::new("app_cross_thread", "deploy_a");
        let key = LiveMetadataKey::new(resource, &binding, "notes");

        let written = facts("shared");
        let writer_arc = written.clone().expect("fixture writes present facts");
        // Each thread resolves its OWN handle, exactly as `ThreadDbContext::new`
        // does. Cloning one `Arc` into both threads instead would test the cache
        // object's thread-safety and NOT that the cache is process-wide - it
        // passes on a per-thread implementation, because both threads were
        // handed the same object by the test rather than finding it themselves.
        {
            let key = key.clone();
            std::thread::spawn(move || process_wide().insert(key, written))
                .join()
                .expect("writer thread");
        }

        let read_back = std::thread::spawn(move || process_wide().get(&key))
            .join()
            .expect("reader thread");

        let read_back = read_back
            .expect("a second OS thread must see the entry the first one wrote")
            .expect("the entry is a present-collection fact, not a cached absence");
        assert!(
            Arc::ptr_eq(&writer_arc, &read_back),
            "both threads must resolve ONE fact object, not equal copies",
        );
    }

    /// Two OS threads see one handle to one cache.
    #[test]
    fn every_thread_resolves_one_cache_object() {
        let here = process_wide();
        let there = std::thread::spawn(process_wide).join().expect("thread");
        assert!(
            Arc::ptr_eq(&here, &there),
            "the live metadata cache must be process-wide, not per thread",
        );
    }

    /// The resource key participates in identity.
    ///
    /// Control for the sharing arm above: same app, same deploy, same
    /// collection, DIFFERENT database must miss. Without the resource key in
    /// the identity, a process-wide map lets two services alias one another's
    /// facts - a per-thread map never had to answer this because a thread had
    /// one database.
    #[test]
    fn a_second_database_does_not_read_the_first_ones_facts() {
        let cache = process_wide();
        let binding = DbBinding::new("app_two_databases", "deploy_a");
        let first = DbResourceKey::for_url("postgres://first-host/db");
        let second = DbResourceKey::for_url("postgres://second-host/db");
        assert_ne!(first, second, "the fixture needs two distinct resources");

        cache.insert(
            LiveMetadataKey::new(first, &binding, "notes"),
            facts("first"),
        );

        assert!(
            cache
                .get(&LiveMetadataKey::new(second, &binding, "notes"))
                .is_none(),
            "a different database must not read the first one's facts",
        );
        assert!(
            cache
                .get(&LiveMetadataKey::new(first, &binding, "notes"))
                .is_some(),
            "the fixture's own entry must still be readable",
        );
    }

    /// The same property through the REAL per-thread accessors, not the cache
    /// object directly.
    ///
    /// Each thread installs the service's resources the way `DbPlugin::register`
    /// does, then goes through `ThreadDbContext`. This is what catches a
    /// resource key that fails to travel: the cache-object arm above would still
    /// pass if `install_db_resources` stamped the wrong key, because it never
    /// touches the context.
    ///
    /// The reader thread also asserts it did NOT have to introspect - a
    /// `has_introspected_schema` of `false` there is the pre-change behaviour,
    /// where every worker thread paid its own whole-catalog read.
    #[test]
    fn a_second_worker_thread_reads_the_first_ones_facts_through_its_context() {
        let url = "postgres://live-metadata-context-cross-thread/db";
        let binding = DbBinding::new("app_ctx_cross_thread", "deploy_a");
        let written = Arc::new(json!({ "notes": { "type": "string" } }));

        fn install(url: &str) {
            let key = DbResourceKey::for_url(url);
            let backend = crate::service::select_backend(url).expect("fixture URL");
            crate::context::with_mut(|c| {
                c.install_db_resources(url, key, backend, process_wide());
            });
        }

        {
            let binding = binding.clone();
            let written = Arc::clone(&written);
            std::thread::spawn(move || {
                install(url);
                crate::context::with(|c| {
                    c.cache_introspected_schema(&binding, "notes", Some(written))
                });
            })
            .join()
            .expect("writer thread");
        }

        let (seen, read_back) = {
            let binding = binding.clone();
            std::thread::spawn(move || {
                install(url);
                crate::context::with(|c| {
                    (
                        c.has_introspected_schema(&binding, "notes"),
                        c.introspected_schema_for(&binding, "notes"),
                    )
                })
            })
            .join()
            .expect("reader thread")
        };

        assert!(
            seen,
            "a second worker thread must not have to re-introspect what the first already read",
        );
        let read_back = read_back
            .expect("the entry must be visible through the second thread's context")
            .expect("the entry is a present-collection fact");
        assert!(
            Arc::ptr_eq(&written, &read_back),
            "both threads' contexts must resolve ONE fact object",
        );
    }

    /// A cached absence is distinguishable from a cache miss.
    #[test]
    fn absence_is_cached_and_distinguishable_from_a_miss() {
        let cache = process_wide();
        let resource = DbResourceKey::for_url("postgres://negative-cache/db");
        let binding = DbBinding::new("app_negative", "deploy_a");
        let key = LiveMetadataKey::new(resource, &binding, "ghosts");

        assert!(cache.get(&key).is_none(), "nothing cached yet");
        cache.insert(key.clone(), None);
        assert!(cache.contains(&key), "absence must be recorded");
        assert!(
            matches!(cache.get(&key), Some(None)),
            "a cached absence reads as Some(None), never as a miss",
        );
    }
}
