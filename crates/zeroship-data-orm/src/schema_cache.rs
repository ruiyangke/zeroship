//! Runtime descriptors owned by an ORM context and keyed by the complete binding.
//! Installation validates before replacing a binding's collection set.

use std::collections::HashMap;
use std::sync::Arc;

use crate::schema::FieldMap;

use crate::error::DbError;

use crate::binding::DbBinding;

/// Collection descriptors keyed by the complete app, deploy and schema binding.
#[derive(Debug, Default)]
pub struct SchemaCache {
    entries: HashMap<(DbBinding, String), Arc<FieldMap>>,
}

impl SchemaCache {
    /// An empty cache. A fresh isolate has installed no schema.
    #[must_use]
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// The store key for one collection under one binding.
    fn key(binding: &DbBinding, collection: &str) -> (DbBinding, String) {
        (binding.clone(), collection.to_owned())
    }

    /// Replace the complete descriptor for one app-at-deploy binding.
    ///
    /// Callers fully validate and collect every entry before calling. The
    /// retain-and-insert sequence is one synchronous publication point: no
    /// callback can observe a partial descriptor, removed collections do not
    /// survive a dev isolate restart, and an empty input leaves a schema-less
    /// binding empty.
    pub fn replace_for_binding(&mut self, binding: &DbBinding, schemas: Vec<(String, FieldMap)>) {
        self.entries.retain(|(owner, _), _| owner != binding);
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
    pub fn get(&self, binding: &DbBinding, collection: &str) -> Option<Arc<FieldMap>> {
        self.entries.get(&Self::key(binding, collection)).cloned()
    }

    /// Every `(collection, schema)` pair held for one BINDING. `mint_tx_view`
    /// uses it to mint one `Collection` per declared name; the drift-check sweep
    /// uses it to walk every declared collection. Empty when the isolate has
    /// installed no schema.
    #[must_use]
    pub fn entries_for_binding(&self, binding: &DbBinding) -> Vec<(String, Arc<FieldMap>)> {
        self.entries
            .iter()
            .filter(|(key, _)| key.0 == *binding)
            .map(|(key, value)| (key.1.clone(), value.clone()))
            .collect()
    }

    /// The descriptor entry for one collection, or a typed error.
    ///
    /// An undeclared collection is denied so projection and identifier checks
    /// cannot run without their schema authority.
    ///
    /// # Errors
    ///
    /// [`DbError::config`] with code `collection_not_declared` when the
    /// descriptor this isolate was built from does not declare `collection`.
    pub fn require(&self, binding: &DbBinding, collection: &str) -> Result<Arc<FieldMap>, DbError> {
        self.get(binding, collection).ok_or_else(|| {
            DbError::config(
                "collection_not_declared",
                format!("collection '{collection}' has no installed ORM schema"),
            )
        })
    }

    /// Install ONE collection's entry. Test fixtures use this narrow helper;
    /// production boot replaces a binding's complete descriptor through
    /// [`Self::replace_for_binding`].
    #[cfg(test)]
    pub fn insert_one(&mut self, binding: &DbBinding, collection: &str, schema: FieldMap) {
        self.entries
            .insert(Self::key(binding, collection), Arc::new(schema));
    }
}

// Accessors for the currently scoped ORM owner.

/// Read the current ORM context's descriptor store.
pub fn with<R>(f: impl FnOnce(&SchemaCache) -> R) -> R {
    crate::orm_context::current().schemas(f)
}

/// Mutate the current ORM context's descriptor store.
///
/// Callers must not re-enter [`with`] from inside `f`: the borrow is held for
/// the closure's whole body and a nested read panics rather than deadlocks.
pub fn with_mut<R>(f: impl FnOnce(&mut SchemaCache) -> R) -> R {
    crate::orm_context::current().schemas_mut(f)
}

/// Empty this isolate's descriptor store.
///
/// Reset alongside lane and context state when a fixture installs another
/// database or deployment on the same thread.
#[cfg(test)]
pub fn reset_for_tests() {
    with_mut(|c| *c = SchemaCache::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(default: i64) -> FieldMap {
        let mut column = crate::schema::ColumnSchema::new(crate::schema::LogicalType::Integer);
        column.default = Some(default.into());
        [("value".into(), column)].into()
    }

    fn binding(app: &str, deploy: &str) -> DbBinding {
        let schema = crate::sql::SchemaName::new(app).expect("fixture schema name");
        DbBinding::new(app, deploy, schema)
    }

    #[test]
    fn replace_scopes_to_one_binding_and_drops_removed_collections() {
        let mut cache = SchemaCache::new();
        let a = binding("app_a", "d1");
        let b = binding("app_b", "d1");

        cache.replace_for_binding(
            &a,
            vec![("users".into(), contract(1)), ("posts".into(), contract(1))],
        );
        cache.replace_for_binding(&b, vec![("users".into(), contract(9))]);

        // Replacing app_a with a SHORTER list must drop `posts` and must not
        // touch app_b - the retain is prefix-scoped, which is the property a
        // plain `clear()` would break.
        cache.replace_for_binding(&a, vec![("users".into(), contract(2))]);

        assert_eq!(
            cache.get(&a, "users").as_deref(),
            Some(&contract(2)),
            "the surviving collection must carry the NEW value"
        );
        assert!(
            cache.get(&a, "posts").is_none(),
            "a collection absent from the replacement must become unreachable"
        );
        assert_eq!(
            cache.get(&b, "users").as_deref(),
            Some(&contract(9)),
            "a different binding must be untouched by the replace"
        );
    }

    #[test]
    fn deploy_token_is_part_of_the_key_so_a_stale_deploy_cannot_be_read() {
        let mut cache = SchemaCache::new();
        let old = binding("app_a", "deploy_1");
        let new = binding("app_a", "deploy_2");

        cache.replace_for_binding(&old, vec![("users".into(), contract(1))]);

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
                ("users".into(), FieldMap::new()),
                ("posts".into(), FieldMap::new()),
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
        cache.replace_for_binding(&a, vec![("users".into(), contract(1))]);

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
        cache.replace_for_binding(&a, vec![("users".into(), FieldMap::new())]);
        cache.replace_for_binding(&a, vec![]);
        assert!(cache.get(&a, "users").is_none());
        assert!(cache.entries_for_binding(&a).is_empty());
    }
}
