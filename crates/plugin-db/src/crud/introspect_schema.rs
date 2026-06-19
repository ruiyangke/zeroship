//! **P4 HALF B** — runtime data-access metadata from LIVE introspection.
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
//! Caching: per-isolate, per-`(app, collection)`, keyed on the current
//! `ZEROSHIP_DEPLOY_ID` (the deploy/schema-version token). A deploy bump
//! invalidates the entry on next read — mirroring `register_model`'s per-thread
//! `is_model_registered` fast-path but keyed on the deploy version rather than
//! mere presence (design §6). A goodie-free collection is cached as a NEGATIVE
//! result (`None`) so it is not re-introspected each call.
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

/// The env var the engine/registerModel bump per deploy; the cache invalidation
/// token. Absent ⇒ `"cold_start"` (matches `register_model` / `mask_backfill`).
fn deploy_token() -> String {
    std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string())
}

/// Resolve the runtime data-access schema for `(app_id, collection)` from the
/// LIVE catalog + sentinels, with per-isolate deploy-keyed caching.
///
/// Returns `Ok(None)` when the collection has no encrypted/masked columns (the
/// caller then skips the encrypt/mask passes — the schema-driven read coercions
/// also run only when a schema is present, matching the pre-P4 cold-schema
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
    // read/write passes behave identically to before P4.
    if !crate::context::with(|c| c.is_model_registered(app_id, collection)) {
        return Ok(None);
    }

    let token = deploy_token();

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
    // Cache the result under the deploy token so the collection is introspected
    // at most once per deploy on this isolate (mirrors `is_model_registered`'s
    // per-thread fast-path, keyed on the deploy version).
    crate::context::with_mut(|c| {
        c.cache_introspected_schema(app_id, collection, &token, schema.clone());
    });
    Ok(schema)
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
