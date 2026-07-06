//! **Migration-first P2b §6(a) — the `.d.ts` golden over the FULL type matrix.**
//!
//! Mirrors `generate_all_types_parity.rs`: author an op stream covering the full
//! portable type/facet matrix (text/int/smallInt/bigInt/float/real/bool/json/
//! timestamp/uuid/inet/textArray/bytes/numeric + ref + vector + encrypted + id-with-prefix),
//! generate the `env.db.ts`, and assert the emitted file contains the expected
//! `@zeroship/db` `t.*()` builder chain per column. The richer reverse renderer
//! (vs `scaffold.rs::render_t_for`, which TODO-stubs goodies) is the thing under test.
//!
//! WHY this is RED pre-P2b: the gen-types emitter + the `t.*()` reverse renderer did
//! not exist before P2b — there was no `.d.ts` artifact to assert over, and the
//! lossy scaffold renderer emits `t.text() /* TODO */` for every goodie.
//!
//! FAITHFUL: the op stream is folded through the REAL `fold_to_field_defs` seam and
//! the REAL `render_artifacts` emitter — no stubs.

use zeroship_migrate::model::ir::{
    ColType, IndexElement, IrColumn, IrConstraint, IrConstraintKind, Op, RefAction,
    TableRuntimeOptions, TableStrictness, VectorMetric,
};
use zeroship_migrate::frontend::render_artifacts;

/// A non-required column of the given type (the `t.*` default-nullable image).
fn col(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.into(),
        ty,
        nullable: None,
        default: None,
        unique: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

/// The op stream covering the full type/facet matrix in one `gadgets` table.
fn all_types_ops() -> Vec<Op> {
    let id = IrColumn {
        name: "id".into(),
        ty: ColType::Uuid,
        nullable: Some(false),
        default: None,
        unique: None,
        id_prefix: Some("gdt".into()),
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    };
    let embedding = IrColumn {
        name: "embedding".into(),
        ty: ColType::Vector { vector: 768 },
        nullable: Some(true),
        default: None,
        unique: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: Some(VectorMetric::L2),
        mask: None,
        generated: None,
        identity: None,
    };
    let columns = vec![
        id,
        col("c_text", ColType::Text),
        col("c_int", ColType::Int),
        col("c_smallint", ColType::SmallInt),
        col("c_bigint", ColType::BigInt),
        col("c_float", ColType::Double),
        col("c_real", ColType::Real),
        col("c_bool", ColType::Boolean),
        col("c_json", ColType::Json),
        col("c_ts", ColType::Timestamp),
        col("c_inet", ColType::Inet),
        col("c_text_array", ColType::TextArray),
        col("c_bytes", ColType::Bytes),
        col("c_num", ColType::Decimal { precision: 38, scale: 9 }),
        col("owner", ColType::Ref { references: "users".into() }),
        col("secret", ColType::Encrypted { of: Box::new(ColType::Text) }),
        embedding,
        col("age", ColType::Int),
        col("status", ColType::Text),
    ];
    // The FK constraint carrying the ref POLICY (so onDelete round-trips).
    let fk = IrConstraint {
        name: Some("gadgets_owner_fkey".into()),
        kind: IrConstraintKind::Fk {
            columns: vec!["owner".into()],
            references_table: "users".into(),
            references_columns: vec!["id".into()],
            on_delete: Some(RefAction::Cascade),
            on_update: None,
            deferrable: None,
            initially_deferred: None,
        
            not_valid: None,
        },
    };
    vec![Op::CreateTable {
        name: "gadgets".into(),
        columns,
        primary_key: None,
        constraints: vec![fk],
        indexes: Vec::new(),

    partition_by: None,

    runtime_options: None,
        schema: None,
        existence_guard: None,
    }]
}

#[test]
fn env_dts_golden_covers_full_type_matrix() {
    let ops = all_types_ops();
    let artifacts = render_artifacts(&ops, "public").expect("render gen-types artifacts");
    let dts = &artifacts.env_dts;

    // The generated file imports the SDK `t` + `Db` and binds the `zeroship`
    // module's `Env.db` to `Db<typeof schema>` (reusing the inference chain).
    assert!(
        dts.contains("import { t, schema as defineSchema, type Db } from \"@zeroship/db\";"),
        "imports the SDK t + schema builder + Db: {dts}"
    );
    assert!(dts.contains("const schema = {"), "emits a const schema object");
    assert!(dts.contains("as const;"), "the schema is `as const` (narrows enums to unions)");
    assert!(
        dts.contains("declare module \"zeroship\" {") && dts.contains("db: Db<typeof schema>;"),
        "augments the zeroship Env with Db<typeof schema>: {dts}"
    );

    // Each column's expected `t.*()` chain. These are RICHER than the lossy scaffold
    // renderer (which TODO-stubs every goodie).
    for chain in [
        "id: t.id(\"gdt\")",                          // typed-id + prefix
        "c_text: t.string()",
        "c_int: t.number()",                          // op.* int → SDK numeric builder
        "c_smallint: t.number()",
        "c_bigint: t.number()",
        "c_float: t.number()",
        "c_real: t.number()",
        "c_bool: t.boolean()",
        "c_json: t.json()",
        "c_ts: t.timestamp()",
        "c_inet: t.string()",
        "c_text_array: t.array(t.string())",
        "c_bytes: t.bytes()",
        "c_num: t.number()",
        "owner: t.ref(\"users\", { onDelete: \"cascade\"",  // ref + recovered FK policy
        "secret: t.encrypted()",                      // encrypted (default mode)
        "embedding: t.vector(768, { metric: \"l2\" })", // vector + recovered metric
        "age: t.number()",
        "status: t.string()",
    ] {
        assert!(
            dts.contains(chain),
            "the emitted env.db.ts must render the `{chain}` chain; got:\n{dts}"
        );
    }
}

#[test]
fn runtime_descriptor_is_the_wire_fielddef_map() {
    let ops = all_types_ops();
    let artifacts = render_artifacts(&ops, "public").expect("render");
    let value: serde_json::Value =
        serde_json::from_str(&artifacts.runtime_descriptor).expect("runtime descriptor is JSON");
    assert_eq!(value["version"], 1);
    let gadgets = value["collections"].get("gadgets").expect("gadgets collection present");
    let fields = &gadgets["fields"];
    // A spot-check that descriptor v1 nests the per-column wire-FieldDef map under
    // `collections[*].fields` while carrying collection metadata alongside it.
    assert_eq!(fields["id"]["type"], "id");
    assert_eq!(fields["id"]["idPrefix"], "gdt");
    assert_eq!(fields["owner"]["refTarget"], "users");
    assert_eq!(fields["owner"]["onDelete"], "cascade");
    assert_eq!(fields["embedding"]["vectorDims"], 768);
    assert_eq!(fields["embedding"]["vectorMetric"], "l2");
    assert_eq!(fields["age"]["type"], "int");
    assert_eq!(fields["status"]["type"], "string");
    assert_eq!(
        gadgets["options"],
        serde_json::json!({ "softDelete": false, "versioning": false, "strictness": "strict" })
    );
    assert_eq!(gadgets["indexes"], serde_json::json!([]));
}

#[test]
fn runtime_descriptor_v1_carries_collection_options_and_compound_indexes() {
    let ops = vec![
        Op::CreateTable {
            name: "posts".into(),
            columns: vec![
                IrColumn { name: "title".into(), ty: ColType::Text, nullable: Some(false), default: None, unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                IrColumn { name: "author_id".into(), ty: ColType::Text, nullable: Some(false), default: None, unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                IrColumn { name: "status".into(), ty: ColType::Text, nullable: Some(false), default: None, unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
            ],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),

        partition_by: None,

        runtime_options: Some(TableRuntimeOptions {
                soft_delete: true,
                versioning: true,
                strictness: TableStrictness::Lenient,
            }),
            schema: None,
            existence_guard: None,
        },
        Op::CreateIndex {
            table: "posts".into(),
            columns: vec![
                IndexElement::Column {
                    name: "author_id".into(),
                    order: None,
                    opclass: None,
                    collation: None,
                },
                IndexElement::Column {
                    name: "status".into(),
                    order: None,
                    opclass: None,
                    collation: None,
                },
            ],
            name: Some("posts_author_status_idx".into()),
            unique: Some(false),
            using: None,
            r#where: None,

        include: Vec::new(),
        with: None,
        only: None,
        concurrently: None,
            schema: None,
            existence_guard: None,
            nulls_not_distinct: None,
        },
    ];
    let artifacts = render_artifacts(&ops, "public").expect("render");
    let dts = &artifacts.env_dts;
    assert!(
        dts.contains("posts: defineSchema({"),
        "metadata-bearing collections are wrapped with the SDK schema builder: {dts}"
    );
    assert!(
        dts.contains(
            "}).softDelete().withVersioning().strictness(\"lenient\").index(\"posts_author_status_idx\", [\"author_id\",\"status\"]),"
        ),
        "env.db.ts mirrors folded softDelete/versioning/strictness/index metadata: {dts}"
    );

    let value: serde_json::Value =
        serde_json::from_str(&artifacts.runtime_descriptor).expect("runtime descriptor is JSON");
    assert_eq!(
        value,
        serde_json::json!({
            "version": 1,
            "collections": {
                "posts": {
                    "fields": {
                        "title": { "type": "string", "required": true },
                        "author_id": { "type": "string", "required": true },
                        "status": { "type": "string", "required": true }
                    },
                    "options": {
                        "softDelete": true,
                        "versioning": true,
                        "strictness": "lenient"
                    },
                    "indexes": [
                        {
                            "name": "posts_author_status_idx",
                            "fields": ["author_id", "status"]
                        }
                    ]
                }
            }
        })
    );
}

#[test]
fn runtime_descriptor_generated_index_names_match_lowered_capped_names() {
    let table = "posts_with_an_extremely_long_runtime_descriptor_table_name".to_string();
    let owner = "owner_identifier_column_with_extra_descriptor_length".to_string();
    let status = "publication_status_column_with_extra_descriptor_length".to_string();
    let slug = "slug_unique_column_with_extra_descriptor_length".to_string();
    let natural = format!("{table}_{}_idx", [owner.clone(), status.clone()].join("_"));
    let unique_natural = format!("{table}_{slug}_key");
    assert!(natural.len() > 63, "fixture must exercise identifier capping");
    assert!(unique_natural.len() > 63, "fixture must exercise unique-name capping");
    let expected = zeroship_migrate::plan::author::cap_ident_name(&natural);
    let expected_unique = zeroship_migrate::plan::author::cap_ident_name(&unique_natural);
    let mut unique_slug = col(&slug, ColType::Text);
    unique_slug.unique = Some(true);

    let ops = vec![
        Op::CreateTable {
            name: table.clone(),
            columns: vec![
                col(&owner, ColType::Text),
                col(&status, ColType::Text),
                unique_slug,
            ],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        },
        Op::CreateIndex {
            table: table.clone(),
            columns: vec![
                IndexElement::Column {
                    name: owner.clone(),
                    order: None,
                    opclass: None,
                    collation: None,
                },
                IndexElement::Column {
                    name: status.clone(),
                    order: None,
                    opclass: None,
                    collation: None,
                },
            ],
            name: None,
            unique: Some(false),
            using: None,
            r#where: None,

        include: Vec::new(),
        with: None,
        only: None,
        concurrently: None,
            schema: None,
            existence_guard: None,
            nulls_not_distinct: None,
        },
    ];

    let artifacts = render_artifacts(&ops, "public").expect("render");
    let value: serde_json::Value =
        serde_json::from_str(&artifacts.runtime_descriptor).expect("runtime descriptor is JSON");
    let indexes = value["collections"][table.as_str()]["indexes"]
        .as_array()
        .expect("indexes array");
    assert_eq!(indexes.len(), 2);
    assert_eq!(indexes[0]["name"], expected_unique);
    assert_eq!(indexes[1]["name"], expected);
    assert!(indexes[0]["name"].as_str().unwrap().len() <= 63);
    assert!(indexes[1]["name"].as_str().unwrap().len() <= 63);
}
