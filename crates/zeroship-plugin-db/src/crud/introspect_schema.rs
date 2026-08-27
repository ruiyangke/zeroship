//! Runtime data-access metadata from LIVE introspection.
//!
//! Per the schema-authority split (`docs/proposals/2026-06-18-schema-authority-
//! drizzle-model-design.md` §6), plugin-db learns a collection's column
//! behaviour — types (for read coercions), which columns are `encrypted`
//! (mode/keyId/wraps), which are `masked` (kind/classification) — by
//! **introspecting the live catalog + parsing the engine's sentinels**, NOT by
//! consulting the in-memory declared schema. This module turns the shared
//! introspector's [`zeroship_schema::diff::LiveSchema`] into the SAME
//! `{ <col>: { type, encrypted?, mask? } }` JSON shape the CRUD encryption +
//! mask passes already consume, so those passes are byte-identical whether their
//! metadata came from `registerModel`'s declared schema (the old source) or from
//! introspection (the new one).
//!
//! Caching: the per-worker-thread context keys each entry by
//! `(DbBinding { app_id, deploy_token }, collection)`. An absent collection is
//! cached as a NEGATIVE result (`None`) so it is not re-introspected each
//! call. Cold misses are singleflight per exact key. One successful catalog
//! read admits the requested collection plus at most 31 uncached siblings.
//!
//! `mint_db` captures `ZEROSHIP_DEPLOY_ID` (= the app's `deploy_hash`) from the
//! active runtime's own environment and stores that immutable [`DbBinding`] on
//! the `Db` and every `Collection` wrapper it mints. The binding is then passed
//! through each asynchronous CRUD continuation into this resolver. It is not
//! recovered from process-global environment or app-keyed thread-local state:
//! either would be wrong when one worker thread keeps current and deploy-pinned
//! isolates of the same app alive together.
//!
//! ## Recoverability gap (flagged, not papered over)
//!
//! Live introspection recovers EVERYTHING the encryption + mask passes need
//! (`encrypted` mode/keyId/wraps from the `zsenc` comment; `mask`
//! kind/classification from the `__zsmask` comment — both byte-exact). The one
//! thing it cannot recover with full token fidelity is the *logical DSL type*
//! for the JSON-backed family (`json`/`object`/`array`/`union` all collapse to
//! physical `jsonb`) and the date family (`date`/`calendarDate` both
//! `timestamptz`/`date`). This is BEHAVIOURALLY inert for the read pipeline: the
//! `normalize_row_on_read` coercion routes the whole jsonb family through
//! `normalize_json_value` and the whole date family through
//! `normalize_timestamp_value`, so mapping the families to their representative
//! token (`json` / `date`) yields identical coercion. The exact sub-token is not
//! observable post-coercion. (PG is the engine's target backend — design §10;
//! the SQLite dev-tier introspector is a documented follow-up.)

use std::future::Future;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::binding::DbBinding;
use crate::live_metadata::CachedFacts;
use crate::diff::{ColumnInfo, EncryptionMeta, LiveSchema, MaskMeta, WrappedType};
use crate::error::DbError;

/// Maximum number of cache entries one whole-catalog read may admit.
///
/// The measured 16-column fixture costs at least 3,830 structural bytes per
/// entry, so 32 such entries are about 120 KiB before allocator and map
/// overhead. The cap still covers the repository's largest documented
/// first-party domain (31 billing tables) in one read.
const MAX_COLLECTIONS_PER_POPULATE: usize = 32;

/// Cancellation-safe ownership of one per-thread introspection flight.
///
/// The `Rc` marker makes the guard explicitly thread-bound: releasing it on a
/// different OS thread would address the wrong thread-local context.
struct SchemaIntrospectionGuard {
    binding: DbBinding,
    collection: String,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl SchemaIntrospectionGuard {
    fn new(binding: &DbBinding, collection: &str) -> Self {
        Self {
            binding: binding.clone(),
            collection: collection.to_string(),
            _thread_bound: std::marker::PhantomData,
        }
    }
}

impl Drop for SchemaIntrospectionGuard {
    fn drop(&mut self) {
        let waiters = crate::context::with_mut(|ctx| {
            ctx.finish_schema_introspection(&self.binding, &self.collection)
        });
        for waiter in waiters {
            waiter.wake();
        }
    }
}

/// Resolve the runtime data-access schema for `(app_id, collection)` from the
/// LIVE catalog + sentinels, with binding-keyed caching.
///
/// A live Postgres collection returns `Ok(Some(schema))` with every column,
/// including when it has no encrypted or masked columns, so schema-driven read
/// coercions still run. An absent or never-registered collection returns
/// `Ok(None)`.
///
/// On a backend without a PG pool, the SQLite dev tier returns the declared
/// schema cache result. Its live introspector remains a documented follow-up.
pub(crate) async fn runtime_schema_for(
    binding: &DbBinding,
    collection: &str,
) -> Result<CachedFacts, DbError> {
    let app_id = binding.app_id();
    // Behaviour-identity gate: the OLD `schema_for` returned `Some(schema)` ONLY
    // when `register_model` had cached it on this worker thread, and `None`
    // otherwise
    // (the deliberate cold-schema contract — raw-JS / pre-register reads stay
    // lossless instead of guessing). We preserve that EXACTLY: introspection
    // only sources a schema once the model is registered this deploy. A
    // never-registered collection (raw insert path) stays cold (`None`), so the
    // read/write passes behave identically to how they did before this module
    // existed.
    if !crate::context::with(|c| c.is_model_registered(app_id, collection)) {
        return Ok(None);
    }

    // Fast path: cache hit under the immutable binding captured from the
    // Collection's owning isolate. No thread-ambient lookup participates.
    if let Some(cached) = crate::context::with(|c| c.introspected_schema_for(binding, collection)) {
        return Ok(cached);
    }

    // Cache miss or stale → introspect the live catalog. PG only; SQLite has no
    // `Pool`, so we fall back to the declared schema cache (the SQLite-arm
    // introspector is the flagged dev-tier follow-up — design §9/§10).
    let Some(pool) = crate::context::with(|c| c.pool()) else {
        return Ok(sqlite_fallback_schema(app_id, collection));
    };

    let app_id = app_id.to_string();
    resolve_cache_miss_with_reader(binding, collection, move || async move {
        crate::diff::read_live_schema(pool.as_ref(), &app_id)
            .await
            .map_err(DbError::from)
    })
    .await
}

/// Resolve a catalog-backed miss using an injected reader.
///
/// Keeping the reader behind this seam lets the concurrency tests count and
/// control catalog reads without weakening the assertion to cache side effects.
async fn resolve_cache_miss_with_reader<Read, ReadFuture>(
    binding: &DbBinding,
    collection: &str,
    read: Read,
) -> Result<CachedFacts, DbError>
where
    Read: FnOnce() -> ReadFuture,
    ReadFuture: Future<Output = Result<LiveSchema, DbError>>,
{
    let state = std::future::poll_fn(|cx| {
        crate::context::with_mut(|ctx| ctx.poll_schema_introspection(binding, collection, cx))
    })
    .await;
    match state {
        crate::context::SchemaIntrospectionState::Cached(schema) => return Ok(schema),
        crate::context::SchemaIntrospectionState::Acquired => {}
    }
    let _flight = SchemaIntrospectionGuard::new(binding, collection);

    let live = read().await?;

    // Cache the requested collection and a bounded set of siblings this read
    // already covered under the same deploy token.
    //
    // `read_live_schema` has no table predicate: it selects every column of
    // every table in the app's schema, joining `pg_attribute`/`pg_class`/
    // `pg_namespace`, LEFT JOINing `pg_attrdef` and `pg_description`, with a
    // correlated subquery per column. Caching one slice of that meant an app
    // with N collections paid N whole-schema catalog reads on cold start to
    // learn what a single read had already returned - quadratic work for
    // linear information.
    //
    // Cold start is the cost that matters at platform scale: the long tail is
    // rarely-hit apps, so a large share of requests are cold. It is also
    // invisible to any benchmark that warms one app and then measures steady
    // state, which is how this would be measured by default.
    // `with`, not `with_mut`: the cache is process-wide and carries its own
    // synchronisation, so publishing a read is no longer a mutation of this
    // thread's state. The singleflight marker still is, and is released by
    // `_flight` above.
    let schema =
        crate::context::with(|c| cache_every_collection_and_request(c, binding, &live, collection));
    Ok(schema)
}

/// Populate the deploy-keyed cache for the requested collection and a bounded
/// set of creator-collection siblings present in one `LiveSchema` read.
///
/// Split out from the caller so it is unit-testable against a `LiveSchema`
/// fixture. The requested entry always consumes the first admission, including
/// when the catalog does not contain it. Remaining admissions prefer siblings
/// not already cached under this binding.
///
/// The second half is not a special case, it is the negative-caching contract
/// this module has always had: `build_runtime_schema` returns `None` for a
/// collection that is registered but absent, and that `None` must be
/// remembered. Populating only `live.tables` silently drops it, and the cost is
/// not one extra read - it is a whole-schema catalog read on **every**
/// subsequent operation for that collection, because nothing ever caches the
/// miss.
fn cache_every_collection_and_request(
    ctx: &crate::context::ThreadDbContext,
    binding: &DbBinding,
    live: &LiveSchema,
    requested: &str,
) -> CachedFacts {
    // Reserve the first admission for the requested collection. It must be
    // cached even when absent, or every later operation would repeat the whole
    // catalog read forever.
    let mut admissions = usize::from(!ctx.has_introspected_schema(binding, requested));
    // Return the PUBLISHED entry, not the local `Arc` just built. A second
    // thread racing this same cold miss walks the catalog too - the singleflight
    // is per thread by design - and builds its own structurally equal `Arc`.
    // Handing each caller its own allocation left the two of them holding
    // different objects for one immutable fact, and the map holding whichever
    // published last. `cache_introspected_schema` resolves that under one write
    // lock and hands both the winner.
    let requested_schema = ctx.cache_introspected_schema(
        binding,
        requested,
        build_runtime_schema(live, requested).map(Arc::new),
    );

    for table in live.tables.keys() {
        if table == requested
            || table.starts_with("__zeroship")
            || table.starts_with("__zs_")
            || ctx.has_introspected_schema(binding, table)
        {
            continue;
        }
        if admissions >= MAX_COLLECTIONS_PER_POPULATE {
            break;
        }
        // Internal platform tables are not creator collections. They would
        // never be requested, and caching them would spend the per-populate
        // admission budget on entries nobody reads.
        let schema = build_runtime_schema(live, table).map(Arc::new);
        // One read, one token: every entry from this `LiveSchema` is stamped
        // with the same deploy token, so a redeploy landing mid-populate can
        // never leave entries from two schema versions under one identity.
        // Nothing returns a sibling entry to a caller, so the published object
        // is dropped here on purpose.
        let _published = ctx.cache_introspected_schema(binding, table, schema);
        admissions += 1;
    }
    requested_schema
}

/// SQLite dev-tier fallback (the documented gap, design §9/§10): the shared
/// `read_live_schema` is PG-only (`pg_catalog`), so on the SQLite backend we
/// fall back to the declared schema cache `register_model` populated. This keeps
/// the dev tier working until the SQLite-arm introspector (regex over
/// `sqlite_master.sql`, which already recovers mask sentinels) is wired in.
fn sqlite_fallback_schema(app_id: &str, collection: &str) -> CachedFacts {
    crate::context::with(|c| c.schema_for(app_id, collection)).map(Arc::new)
}

/// Build the declared-shape `{ <col>: { type, encrypted?, mask? } }` JSON for
/// one collection from the introspected [`LiveSchema`], or `None` if the
/// collection is ABSENT from the live catalog.
///
/// When the collection exists, EVERY parent column becomes a schema field with
/// its DSL `type` (so the read-coercion pass runs identically to the declared
/// source — see the module note on the inert jsonb/date token collapse), plus an
/// `encrypted` / `mask` block for any column carrying that sentinel. Returning
/// the full column set (not just the goodie columns) is what keeps the
/// non-goodie read coercions byte-identical to the old declared-schema source.
///
/// The hidden `<col>_masked` sibling columns are NOT emitted as schema fields —
/// the mask pass derives the sibling name from the parent (`{col}_masked`), and
/// `read_live_schema` already stamps the parent's `MaskMeta` from the sibling's
/// `__zsmask` sentinel. Only parent columns become schema fields.
fn build_runtime_schema(live: &LiveSchema, collection: &str) -> Option<Value> {
    let cols = live.tables.get(collection)?;
    let mut out = Map::new();
    let mut has_goodie = false;

    for (name, info) in cols {
        // Skip the hidden mask siblings — they are not creator-visible fields and
        // the mask pass re-derives them from the parent.
        if name.ends_with("_masked") {
            continue;
        }
        let def = column_to_def(info, &mut has_goodie);
        out.insert(name.clone(), def);
    }
    let _ = has_goodie; // the schema is returned whenever the table exists.

    Some(Value::Object(out))
}

/// Map one introspected [`ColumnInfo`] to the `{ type, encrypted?, mask? }`
/// field def. Sets `*has_goodie` when the column carries an encryption or mask
/// sentinel — the signal that the collection needs a runtime schema at all.
fn column_to_def(info: &ColumnInfo, has_goodie: &mut bool) -> Value {
    let mut def = Map::new();

    // The logical DSL type token. An encrypted column's physical type is BYTEA;
    // its def carries the `encrypted` block (the encrypt pass keys on that, and
    // the read coercion explicitly skips encrypted columns), so we still record
    // its `wraps`-derived logical type for completeness / parity with the
    // declared schema.
    def.insert("type".into(), Value::String(dsl_type_for(info)));

    if let Some(enc) = &info.encryption {
        *has_goodie = true;
        def.insert("encrypted".into(), encryption_to_json(enc));
    }
    if let Some(mask) = &info.mask {
        *has_goodie = true;
        def.insert("mask".into(), mask_to_json(mask));
    }

    Value::Object(def)
}

/// Map the introspected physical type (+ encryption wrap) to the DSL type token
/// the read-coercion pass keys on. See the module-level recoverability note: the
/// jsonb / date families collapse to their representative token, which is
/// behaviourally identical post-coercion.
fn dsl_type_for(info: &ColumnInfo) -> String {
    // An encrypted column's logical type is its `wraps`; the physical BYTEA is an
    // implementation detail the encrypt pass owns.
    if let Some(enc) = &info.encryption {
        return match enc.wraps {
            WrappedType::String => "string",
            WrappedType::Number => "number",
            WrappedType::Bytes => "bytes",
        }
        .to_string();
    }
    // `format_type(...)` spellings from `read_live_schema`.
    let t = info.pg_type.to_ascii_lowercase();
    match t.as_str() {
        "boolean" => "boolean",
        "jsonb" | "json" => "json",
        "bytea" => "bytes",
        "timestamp with time zone" | "timestamptz" | "date" => "date",
        "double precision" | "numeric" | "integer" | "bigint" | "real" => "number",
        // text / varchar / char / everything else → string (the read coercion
        // has no string-specific branch, so any non-listed type is inert here).
        _ => "string",
    }
    .to_string()
}

/// `EncryptionMeta` → the `{ mode, keyId, wraps }` JSON the encryption pass
/// reads (`parse_mode` / `keyId` / `parse_wraps`). Spellings match the SDK wire
/// shape so the pass behaves identically to the declared-schema source.
fn encryption_to_json(enc: &EncryptionMeta) -> Value {
    use crate::backend::EncryptionMode;
    let mode = match enc.mode {
        EncryptionMode::Randomised => "randomised",
        EncryptionMode::Deterministic => "deterministic",
    };
    let wraps = match enc.wraps {
        WrappedType::String => "string",
        WrappedType::Number => "number",
        WrappedType::Bytes => "bytes",
    };
    json!({ "mode": mode, "keyId": enc.key_id, "wraps": wraps })
}

/// `MaskMeta` → the `{ kind, classification }` JSON the mask pass reads. Uses the
/// shared codec's canonical SDK-wire strings (`MaskKind::as_sql` /
/// `Classification::as_sql`) so the mask transform is byte-identical to the
/// declared-schema source.
fn mask_to_json(mask: &MaskMeta) -> Value {
    json!({
        "kind": mask.kind.as_sql(),
        "classification": mask.classification.as_sql(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::EncryptionMode;
    use crate::diff::{Classification, MaskKind};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::task::Poll;

    fn binding(deploy_token: &str) -> DbBinding {
        DbBinding::new("app_1", deploy_token)
    }

    fn col(pg_type: &str) -> ColumnInfo {
        ColumnInfo {
            pg_type: pg_type.into(),
            not_null: false,
            ..Default::default()
        }
    }

    fn live_with(collection: &str, cols: Vec<(&str, ColumnInfo)>) -> LiveSchema {
        let mut tables = HashMap::new();
        let mut m = HashMap::new();
        for (name, info) in cols {
            m.insert(name.to_string(), info);
        }
        tables.insert(collection.to_string(), m);
        LiveSchema {
            tables,
            ..Default::default()
        }
    }

    #[test]
    fn goodie_free_collection_still_returns_full_schema_for_read_coercions() {
        // A plain collection (no encrypted/masked column) still yields a schema
        // carrying every column's type, so the read-coercion pass runs
        // identically to the old declared-schema source.
        let live = live_with(
            "notes",
            vec![("title", col("text")), ("flag", col("boolean"))],
        );
        let schema = build_runtime_schema(&live, "notes").expect("table exists → Some");
        assert_eq!(schema["title"]["type"], "string");
        assert_eq!(schema["flag"]["type"], "boolean");
        assert!(schema["title"].get("encrypted").is_none());
        assert!(schema["title"].get("mask").is_none());
    }

    fn live_with_tables(specs: Vec<(&str, Vec<(&str, ColumnInfo)>)>) -> LiveSchema {
        let mut tables = HashMap::new();
        for (name, cols) in specs {
            let mut m = HashMap::new();
            for (c, info) in cols {
                m.insert(c.to_string(), info);
            }
            tables.insert(name.to_string(), m);
        }
        LiveSchema {
            tables,
            ..Default::default()
        }
    }

    fn live_with_numbered_tables(count: usize) -> (LiveSchema, Vec<String>) {
        let names: Vec<String> = (0..count).map(|i| format!("table_{i:04}")).collect();
        let tables = names
            .iter()
            .map(|name| {
                let cols = HashMap::from([("value".to_string(), col("text"))]);
                (name.clone(), cols)
            })
            .collect();
        (
            LiveSchema {
                tables,
                ..Default::default()
            },
            names,
        )
    }

    /// A fresh context bound to a database of this test's own.
    ///
    /// Constructing a `ThreadDbContext` no longer implies a fresh metadata
    /// cache: the cache is process-wide and `DbService` owns it. Two tests
    /// using the same app id and deploy token would otherwise read each
    /// other's entries. Binding each to its own URL gives it its own
    /// `DbResourceKey`, which is the identity the cache partitions on - the
    /// same mechanism that keeps two databases apart in production, exercised
    /// here rather than worked around.
    fn ctx_for(test: &str) -> crate::context::ThreadDbContext {
        let mut ctx = crate::context::ThreadDbContext::new();
        let url = format!("postgres://introspect-fixture/{test}");
        ctx.install_db_resources(
            &url,
            crate::service::DbResourceKey::for_url(&url),
            crate::service::select_backend(&url).expect("fixture URL must be valid"),
            crate::live_metadata::process_wide(),
        );
        ctx
    }

    fn cached_fixture_count(
        ctx: &crate::context::ThreadDbContext,
        binding: &DbBinding,
        names: &[String],
    ) -> usize {
        assert!(
            !names.is_empty(),
            "cache-admission fixture must contain creator tables"
        );
        names
            .iter()
            .filter(|name| ctx.introspected_schema_for(binding, name).is_some())
            .count()
    }

    async fn yield_once() {
        let mut yielded = false;
        std::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    /// One live-schema read must populate the cache for every creator
    /// collection it covered when the fixture fits within the admission cap,
    /// not only the one that triggered it.
    ///
    /// `read_live_schema` reads the whole app schema regardless of which
    /// collection was asked for, so caching a single slice made an app with N
    /// collections perform N whole-schema catalog reads on cold start to
    /// obtain what one read already returned.
    #[test]
    fn one_read_populates_every_collection() {
        let live = live_with_tables(vec![
            ("notes", vec![("title", col("text"))]),
            ("users", vec![("email", col("text"))]),
            ("todos", vec![("done", col("boolean"))]),
        ]);
        let ctx = ctx_for("one_read_populates_every_collection");

        let deploy_a = binding("deploy_a");
        let _ = cache_every_collection_and_request(&ctx, &deploy_a, &live, "notes");

        // Assert the INNER value, not just that a cache entry exists.
        // `introspected_schema_for` returns `Option<Option<_>>` -- the outer
        // is "was it cached", the inner is "does the collection exist" -- so a
        // bare `.is_some()` on the outer stays green if populate-all cached
        // every table as a NEGATIVE. Each fixture carries a distinct column,
        // so checking for it also rules out entries cross-wired between
        // sibling collections, which a presence-only check cannot see.
        for (coll, expected_col) in [("notes", "title"), ("users", "email"), ("todos", "done")] {
            let cached = ctx
                .introspected_schema_for(&deploy_a, coll)
                .unwrap_or_else(|| panic!("'{coll}' must be cached by the single read that covered it"))
                .unwrap_or_else(|| panic!("'{coll}' was cached as ABSENT, but the read covered it"));
            assert!(
                cached.to_string().contains(expected_col),
                "'{coll}' cached a schema without its own column '{expected_col}': {cached}",
            );
        }
    }

    /// Internal platform tables are not creator collections: caching them
    /// would spend the per-populate admission budget on entries nothing ever
    /// requests.
    ///
    /// Production skips TWO prefixes (`__zeroship` and `__zs_`), so this arm
    /// supplies a fixture for each. Covering only one left the other's
    /// `starts_with` deletable with the test still green - which is how it was
    /// written first, and what a mutation check caught.
    #[test]
    fn internal_tables_are_not_cached_as_collections() {
        let live = live_with_tables(vec![
            ("notes", vec![("title", col("text"))]),
            ("__zeroship_audit_unmask", vec![("actor", col("text"))]),
            ("__zs_mask_policy", vec![("kind", col("text"))]),
        ]);
        let ctx = ctx_for("internal_tables_are_not_cached_as_collections");

        let deploy_a = binding("deploy_a");
        let _ = cache_every_collection_and_request(&ctx, &deploy_a, &live, "notes");

        assert!(ctx.introspected_schema_for(&deploy_a, "notes").is_some());
        for internal in ["__zeroship_audit_unmask", "__zs_mask_policy"] {
            assert!(
                ctx.introspected_schema_for(&deploy_a, internal).is_none(),
                "internal table {internal} must not occupy the per-app cache",
            );
        }
    }

    /// Every entry from one read carries the SAME deploy token. Asserted by
    /// reading them back under a DIFFERENT token, which must miss uniformly
    /// rather than partially.
    ///
    /// WHAT THIS DOES NOT CATCH. It passes a constant token into a fresh local
    /// context: there is no `mint_db`, no shared thread-local context, no
    /// await, and no second runtime. So it cannot see any race, and in
    /// particular it stayed green under the pre-fix deploy-token identity bug,
    /// where `deploy_tokens` was keyed by `app_id` alone and a second runtime
    /// of the SAME app at a different deploy overwrote the first's token
    /// last-writer-wins. This comment used to claim the test showed "a
    /// redeploy landing mid-populate cannot leave two schema versions under one
    /// identity", which is precisely the property it is blind to. Uniform
    /// stamping within one populate call is a real but much narrower guarantee,
    /// and it is all that is asserted here.
    #[test]
    fn one_read_stamps_one_token() {
        let live = live_with_tables(vec![
            ("notes", vec![("title", col("text"))]),
            ("users", vec![("email", col("text"))]),
        ]);
        let ctx = ctx_for("one_read_stamps_one_token");

        let deploy_a = binding("deploy_a");
        let deploy_b = binding("deploy_b");
        let _ = cache_every_collection_and_request(&ctx, &deploy_a, &live, "notes");

        for coll in ["notes", "users"] {
            // The HIT half is not padding. An earlier version of this test
            // asserted only the miss, which passes just as happily when
            // nothing was cached at all - the deny-only shape. Mutation-testing
            // it caught that: with the population reduced to one collection the
            // miss-only assertion stayed green, so it was measuring the token
            // rule while blind to whether anything had been stored.
            assert!(
                ctx.introspected_schema_for(&deploy_a, coll).is_some(),
                "'{coll}' must be cached under the token it was stamped with",
            );
            assert!(
                ctx.introspected_schema_for(&deploy_b, coll).is_none(),
                "'{coll}' must miss under a different deploy token",
            );
        }
    }

    /// A collection that is REGISTERED but absent from the live catalog must
    /// still be cached, as a negative result.
    ///
    /// This is the arm the populate-all rewrite regressed. The original cached
    /// `build_runtime_schema`'s `Option` for the requested collection
    /// unconditionally, so an absent collection was remembered as `None` and
    /// cost one introspection. Iterating `live.tables` instead caches only
    /// tables that EXIST, so an absent one is never remembered and re-reads the
    /// whole app schema on every operation, forever - strictly worse on that
    /// path than the cold-start defect the rewrite was fixing.
    ///
    /// It is not an exotic state: it is the ordinary first-deploy window before
    /// the migration lands, and any drift or rolling deploy where a registered
    /// collection is not yet in the catalog.
    #[test]
    fn absent_collection_is_cached_as_a_negative_result() {
        let live = live_with_tables(vec![("notes", vec![("title", col("text"))])]);
        let ctx = ctx_for("absent_collection_is_cached_as_a_negative_result");

        let deploy_a = binding("deploy_a");
        let _ = cache_every_collection_and_request(&ctx, &deploy_a, &live, "ghosts");

        assert_eq!(
            ctx.introspected_schema_for(&deploy_a, "ghosts"),
            Some(None),
            "an absent collection must be cached as a negative result, not left \
             uncached to re-introspect on every op",
        );
        // The present collection is still populated by the same call.
        assert!(ctx
            .introspected_schema_for(&deploy_a, "notes")
            .is_some());
    }

    #[test]
    fn one_populate_admits_at_most_the_per_populate_cap() {
        let (live, names) = live_with_numbered_tables(500);
        assert_eq!(
            MAX_COLLECTIONS_PER_POPULATE, 32,
            "the reviewed per-populate security ceiling must stay explicit"
        );
        assert!(names.len() > MAX_COLLECTIONS_PER_POPULATE);

        let ctx = ctx_for("one_populate_admits_at_most_the_per_populate_cap");
        let neighbour = DbBinding::new("app_neighbour", "deploy_neighbour");
        let neighbour_schema = Some(Arc::new(json!({ "marker": { "type": "string" } })));
        ctx.cache_introspected_schema(&neighbour, "keep_me", neighbour_schema.clone());

        let attacker = DbBinding::new("app_attacker", "deploy_attacker");
        let _ = cache_every_collection_and_request(&ctx, &attacker, &live, &names[0]);

        assert_eq!(
            cached_fixture_count(&ctx, &attacker, &names),
            MAX_COLLECTIONS_PER_POPULATE,
            "one app's single populate must have a fixed admission ceiling"
        );
        assert_eq!(
            ctx.introspected_schema_for(&neighbour, "keep_me"),
            Some(neighbour_schema),
            "bounding one tenant's populate must not disturb a co-resident tenant"
        );
    }

    #[test]
    fn requested_present_collection_is_cached_when_cap_is_hit() {
        let (live, names) = live_with_numbered_tables(500);
        assert!(names.len() > MAX_COLLECTIONS_PER_POPULATE);
        let requested = live
            .tables
            .keys()
            .nth(MAX_COLLECTIONS_PER_POPULATE)
            .cloned()
            .expect("fixture must have a table beyond the capped iteration prefix");
        let ctx = ctx_for("requested_present_collection_is_cached_when_cap_is_hit");
        let deploy = DbBinding::new("app_requested_present", "deploy_a");

        let _ = cache_every_collection_and_request(&ctx, &deploy, &live, &requested);

        let cached = ctx
            .introspected_schema_for(&deploy, &requested)
            .expect("requested collection must be admitted even at the cap")
            .expect("requested present collection must not be cached as absent");
        assert_eq!(cached["value"]["type"], "string");
        assert_eq!(
            cached_fixture_count(&ctx, &deploy, &names),
            MAX_COLLECTIONS_PER_POPULATE
        );
    }

    #[test]
    fn requested_absent_collection_is_cached_when_cap_is_hit() {
        let (live, names) = live_with_numbered_tables(500);
        assert!(names.len() > MAX_COLLECTIONS_PER_POPULATE);
        let ctx = ctx_for("requested_absent_collection_is_cached_when_cap_is_hit");
        let deploy = DbBinding::new("app_requested_absent", "deploy_a");

        let _ = cache_every_collection_and_request(&ctx, &deploy, &live, "ghosts");

        assert_eq!(
            ctx.introspected_schema_for(&deploy, "ghosts"),
            Some(None),
            "an absent requested collection must retain its negative entry at the cap"
        );
        assert_eq!(
            cached_fixture_count(&ctx, &deploy, &names) + 1,
            MAX_COLLECTIONS_PER_POPULATE,
            "the requested negative entry must consume one of the bounded admissions"
        );
    }

    /// TWO OS THREADS racing ONE cold miss must end up holding ONE fact object.
    ///
    /// The singleflight is per thread by design, so both threads legitimately
    /// walk the catalog and both build an `Arc` of their own. What they must not
    /// do is return those two `Arc`s: the entry's identity is immutable, so one
    /// immutable fact would exist as two allocations, the map would keep
    /// whichever published last, and the object a caller holds would be
    /// unrelated to the object every subsequent reader gets.
    ///
    /// **This is a real race, not a sequential fixture, and that distinction is
    /// the reason the arm exists.** `a_second_worker_thread_reads_the_first_ones
    /// _facts_through_its_context` in `live_metadata` is strictly ordered - the
    /// writer thread is `join`ed before the reader thread starts - so the second
    /// thread always takes the cache-hit path and never publishes at all. It
    /// passes on the defective code. Here the barrier holds both threads inside
    /// their readers until both have passed their singleflight check, so both
    /// reach the publish with a live `Arc` in hand.
    ///
    /// It is deterministic in both directions: on the pre-fix code each thread
    /// returns the allocation it built itself, so the pointers differ on every
    /// interleaving; on the fixed code `publish` resolves both under one write
    /// lock, so they agree on every interleaving.
    #[test]
    fn two_threads_racing_one_cold_miss_resolve_one_fact_object() {
        const FIXTURE_URL: &str = "postgres://introspect-fixture/cold-miss-race";
        let deploy = DbBinding::new("app_cold_miss_race", "deploy_a");
        // 2, because a barrier of 1 releases immediately and would turn this
        // back into the sequential arm it exists to be distinguishable from.
        let gate = Arc::new(std::sync::Barrier::new(2));

        let racers: Vec<_> = (0..2)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let deploy = deploy.clone();
                std::thread::spawn(move || {
                    // Each thread installs the SAME database, so both resolve
                    // one `DbResourceKey` and therefore one cache identity.
                    crate::context::with_mut(|c| {
                        c.install_db_resources(
                            FIXTURE_URL,
                            crate::service::DbResourceKey::for_url(FIXTURE_URL),
                            crate::service::select_backend(FIXTURE_URL).expect("fixture URL"),
                            crate::live_metadata::process_wide(),
                        );
                    });
                    futures::executor::block_on(resolve_cache_miss_with_reader(
                        &deploy,
                        "notes",
                        move || async move {
                            // Both readers are inside the catalog read before
                            // either publishes.
                            gate.wait();
                            Ok(live_with("notes", vec![("title", col("text"))]))
                        },
                    ))
                })
            })
            .collect();

        let facts: Vec<Arc<Value>> = racers
            .into_iter()
            .map(|racer| {
                racer
                    .join()
                    .expect("racing resolver thread")
                    .expect("catalog read must succeed")
                    .expect("requested present collection must resolve")
            })
            .collect();

        assert_eq!(facts.len(), 2, "the race needs both racers' results");
        assert!(
            Arc::ptr_eq(&facts[0], &facts[1]),
            "two threads racing one cold miss returned two allocations for one \
             immutable fact: {:p} and {:p}",
            Arc::as_ptr(&facts[0]),
            Arc::as_ptr(&facts[1]),
        );
    }

    #[test]
    fn concurrent_cold_resolutions_share_one_catalog_read() {
        let concurrency = 8usize;
        assert!(concurrency > 1, "singleflight fixture must be concurrent");
        let reads = Rc::new(Cell::new(0usize));
        let deploy = DbBinding::new("app_singleflight_success", "deploy_a");

        let operations: Vec<_> = (0..concurrency)
            .map(|_| {
                let reads = Rc::clone(&reads);
                resolve_cache_miss_with_reader(&deploy, "notes", move || async move {
                    reads.set(reads.get() + 1);
                    yield_once().await;
                    Ok(live_with("notes", vec![("title", col("text"))]))
                })
            })
            .collect();
        assert!(
            !operations.is_empty(),
            "singleflight test must drive at least one cold resolution"
        );

        let results = futures::executor::block_on(futures::future::join_all(operations));
        for result in &results {
            let schema = result
                .as_ref()
                .expect("catalog read must succeed")
                .as_ref()
                .expect("requested present collection must resolve");
            assert_eq!(schema["title"]["type"], "string");
        }
        assert_eq!(
            reads.get(),
            1,
            "K concurrent cold resolutions for one cache key must read the catalog once"
        );
    }

    #[test]
    fn failed_catalog_read_is_not_cached_or_shared() {
        let concurrency = 4usize;
        assert!(concurrency > 1, "singleflight fixture must be concurrent");
        let reads = Rc::new(Cell::new(0usize));
        let deploy = DbBinding::new("app_singleflight_retry", "deploy_a");

        let operations: Vec<_> = (0..concurrency)
            .map(|_| {
                let reads = Rc::clone(&reads);
                resolve_cache_miss_with_reader(&deploy, "notes", move || async move {
                    let attempt = reads.get() + 1;
                    reads.set(attempt);
                    yield_once().await;
                    if attempt == 1 {
                        Err(DbError::internal("injected catalog failure"))
                    } else {
                        Ok(live_with("notes", vec![("title", col("text"))]))
                    }
                })
            })
            .collect();
        assert!(!operations.is_empty());

        let results = futures::executor::block_on(futures::future::join_all(operations));
        assert_eq!(
            results.iter().filter(|result| result.is_err()).count(),
            1,
            "only the failed owner must receive the catalog error"
        );
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            concurrency - 1,
            "waiters must retry after a failure instead of sharing it"
        );
        assert_eq!(
            reads.get(),
            2,
            "one failed read must be followed by exactly one successful retry"
        );
    }

    /// MEASUREMENT, not an assertion - run with `--nocapture`.
    ///
    /// The per-populate admission cap has to be a number, and a number nobody
    /// measured is a guess with a decimal point. This reports the serialized
    /// size of one cached entry so the cap can be derived from the actual shape
    /// being stored.
    ///
    /// It measures the DATA STRUCTURE, not a production app: the column count
    /// is varied and the names are realistic-length, but a real creator schema
    /// with encrypted/masked facets stores more per column. Read the result as
    /// a floor.
    #[test]
    fn measure_cached_entry_size() {
        for (label, ncols) in [("narrow", 8usize), ("typical", 16), ("wide", 40)] {
            let owned: Vec<(String, ColumnInfo)> = (0..ncols)
                .map(|i| (format!("column_name_{i}"), col("text")))
                .collect();
            let refs: Vec<(&str, ColumnInfo)> =
                owned.iter().map(|(n, c)| (n.as_str(), c.clone())).collect();
            let live = live_with("collection_name", refs);
            let schema = build_runtime_schema(&live, "collection_name");
            let bytes = serde_json::to_string(&schema).unwrap().len();
            println!(
                "MEASURED {label}: {ncols} cols -> {bytes} bytes ({} b/col)",
                bytes / ncols
            );
        }
    }

    /// MEASUREMENT - run with `--nocapture`.
    ///
    /// The serialized figure above is a floor; the cache stores a live
    /// `serde_json::Value`. This reports the STRUCTURAL heap cost so the
    /// multiplier is a measured quantity rather than the phrase "several
    /// times".
    ///
    /// Counts what the type system fixes: the `Value` enum's own size, the
    /// per-entry cost of the map that backs an object, and the `String` header
    /// for every key. It does NOT include allocator rounding or fragmentation,
    /// so like the serialized figure it is a floor - but a much tighter one.
    #[test]
    fn measure_value_memory_overhead() {
        use std::mem::size_of;
        let value_sz = size_of::<serde_json::Value>();
        let string_sz = size_of::<String>();

        for (label, ncols) in [("narrow", 8usize), ("typical", 16), ("wide", 40)] {
            let owned: Vec<(String, ColumnInfo)> = (0..ncols)
                .map(|i| (format!("column_name_{i}"), col("text")))
                .collect();
            let refs: Vec<(&str, ColumnInfo)> =
                owned.iter().map(|(n, c)| (n.as_str(), c.clone())).collect();
            let live = live_with("collection_name", refs);
            let schema = build_runtime_schema(&live, "collection_name").expect("table exists");

            let json = serde_json::to_string(&schema).unwrap();
            // Outer object: one entry per column. Each entry is a String key
            // plus a Value (itself an object holding `type`, and possibly
            // `encrypted`/`mask`).
            let obj = schema.as_object().expect("object");
            let mut structural = 0usize;
            for (k, v) in obj {
                structural += string_sz + k.len() + value_sz;
                if let Some(inner) = v.as_object() {
                    for (ik, iv) in inner {
                        structural += string_sz + ik.len() + value_sz;
                        if let Some(s) = iv.as_str() {
                            structural += string_sz + s.len();
                        }
                    }
                }
            }
            println!(
                "MEASURED {label}: {ncols} cols -> serialized {} B, structural {} B, ratio {:.1}x",
                json.len(),
                structural,
                structural as f64 / json.len() as f64,
            );
        }
        println!("MEASURED sizeof(serde_json::Value) = {value_sz} B, sizeof(String) = {string_sz} B");
    }

    #[test]
    fn missing_collection_is_none() {
        let live = live_with("notes", vec![("title", col("text"))]);
        assert!(build_runtime_schema(&live, "absent").is_none());
    }

    #[test]
    fn encrypted_column_maps_to_declared_shape() {
        let mut secret = col("bytea");
        secret.encryption = Some(EncryptionMeta {
            mode: EncryptionMode::Deterministic,
            key_id: "k7".into(),
            wraps: WrappedType::Number,
        });
        let live = live_with("vault", vec![("secret", secret), ("label", col("text"))]);
        let schema = build_runtime_schema(&live, "vault").expect("has goodie");
        let def = &schema["secret"];
        assert_eq!(def["type"], "number", "encrypted wraps drives logical type");
        assert_eq!(def["encrypted"]["mode"], "deterministic");
        assert_eq!(def["encrypted"]["keyId"], "k7");
        assert_eq!(def["encrypted"]["wraps"], "number");
        // The plain column is still present (so read coercions see it) but carries
        // no goodie block.
        assert!(schema["label"].get("encrypted").is_none());
    }

    #[test]
    fn masked_parent_maps_and_sibling_is_dropped() {
        let mut phone = col("text");
        phone.mask = Some(MaskMeta {
            kind: MaskKind::Last4,
            classification: Classification::Pci,
            sibling_column: "phone_masked".into(),
        });
        let live = live_with(
            "vault",
            vec![("phone", phone), ("phone_masked", col("text"))],
        );
        let schema = build_runtime_schema(&live, "vault").expect("has goodie");
        assert_eq!(schema["phone"]["mask"]["kind"], "last4");
        assert_eq!(schema["phone"]["mask"]["classification"], "pci");
        // The hidden sibling is NOT a schema field.
        assert!(
            schema.get("phone_masked").is_none(),
            "the _masked sibling must not appear as a schema field"
        );
    }

    #[test]
    fn jsonb_and_date_families_collapse_to_representative_tokens() {
        let live = live_with(
            "x",
            vec![
                ("j", col("jsonb")),
                ("d", col("timestamp with time zone")),
                ("b", col("boolean")),
                ("n", col("double precision")),
            ],
        );
        // No goodie → None overall, but the per-column mapping is exercised via
        // column_to_def directly:
        let mut hg = false;
        assert_eq!(column_to_def(&col("jsonb"), &mut hg)["type"], "json");
        assert_eq!(
            column_to_def(&col("timestamp with time zone"), &mut hg)["type"],
            "date"
        );
        assert_eq!(column_to_def(&col("boolean"), &mut hg)["type"], "boolean");
        assert_eq!(column_to_def(&col("bytea"), &mut hg)["type"], "bytes");
        assert!(!hg, "plain columns set no goodie flag");
        let _ = live;
    }
}
