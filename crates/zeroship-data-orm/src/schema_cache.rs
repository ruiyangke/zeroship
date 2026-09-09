//! The isolate's slice of THE schema authority: the runtime descriptor.
//!
//! # Why this is its own type, in this crate
//!
//! It was four methods and a `HashMap` field on `ThreadDbContext`, which
//! `docs/proposals/2026-09-02-thread-context-ownership.md` identifies as FOUR
//! OWNERS wearing one struct. That proposal assigns this one to
//! `zeroship-data-core`, and the assignment is not a preference: the whole owner
//! names `DbBinding`, `zeroship_data_sql::value::Value` and `Arc` and NOTHING else - no
//! driver, no V8, no runtime, no engine type. It was rank-0 vocabulary sitting
//! in the adapter.
//!
//! Separating it is what lets the lane owner move to `data-engine` later without
//! dragging the descriptor store along, since the two shared only a struct.
//!
//! # The key shape is load-bearing
//!
//! Entries are keyed `<app_id>:<deploy_token>:<collection>`. The deploy token is
//! in the key, not just the app id, because one isolate can outlive a deploy:
//! a stale entry from the previous descriptor must be unreachable rather than
//! merely unlikely. [`SchemaCache::replace_for_binding`] therefore retains by
//! prefix and re-inserts, which is one synchronous publication point - no caller
//! can observe a half-replaced descriptor.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use zeroship_data_sql::value::Value;

use crate::error::DbError;

use crate::binding::DbBinding;

/// One isolate's descriptor entries, keyed by `(app, deploy, collection)`.
#[derive(Debug, Default)]
pub struct SchemaCache {
    entries: HashMap<String, Arc<Value>>,
}

impl SchemaCache {
    /// An empty cache. A fresh isolate has installed no schema.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The store key for one collection under one binding.
    fn key(binding: &DbBinding, collection: &str) -> String {
        format!(
            "{}:{}:{}",
            binding.app_id(),
            binding.deploy_token(),
            collection
        )
    }

    /// Replace the complete descriptor for one app-at-deploy binding.
    ///
    /// Callers fully validate and collect every entry before calling. The
    /// retain-and-insert sequence is one synchronous publication point: no
    /// callback can observe a partial descriptor, removed collections do not
    /// survive a dev isolate restart, and an empty input leaves a schema-less
    /// binding empty.
    pub fn replace_for_binding(&mut self, binding: &DbBinding, schemas: Vec<(String, Value)>) {
        let prefix = format!("{}:{}:", binding.app_id(), binding.deploy_token());
        self.entries.retain(|key, _| !key.starts_with(&prefix));
        for (collection, schema) in schemas {
            self.entries
                .insert(Self::key(binding, &collection), Arc::new(schema));
        }
    }

    /// The descriptor entry for one collection under one binding.
    ///
    /// `None` means the descriptor this isolate was built from does not declare
    /// the collection. Callers must NOT treat that as "carry on without a
    /// schema" - the adapter's `descriptor::collection_schema` is the only thing
    /// that should call this, and it turns the miss into a typed error.
    #[must_use]
    pub fn get(&self, binding: &DbBinding, collection: &str) -> Option<Arc<Value>> {
        self.entries.get(&Self::key(binding, collection)).cloned()
    }

    /// Every `(collection, schema)` pair held for one BINDING. `mint_tx_view`
    /// uses it to mint one `Collection` per declared name; the drift-check sweep
    /// uses it to walk every declared collection. Empty when the isolate has
    /// installed no schema.
    #[must_use]
    pub fn entries_for_binding(&self, binding: &DbBinding) -> Vec<(String, Arc<Value>)> {
        let prefix = format!("{}:{}:", binding.app_id(), binding.deploy_token());
        self.entries
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix(&prefix)
                    .map(|coll| (coll.to_string(), v.clone()))
            })
            .collect()
    }

    /// The descriptor entry for one collection, or a typed error.
    ///
    /// **There is no `Option` here on purpose, and that is a security rule.**
    /// The read path used to treat an absent schema as "carry on", which is how
    /// L24 happened: the projection allowlist stopped applying and the
    /// read-identifier check silently passed, so a read served before the
    /// schema arrived returned every physical column and accepted any field
    /// name. A collection the descriptor does not declare is not a collection
    /// this isolate can serve.
    ///
    /// This lives HERE rather than in the adapter's `descriptor.rs`, where it
    /// sat until 2026-09-02, because the rule is about the store's contract -
    /// what a miss MEANS - not about how a caller reached the store. The
    /// adapter kept only the thread-local lookup.
    ///
    /// # Errors
    ///
    /// [`DbError::config`] with code `collection_not_declared` when the
    /// descriptor this isolate was built from does not declare `collection`.
    pub fn require(&self, binding: &DbBinding, collection: &str) -> Result<Arc<Value>, DbError> {
        self.get(binding, collection).ok_or_else(|| {
            DbError::config(
                "collection_not_declared",
                format!(
                    "db: collection '{collection}' is not declared by this deploy's runtime schema \
                     descriptor; the descriptor is the sole schema authority and nothing else may \
                     be read"
                ),
            )
        })
    }

    /// Install ONE collection's entry. Test fixtures use this narrow helper;
    /// production boot replaces a binding's complete descriptor through
    /// [`Self::replace_for_binding`].
    ///
    /// **Deliberately NOT feature-gated**, though its only callers are tests.
    /// A `cfg(feature = "test-helpers")` here would be unreachable from a
    /// consumer's `cargo test`: across a crate boundary the consumer's `test`
    /// cfg cannot turn THIS crate's feature on, so plugin-db's test code would
    /// call a method that does not exist. That is the hazard recorded in #162,
    /// and three lines of always-present code is the cheap side of it.
    pub fn insert_one(&mut self, binding: &DbBinding, collection: &str, schema: Value) {
        self.entries
            .insert(Self::key(binding, collection), Arc::new(schema));
    }
}

// ----- THE ISOLATE'S CACHE ---------------------------------------------

thread_local! {
    /// This isolate's descriptor store.
    ///
    /// It lives HERE rather than as a field on the adapter's `ThreadDbContext`
    /// because `docs/proposals/2026-09-02-thread-context-ownership.md` assigns
    /// `schemas` to `zeroship-data-core`, and a field on a struct in the adapter
    /// cannot be read from a crate the adapter depends on. `descriptor.rs` is
    /// core-destined and is the store's only production reader, so leaving the
    /// cache on the context would have made the core tier reach UP into the
    /// adapter - the one edge direction the split forbids outright.
    ///
    /// PER-THREAD, like every other piece of isolate state: a worker thread runs
    /// one isolate, and two threads must not see each other's descriptors.
    static SCHEMAS: RefCell<SchemaCache> = RefCell::new(SchemaCache::new());
}

/// Read this isolate's descriptor store.
pub fn with<R>(f: impl FnOnce(&SchemaCache) -> R) -> R {
    SCHEMAS.with_borrow(f)
}

/// Mutate this isolate's descriptor store.
///
/// Callers must not re-enter [`with`] from inside `f`: the borrow is held for
/// the closure's whole body and a nested read panics rather than deadlocks.
pub fn with_mut<R>(f: impl FnOnce(&mut SchemaCache) -> R) -> R {
    SCHEMAS.with_borrow_mut(f)
}

/// Empty this isolate's descriptor store.
///
/// The peer of the lane and context resets, called from the same helper. See
/// `zeroship_plugin_db::reset_context_for_tests` for why a reset is needed
/// WITHIN one test even though libtest gives each test its own thread.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_tests() {
    with_mut(|c| *c = SchemaCache::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(app: &str, deploy: &str) -> DbBinding {
        let schema = zeroship_data_sql::SchemaName::new(app).expect("fixture schema name");
        DbBinding::new(app, deploy, schema)
    }

    #[test]
    fn replace_scopes_to_one_binding_and_drops_removed_collections() {
        let mut cache = SchemaCache::new();
        let a = binding("app_a", "d1");
        let b = binding("app_b", "d1");

        cache.replace_for_binding(
            &a,
            vec![
                ("users".into(), zeroship_data_sql::value!({"v": 1})),
                ("posts".into(), zeroship_data_sql::value!({"v": 1})),
            ],
        );
        cache.replace_for_binding(
            &b,
            vec![("users".into(), zeroship_data_sql::value!({"v": 9}))],
        );

        // Replacing app_a with a SHORTER list must drop `posts` and must not
        // touch app_b - the retain is prefix-scoped, which is the property a
        // plain `clear()` would break.
        cache.replace_for_binding(
            &a,
            vec![("users".into(), zeroship_data_sql::value!({"v": 2}))],
        );

        assert_eq!(
            cache.get(&a, "users").as_deref(),
            Some(&zeroship_data_sql::value!({"v": 2})),
            "the surviving collection must carry the NEW value"
        );
        assert!(
            cache.get(&a, "posts").is_none(),
            "a collection absent from the replacement must become unreachable"
        );
        assert_eq!(
            cache.get(&b, "users").as_deref(),
            Some(&zeroship_data_sql::value!({"v": 9})),
            "a different binding must be untouched by the replace"
        );
    }

    #[test]
    fn deploy_token_is_part_of_the_key_so_a_stale_deploy_cannot_be_read() {
        let mut cache = SchemaCache::new();
        let old = binding("app_a", "deploy_1");
        let new = binding("app_a", "deploy_2");

        cache.replace_for_binding(
            &old,
            vec![("users".into(), zeroship_data_sql::value!({"v": 1}))],
        );

        assert!(
            cache.get(&new, "users").is_none(),
            "the same app at a NEW deploy must not read the previous deploy's entry"
        );
        assert_eq!(
            cache.entries_for_binding(&new).len(),
            0,
            "and the enumeration must agree with the point lookup"
        );
        assert_eq!(cache.entries_for_binding(&old).len(), 1);
    }

    #[test]
    fn entries_for_binding_strips_the_prefix_and_returns_bare_collection_names() {
        let mut cache = SchemaCache::new();
        let a = binding("app_a", "d1");
        cache.replace_for_binding(
            &a,
            vec![
                ("users".into(), zeroship_data_sql::value!({})),
                ("posts".into(), zeroship_data_sql::value!({})),
            ],
        );

        let mut names: Vec<String> = cache
            .entries_for_binding(&a)
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["posts".to_string(), "users".to_string()],
            "callers mint a Collection per NAME, so the key prefix must not leak"
        );
    }

    /// **L24 regression guard.** An undeclared collection must be an ERROR, not
    /// `None` that a caller can shrug off - treating a miss as "carry on" is
    /// what let a read served before the schema arrived return every physical
    /// column and accept any field name.
    ///
    /// The guard lives beside the rule now. It was in the adapter's
    /// `descriptor.rs` tests, one indirection away from the code it protects.
    #[test]
    fn an_undeclared_collection_is_an_error_and_never_a_silent_miss() {
        let mut cache = SchemaCache::new();
        let a = binding("app_a", "d1");
        cache.replace_for_binding(
            &a,
            vec![("users".into(), zeroship_data_sql::value!({"v": 1}))],
        );

        let err = cache
            .require(&a, "posts")
            .expect_err("an undeclared collection must not resolve");
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("collection_not_declared"),
            "the miss must carry the typed code callers branch on, got: {rendered}"
        );

        // And the declared one still resolves, so the guard is not just
        // asserting that everything fails.
        assert!(cache.require(&a, "users").is_ok());
    }

    #[test]
    fn empty_replacement_leaves_the_binding_schema_less() {
        let mut cache = SchemaCache::new();
        let a = binding("app_a", "d1");
        cache.replace_for_binding(&a, vec![("users".into(), zeroship_data_sql::value!({}))]);
        cache.replace_for_binding(&a, vec![]);
        assert!(cache.get(&a, "users").is_none());
        assert!(cache.entries_for_binding(&a).is_empty());
    }
}
