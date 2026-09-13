mod support;

use serde_json::{json, Value};
use zeroship_migrate::{DialectId, Op};
use zeroship_migrate_node::descriptors::{descriptor_dto_to_engine, descriptor_to_dto};

fn ops(relation: Option<&str>) -> Vec<Op> {
    let mut reference = json!({"table": "users", "column": "id", "name": "posts_author_fk"});
    if let Some(relation) = relation {
        reference["relation"] = json!(relation);
    }
    serde_json::from_value(json!([
        {"op":"createTable", "name":"users", "primaryKey":["id"], "columns":[
            {"name":"id", "type":"text", "nullable":false},
            {"name":"name", "type":"text"}
        ]},
        {"op":"createTable", "name":"posts", "primaryKey":["id"], "columns":[
            {"name":"id", "type":"text", "nullable":false},
            {"name":"title", "type":"text"},
            {"name":"author_id", "type":"text", "references":reference}
        ]}
    ]))
    .expect("named reference IR deserializes")
}

#[test]
fn named_relations_survive_artifacts_and_descriptor_round_trips_without_changing_ddl() {
    let vendors = zeroship_migrate::shipping_vendors();
    let policy = support::no_inject("public");
    for dialect in [
        zeroship_migrate_postgres::DIALECT,
        zeroship_migrate_sqlite::DIALECT,
    ] {
        let export = zeroship_migrate::render_schema_export(
            vendors,
            &ops(Some("author")),
            &dialect,
            "public",
            &policy,
        )
        .unwrap();
        let runtime: Value = serde_json::from_str(&export.artifacts.runtime_json).unwrap();
        let field = &runtime["collections"]["posts"]["fields"]["author_id"];
        assert_eq!(field["relation"], "author");
        assert_eq!(field["refTarget"], "users");
        assert_eq!(field["refColumn"], "id");
        assert_eq!(field["refName"], "posts_author_fk");
        assert!(export.artifacts.env_db_ts.contains("relation: \"author\""));
        let crossed = export
            .collections
            .values()
            .map(|descriptor| descriptor_dto_to_engine(descriptor_to_dto(descriptor)).unwrap())
            .collect::<Vec<_>>();
        let refolded = zeroship_migrate::render_schema_export_from_descriptors(
            vendors, &crossed, &dialect, "public", &policy,
        )
        .unwrap();
        let refolded: Value = serde_json::from_str(&refolded.artifacts.runtime_json).unwrap();
        assert_eq!(
            refolded["collections"]["posts"]["fields"]["author_id"]["relation"],
            "author"
        );
        assert_eq!(
            catalog(&ops(Some("author")), &dialect),
            catalog(&ops(None), &dialect)
        );
    }
}

fn catalog(ops: &[Op], dialect: &DialectId) -> zeroship_migrate::SchemaSnapshot {
    zeroship_migrate::fold_ops(
        zeroship_migrate::shipping_vendors(),
        ops,
        dialect,
        "public",
        &support::no_inject("public"),
    )
    .unwrap()
}

#[test]
fn named_relations_reject_ambiguous_and_reserved_output_names() {
    let vendors = zeroship_migrate::shipping_vendors();
    for name in [
        "_meta",
        "_custom",
        "",
        "title",
        "author_id",
        "__proto__",
        "constructor",
        "prototype",
        "__zs_meta",
        "__ZEROSHIP_meta",
        "SQLITE_edge",
        "author-name",
    ] {
        assert!(
            zeroship_migrate::validate_declared_identifiers(
                vendors,
                &ops(Some(name)),
                &zeroship_migrate_postgres::DIALECT
            )
            .is_err(),
            "accepted {name:?}"
        );
    }
    let mut duplicate = ops(Some("author"));
    if let Op::CreateTable { columns, .. } = &mut duplicate[1] {
        let mut reviewer = columns[2].clone();
        reviewer.name = "reviewer_id".into();
        columns.push(reviewer);
    }
    assert!(zeroship_migrate::validate_declared_identifiers(
        vendors,
        &duplicate,
        &zeroship_migrate_postgres::DIALECT
    )
    .is_err());
    assert!(zeroship_migrate::validate_declared_identifiers(
        vendors,
        &ops(Some("author")),
        &zeroship_migrate_postgres::DIALECT
    )
    .is_ok());
}

#[test]
fn named_relations_keep_their_name_when_the_foreign_key_is_renamed() {
    let vendors = zeroship_migrate::shipping_vendors();
    let dialect = zeroship_migrate_postgres::DIALECT;
    let policy = support::no_inject("public");
    let mut renamed = ops(Some("author"));
    renamed.push(
        serde_json::from_value(json!({
            "op":"renameColumn", "table":"posts", "from":"author_id", "to":"owner_id", "type":"text"
        }))
        .unwrap(),
    );
    let export =
        zeroship_migrate::render_schema_export(vendors, &renamed, &dialect, "public", &policy)
            .unwrap();
    let runtime: Value = serde_json::from_str(&export.artifacts.runtime_json).unwrap();
    let fields = &runtime["collections"]["posts"]["fields"];
    assert_eq!(fields["owner_id"]["relation"], "author");
    assert!(fields.get("author_id").is_none());
    renamed.push(
        serde_json::from_value(json!({
            "op":"renameColumn", "table":"posts", "from":"title", "to":"author", "type":"text"
        }))
        .unwrap(),
    );
    assert!(
        zeroship_migrate::render_schema_export(vendors, &renamed, &dialect, "public", &policy)
            .is_err()
    );
}

#[test]
fn manual_named_relations_require_explicit_reference_metadata() {
    let vendors = zeroship_migrate::shipping_vendors();
    let dialect = zeroship_migrate_postgres::DIALECT;
    let policy = support::no_inject("public");
    let export = zeroship_migrate::render_schema_export(
        vendors,
        &ops(Some("author")),
        &dialect,
        "public",
        &policy,
    )
    .unwrap();
    for missing_target in [true, false] {
        let mut descriptors = export.collections.values().cloned().collect::<Vec<_>>();
        let field = descriptors
            .iter_mut()
            .find(|collection| collection.name == "posts")
            .unwrap()
            .fields
            .iter_mut()
            .find(|field| field.name == "author_id")
            .unwrap();
        if missing_target {
            field.references = None;
        } else {
            field.reference_column = None;
        }
        assert!(zeroship_migrate::validate_declared_descriptor_identifiers(
            vendors,
            &descriptors,
            &dialect
        )
        .is_err());
        assert!(
            zeroship_migrate::render_schema_export_from_descriptors(
                vendors,
                &descriptors,
                &dialect,
                "public",
                &policy
            )
            .is_err(),
            "manual source silently lost the relation"
        );
    }
}
