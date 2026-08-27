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
use std::sync::{
    Arc, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

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
    /// The entries map for reading, recovering from a poisoned lock.
    ///
    /// **This cache deliberately has no poison semantics, and the recovery is
    /// not a shortcut.** Poisoning exists to stop a reader observing an
    /// invariant a panicking writer left half-established. This map has no such
    /// invariant: keys and values are plain owned data, already constructed
    /// before either lock is taken, and neither their `Hash`, their `Eq` nor
    /// their `Drop` can panic - so no panic can be taken *between* two steps
    /// that must happen together. What propagating the poison DOES do is turn
    /// one panic anywhere in the process into a permanent panic on every
    /// subsequent `env.db` operation on every thread, because this map now sits
    /// on the per-operation path for the whole process rather than inside one
    /// thread's context.
    fn read_entries(&self) -> RwLockReadGuard<'_, HashMap<LiveMetadataKey, CachedFacts>> {
        self.entries.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The entries map for writing. See [`Self::read_entries`].
    fn write_entries(&self) -> RwLockWriteGuard<'_, HashMap<LiveMetadataKey, CachedFacts>> {
        self.entries.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Read one identity's facts.
    ///
    /// `None` - never introspected, the caller must read the catalog.
    /// `Some(None)` - introspected, the collection is absent.
    /// `Some(Some(facts))` - introspected, here are the facts.
    ///
    /// The `Arc` is handed out rather than the `Value` cloned, so every reader
    /// of one identity - on any thread - observes the same allocation.
    pub(crate) fn get(&self, key: &LiveMetadataKey) -> Option<CachedFacts> {
        self.read_entries().get(key).cloned()
    }

    /// True when an entry exists, INCLUDING a cached-absent one. Cheaper than
    /// [`Self::get`] when admission accounting only needs presence.
    pub(crate) fn contains(&self, key: &LiveMetadataKey) -> bool {
        self.read_entries().contains_key(key)
    }

    /// Publish one identity's facts, and return what the cache holds for that
    /// identity afterwards. `facts = None` records absence.
    ///
    /// **The return value is the resident entry, never necessarily the caller's
    /// own allocation, and that is the whole point.** Two threads racing a cold
    /// miss both walk the catalog (the singleflight is per thread, by design)
    /// and both arrive here with structurally equal facts in two different
    /// `Arc`s. Publishing each and returning your own hands the two callers two
    /// allocations for one immutable fact and leaves the map holding whichever
    /// wrote last - so the `Arc` a caller holds and the `Arc` the next reader
    /// gets are unrelated objects. Insert-if-vacant and return the resident
    /// value, both under ONE write lock, makes the first publisher's object the
    /// only one anybody ends up with.
    ///
    /// First-publisher-wins is not a policy invented here; it follows from the
    /// identity. Every component of [`LiveMetadataKey`] is immutable for the
    /// life of the entry, so two publishers of one key are publishing the same
    /// facts and only the allocation differs.
    pub(crate) fn publish(&self, key: LiveMetadataKey, facts: CachedFacts) -> CachedFacts {
        self.write_entries().entry(key).or_insert(facts).clone()
    }

    /// Number of cached identities. Test observability only.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.read_entries().len()
    }

    /// True when nothing is cached. Present because clippy requires it beside
    /// [`Self::len`].
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every entry in THIS cache object.
    ///
    /// **Never call this on [`process_wide`].** The per-thread map this replaces
    /// was dropped whenever a test replaced its `ThreadDbContext`, which made a
    /// wipe a private act; on the process-wide instance it is not. A test binary
    /// is multi-threaded unless the invocation says otherwise, so a global clear
    /// from one test's teardown empties a concurrently running test's entries
    /// mid-assertion. [`crate::reset_context_for_tests`] used to do exactly that
    /// and no longer does; a fixture that needs isolation takes its own key
    /// instead. This remains for tests that construct their OWN cache object,
    /// where the wipe reaches nobody else.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn clear(&self) {
        self.write_entries().clear();
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
            std::thread::spawn(move || {
                process_wide().publish(key, written);
            })
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
    ///
    /// **Near-tautological, and recorded as such.** [`process_wide`] is a
    /// `OnceLock`, so "two calls return one value" is what the type guarantees;
    /// the only edit this can fail on is `process_wide` being rewritten around
    /// a `thread_local!`. It says NOTHING about entries being shared - a cache
    /// object can be common while the write path is not, which is why
    /// [`facts_written_on_one_thread_are_the_same_allocation_on_another`] above
    /// is the discriminating arm and this one is the cheap structural guard
    /// beneath it.
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

        cache.publish(
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

    /// One test's teardown must not empty the map every other test is using.
    ///
    /// [`crate::reset_context_for_tests`] is reached from `drain_pg()`, the
    /// teardown of essentially every Postgres integration test, and a test
    /// binary runs multi-threaded unless the invocation says otherwise. While it
    /// called `process_wide().clear()` it was a process-global wipe fired from
    /// an arbitrary thread at an arbitrary moment - so an entry a
    /// concurrently-running test had just published could vanish before that
    /// test read it back. `v8_classes::db` refuses the same call, with the same
    /// reasoning, 200 lines from where it was being made.
    ///
    /// This arm stands in for the neighbouring test: it publishes under an
    /// identity of its own, lets somebody else's teardown run, and requires the
    /// entry - the same allocation, not an equal one - to still be there.
    #[test]
    fn the_test_reset_leaves_a_neighbouring_fixtures_entry_alone() {
        let resource = DbResourceKey::for_url("postgres://live-metadata-reset-neighbour/db");
        let binding = DbBinding::new("app_reset_neighbour", "deploy_a");
        let key = LiveMetadataKey::new(resource, &binding, "notes");

        let published = process_wide()
            .publish(key.clone(), facts("neighbour"))
            .expect("the fixture publishes present facts");

        // Another test's teardown, on this thread.
        crate::reset_context_for_tests();

        let survivor = process_wide()
            .get(&key)
            .expect("a neighbouring fixture's entry must survive another test's teardown")
            .expect("the entry is a present-collection fact, not a cached absence");
        assert!(
            Arc::ptr_eq(&published, &survivor),
            "the teardown replaced the neighbouring fixture's fact object",
        );
    }

    /// A panic taken while a writer holds the lock must not disable the cache
    /// for the rest of the process.
    ///
    /// The cache moved onto the per-operation path of every `env.db` call on
    /// every thread. With `RwLock`'s default poison propagation, ONE panic
    /// anywhere in the process turned all five accessors into panics forever -
    /// a whole-process outage grown from a single failed request.
    ///
    /// **On a LOCAL cache, not `process_wide()`, deliberately.** Poisoning is
    /// permanent, so poisoning the shared instance would sabotage every other
    /// test in a binary cargo runs multi-threaded - the same mistake
    /// `reset_context_for_tests` used to make with a global `clear()`. The
    /// property under test belongs to the type, and the type is what is
    /// instantiated here.
    ///
    /// The `is_poisoned` assertion is the control: without it this arm passes
    /// vacuously if the fixture ever stops actually poisoning the lock.
    #[test]
    fn a_panic_under_the_write_lock_does_not_disable_the_cache() {
        let cache = Arc::new(LiveMetadataCache::default());

        let poisoner = Arc::clone(&cache);
        let outcome = std::thread::spawn(move || {
            let _held = poisoner.entries.write().expect("a fresh lock is unpoisoned");
            panic!("a writer panicked while holding the live-metadata lock");
        })
        .join();
        assert!(
            outcome.is_err(),
            "the fixture must actually panic while holding the write lock",
        );
        assert!(
            cache.entries.is_poisoned(),
            "the fixture must actually poison the lock, or this arm proves nothing",
        );

        // All five accessors, because all five carried the `.expect`.
        let resource = DbResourceKey::for_url("postgres://live-metadata-poison/db");
        let binding = DbBinding::new("app_poison", "deploy_a");
        let key = LiveMetadataKey::new(resource, &binding, "notes");

        cache.publish(key.clone(), facts("after the panic"));
        assert!(cache.contains(&key), "contains must survive the poison");
        assert!(
            matches!(cache.get(&key), Some(Some(_))),
            "get must survive the poison",
        );
        assert_eq!(cache.len(), 1, "len must survive the poison");
        assert!(!cache.is_empty(), "is_empty must survive the poison");
        cache.clear();
        assert!(cache.is_empty(), "clear must survive the poison");
    }

    /// A cached absence is distinguishable from a cache miss.
    #[test]
    fn absence_is_cached_and_distinguishable_from_a_miss() {
        let cache = process_wide();
        let resource = DbResourceKey::for_url("postgres://negative-cache/db");
        let binding = DbBinding::new("app_negative", "deploy_a");
        let key = LiveMetadataKey::new(resource, &binding, "ghosts");

        assert!(cache.get(&key).is_none(), "nothing cached yet");
        cache.publish(key.clone(), None);
        assert!(cache.contains(&key), "absence must be recorded");
        assert!(
            matches!(cache.get(&key), Some(None)),
            "a cached absence reads as Some(None), never as a miss",
        );
    }
}
