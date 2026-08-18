//! **The two artifacts one `genArtifacts` call emits must agree about an enum column.**
//!
//! `genArtifacts` returns `envDbTs` AND `runtimeJson` from ONE fold. `envDbTs`
//! rendered `t.enum("issue_status")`; `runtimeJson` reduced the same column to
//! `{"type":"string"}` and dropped the membership. `RuntimeSchemaDescriptor` is the
//! artifact a deployed app installs `env.db` from, so the half that lost the closed
//! set is the half that decides what the runtime validates.
//!
//! WHY THE FIX IS NOT A MATCH ARM. `ColType::Enum { name, schema }` carries the
//! NAME ONLY - the members live in a separate `Op::CreateEnum`. So no signature
//! given `&IrColumn` alone can populate `enum_values`; the members have to be
//! resolved from the op stream, which is what the fold behind the `FieldDef` map does
//! through the same `NamedTypeRegistry` the DDL lower and the snapshot fold resolve them
//! through. (That was `fold_to_field_defs`'s registry until step 4 consumer 3 of
//! `docs/proposals/single-fold-and-effects.md` deleted the walker; the registry is
//! carried on `FoldedSchema` now and `project_field_defs` reads it.)
//!
//! WHAT IS ASSERTED. Content, never `ok`: a silently dropped enum also returns
//! `ok=true`. The members must be present, in DECLARATION order, on the right
//! field. `t.text()` (control A) is what makes the loss legible - `status` and
//! `summary` were indistinguishable in `runtimeJson`. `t.int()` (control B) refutes
//! "the descriptor is coarse on purpose": it keeps `"type": "int"` in the same call.
//!
//! BOTH DIALECTS, because they enforce the set by different mechanisms: SQLite gets
//! `TEXT ... CHECK ("status" IN (...))`, PostgreSQL a native `CREATE TYPE`. The
//! descriptor must carry the members either way.
//!
//! Runs on the napi-free build (`--no-default-features`).

mod support;

use serde_json::{json, Value};

use zero_migrate::model::ir::TableRuntimeOptions;
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};
use zero_migrate_node::api::{gen_artifacts_from_descriptors, gen_artifacts_from_envelopes};

const SCHEMA: &str = "public";

fn charter() -> String {
    support::no_inject_charter_toml(SCHEMA)
}

/// The reported reproduction: one table, the `t.enum` subject plus the two
/// controls, with the enum's members declared by their own op.
fn issues_history() -> Vec<Value> {
    vec![json!({
        "ir_version": 1,
        "name": "create_issues",
        "ops": [
            {
                "op": "createEnum",
                "name": "issue_status",
                "values": ["UNCONFIRMED", "CONFIRMED", "RESOLVED"],
            },
            {
                "op": "createTable",
                "name": "issues",
                "columns": [
                    {
                        "name": "status",
                        "type": { "enum": { "name": "issue_status" } },
                        "nullable": false,
                        "default": { "literal": { "value": "UNCONFIRMED" } },
                    },
                    { "name": "summary", "type": "text", "nullable": false },
                    {
                        "name": "weight",
                        "type": "int",
                        "nullable": false,
                        "default": { "literal": { "value": 0 } },
                    },
                ],
                "primaryKey": null,
            },
        ],
    })]
}

/// The same table, with the enum declared by an EARLIER migration file. The
/// envelope source concatenates every envelope into one op stream, so the members
/// are still reachable - this arm proves the fix is not confined to a single file.
fn issues_history_split_across_migrations() -> Vec<Value> {
    let mut split = vec![json!({
        "ir_version": 1,
        "name": "create_issue_status",
        "ops": [{
            "op": "createEnum",
            "name": "issue_status",
            "values": ["UNCONFIRMED", "CONFIRMED", "RESOLVED"],
        }],
    })];
    let mut later = issues_history().remove(0);
    let ops = later["ops"].as_array_mut().expect("ops is an array");
    ops.remove(0);
    later["name"] = json!("create_issues_only");
    split.push(later);
    split
}

/// The table WITHOUT its `createEnum` anywhere in the stream - the enum exists only
/// in the live catalog, which `genArtifacts` never reads (it is DB-free by
/// contract). The members are genuinely unprovable here.
fn issues_history_without_the_enum_declaration() -> Vec<Value> {
    let mut orphan = issues_history();
    let ops = orphan[0]["ops"].as_array_mut().expect("ops is an array");
    ops.remove(0);
    orphan
}

fn runtime_fields(runtime_json: &str, collection: &str) -> Value {
    let parsed: Value = serde_json::from_str(runtime_json).expect("runtime json parses");
    parsed["collections"][collection]["fields"].clone()
}

/// The enum members, in declaration order, that a `runtimeJson` field carries.
fn field_enum(fields: &Value, column: &str) -> Option<Vec<String>> {
    fields[column].get("enum").map(|members| {
        members
            .as_array()
            .expect("enum membership is an array")
            .iter()
            .map(|v| v.as_str().expect("enum member is a string").to_string())
            .collect()
    })
}

#[test]
fn both_artifacts_keep_the_enum_membership_on_every_dialect() {
    for dialect in ["sqlite", "postgres", "mysql"] {
        let reply = gen_artifacts_from_envelopes(
            &issues_history(),
            dialect,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            reply.ok,
            "{dialect}: the reported history folds: {:?}",
            reply.error
        );
        let runtime_json = reply.runtime_json.expect("ok reply carries runtimeJson");
        let env_db_ts = reply.env_db_ts.expect("ok reply carries envDbTs");
        let fields = runtime_fields(&runtime_json, "issues");

        // The half that was already right.
        assert!(
            env_db_ts.contains(r#"t.enum("issue_status")"#),
            "{dialect}: envDbTs keeps the enum builder:\n{env_db_ts}"
        );

        // The half that lost it. Order is the DECLARED order, not sorted: a
        // PostgreSQL enum's declaration order is its sort order.
        assert_eq!(
            field_enum(&fields, "status").as_deref(),
            Some(
                &[
                    "UNCONFIRMED".to_string(),
                    "CONFIRMED".to_string(),
                    "RESOLVED".to_string()
                ][..]
            ),
            "{dialect}: runtimeJson carries the enum members in declaration order:\n{runtime_json}"
        );

        // Control A: the loss was legible precisely because these two were
        // indistinguishable. They must not be any more.
        assert_eq!(
            fields["summary"]["type"], "string",
            "{dialect}: control A stays a plain string: {runtime_json}"
        );
        assert_eq!(
            field_enum(&fields, "summary"),
            None,
            "{dialect}: control A is free text and must NOT gain a membership: {runtime_json}"
        );
        assert_ne!(
            fields["status"], fields["summary"],
            "{dialect}: a closed set and free text must not serialize identically: {runtime_json}"
        );

        // Control B: the descriptor preserves a narrowing it cannot express
        // downstream, so "coarse on purpose" was never the rule.
        assert_eq!(
            fields["weight"]["type"], "int",
            "{dialect}: control B keeps its integer token: {runtime_json}"
        );

        // The enum column is still a string-shaped field carrying its default.
        assert_eq!(
            fields["status"]["type"], "string",
            "{dialect}: the enum column's storage token is unchanged: {runtime_json}"
        );
        assert_eq!(
            fields["status"]["default"], "UNCONFIRMED",
            "{dialect}: the enum column keeps its default: {runtime_json}"
        );
    }
}

/// The members reach the descriptor when the `createEnum` is in an EARLIER
/// envelope, because the envelope source folds one concatenated op stream.
#[test]
fn an_enum_declared_in_an_earlier_migration_still_reaches_the_descriptor() {
    for dialect in ["sqlite", "postgres", "mysql"] {
        let reply = gen_artifacts_from_envelopes(
            &issues_history_split_across_migrations(),
            dialect,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            reply.ok,
            "{dialect}: a split history folds: {:?}",
            reply.error
        );
        let runtime_json = reply.runtime_json.expect("ok reply carries runtimeJson");
        let fields = runtime_fields(&runtime_json, "issues");
        assert_eq!(
            field_enum(&fields, "status").as_deref(),
            Some(
                &[
                    "UNCONFIRMED".to_string(),
                    "CONFIRMED".to_string(),
                    "RESOLVED".to_string()
                ][..]
            ),
            "{dialect}: the earlier file's members reach the later file's column:\n{runtime_json}"
        );
    }
}

/// A WRONG membership is worse than an absent one.
///
/// With no `createEnum` in the stream the members are unprovable, and the two
/// dialect families answer differently for a measured reason: SQLite and MySQL
/// INLINE the value list into the column's storage, so the fold already fails
/// closed without it; PostgreSQL only needs the type's NAME to render
/// `schema.issue_status`, so the fold succeeds. The descriptor must then omit the
/// membership rather than guess one.
#[test]
fn an_unprovable_membership_is_omitted_not_invented() {
    let pg = gen_artifacts_from_envelopes(
        &issues_history_without_the_enum_declaration(),
        "postgres",
        Some(SCHEMA),
        &[charter().as_str()],
    );
    assert!(
        pg.ok,
        "postgres renders a native enum column from the type NAME alone: {:?}",
        pg.error
    );
    let runtime_json = pg.runtime_json.expect("ok reply carries runtimeJson");
    let fields = runtime_fields(&runtime_json, "issues");
    assert_eq!(
        field_enum(&fields, "status"),
        None,
        "an unprovable membership is left absent, never fabricated:\n{runtime_json}"
    );

    for inlining in ["sqlite", "mysql"] {
        let reply = gen_artifacts_from_envelopes(
            &issues_history_without_the_enum_declaration(),
            inlining,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            !reply.ok,
            "{inlining} inlines the value list into the column type, so it cannot \
             render the column without the declaration"
        );
    }
}

/// The DESCRIPTOR source never had this defect, and the fix must not give it one.
///
/// A `CollectionDescriptor` has no way to name a native enum TYPE - `enum_values`
/// on a `string` field is the only enum a descriptor can express, and
/// `descriptors_to_create_ops` turns it into a table-level CHECK that
/// `recover_check_facet` lifts straight back. So the membership already survived
/// that path, by a different route, and the two sources reach the same
/// `runtimeJson` field by two different mechanisms.
///
/// MEASURED, and PRE-EXISTING: that route is PostgreSQL-only. The CHECK the
/// producer emits is table-level, and `createTable table-level CHECK is
/// PostgreSQL-only`, so a descriptor declaring ANY membership (or any `min`/`max`)
/// is refused outright on SQLite. That is a separate limitation of the descriptor
/// producer, older than this fix and untouched by it; it is pinned here so the
/// asymmetry between the two sources is on the record rather than rediscovered.
#[test]
fn the_descriptor_source_keeps_a_membership_it_declares() {
    let descriptors = vec![CollectionDescriptor {
        name: "issues".to_string(),
        owner_app: "app_test".to_string(),
        fields: vec![
            FieldDescriptor {
                name: "status".to_string(),
                ty: "string".to_string(),
                required: true,
                enum_values: Some(vec![
                    json!("UNCONFIRMED"),
                    json!("CONFIRMED"),
                    json!("RESOLVED"),
                ]),
                ..Default::default()
            },
            FieldDescriptor {
                name: "summary".to_string(),
                ty: "string".to_string(),
                required: true,
                ..Default::default()
            },
        ],
        indexes: Vec::new(),
        runtime_options: TableRuntimeOptions::default(),
    }];

    let pg = gen_artifacts_from_descriptors(
        &descriptors,
        "postgres",
        Some(SCHEMA),
        &[charter().as_str()],
    );
    assert!(
        pg.ok,
        "postgres: the descriptor set renders: {:?}",
        pg.error
    );
    let runtime_json = pg.runtime_json.expect("ok reply carries runtimeJson");
    let fields = runtime_fields(&runtime_json, "issues");
    assert_eq!(
        field_enum(&fields, "status").as_deref(),
        Some(
            &[
                "UNCONFIRMED".to_string(),
                "CONFIRMED".to_string(),
                "RESOLVED".to_string()
            ][..]
        ),
        "the descriptor-declared membership round-trips:\n{runtime_json}"
    );
    assert_eq!(
        field_enum(&fields, "summary"),
        None,
        "the control gains nothing: {runtime_json}"
    );

    let sqlite =
        gen_artifacts_from_descriptors(&descriptors, "sqlite", Some(SCHEMA), &[charter().as_str()]);
    assert!(
        !sqlite.ok,
        "a descriptor-declared membership is a table-level CHECK, which SQLite refuses"
    );
    assert!(
        sqlite
            .error
            .as_deref()
            .is_some_and(|e| e.contains("table-level CHECK is PostgreSQL-only")),
        "the refusal names the pre-existing producer limitation: {:?}",
        sqlite.error
    );
}
