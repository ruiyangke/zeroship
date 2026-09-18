//! Structured types in migrations: `t.object`/`t.literal`/`t.union` lower to
//! JSON columns and the CHECK constraints the structured shapes imply.
//!
//! The migration recorder emits these shapes (see
//! `packages/zero-migrate/src/ops.ts`); this fixture authors the same IR
//! directly and pins the engine render + fold on PostgreSQL, plus the union
//! flat-column layout on SQLite.

use crate::support;

use serde_json::{json, Value};
use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zeroship_migrate::{fold_ops, IrAuthor, LiveSchema};
use zeroship_migrate_postgres::backend::drift_sql::snapshot_schema;

const PROJECT_SCHEMA: &str = "app";
const OWNER: &str = "app_structured_types";

fn ir(name: &str, ops: Vec<Value>) -> MigrationIr {
    serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": ops,
    }))
    .expect("structured-types fixture must deserialize")
}

fn column(name: &str, ty: Value, nullable: bool) -> Value {
    json!({ "name": name, "type": ty, "nullable": nullable })
}

fn check(name: &str, expr: Value) -> Value {
    json!({ "name": name, "kind": { "kind": "check", "expr": expr } })
}

fn col_ref(name: &str) -> Value {
    json!({ "node": "colRef", "name": name })
}

fn literal(value: Value) -> Value {
    json!({ "node": "literal", "value": value })
}

fn create_table(name: &str, columns: Vec<Value>, constraints: Vec<Value>) -> Value {
    json!({
        "op": "createTable",
        "name": name,
        "columns": columns,
        "constraints": constraints,
        "indexes": [],
    })
}

fn structured_ir() -> MigrationIr {
    ir(
        "structured_types",
        vec![
            create_table(
                "objects",
                vec![column("payload", json!("json"), true)],
                vec![],
            ),
            create_table(
                "literals",
                vec![column("kind", json!("text"), false)],
                vec![check(
                    "literals_kind_lit_chk",
                    json!({ "node": "binOp", "op": "eq", "lhs": col_ref("kind"), "rhs": literal(json!("login")) }),
                )],
            ),
            create_table(
                "events",
                vec![
                    column("kind", json!("text"), false),
                    column("userId", json!("double"), true),
                    column("ip", json!("text"), true),
                    column("message", json!("text"), true),
                    column("stack", json!("text"), true),
                    column("name", json!("text"), true),
                    column("value", json!("double"), true),
                ],
                vec![
                    check(
                        "events_kind_enum_chk",
                        json!({
                            "node": "inList",
                            "expr": col_ref("kind"),
                            "elems": ["login", "error", "metric"],
                            "negated": false
                        }),
                    ),
                    check(
                        "events_kind_login_chk",
                        json!({
                            "node": "binOp",
                            "op": "or",
                            "lhs": { "node": "binOp", "op": "ne", "lhs": col_ref("kind"), "rhs": literal(json!("login")) },
                            "rhs": {
                                "node": "binOp",
                                "op": "and",
                                "lhs": { "node": "unaryOp", "op": "isNotNull", "operand": col_ref("userId") },
                                "rhs": { "node": "unaryOp", "op": "isNotNull", "operand": col_ref("ip") }
                            }
                        }),
                    ),
                    check(
                        "events_kind_error_chk",
                        json!({
                            "node": "binOp",
                            "op": "or",
                            "lhs": { "node": "binOp", "op": "ne", "lhs": col_ref("kind"), "rhs": literal(json!("error")) },
                            "rhs": { "node": "unaryOp", "op": "isNotNull", "operand": col_ref("message") }
                        }),
                    ),
                    check(
                        "events_kind_metric_chk",
                        json!({
                            "node": "binOp",
                            "op": "or",
                            "lhs": { "node": "binOp", "op": "ne", "lhs": col_ref("kind"), "rhs": literal(json!("metric")) },
                            "rhs": {
                                "node": "binOp",
                                "op": "and",
                                "lhs": { "node": "unaryOp", "op": "isNotNull", "operand": col_ref("name") },
                                "rhs": { "node": "unaryOp", "op": "isNotNull", "operand": col_ref("value") }
                            }
                        }),
                    ),
                ],
            ),
        ],
    )
}

fn live_pg_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let ordinal = NEXT.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_nanos();
    format!("{}_{}_{}", std::process::id(), nanos, ordinal)
}

fn create_marker(dialect: &zeroship_migrate::DialectId, table: &str) -> String {
    if dialect == &zeroship_migrate_postgres::DIALECT {
        format!("CREATE TABLE \"{PROJECT_SCHEMA}\".\"{table}\"")
    } else if dialect == &zeroship_migrate_sqlite::DIALECT {
        format!("CREATE TABLE \"{table}\"")
    } else {
        panic!("unregistered test dialect {dialect}")
    }
}

fn create_sql<'a>(
    migrations: &'a [zeroship_migrate::Migration],
    dialect: &zeroship_migrate::DialectId,
    table: &str,
) -> &'a str {
    let marker = create_marker(dialect, table);
    migrations
        .iter()
        .find(|migration| migration.up.starts_with(&marker))
        .unwrap_or_else(|| panic!("missing {marker} in {migrations:#?}"))
        .up
        .as_str()
}

#[test]
fn postgres_renders_object_literal_and_union_columns_with_checks() {
    let ir = structured_ir();
    let migrations = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        PROJECT_SCHEMA,
        OWNER,
        &zeroship_migrate_postgres::DIALECT,
        &support::no_inject(PROJECT_SCHEMA),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("structured types must lower on PostgreSQL");

    let objects = create_sql(&migrations, &zeroship_migrate_postgres::DIALECT, "objects");
    assert!(objects.contains("\"payload\" jsonb"), "{objects}");

    let literals = create_sql(&migrations, &zeroship_migrate_postgres::DIALECT, "literals");
    assert!(
        literals.contains("CONSTRAINT \"literals_kind_lit_chk\" CHECK")
            && literals.contains("\"kind\" = 'login'"),
        "{literals}"
    );

    let events = create_sql(&migrations, &zeroship_migrate_postgres::DIALECT, "events");
    assert!(events.contains("\"kind\" text NOT NULL"), "{events}");
    assert!(events.contains("\"userId\" double precision"), "{events}");
    assert!(
        events.contains(
            "\"kind\" = ANY (ARRAY['login'::text, 'error'::text, 'metric'::text])"
        ),
        "{events}"
    );
    assert!(
        events.contains("(\"kind\" <> 'login') OR ((\"userId\" IS NOT NULL) AND (\"ip\" IS NOT NULL))"),
        "{events}"
    );
    assert!(
        events.contains("(\"kind\" <> 'error') OR (\"message\" IS NOT NULL)"),
        "{events}"
    );
    assert!(
        !events.contains("\"stack\" IS NOT NULL"),
        "an optional variant field must not be in a CHECK: {events}"
    );
}

#[test]
fn postgres_fold_recovers_the_literal_and_union_check_constraints() {
    let ir = structured_ir();
    let snapshot = fold_ops(
        zeroship_migrate::shipping_vendors(),
        &ir.ops,
        &zeroship_migrate_postgres::DIALECT,
        PROJECT_SCHEMA,
        &support::no_inject(PROJECT_SCHEMA),
    )
    .expect("structured types must fold on PostgreSQL");

    let literals = &snapshot.tables["literals"];
    let literal_check = literals
        .constraints
        .iter()
        .find(|c| c.name == "literals_kind_lit_chk")
        .expect("literal CHECK is folded");
    assert_eq!(literal_check.kind, "CHECK");

    let events = &snapshot.tables["events"];
    for name in [
        "events_kind_enum_chk",
        "events_kind_login_chk",
        "events_kind_error_chk",
        "events_kind_metric_chk",
    ] {
        assert!(
            events.constraints.iter().any(|c| c.name == name),
            "missing folded constraint {name}: {:#?}",
            events.constraints
        );
    }
}

#[test]
fn sqlite_renders_the_union_flat_column_layout() {
    // SQLite has no fold scope for arbitrary authored CHECK identity, so the
    // constraint-free column layout is what the engine carries there.
    let ir = ir(
        "structured_types_sqlite",
        vec![create_table(
            "events",
            vec![
                column("kind", json!("text"), false),
                column("userId", json!("double"), true),
                column("ip", json!("text"), true),
            ],
            vec![],
        )],
    );
    let migrations = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        PROJECT_SCHEMA,
        OWNER,
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject(PROJECT_SCHEMA),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("the union flat-column layout must lower on SQLite");

    let events = create_sql(&migrations, &zeroship_migrate_sqlite::DIALECT, "events");
    assert!(events.contains("\"kind\" TEXT NOT NULL"), "{events}");
    assert!(events.contains("\"userId\""), "{events}");
    assert!(events.contains("\"ip\""), "{events}");
}

#[compio::test]
async fn live_postgres_introspection_recovers_the_structured_type_checks() {
    let url = require_live_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("structured_{}", live_pg_token());
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create isolated structured-types schema");

    let ir = structured_ir();
    let migrations = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        &schema,
        OWNER,
        &zeroship_migrate_postgres::DIALECT,
        &support::no_inject(&schema),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("structured types must lower on PostgreSQL");
    for migration in &migrations {
        session
            .batch(&migration.up)
            .await
            .expect("apply structured-types migration");
    }

    let snapshot = snapshot_schema(&session, &schema)
        .await
        .expect("introspect structured types");
    let literals = snapshot.tables.get("literals").expect("literals table");
    assert!(
        literals
            .constraints
            .iter()
            .any(|c| c.name == "literals_kind_lit_chk"),
        "live introspection lost the literal CHECK: {:#?}",
        literals.constraints
    );
    let events = snapshot.tables.get("events").expect("events table");
    for name in [
        "events_kind_enum_chk",
        "events_kind_login_chk",
        "events_kind_error_chk",
        "events_kind_metric_chk",
    ] {
        assert!(
            events.constraints.iter().any(|c| c.name == name),
            "live introspection lost {name}: {:#?}",
            events.constraints
        );
    }
}
