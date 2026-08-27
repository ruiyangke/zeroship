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
//! Caching: per-isolate, per-`(app, collection)`, keyed on the app's current
//! deploy/schema-version token. A deploy bump invalidates the entry on next read
//! — mirroring `register_model`'s per-thread `is_model_registered` fast-path but
//! keyed on the deploy version rather than mere presence (design §6). A
//! goodie-free collection is cached as a NEGATIVE result (`None`) so it is not
//! re-introspected each call.
//!
//! The token is the per-`app_id` value the worker injects as
//! `ZEROSHIP_DEPLOY_ID` (= the app's `deploy_hash`), stamped into the per-isolate
//! [`crate::context::IsolateDbContext`] when the `Db` wrapper is minted
//! (`mint_db`). It is read here via [`crate::context::IsolateDbContext::
//! deploy_token_for`], NOT from the process-global `std::env::var` — that env var
//! was never set by any worker/runtime/control vector (so the token was pinned at
//! `"cold_start"` for the isolate's whole life and the deploy-keyed cache never
//! invalidated), and a process-global would in any case be wrong for a multi-app
//! worker thread.
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

use serde_json::{json, Map, Value};

use crate::diff::{ColumnInfo, EncryptionMeta, LiveSchema, MaskMeta, WrappedType};
use crate::error::DbError;

/// Resolve the runtime data-access schema for `(app_id, collection)` from the
/// LIVE catalog + sentinels, with per-isolate deploy-keyed caching.
///
/// Returns `Ok(None)` when the collection has no encrypted/masked columns (the
/// caller then skips the encrypt/mask passes — the schema-driven read coercions
/// also run only when a schema is present, matching the original cold-schema
/// contract). Returns `Ok(Some(schema))` with the declared-shape JSON otherwise.
///
/// On a backend without a PG pool (SQLite), returns `Ok(None)` — the SQLite
/// introspector is the documented dev-tier follow-up (design §9/§10); plugin-db
/// then behaves as it does for a cold schema cache.
pub(crate) async fn runtime_schema_for(
    app_id: &str,
    collection: &str,
) -> Result<Option<Value>, DbError> {
    // Behaviour-identity gate: the OLD `schema_for` returned `Some(schema)` ONLY
    // when `register_model` had cached it on this isolate, and `None` otherwise
    // (the deliberate cold-schema contract — raw-JS / pre-register reads stay
    // lossless instead of guessing). We preserve that EXACTLY: introspection
    // only sources a schema once the model is registered this deploy. A
    // never-registered collection (raw insert path) stays cold (`None`), so the
    // read/write passes behave identically to how they did before this module
    // existed.
    if !crate::context::with(|c| c.is_model_registered(app_id, collection)) {
        return Ok(None);
    }

    // The per-app deploy/schema-version token (the worker-injected
    // `ZEROSHIP_DEPLOY_ID` = `deploy_hash`, stamped at `mint_db`). A redeploy
    // re-mints the wrapper with the new hash, so this token changes and the
    // deploy-keyed cache below invalidates on the next op. See the module note.
    let token = crate::context::with(|c| c.deploy_token_for(app_id));

    // Fast path: per-isolate cache hit under the current deploy token.
    if let Some(cached) =
        crate::context::with(|c| c.introspected_schema_for(app_id, collection, &token))
    {
        return Ok(cached);
    }

    // Cache miss or stale → introspect the live catalog. PG only; SQLite has no
    // `Pool`, so we fall back to the declared schema cache (the SQLite-arm
    // introspector is the flagged dev-tier follow-up — design §9/§10).
    let Some(pool) = crate::context::with(|c| c.pool()) else {
        return Ok(sqlite_fallback_schema(app_id, collection));
    };

    let live = crate::diff::read_live_schema(pool.as_ref(), app_id)
        .await
        .map_err(DbError::from)?;

    let schema = build_runtime_schema(&live, collection);
    // Cache EVERY collection this read already covered, not just the requested
    // one, under the SAME deploy token.
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
    crate::context::with_mut(|c| {
        cache_every_collection_and_request(c, app_id, &token, &live, collection);
    });
    Ok(schema)
}

/// Populate the deploy-keyed cache for every collection present in one
/// `LiveSchema` read.
///
/// Split out from the caller so it is unit-testable against a `LiveSchema`
/// fixture: the property that matters ("one read populates all collections")
/// needs no pool to assert, and asserting it by counting queries would need
/// one.
/// Populate every collection this read covered, **and** the requested one even
/// when the catalog does not contain it.
///
/// The second half is not a special case, it is the negative-caching contract
/// this module has always had: `build_runtime_schema` returns `None` for a
/// collection that is registered but absent, and that `None` must be
/// remembered. Populating only `live.tables` silently drops it, and the cost is
/// not one extra read - it is a whole-schema catalog read on **every**
/// subsequent operation for that collection, because nothing ever caches the
/// miss.
fn cache_every_collection_and_request(
    ctx: &mut crate::context::IsolateDbContext,
    app_id: &str,
    token: &str,
    live: &LiveSchema,
    requested: &str,
) {
    cache_every_collection(ctx, app_id, token, live);
    // `cache_every_collection` skips internal tables and anything absent from
    // the catalog. If the caller asked about a collection in neither set, its
    // result - `None` - still has to be recorded.
    if ctx.introspected_schema_for(app_id, requested, token).is_none() {
        let schema = build_runtime_schema(live, requested);
        ctx.cache_introspected_schema(app_id, requested, token, schema);
    }
}

fn cache_every_collection(
    ctx: &mut crate::context::IsolateDbContext,
    app_id: &str,
    token: &str,
    live: &LiveSchema,
) {
    for table in live.tables.keys() {
        // Internal platform tables are not creator collections. They would
        // never be requested, and caching them would spend the per-app cache
        // budget on entries nobody reads.
        if table.starts_with("__zeroship") || table.starts_with("__zs_") {
            continue;
        }
        let schema = build_runtime_schema(live, table);
        // One read, one token: every entry from this `LiveSchema` is stamped
        // with the same deploy token, so a redeploy landing mid-populate can
        // never leave entries from two schema versions under one identity.
        ctx.cache_introspected_schema(app_id, table, token, schema);
    }
}

/// SQLite dev-tier fallback (the documented gap, design §9/§10): the shared
/// `read_live_schema` is PG-only (`pg_catalog`), so on the SQLite backend we
/// fall back to the declared schema cache `register_model` populated. This keeps
/// the dev tier working until the SQLite-arm introspector (regex over
/// `sqlite_master.sql`, which already recovers mask sentinels) is wired in.
fn sqlite_fallback_schema(app_id: &str, collection: &str) -> Option<Value> {
    crate::context::with(|c| c.schema_for(app_id, collection))
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
    use std::collections::HashMap;

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

    /// One live-schema read must populate the cache for EVERY collection it
    /// covered, not only the one that triggered it.
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
        let mut ctx = crate::context::IsolateDbContext::new();

        cache_every_collection(&mut ctx, "app_1", "deploy_a", &live);

        // Assert the INNER value, not just that a cache entry exists.
        // `introspected_schema_for` returns `Option<Option<_>>` -- the outer
        // is "was it cached", the inner is "does the collection exist" -- so a
        // bare `.is_some()` on the outer stays green if populate-all cached
        // every table as a NEGATIVE. Each fixture carries a distinct column,
        // so checking for it also rules out entries cross-wired between
        // sibling collections, which a presence-only check cannot see.
        for (coll, expected_col) in [("notes", "title"), ("users", "email"), ("todos", "done")] {
            let cached = ctx
                .introspected_schema_for("app_1", coll, "deploy_a")
                .unwrap_or_else(|| panic!("'{coll}' must be cached by the single read that covered it"))
                .unwrap_or_else(|| panic!("'{coll}' was cached as ABSENT, but the read covered it"));
            assert!(
                cached.to_string().contains(expected_col),
                "'{coll}' cached a schema without its own column '{expected_col}': {cached}",
            );
        }
    }

    /// Internal platform tables are not creator collections: caching them
    /// would spend the per-app cache budget on entries nothing ever requests.
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
        let mut ctx = crate::context::IsolateDbContext::new();

        cache_every_collection(&mut ctx, "app_1", "deploy_a", &live);

        assert!(ctx.introspected_schema_for("app_1", "notes", "deploy_a").is_some());
        for internal in ["__zeroship_audit_unmask", "__zs_mask_policy"] {
            assert!(
                ctx.introspected_schema_for("app_1", internal, "deploy_a")
                    .is_none(),
                "internal table {internal} must not occupy the per-app cache",
            );
        }
    }

    /// Every entry from one read carries the SAME deploy token, so a redeploy
    /// landing mid-populate cannot leave two schema versions under one
    /// identity. Asserted by reading them back under a DIFFERENT token, which
    /// must miss uniformly rather than partially.
    #[test]
    fn one_read_stamps_one_token() {
        let live = live_with_tables(vec![
            ("notes", vec![("title", col("text"))]),
            ("users", vec![("email", col("text"))]),
        ]);
        let mut ctx = crate::context::IsolateDbContext::new();

        cache_every_collection(&mut ctx, "app_1", "deploy_a", &live);

        for coll in ["notes", "users"] {
            // The HIT half is not padding. An earlier version of this test
            // asserted only the miss, which passes just as happily when
            // nothing was cached at all - the deny-only shape. Mutation-testing
            // it caught that: with the population reduced to one collection the
            // miss-only assertion stayed green, so it was measuring the token
            // rule while blind to whether anything had been stored.
            assert!(
                ctx.introspected_schema_for("app_1", coll, "deploy_a").is_some(),
                "'{coll}' must be cached under the token it was stamped with",
            );
            assert!(
                ctx.introspected_schema_for("app_1", coll, "deploy_b").is_none(),
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
        let mut ctx = crate::context::IsolateDbContext::new();

        cache_every_collection_and_request(&mut ctx, "app_1", "deploy_a", &live, "ghosts");

        assert_eq!(
            ctx.introspected_schema_for("app_1", "ghosts", "deploy_a"),
            Some(None),
            "an absent collection must be cached as a negative result, not left \
             uncached to re-introspect on every op",
        );
        // The present collection is still populated by the same call.
        assert!(ctx
            .introspected_schema_for("app_1", "notes", "deploy_a")
            .is_some());
    }

    /// MEASUREMENT, not an assertion - run with `--nocapture`.
    ///
    /// The per-app cache bound has to be a number, and a number nobody
    /// measured is a guess with a decimal point. This reports the serialized
    /// size of one cached entry so the bound can be derived from the actual
    /// shape being stored.
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
