//! **`genArtifacts` hands the folded schema back as STRUCTURE, not only as strings.**
//!
//! `env.db.ts` and `schema.runtime.json` are consumed downstream by a platform that
//! renders its own files. Until now the only way to get the metadata was to parse
//! `runtimeJson` back out of the reply that had just serialized it. `collections` is
//! that metadata, typed, in the same vocabulary the MANUAL source accepts.
//!
//! This binary measures the REPLY WIRING — that the verb actually populates the new
//! fields, and that it withholds them on a refusal. `collection_export_round_trip.rs`
//! measures the CONVERSION. They are separate binaries on purpose: the round trip
//! calls `field_to_dto` directly and would pass unchanged if `gen_artifacts_*` never
//! called it at all, so one file cannot be evidence for both.
//!
//! # What this does NOT prove
//!
//! * NOT that a JS caller sees the fields — this is the napi-free build. The `.node`
//!   surface is `index.d.ts` plus the host suite.
//! * NOT that `collections` and `runtimeJson` agree field-for-field. They are read off
//!   ONE recovery in the engine (`project_collection_descriptors`, which
//!   `project_field_defs` is a map over), so agreement is structural rather than
//!   asserted here; the collection SET is compared below, which is the part a
//!   plumbing mistake could break.

mod support;

use serde_json::{json, Value};

use zero_migrate_node::api::{gen_artifacts_from_descriptors, gen_artifacts_from_envelopes};

const SCHEMA: &str = "public";

fn charter() -> String {
    support::no_inject_charter_toml(SCHEMA)
}

fn history() -> Vec<Value> {
    vec![json!({
        "ir_version": 1,
        "name": "create_notes",
        "ops": [{
            "op": "createTable",
            "name": "notes",
            "columns": [
                { "name": "id", "type": "text", "nullable": false },
                { "name": "body", "type": "text" },
                { "name": "slug", "type": { "string": { "length": 40 } }, "nullable": false },
            ],
        }],
    })]
}

#[test]
fn a_successful_call_carries_the_folded_collections() {
    let charter = charter();
    let reply =
        gen_artifacts_from_envelopes(&history(), "postgres", Some(SCHEMA), &[charter.as_str()]);
    assert!(reply.ok, "gen_artifacts refused: {:?}", reply.error);

    let collections = reply
        .collections
        .expect("a successful call reports collections");
    assert_eq!(collections.len(), 1);
    let notes = &collections[0];
    assert_eq!(notes.name, "notes");

    let names: Vec<&str> = notes.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["id", "body", "slug"]);

    // The WIDTH facet, which had no wire slot before this change and is the reason a
    // downstream renderer could not emit `VARCHAR(40)` from the export.
    let slug = notes
        .fields
        .iter()
        .find(|f| f.name == "slug")
        .expect("the slug column is exported");
    assert_eq!(slug.max_length, Some(40));
    assert_eq!(slug.required, Some(true));

    // The collection SET matches the artifact the same call emitted. A plumbing
    // mistake that exported a stale or differently-folded schema shows up here.
    let runtime: Value = serde_json::from_str(
        reply
            .runtime_json
            .as_deref()
            .expect("a successful call reports runtime_json"),
    )
    .expect("runtime_json is JSON");
    let in_json: Vec<&str> = runtime["collections"]
        .as_object()
        .expect("collections is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(in_json, ["notes"]);
}

/// The dialect rides IN THE PAYLOAD, because leg selection changes which columns
/// exist and an export is uninterpretable without the target it was folded under.
///
/// Asserted as the RESOLVED engine token rather than an echo of the input.
#[test]
fn the_reply_names_the_dialect_it_folded_under() {
    let charter = charter();
    for dialect in ["postgres", "mysql", "sqlite"] {
        let reply =
            gen_artifacts_from_envelopes(&history(), dialect, Some(SCHEMA), &[charter.as_str()]);
        assert!(reply.ok, "{dialect}: refused: {:?}", reply.error);
        assert_eq!(reply.dialect.as_deref(), Some(dialect));
    }
}

/// A refusal reports NEITHER field — the `has_dialectal_ops` discipline, extended.
///
/// `None` rather than an empty list or an echoed input string, so a consumer cannot
/// read "this schema declares no collections" off a call that never folded one, and
/// cannot read a target off a call that was refused BECAUSE of its target.
#[test]
fn a_refusal_reports_no_collections_and_no_dialect() {
    let charter = charter();

    // Refused on the dialect itself: echoing the input here would name a rejected
    // target as though it had been used.
    let bad_dialect =
        gen_artifacts_from_envelopes(&history(), "duckdb", Some(SCHEMA), &[charter.as_str()]);
    assert!(!bad_dialect.ok);
    assert!(bad_dialect.collections.is_none());
    assert!(bad_dialect.dialect.is_none());
    assert!(bad_dialect.has_dialectal_ops.is_none());

    // Refused on the source, after the dialect resolved cleanly.
    let bad_source = gen_artifacts_from_envelopes(
        &[json!({ "ir_version": 1, "name": "broken" })],
        "postgres",
        Some(SCHEMA),
        &[charter.as_str()],
    );
    assert!(!bad_source.ok);
    assert!(bad_source.collections.is_none());
    assert!(bad_source.dialect.is_none());
}

/// An empty schema reports `Some([])`, not `None`.
///
/// The distinction is the whole reason the field is an `Option` of a list rather than
/// a list: absent means "this addon does not report collections" (an older `.node`),
/// empty means "this schema has none". A consumer that tests falsiness cannot tell
/// them apart, so the two must not collapse on the producing side.
#[test]
fn an_empty_schema_reports_an_empty_list_not_an_absent_one() {
    let charter = charter();
    let reply = gen_artifacts_from_envelopes(&[], "postgres", Some(SCHEMA), &[charter.as_str()]);
    assert!(reply.ok, "an empty history folds: {:?}", reply.error);
    assert_eq!(
        reply.collections.map(|c| c.len()),
        Some(0),
        "an empty schema must report an empty list, never an absent one"
    );
}

/// The MANUAL source exports too, and reports the same dialect discipline.
///
/// It cannot carry a `dialect()` wrapper — `has_dialectal_ops` is `false` by
/// construction there — but it is still folded under a target, so it still has one to
/// name.
#[test]
fn the_manual_source_exports_collections_as_well() {
    use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};

    let descriptors = [CollectionDescriptor {
        name: "hits".to_string(),
        owner_app: "app_export".to_string(),
        fields: vec![FieldDescriptor {
            name: "id".to_string(),
            ty: "string".to_string(),
            required: true,
            ..Default::default()
        }],
        indexes: Vec::new(),
        runtime_options: zero_migrate::TableRuntimeOptions::default(),
    }];
    let charter = charter();
    let reply =
        gen_artifacts_from_descriptors(&descriptors, "sqlite", Some(SCHEMA), &[charter.as_str()]);
    assert!(reply.ok, "refused: {:?}", reply.error);
    assert_eq!(reply.dialect.as_deref(), Some("sqlite"));
    assert_eq!(reply.has_dialectal_ops, Some(false));
    let collections = reply.collections.expect("the manual source exports too");
    assert_eq!(collections.len(), 1);
    assert_eq!(collections[0].name, "hits");
}

/// The export carries the two per-collection facts that are NOT columns: the plain
/// indexes and the runtime options.
///
/// They are the reason `project_collection_descriptors` reads
/// `project_runtime_metadata` instead of leaving the intermediate's empty `indexes` /
/// default options in place — the shape `project_field_defs` discarded them from. A
/// consumer rendering its own `schema.runtime.json` needs all three parts, and without
/// this arm the merge would be entirely unmeasured: `descriptor_to_sdk_schema` reads
/// only `fields`, so dropping the merge moves no artifact byte and no drift gate.
#[test]
fn the_export_carries_indexes_and_runtime_options_not_only_fields() {
    use zero_migrate::render::declarative::{
        CollectionDescriptor, FieldDescriptor, IndexDescriptor,
    };

    let descriptors = [CollectionDescriptor {
        name: "hits".to_string(),
        owner_app: "app_export".to_string(),
        fields: vec![
            FieldDescriptor {
                name: "id".to_string(),
                ty: "string".to_string(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "path".to_string(),
                ty: "string".to_string(),
                ..Default::default()
            },
        ],
        indexes: vec![IndexDescriptor {
            name: "ix_hits_path".to_string(),
            columns: vec!["path".to_string()],
            unique: false,
        }],
        runtime_options: zero_migrate::TableRuntimeOptions {
            soft_delete: true,
            versioning: true,
            strictness: zero_migrate::TableStrictness::Lenient,
        },
    }];
    let charter = charter();
    let reply =
        gen_artifacts_from_descriptors(&descriptors, "postgres", Some(SCHEMA), &[charter.as_str()]);
    assert!(reply.ok, "refused: {:?}", reply.error);
    let collections = reply.collections.expect("a successful call exports");
    let hits = &collections[0];

    let indexes = hits
        .indexes
        .as_ref()
        .expect("indexes are reported as a list, never as absent");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "ix_hits_path");
    assert_eq!(indexes[0].columns, ["path"]);
    assert_eq!(indexes[0].unique, Some(false));

    let options = hits
        .runtime_options
        .as_ref()
        .expect("runtime options are reported");
    assert_eq!(options.soft_delete, Some(true));
    assert_eq!(options.versioning, Some(true));
    assert_eq!(options.strictness.as_deref(), Some("lenient"));
}
