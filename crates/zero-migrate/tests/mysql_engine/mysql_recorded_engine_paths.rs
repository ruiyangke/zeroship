//! The engine's apply, drift and fold paths driven over a canned MySQL backend.
//!
//! # Why these fourteen live here and the other fifty-five do not
//!
//! They arrived from `zero-migrate/src/apply/backend/mysql/mod.rs`, where all
//! sixty-nine were unit tests beside the backend, and they came HERE rather than to
//! `zero-migrate-mysql` because of what they name: `apply`/`apply_with_lock_backend`,
//! `MigrationEngine`, `AppliedPlan`, `diff_snapshots` and `fold_ops` are the ENGINE's,
//! and `zero-migrate` depends on `zero-migrate-mysql`, so a vendor crate reaching back
//! for them is a cycle Cargo refuses. That is not a technicality about where a file
//! sits: a test that drives the engine over a MySQL backend IS an integration test of
//! the engine, and this is the engine's test tree.
//!
//! The other fifty-five ask only what SQL the backend emits, in what order, with what
//! binds. Those stayed with the code they describe.
//!
//! # The recorder is the SAME object, not a copy
//!
//! Both halves drive `zero_migrate_mysql::backend::recording::RecordingSession` and
//! its canned `information_schema` rows. That shared premise is the whole reason it is
//! published through the vendor crate's `testing` feature instead of copied over here:
//! two copies of a canned catalog drift silently, and one suite would go on asserting
//! against a table shape the other had already corrected.

use serde_json::json;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::drift::diff_snapshots;
use zero_migrate::apply::executor::ApplyError;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::driver::Bind;
use zero_migrate::driver::{Row, Value};
use zero_migrate::model::expr::Expr;
use zero_migrate::model::ir::{
    ColType, IdentityCol, IrColumn, IrConstraint, IrConstraintKind, IrDefault, MigrationIr, Op,
    ValueFormat, CURRENT_IR_VERSION,
};
use zero_migrate::model::snapshot::{IdDefaultSnapshot, SchemaSnapshot};
use zero_migrate::render::plan::DatabaseFeature;
use zero_migrate_mysql::backend::recording::*;
use zero_migrate_mysql::MysqlBackend;

use crate::support;

/// The dialect these snapshots are folded and diffed under.
///
/// Spelled from the IR's open dialect identity rather than reached through the
/// backend's `DIALECT` const, which is `pub(crate)` to `zero-migrate-mysql` and stays
/// that way: exactly one line in that crate names the vendor, and a test on this side
/// of the boundary is not a reason to make it two.
const DIALECT: zero_migrate_ir::dialect::DialectId = zero_migrate_mysql::DIALECT;

/// This vendor's own renderers, for the value-format doors these tests call.
///
/// The engine's `render::value_format` doors are a PRIVATE module: they resolve a
/// renderer from a `DialectId` for the engine's own callers, and nothing outside the
/// crate reaches them. These tests hold the vendor, so they name the neutral seam in
/// `zero-migrate-backend` and hand it MySQL's pair directly — the same `&'static`
/// objects `VENDOR` registers, so the metadata is byte-for-byte what the backend
/// itself renders.
const MYSQL_VALUE_FORMAT: &dyn zero_migrate_backend::value_format::ValueFormatRenderer =
    zero_migrate_mysql::VENDOR.value_format;
const MYSQL_DML: &dyn zero_migrate_backend::renderer::DmlRenderer = zero_migrate_mysql::VENDOR.dml;

#[compio::test]
async fn uuid_v4_capability_failure_precedes_authored_sql() {
    let rec =
        RecordingSession::with_uuid_capabilities("8.0.12", "InnoDB", Some("DEFAULT"), "ROW", "ROW");
    let backend = MysqlBackend::new_generic(&rec);
    let cfg = ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x"));
    let migration = trivial_migration();
    let authored_sql = migration.up.clone();
    let mut plan = zero_migrate::AppliedPlan::single_step(migration);
    plan.database_requirements
        .require(DatabaseFeature::UuidV4Generation);

    let error = zero_migrate::MigrationEngine::new(zero_migrate::shipping_vendors())
        .apply_applied_plan_with_touched_and_depends(
            &plan,
            &[],
            &[],
            zero_migrate::approval::Approval::None,
            &backend,
            &cfg,
            "tester",
            zero_migrate::apply::executor::LockMode::Acquire,
        )
        .await
        .expect_err("the unsupported server must stop the complete plan");
    assert!(error.to_string().contains("MySQL 8.0.13"), "got: {error}");
    let log = rec.log.borrow();
    assert!(
        log.iter()
            .any(|entry| entry.contains("VERSION() AS server_version")),
        "the capability probe must run: {log:?}"
    );
    assert!(
        !log.iter().any(|entry| entry.contains(&authored_sql)),
        "authored SQL must not run after capability failure: {log:?}"
    );
}

#[compio::test]
async fn snapshot_schema_reads_canonical_columns_and_ordered_unique_indexes() {
    let rec = RecordingSession::with_catalog(
        vec![Row::new(
            vec!["table_name".into()],
            vec![Value::Text("users".into())],
        )],
        vec![
            catalog_column("users", "id", "bigint(20)", None, None, false, 1),
            catalog_column("users", "tenant_id", "bigint", None, None, false, 2),
            catalog_column(
                "users",
                "email",
                "varchar(191)",
                Some("utf8mb4"),
                Some("utf8mb4_0900_ai_ci"),
                false,
                3,
            ),
            catalog_column(
                "users",
                "nickname",
                "varchar(40)",
                Some("utf8mb4"),
                Some("utf8mb4_bin"),
                true,
                4,
            ),
        ],
        vec![
            catalog_index_part("users", "PRIMARY", 0, 1, Some("id"), None, Some("A"), None),
            catalog_index_part(
                "users",
                "idx_nickname_prefix",
                1,
                1,
                Some("nickname"),
                Some(8),
                Some("A"),
                None,
            ),
            catalog_index_part(
                "users",
                "uq_users_tenant_email",
                0,
                1,
                Some("tenant_id"),
                None,
                Some("A"),
                None,
            ),
            catalog_index_part(
                "users",
                "uq_users_tenant_email",
                0,
                2,
                Some("email"),
                None,
                Some("D"),
                None,
            ),
        ],
        vec![
            catalog_foreign_key_part(
                "users",
                "users_id_fkey",
                1,
                1,
                "id",
                "proj_x",
                "users",
                "id",
                "RESTRICT",
                "CASCADE",
            ),
            catalog_foreign_key_part(
                "users",
                "users_tenant_email_fkey",
                1,
                1,
                "tenant_id",
                "proj_x",
                "users",
                "tenant_id",
                "CASCADE",
                "SET NULL",
            ),
            catalog_foreign_key_part(
                "users",
                "users_tenant_email_fkey",
                2,
                2,
                "email",
                "proj_x",
                "users",
                "email",
                "CASCADE",
                "SET NULL",
            ),
        ],
    );
    let backend = MysqlBackend::new_generic(&rec);
    let cfg = ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x"));

    let snapshot = backend
        .snapshot_schema(&cfg)
        .await
        .expect("MySQL catalog snapshot");

    let users = snapshot.tables.get("users").expect("users table");
    assert_eq!(
        users
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.data_type.as_str(),
                column.nullable,
                column.case_sensitive,
                column
                    .text_storage
                    .as_ref()
                    .map(|storage| (storage.character_set.as_str(), storage.collation.as_str(),)),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "email",
                "text",
                false,
                Some(false),
                Some(("utf8mb4", "utf8mb4_0900_ai_ci")),
            ),
            ("id", "bigint", false, None, None),
            (
                "nickname",
                "text",
                true,
                None,
                Some(("utf8mb4", "utf8mb4_bin")),
            ),
            ("tenant_id", "bigint", false, None, None),
        ]
    );
    assert_eq!(
        users
            .indexes
            .iter()
            .map(|index| index.name.as_str())
            .collect::<Vec<_>>(),
        // `PRIMARY` is the canned catalog's OWN value for this row, carried through
        // instead of replaced. The reader used to substitute `users_pkey` here — a
        // name no MySQL server holds — to satisfy a primary-key predicate in the
        // neutral contract crate that recognised only one other vendor's
        // convention. It sorts first now because it is uppercase, which is the only
        // reason the two entries below moved.
        vec!["PRIMARY", "idx_nickname_prefix", "uq_users_tenant_email"]
    );
    assert!(users.indexes[0].unique);
    assert_eq!(users.indexes[0].columns, ["id"]);
    assert!(users.indexes[1].columns.is_empty());
    assert!(users.indexes[2].unique);
    assert_eq!(users.indexes[2].columns, ["tenant_id", "email"]);
    assert_eq!(users.constraints.len(), 3);
    let primary = users
        .constraints
        .iter()
        .find(|constraint| constraint.name == "PRIMARY")
        .expect("primary key");
    assert_eq!(primary.kind, "PRIMARY KEY");
    assert_eq!(primary.definition, "PRIMARY KEY (id)");
    let single_fk = users
        .constraints
        .iter()
        .find(|constraint| constraint.name == "users_id_fkey")
        .expect("single-column FK");
    assert_eq!(single_fk.kind, "FOREIGN KEY");
    assert_eq!(
        single_fk.definition,
        "FOREIGN KEY (id) REFERENCES proj_x.users(id) ON DELETE CASCADE"
    );
    let composite_fk = users
        .constraints
        .iter()
        .find(|constraint| constraint.name == "users_tenant_email_fkey")
        .expect("composite FK");
    assert_eq!(composite_fk.kind, "FOREIGN KEY");
    assert_eq!(
        composite_fk.definition,
        "FOREIGN KEY (tenant_id, email) REFERENCES proj_x.users(tenant_id, email) ON UPDATE CASCADE ON DELETE SET NULL"
    );
    assert!(
        diff_snapshots(zero_migrate::shipping_vendors(), &snapshot, &snapshot).is_clean(),
        "the exact single/composite FK catalog must stay clean"
    );
    let assert_fk_drop = |constraint_name: &str, scenario: &str| {
        let mut changed = snapshot.clone();
        changed
            .tables
            .get_mut("users")
            .expect("users table")
            .constraints
            .retain(|constraint| constraint.name != constraint_name);
        let drift = diff_snapshots(zero_migrate::shipping_vendors(), &snapshot, &changed);
        assert!(
            drift
                .missing_objects
                .iter()
                .any(|missing| missing.contains(constraint_name)),
            "{scenario} must drift: {drift:?}"
        );
    };
    let assert_fk_definition_drift = |constraint_name: &str, definition: &str, scenario: &str| {
        let mut changed = snapshot.clone();
        changed
            .tables
            .get_mut("users")
            .expect("users table")
            .constraints
            .iter_mut()
            .find(|constraint| constraint.name == constraint_name)
            .expect("foreign key")
            .definition = definition.to_string();
        let drift = diff_snapshots(zero_migrate::shipping_vendors(), &snapshot, &changed);
        assert!(
            drift.altered_objects.iter().any(|altered| {
                altered.object == format!("constraint {constraint_name}")
                    && altered.field == "definition"
            }),
            "{scenario} must drift: {drift:?}"
        );
    };

    assert_fk_drop("users_id_fkey", "a dropped single-column FK");
    assert_fk_definition_drift(
        "users_id_fkey",
        "FOREIGN KEY (id) REFERENCES proj_x.accounts(id) ON DELETE CASCADE",
        "a repointed single-column FK",
    );
    assert_fk_definition_drift(
        "users_id_fkey",
        "FOREIGN KEY (id) REFERENCES proj_x.users(id) ON UPDATE CASCADE ON DELETE SET NULL",
        "a single-column FK action change",
    );

    assert_fk_drop("users_tenant_email_fkey", "a dropped composite foreign key");
    assert_fk_definition_drift(
        "users_tenant_email_fkey",
        "FOREIGN KEY (tenant_id, email) REFERENCES proj_x.accounts(tenant_id, email) ON UPDATE CASCADE ON DELETE SET NULL",
        "a repointed composite foreign key",
    );
    assert_fk_definition_drift(
        "users_tenant_email_fkey",
        "FOREIGN KEY (email, tenant_id) REFERENCES proj_x.users(email, tenant_id) ON UPDATE CASCADE ON DELETE SET NULL",
        "a reordered composite foreign key",
    );
    assert_fk_definition_drift(
        "users_tenant_email_fkey",
        "FOREIGN KEY (tenant_id, email) REFERENCES proj_x.users(tenant_id, email) ON UPDATE NO ACTION ON DELETE CASCADE",
        "a composite foreign-key action change",
    );
    let all = rec.log.borrow().join("\n");
    assert!(
        all.contains("TABLE_TYPE = 'BASE TABLE'")
            && all.contains("COLUMN_TYPE AS column_type")
            && all.contains("CHARACTER_SET_NAME AS character_set_name")
            && all.contains("COLLATION_NAME AS collation_name")
            && all.contains("ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX")
            && all.contains("information_schema.REFERENTIAL_CONSTRAINTS")
            && all.contains("POSITION_IN_UNIQUE_CONSTRAINT")
            && all
                .contains("ORDER BY kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION")
            && all.contains("information_schema.VIEWS")
            && all.contains("VIEW_DEFINITION AS view_definition")
            && !all.contains("information_schema.CHECK_CONSTRAINTS")
            && !all.contains("INDEX_NAME <> 'PRIMARY'"),
        "snapshot must use schema-scoped authoritative catalog reads and gate CHECK_CONSTRAINTS below MySQL 8.0.16: {all}"
    );
    assert_eq!(
        rec.binds
            .borrow()
            .iter()
            .filter(|binds| binds.as_slice() == [Bind::Text("proj_x".to_string())])
            .count(),
        5,
        "every catalog query scopes itself with the project database bind"
    );
}

#[compio::test]
async fn mysql_single_and_composite_fk_mutations_drift_through_catalog_rows() {
    let single = |target: &str, update: &str, delete: &str| {
        catalog_foreign_key_part(
            "child",
            "child_single_fkey",
            1,
            1,
            "single_parent_id",
            "proj_x",
            target,
            "id",
            update,
            delete,
        )
    };
    let composite = |target: &str, reversed: bool, update: &str, delete: &str| -> Vec<Row> {
        let (first_local, first_target, second_local, second_target) = if reversed {
            ("parent_right", "right_id", "parent_left", "left_id")
        } else {
            ("parent_left", "left_id", "parent_right", "right_id")
        };
        vec![
            catalog_foreign_key_part(
                "child",
                "child_composite_fkey",
                1,
                1,
                first_local,
                "proj_x",
                target,
                first_target,
                update,
                delete,
            ),
            catalog_foreign_key_part(
                "child",
                "child_composite_fkey",
                2,
                2,
                second_local,
                "proj_x",
                target,
                second_target,
                update,
                delete,
            ),
        ]
    };
    let baseline_rows = || {
        let mut rows = vec![single("parent", "RESTRICT", "CASCADE")];
        rows.extend(composite("parent", false, "CASCADE", "SET NULL"));
        rows
    };
    let snapshot = |foreign_keys: Vec<Row>| async move {
        let session = RecordingSession::with_catalog(
            ["alternate_parent", "child", "parent"]
                .into_iter()
                .map(|table| {
                    Row::new(
                        vec!["table_name".into()],
                        vec![Value::Text(table.to_string())],
                    )
                })
                .collect(),
            vec![
                catalog_column("alternate_parent", "id", "bigint", None, None, false, 1),
                catalog_column(
                    "alternate_parent",
                    "left_id",
                    "bigint",
                    None,
                    None,
                    false,
                    2,
                ),
                catalog_column(
                    "alternate_parent",
                    "right_id",
                    "bigint",
                    None,
                    None,
                    false,
                    3,
                ),
                catalog_column("child", "single_parent_id", "bigint", None, None, true, 1),
                catalog_column("child", "parent_left", "bigint", None, None, true, 2),
                catalog_column("child", "parent_right", "bigint", None, None, true, 3),
                catalog_column("parent", "id", "bigint", None, None, false, 1),
                catalog_column("parent", "left_id", "bigint", None, None, false, 2),
                catalog_column("parent", "right_id", "bigint", None, None, false, 3),
            ],
            Vec::new(),
            foreign_keys,
        );
        MysqlBackend::new_generic(&session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                support::no_inject("proj_x"),
            ))
            .await
            .expect("MySQL FK catalog snapshot")
    };

    let expected = snapshot(baseline_rows()).await;
    let clean = snapshot(baseline_rows()).await;
    assert!(
        diff_snapshots(zero_migrate::shipping_vendors(), &expected, &clean).is_clean(),
        "unchanged catalog FK rows must stay clean"
    );
    let assert_missing = |actual: &SchemaSnapshot, constraint: &str, label: &str| {
        let drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, actual);
        assert!(
            drift
                .missing_objects
                .iter()
                .any(|missing| missing.contains(constraint)),
            "{label} must be missing drift after catalog introspection: {drift:#?}"
        );
    };
    let assert_altered = |actual: &SchemaSnapshot, constraint: &str, label: &str| {
        let drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, actual);
        assert!(
            drift.altered_objects.iter().any(|altered| {
                altered.object == format!("constraint {constraint}")
                    && altered.field == "definition"
            }),
            "{label} must be definition drift after catalog introspection: {drift:#?}"
        );
    };

    let dropped_single = snapshot(composite("parent", false, "CASCADE", "SET NULL")).await;
    assert_missing(
        &dropped_single,
        "child_single_fkey",
        "dropped single-column FK",
    );
    let mut repointed_single_rows = vec![single("alternate_parent", "RESTRICT", "CASCADE")];
    repointed_single_rows.extend(composite("parent", false, "CASCADE", "SET NULL"));
    assert_altered(
        &snapshot(repointed_single_rows).await,
        "child_single_fkey",
        "repointed single-column FK",
    );
    let mut changed_single_action = vec![single("parent", "CASCADE", "SET NULL")];
    changed_single_action.extend(composite("parent", false, "CASCADE", "SET NULL"));
    assert_altered(
        &snapshot(changed_single_action).await,
        "child_single_fkey",
        "single-column FK action change",
    );

    let dropped_composite = snapshot(vec![single("parent", "RESTRICT", "CASCADE")]).await;
    assert_missing(
        &dropped_composite,
        "child_composite_fkey",
        "dropped composite FK",
    );
    for (label, rows) in [
        (
            "repointed composite FK",
            composite("alternate_parent", false, "CASCADE", "SET NULL"),
        ),
        (
            "reordered composite FK",
            composite("parent", true, "CASCADE", "SET NULL"),
        ),
        (
            "composite FK action change",
            composite("parent", false, "RESTRICT", "CASCADE"),
        ),
    ] {
        let mut variant = vec![single("parent", "RESTRICT", "CASCADE")];
        variant.extend(rows);
        assert_altered(&snapshot(variant).await, "child_composite_fkey", label);
    }
}

#[compio::test]
async fn snapshot_schema_recovers_mysql_identity_id_defaults_and_format_checks() {
    let uuid_generated_check = zero_migrate_backend::value_format::uuid_column_metadata(
        "generated_uuid",
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("UUID metadata")
    .expect("MySQL UUID CHECK")
    .inline_check;
    let uuid_supplied_check = zero_migrate_backend::value_format::uuid_column_metadata(
        "supplied_uuid",
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("UUID metadata")
    .expect("MySQL UUID CHECK")
    .inline_check;
    let type_id_check = zero_migrate_backend::value_format::column_metadata(
        "type_id",
        &ValueFormat::TypeId {
            prefix: "user".to_string(),
        },
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("TypeID metadata")
    .inline_check;
    let ulid_check = zero_migrate_backend::value_format::column_metadata(
        "ulid",
        &ValueFormat::Ulid,
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("ULID metadata")
    .inline_check;
    let table_rows = || {
        vec![Row::new(
            vec!["table_name".into()],
            vec![Value::Text("ids".into())],
        )]
    };
    let primary_index = || {
        vec![catalog_index_part(
            "ids",
            "PRIMARY",
            0,
            1,
            Some("auto_id"),
            None,
            Some("A"),
            None,
        )]
    };
    let checks = || {
        vec![
            catalog_check("ids", "ids_chk_1", true, &uuid_generated_check),
            catalog_check("ids", "ids_chk_2", true, &uuid_supplied_check),
            catalog_check("ids", "ids_chk_3", true, &type_id_check),
            catalog_check("ids", "ids_chk_4", true, &ulid_check),
        ]
    };
    let id_column = |name: &str, ty: ColType| IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(false),
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    };
    let mut auto_id = id_column("auto_id", ColType::BigInt);
    auto_id.identity = Some(IdentityCol { always: false });
    let mut generated_uuid = id_column("generated_uuid", ColType::Uuid);
    generated_uuid.default = Some(IrDefault::Expr { expr: Expr::UuidV4 });
    let supplied_uuid = id_column("supplied_uuid", ColType::Uuid);
    let mut type_id = id_column("type_id", ColType::Text);
    type_id.value_format = Some(ValueFormat::TypeId {
        prefix: "user".to_string(),
    });
    let mut ulid = id_column("ulid", ColType::Text);
    ulid.value_format = Some(ValueFormat::Ulid);
    let ordinary = id_column("ordinary", ColType::Text);
    let expected = zero_migrate::render::fold::fold_ops(
        zero_migrate::shipping_vendors(),
        &[Op::CreateTable {
            attributes: zero_migrate_ir::attribute::TableAttributes::new(),
            name: "ids".to_string(),
            columns: vec![
                auto_id,
                generated_uuid,
                supplied_uuid,
                type_id,
                ulid,
                ordinary,
            ],
            primary_key: Some(vec!["auto_id".to_string()]),
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }],
        &DIALECT,
        "proj_x",
        &support::no_inject("app"),
    )
    .expect("portable ID table fold");

    let rec = RecordingSession::with_catalog_checks(
        table_rows(),
        id_catalog_columns(Some(MYSQL_CATALOG_UUID_V4_DEFAULT), None, None, None),
        primary_index(),
        Vec::new(),
        checks(),
    );
    let snapshot = MysqlBackend::new_generic(&rec)
        .snapshot_schema(&ExecutorConfig::new(
            "prj_x",
            "proj_x",
            support::no_inject("proj_x"),
        ))
        .await
        .expect("ID-aware MySQL catalog snapshot");
    let ids = &snapshot.tables["ids"];
    let column = |name: &str| {
        ids.columns
            .iter()
            .find(|column| column.name == name)
            .expect("catalog column")
    };
    assert_eq!(
        column("auto_id").identity,
        Some(IdentityCol { always: false })
    );
    assert_eq!(
        column("auto_id").id_default,
        Some(IdDefaultSnapshot::Absent)
    );
    assert_eq!(
        column("generated_uuid").id_default,
        Some(IdDefaultSnapshot::UuidV4),
        "real MySQL catalog normalization must retain UUIDv4 semantics"
    );
    assert_eq!(
        column("supplied_uuid").id_default,
        Some(IdDefaultSnapshot::Absent)
    );
    assert_eq!(
        column("type_id").value_format,
        Some(ValueFormat::TypeId {
            prefix: "user".to_string()
        })
    );
    assert_eq!(
        column("type_id").id_default,
        Some(IdDefaultSnapshot::Absent)
    );
    assert_eq!(column("ulid").value_format, Some(ValueFormat::Ulid));
    assert_eq!(column("ulid").id_default, Some(IdDefaultSnapshot::Absent));
    assert_eq!(column("ordinary").id_default, None);
    let clean_drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, &snapshot);
    assert!(
        clean_drift.is_clean(),
        "the portable auto-increment and exact format/default catalog shape must stay clean: {clean_drift:?}"
    );
    assert!(
        rec.log
            .borrow()
            .iter()
            .any(|sql| sql.contains("information_schema.CHECK_CONSTRAINTS")
                && sql.contains("tc.ENFORCED AS enforced")),
        "MySQL 8.0.16+ must introspect enforced CHECK clauses"
    );

    let account_type_id_check = zero_migrate_backend::value_format::column_metadata(
        "type_id",
        &ValueFormat::TypeId {
            prefix: "account".to_string(),
        },
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("altered TypeID metadata")
    .inline_check;
    let altered_formats = RecordingSession::with_catalog_checks(
        table_rows(),
        id_catalog_columns(Some(MYSQL_CATALOG_UUID_V4_DEFAULT), None, None, None),
        primary_index(),
        Vec::new(),
        vec![
            catalog_check("ids", "ids_chk_1", true, &uuid_generated_check),
            catalog_check("ids", "ids_chk_2", true, &uuid_supplied_check),
            catalog_check("ids", "ids_chk_3", true, &account_type_id_check),
            // The ULID CHECK was dropped out of band. A disabled CHECK is
            // likewise not an enforced format contract.
            catalog_check("ids", "ids_chk_4", false, &ulid_check),
        ],
    );
    let altered_formats = MysqlBackend::new_generic(&altered_formats)
        .snapshot_schema(&ExecutorConfig::new(
            "prj_x",
            "proj_x",
            support::no_inject("proj_x"),
        ))
        .await
        .expect("altered format snapshot");
    let format_drift = diff_snapshots(
        zero_migrate::shipping_vendors(),
        &snapshot,
        &altered_formats,
    );
    assert!(
        format_drift.altered_objects.iter().any(|altered| {
            altered.object == "column type_id"
                && altered.field == "format"
                && altered.actual == "typeId(account)"
        }),
        "a TypeID prefix mismatch must drift: {format_drift:?}"
    );
    assert!(
        format_drift
            .altered_objects
            .iter()
            .any(|altered| { altered.object == "column ulid" && altered.field == "format" }),
        "a dropped or unenforced ULID CHECK must drift: {format_drift:?}"
    );

    let altered_defaults = RecordingSession::with_catalog_checks(
        table_rows(),
        id_catalog_columns(
            None,
            Some(MYSQL_CATALOG_UUID_V4_DEFAULT),
            Some("make_typeid()"),
            Some("make_ulid()"),
        ),
        primary_index(),
        Vec::new(),
        checks(),
    );
    let altered_defaults = MysqlBackend::new_generic(&altered_defaults)
        .snapshot_schema(&ExecutorConfig::new(
            "prj_x",
            "proj_x",
            support::no_inject("proj_x"),
        ))
        .await
        .expect("altered default snapshot");
    let default_drift = diff_snapshots(
        zero_migrate::shipping_vendors(),
        &snapshot,
        &altered_defaults,
    );
    for name in ["generated_uuid", "supplied_uuid", "type_id", "ulid"] {
        assert!(
            default_drift.altered_objects.iter().any(|altered| {
                altered.object == format!("column {name}") && altered.field == "default"
            }),
            "an ID-default add/remove/swap on {name} must drift: {default_drift:?}"
        );
    }
    let swapped_default = RecordingSession::with_catalog_checks(
        table_rows(),
        id_catalog_columns(Some("uuid()"), None, None, None),
        primary_index(),
        Vec::new(),
        checks(),
    );
    let swapped_default = MysqlBackend::new_generic(&swapped_default)
        .snapshot_schema(&ExecutorConfig::new(
            "prj_x",
            "proj_x",
            support::no_inject("proj_x"),
        ))
        .await
        .expect("swapped default snapshot");
    let swapped_default_drift = diff_snapshots(
        zero_migrate::shipping_vendors(),
        &snapshot,
        &swapped_default,
    );
    assert!(
        swapped_default_drift.altered_objects.iter().any(|altered| {
            altered.object == "column generated_uuid"
                && altered.field == "default"
                && altered.expected == "uuidV4"
                && altered.actual
                    == "call:uuid()"
        }),
        "swapping the exact UUIDv4 generator for MySQL UUIDv1 must drift: {swapped_default_drift:?}"
    );

    let literal_generator_without_check = RecordingSession::with_catalog_checks(
        table_rows(),
        id_catalog_columns_with_generated_uuid_extra(
            Some(MYSQL_CATALOG_UUID_V4_DEFAULT),
            None,
            None,
            None,
            "",
        ),
        primary_index(),
        Vec::new(),
        vec![
            catalog_check("ids", "ids_chk_2", true, &uuid_supplied_check),
            catalog_check("ids", "ids_chk_3", true, &type_id_check),
            catalog_check("ids", "ids_chk_4", true, &ulid_check),
        ],
    );
    let literal_generator_without_check =
        MysqlBackend::new_generic(&literal_generator_without_check)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                support::no_inject("proj_x"),
            ))
            .await
            .expect("same-text literal UUID default without format CHECK");
    let marker_drift = diff_snapshots(
        zero_migrate::shipping_vendors(),
        &snapshot,
        &literal_generator_without_check,
    );
    assert!(
        marker_drift.altered_objects.iter().any(|altered| {
            altered.object == "column generated_uuid"
                && altered.field == "default"
                && altered.expected == "uuidV4"
                && altered.actual.contains(MYSQL_CATALOG_UUID_V4_DEFAULT)
        }),
        "dropping the UUID CHECK and changing its same-text generator into a literal must drift: {marker_drift:#?}"
    );

    let literal_uuid_snapshot = |value: &'static str| async move {
        let session = RecordingSession::with_catalog_checks(
            table_rows(),
            id_catalog_columns_with_generated_uuid_extra(Some(value), None, None, None, ""),
            primary_index(),
            Vec::new(),
            checks(),
        );
        MysqlBackend::new_generic(&session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                support::no_inject("proj_x"),
            ))
            .await
            .expect("literal UUID catalog snapshot")
    };
    let lowercase_literal = literal_uuid_snapshot("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").await;
    let uppercase_literal = literal_uuid_snapshot("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA").await;
    let case_drift = diff_snapshots(
        zero_migrate::shipping_vendors(),
        &lowercase_literal,
        &uppercase_literal,
    );
    assert!(
        case_drift.altered_objects.iter().any(|altered| {
            altered.object == "column generated_uuid" && altered.field == "default"
        }),
        "MySQL UUID VARCHAR literal case changes must drift: {case_drift:#?}"
    );

    let mut altered_identity = snapshot.clone();
    let altered_ids = altered_identity.tables.get_mut("ids").expect("ids table");
    altered_ids
        .columns
        .iter_mut()
        .find(|column| column.name == "auto_id")
        .expect("auto_id")
        .identity = None;
    altered_ids
        .columns
        .iter_mut()
        .find(|column| column.name == "ordinary")
        .expect("ordinary")
        .identity = Some(IdentityCol { always: false });
    let identity_drift = diff_snapshots(
        zero_migrate::shipping_vendors(),
        &snapshot,
        &altered_identity,
    );
    for name in ["auto_id", "ordinary"] {
        assert!(
            identity_drift.altered_objects.iter().any(|altered| {
                altered.object == format!("column {name}") && altered.field == "identity"
            }),
            "an AUTO_INCREMENT drop/add on {name} must drift: {identity_drift:?}"
        );
    }
    altered_identity
        .tables
        .get_mut("ids")
        .expect("ids table")
        .columns
        .iter_mut()
        .find(|column| column.name == "auto_id")
        .expect("auto_id")
        .identity = Some(IdentityCol { always: true });
    assert!(
        diff_snapshots(
            zero_migrate::shipping_vendors(),
            &snapshot,
            &altered_identity
        )
        .altered_objects
        .iter()
        .any(|altered| { altered.object == "column auto_id" && altered.field == "identity" }),
        "an always/by-default identity flip must drift"
    );
}

#[compio::test]
async fn mysql_auto_increment_add_and_drop_are_recovered_from_catalog_extra() {
    let column = |identity: Option<IdentityCol>| IrColumn {
        name: "id".to_string(),
        ty: ColType::BigInt,
        nullable: Some(false),
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity,
    };
    let expected = |identity| {
        zero_migrate::render::fold::fold_ops(
            zero_migrate::shipping_vendors(),
            &[Op::CreateTable {
                attributes: zero_migrate_ir::attribute::TableAttributes::new(),
                name: "identity_probe".to_string(),
                columns: vec![column(identity)],
                primary_key: Some(vec!["id".to_string()]),
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            &DIALECT,
            "proj_x",
            &support::no_inject("app"),
        )
        .expect("identity probe must fold")
    };
    let snapshot = |extra: &'static str| async move {
        let session = RecordingSession::with_catalog(
            vec![Row::new(
                vec!["table_name".into()],
                vec![Value::Text("identity_probe".into())],
            )],
            vec![catalog_column_with_generation(
                "identity_probe",
                "id",
                "bigint",
                None,
                None,
                false,
                1,
                None,
                extra,
            )],
            vec![catalog_index_part(
                "identity_probe",
                "PRIMARY",
                0,
                1,
                Some("id"),
                None,
                Some("A"),
                None,
            )],
            Vec::new(),
        );
        MysqlBackend::new_generic(&session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                support::no_inject("proj_x"),
            ))
            .await
            .expect("identity catalog snapshot")
    };

    let auto = snapshot("auto_increment").await;
    let plain = snapshot("").await;
    let expected_auto = expected(Some(IdentityCol { always: false }));
    let expected_plain = expected(None);
    assert!(
        diff_snapshots(zero_migrate::shipping_vendors(), &expected_auto, &auto).is_clean(),
        "portable AUTO_INCREMENT must match catalog EXTRA"
    );
    assert!(
        diff_snapshots(zero_migrate::shipping_vendors(), &expected_plain, &plain).is_clean(),
        "portable non-identity key must remain clean"
    );
    for (expected_snapshot, actual_snapshot, label) in
        [(&auto, &plain, "drop"), (&plain, &auto, "add")]
    {
        let drift = diff_snapshots(
            zero_migrate::shipping_vendors(),
            expected_snapshot,
            actual_snapshot,
        );
        assert!(
            drift
                .altered_objects
                .iter()
                .any(|altered| { altered.object == "column id" && altered.field == "identity" }),
            "an out-of-band AUTO_INCREMENT {label} must drift: {drift:#?}"
        );
    }
}

#[compio::test]
async fn snapshot_schema_compares_mysql_literal_defaults_on_format_typed_references() {
    const LITERAL: &str = "account_00000000000000000000000000";
    const CHANGED_LITERAL: &str = "account_00000000000000000000000001";
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_typed_reference_literal_default",
        "owner_app": "app_mysql_drift",
        "ops": [
            {
                "op": "createTable",
                "name": "parents",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } },
                    "default": { "literal": { "value": LITERAL } }
                }],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "children",
                "columns": [{
                    "name": "parent_id",
                    "type": "text",
                    "nullable": true,
                    "valueFormat": { "typeId": { "prefix": "account" } },
                    "default": { "literal": { "value": LITERAL } },
                    "references": {
                        "table": "parents",
                        "column": "id",
                        "onDelete": "cascade",
                        "onUpdate": "cascade"
                    }
                }],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            }
        ]
    }))
    .expect("typed-reference literal fixture must deserialize");
    let expected = zero_migrate::render::fold::fold_ops(
        zero_migrate::shipping_vendors(),
        &ir.ops,
        &DIALECT,
        "proj_x",
        &support::no_inject("app"),
    )
    .expect("typed-reference literal fixture must fold");
    let fk_name = expected.tables["children"]
        .constraints
        .iter()
        .find(|constraint| constraint.kind == "FOREIGN KEY")
        .expect("typed reference folds to a foreign key")
        .name
        .clone();
    let type_id_check = zero_migrate_backend::value_format::column_metadata(
        "id",
        &ValueFormat::TypeId {
            prefix: "account".to_string(),
        },
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("TypeID metadata")
    .inline_check;
    let table_rows = || {
        ["children", "parents"]
            .into_iter()
            .map(|table| {
                Row::new(
                    vec!["table_name".into()],
                    vec![Value::Text(table.to_string())],
                )
            })
            .collect()
    };
    let column_rows = |child_default: Option<&str>| {
        vec![
            catalog_column_with_generation(
                "children",
                "parent_id",
                "varchar(191)",
                Some("ascii"),
                Some("ascii_bin"),
                true,
                1,
                child_default,
                "",
            ),
            catalog_column_with_generation(
                "parents",
                "id",
                "varchar(191)",
                Some("ascii"),
                Some("ascii_bin"),
                false,
                1,
                Some(LITERAL),
                "",
            ),
        ]
    };
    let foreign_keys = || {
        vec![catalog_foreign_key_part(
            "children",
            &fk_name,
            1,
            1,
            "parent_id",
            "proj_x",
            "parents",
            "id",
            "CASCADE",
            "CASCADE",
        )]
    };
    let checks = || {
        vec![catalog_check(
            "parents",
            "parents_chk_1",
            true,
            &type_id_check,
        )]
    };

    let clean_session = RecordingSession::with_catalog_checks(
        table_rows(),
        column_rows(Some(LITERAL)),
        Vec::new(),
        foreign_keys(),
        checks(),
    );
    let clean = MysqlBackend::new_generic(&clean_session)
        .snapshot_schema(&ExecutorConfig::new(
            "prj_x",
            "proj_x",
            support::no_inject("proj_x"),
        ))
        .await
        .expect("clean typed-reference literal snapshot");
    let child = clean.tables["children"]
        .columns
        .iter()
        .find(|column| column.name == "parent_id")
        .expect("typed reference column");
    assert_eq!(
        child.id_default,
        Some(IdDefaultSnapshot::Literal(
            serde_json::to_string(LITERAL).expect("literal serializes")
        )),
        "the child inherits its ID-default comparison surface through the foreign key"
    );
    assert_eq!(child.default.as_deref(), Some(LITERAL));
    assert_eq!(
        clean.tables["parents"].columns[0].id_default,
        Some(IdDefaultSnapshot::Literal(
            serde_json::to_string(LITERAL).expect("literal serializes")
        )),
        "a non-expression MySQL COLUMN_DEFAULT must recover as a string literal"
    );
    let drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, &clean);
    assert!(
        drift.is_clean(),
        "bare MySQL COLUMN_DEFAULT literals must match authored typed literals: {drift:#?}"
    );

    for (catalog_default, label) in [(Some(CHANGED_LITERAL), "changed"), (None, "removed")] {
        let session = RecordingSession::with_catalog_checks(
            table_rows(),
            column_rows(catalog_default),
            Vec::new(),
            foreign_keys(),
            checks(),
        );
        let actual = MysqlBackend::new_generic(&session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                support::no_inject("proj_x"),
            ))
            .await
            .unwrap_or_else(|error| panic!("{label} literal snapshot: {error}"));
        let drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, &actual);
        assert!(
            drift.altered_objects.iter().any(|altered| {
                altered.object == "column parent_id" && altered.field == "default"
            }),
            "an out-of-band {label} typed-reference literal must drift: {drift:#?}"
        );
    }
}

#[compio::test]
async fn snapshot_schema_preserves_mysql_expression_markers_on_fk_columns() {
    let table_rows = || {
        ["children", "parents"]
            .into_iter()
            .map(|table| {
                Row::new(
                    vec!["table_name".into()],
                    vec![Value::Text(table.to_string())],
                )
            })
            .collect()
    };
    let column_rows = |extra: &str| {
        vec![
            catalog_column_with_generation(
                "children",
                "parent_id",
                "varchar(36)",
                Some("ascii"),
                Some("ascii_bin"),
                true,
                1,
                Some(MYSQL_CATALOG_UUID_V4_DEFAULT),
                extra,
            ),
            catalog_column_with_generation(
                "parents",
                "id",
                "varchar(36)",
                Some("ascii"),
                Some("ascii_bin"),
                false,
                1,
                None,
                "",
            ),
        ]
    };
    let foreign_keys = || {
        vec![catalog_foreign_key_part(
            "children",
            "children_parent_id_fkey",
            1,
            1,
            "parent_id",
            "proj_x",
            "parents",
            "id",
            "NO ACTION",
            "NO ACTION",
        )]
    };
    let snapshot = |extra: &'static str| async move {
        let session = RecordingSession::with_catalog(
            table_rows(),
            column_rows(extra),
            Vec::new(),
            foreign_keys(),
        );
        MysqlBackend::new_generic(&session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                support::no_inject("proj_x"),
            ))
            .await
            .expect("typed-reference expression snapshot")
    };

    let expression = snapshot("DEFAULT_GENERATED").await;
    let literal = snapshot("").await;
    let child_default = |snapshot: &SchemaSnapshot| {
        snapshot.tables["children"]
            .columns
            .iter()
            .find(|column| column.name == "parent_id")
            .and_then(|column| column.id_default.clone())
    };
    assert_eq!(child_default(&expression), Some(IdDefaultSnapshot::UuidV4));
    assert_eq!(
        child_default(&literal),
        Some(IdDefaultSnapshot::Literal(
            serde_json::to_string(MYSQL_CATALOG_UUID_V4_DEFAULT)
                .expect("literal generator spelling serializes")
        ))
    );
    let drift = diff_snapshots(zero_migrate::shipping_vendors(), &expression, &literal);
    assert!(
        drift
            .altered_objects
            .iter()
            .any(|altered| { altered.object == "column parent_id" && altered.field == "default" }),
        "a same-text literal must not satisfy an expression default: {drift:#?}"
    );
}

#[compio::test]
async fn mysql_key_format_checks_drop_prefix_and_clause_changes_drift_from_catalog() {
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_key_formats",
        "owner_app": "app_mysql_drift",
        "ops": [
            {
                "op": "createTable",
                "name": "type_keys",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "ulid_keys",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": "ulid"
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            }
        ]
    }))
    .expect("MySQL key-format fixture must deserialize");
    let expected = zero_migrate::render::fold::fold_ops(
        zero_migrate::shipping_vendors(),
        &ir.ops,
        &DIALECT,
        "proj_x",
        &support::no_inject("app"),
    )
    .expect("MySQL key-format fixture must fold");
    let type_check = zero_migrate_backend::value_format::column_metadata(
        "id",
        &ValueFormat::TypeId {
            prefix: "account".to_string(),
        },
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("TypeID key metadata")
    .inline_check;
    let team_check = zero_migrate_backend::value_format::column_metadata(
        "id",
        &ValueFormat::TypeId {
            prefix: "team".to_string(),
        },
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("altered TypeID key metadata")
    .inline_check;
    let ulid_check = zero_migrate_backend::value_format::column_metadata(
        "id",
        &ValueFormat::Ulid,
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("ULID key metadata")
    .inline_check;
    let altered_ulid_check =
        ulid_check.replacen("CHAR_LENGTH(`id`) = 26", "CHAR_LENGTH(`id`) = 25", 1);
    assert_ne!(
        altered_ulid_check, ulid_check,
        "ULID clause fixture must change"
    );

    let snapshot = |checks: Vec<Row>| async move {
        let session = RecordingSession::with_catalog_checks(
            ["type_keys", "ulid_keys"]
                .into_iter()
                .map(|table| {
                    Row::new(
                        vec!["table_name".into()],
                        vec![Value::Text(table.to_string())],
                    )
                })
                .collect(),
            ["type_keys", "ulid_keys"]
                .into_iter()
                .map(|table| {
                    catalog_column(
                        table,
                        "id",
                        "varchar(191)",
                        Some("ascii"),
                        Some("ascii_bin"),
                        false,
                        1,
                    )
                })
                .collect(),
            ["type_keys", "ulid_keys"]
                .into_iter()
                .map(|table| {
                    catalog_index_part(table, "PRIMARY", 0, 1, Some("id"), None, Some("A"), None)
                })
                .collect(),
            Vec::new(),
            checks,
        );
        MysqlBackend::new_generic(&session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                support::no_inject("proj_x"),
            ))
            .await
            .expect("MySQL key-format catalog snapshot")
    };
    let check =
        |table: &str, clause: &str| catalog_check(table, &format!("{table}_chk_1"), true, clause);
    let clean = snapshot(vec![
        check("type_keys", &type_check),
        check("ulid_keys", &ulid_check),
    ])
    .await;
    let clean_drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, &clean);
    assert!(
        clean_drift.is_clean(),
        "authored key formats must match MySQL catalog CHECKs: {clean_drift:#?}"
    );

    for (label, checks, table, actual) in [
        (
            "TypeID drop",
            vec![check("ulid_keys", &ulid_check)],
            "type_keys",
            "",
        ),
        (
            "TypeID prefix change",
            vec![
                check("type_keys", &team_check),
                check("ulid_keys", &ulid_check),
            ],
            "type_keys",
            "typeId(team)",
        ),
        (
            "ULID drop",
            vec![check("type_keys", &type_check)],
            "ulid_keys",
            "",
        ),
        (
            "ULID clause change",
            vec![
                check("type_keys", &type_check),
                check("ulid_keys", &altered_ulid_check),
            ],
            "ulid_keys",
            "",
        ),
    ] {
        let actual_snapshot = snapshot(checks).await;
        let drift = diff_snapshots(
            zero_migrate::shipping_vendors(),
            &expected,
            &actual_snapshot,
        );
        assert!(
            drift.altered_objects.iter().any(|altered| {
                altered.table == table
                    && altered.object == "column id"
                    && altered.field == "format"
                    && altered.actual == actual
            }),
            "{label} must drift through catalog introspection: {drift:#?}"
        );
    }
}

#[compio::test]
async fn snapshot_schema_rejects_semantically_regrouped_mysql_format_check() {
    let value_format = ValueFormat::TypeId {
        prefix: "account".to_string(),
    };
    let ir: MigrationIr = serde_json::from_value(json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "mysql_regrouped_type_id_check",
        "owner_app": "app_mysql_drift",
        "ops": [{
            "op": "createTable",
            "name": "ids",
            "columns": [{
                "name": "id",
                "type": "text",
                "nullable": false,
                "valueFormat": { "typeId": { "prefix": "account" } }
            }],
            "primaryKey": null,
            "constraints": [],
            "indexes": []
        }]
    }))
    .expect("regrouped TypeID fixture must deserialize");
    let expected = zero_migrate::render::fold::fold_ops(
        zero_migrate::shipping_vendors(),
        &ir.ops,
        &DIALECT,
        "proj_x",
        &support::no_inject("app"),
    )
    .expect("regrouped TypeID fixture must fold");
    let canonical = zero_migrate_backend::value_format::column_metadata(
        "id",
        &value_format,
        MYSQL_VALUE_FORMAT,
        MYSQL_DML,
    )
    .expect("TypeID metadata")
    .inline_check;
    let regrouped = canonical.replacen("CHECK (", "CHECK (((", 1).replacen(
        "OR (CHAR_LENGTH(`id`) = 34 AND ",
        "OR CHAR_LENGTH(`id`) = 34) AND ",
        1,
    );
    assert_ne!(regrouped, canonical, "the CHECK mutation must take effect");
    let erase_grouping = |sql: &str| {
        sql.chars()
            .filter(|character| !character.is_whitespace() && !matches!(character, '(' | ')'))
            .collect::<String>()
    };
    assert_eq!(
        erase_grouping(&regrouped),
        erase_grouping(&canonical),
        "the regression must keep token order and differ only in semantic grouping"
    );

    let session = RecordingSession::with_catalog_checks(
        vec![Row::new(
            vec!["table_name".into()],
            vec![Value::Text("ids".into())],
        )],
        vec![catalog_column_with_generation(
            "ids",
            "id",
            "varchar(191)",
            Some("ascii"),
            Some("ascii_bin"),
            false,
            1,
            None,
            "",
        )],
        Vec::new(),
        Vec::new(),
        vec![catalog_check("ids", "ids_chk_1", true, &regrouped)],
    );
    let actual = MysqlBackend::new_generic(&session)
        .snapshot_schema(&ExecutorConfig::new(
            "prj_x",
            "proj_x",
            support::no_inject("proj_x"),
        ))
        .await
        .expect("regrouped TypeID snapshot");
    assert_eq!(
        actual.tables["ids"].columns[0].value_format, None,
        "a regrouped nullable guard is not the canonical TypeID contract"
    );
    let drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, &actual);
    assert!(
        drift
            .altered_objects
            .iter()
            .any(|altered| { altered.object == "column id" && altered.field == "format" }),
        "semantic CHECK regrouping must surface as format drift: {drift:#?}"
    );
}

#[compio::test]
async fn named_table_unique_candidate_and_composite_fk_have_clean_mysql_drift() {
    fn bigint_column(name: &str, nullable: bool) -> IrColumn {
        IrColumn {
            name: name.to_string(),
            ty: ColType::BigInt,
            nullable: Some(nullable),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    let parent_key_name = "parents_tenant_external_key";
    let child_fk_name = "children_parent_fkey";
    let ops = vec![
        Op::CreateTable {
            attributes: zero_migrate_ir::attribute::TableAttributes::new(),
            name: "parents".to_string(),
            columns: vec![
                bigint_column("tenant_id", false),
                bigint_column("external_id", false),
            ],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: Some(parent_key_name.to_string()),
                kind: IrConstraintKind::Unique {
                    columns: vec!["tenant_id".to_string(), "external_id".to_string()],
                },
            }],
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        },
        Op::CreateTable {
            attributes: zero_migrate_ir::attribute::TableAttributes::new(),
            name: "children".to_string(),
            columns: vec![
                bigint_column("parent_tenant", true),
                bigint_column("parent_external", true),
            ],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: Some(child_fk_name.to_string()),
                kind: IrConstraintKind::Fk {
                    columns: vec!["parent_tenant".to_string(), "parent_external".to_string()],
                    references_table: "parents".to_string(),
                    references_columns: vec!["tenant_id".to_string(), "external_id".to_string()],
                    on_delete: None,
                    on_update: None,
                    deferrable: None,
                    initially_deferred: None,
                    not_valid: None,
                },
            }],
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        },
    ];
    let expected = zero_migrate::render::fold::fold_ops(
        zero_migrate::shipping_vendors(),
        &ops,
        &DIALECT,
        "proj_x",
        &support::no_inject("app"),
    )
    .expect("named table UNIQUE and composite FK fold for MySQL");

    let parent_candidate = expected.tables["parents"]
        .indexes
        .iter()
        .find(|index| index.name == parent_key_name)
        .expect("table UNIQUE canonicalizes to its MySQL unique key");
    assert!(parent_candidate.unique);
    assert_eq!(parent_candidate.columns, ["tenant_id", "external_id"]);
    assert!(
        expected.tables["parents"]
            .constraints
            .iter()
            .all(|constraint| constraint.name != parent_key_name),
        "MySQL cannot recover whether a unique key was authored as CONSTRAINT or INDEX"
    );

    let rec = RecordingSession::with_catalog(
        vec![
            Row::new(
                vec!["table_name".into()],
                vec![Value::Text("children".into())],
            ),
            Row::new(
                vec!["table_name".into()],
                vec![Value::Text("parents".into())],
            ),
        ],
        vec![
            catalog_column("children", "parent_tenant", "bigint", None, None, true, 1),
            catalog_column("children", "parent_external", "bigint", None, None, true, 2),
            catalog_column("parents", "tenant_id", "bigint", None, None, false, 1),
            catalog_column("parents", "external_id", "bigint", None, None, false, 2),
        ],
        vec![
            catalog_index_part(
                "children",
                "children_parent_fkey_idx",
                1,
                1,
                Some("parent_tenant"),
                None,
                Some("A"),
                None,
            ),
            catalog_index_part(
                "children",
                "children_parent_fkey_idx",
                1,
                2,
                Some("parent_external"),
                None,
                Some("A"),
                None,
            ),
            catalog_index_part(
                "parents",
                parent_key_name,
                0,
                1,
                Some("tenant_id"),
                None,
                Some("A"),
                None,
            ),
            catalog_index_part(
                "parents",
                parent_key_name,
                0,
                2,
                Some("external_id"),
                None,
                Some("A"),
                None,
            ),
        ],
        vec![
            catalog_foreign_key_part(
                "children",
                child_fk_name,
                1,
                1,
                "parent_tenant",
                "proj_x",
                "parents",
                "tenant_id",
                "RESTRICT",
                "RESTRICT",
            ),
            catalog_foreign_key_part(
                "children",
                child_fk_name,
                2,
                2,
                "parent_external",
                "proj_x",
                "parents",
                "external_id",
                "RESTRICT",
                "RESTRICT",
            ),
        ],
    );
    let actual = MysqlBackend::new_generic(&rec)
        .snapshot_schema(&ExecutorConfig::new(
            "prj_x",
            "proj_x",
            support::no_inject("proj_x"),
        ))
        .await
        .expect("equivalent MySQL catalog snapshot");

    assert_eq!(
        actual.tables["parents"]
            .indexes
            .iter()
            .find(|index| index.name == parent_key_name)
            .expect("introspected candidate key")
            .columns,
        ["tenant_id", "external_id"],
        "candidate-key tuple order must survive MySQL introspection"
    );
    let drift = diff_snapshots(zero_migrate::shipping_vendors(), &expected, &actual);
    assert!(
        drift.is_clean(),
        "named table UNIQUE + composite FK must round-trip without false MySQL drift: {drift:?}"
    );
}

#[compio::test]
async fn snapshot_failure_aborts_before_author_sql_and_releases_lock() {
    let rec = RecordingSession::with_failure("@@SESSION.sql_mode");
    let backend = MysqlBackend::new_generic(&rec);
    let cfg = ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x"));
    let migration = trivial_migration();

    let result = zero_migrate::apply::executor::apply(
        zero_migrate::shipping_vendors(),
        &backend,
        &cfg,
        &[migration],
        zero_migrate::Approval::Approved,
        "tester",
    )
    .await;

    assert!(
        matches!(result, Err(ApplyError::Backend(ref message))
            if message.contains("cannot verify the dedicated session is idle")),
        "{result:?}"
    );
    let all = rec.log.borrow().join("\n");
    assert!(all.contains("GET_LOCK(?, ?)") && all.contains("RELEASE_LOCK(?)"));
    assert!(
        !all.contains("CREATE TABLE t (id INT)") && !all.contains("CREATE DATABASE IF NOT EXISTS"),
        "snapshot failure must stop before journal bootstrap and author SQL: {all}"
    );
}

#[compio::test]
async fn active_caller_transaction_is_rejected_before_autocommit_or_author_sql() {
    let rec = RecordingSession::with_in_transaction(1);
    let backend = MysqlBackend::new_generic(&rec);
    let cfg = ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x"));
    let migration = trivial_migration();

    let result = zero_migrate::apply::executor::apply(
        zero_migrate::shipping_vendors(),
        &backend,
        &cfg,
        &[migration],
        zero_migrate::Approval::Approved,
        "tester",
    )
    .await;

    assert!(
        matches!(result, Err(ApplyError::Backend(ref message))
            if message.contains("dedicated idle session")
                && message.contains("active transaction")),
        "{result:?}"
    );
    let all = rec.log.borrow().join("\n");
    assert!(
        all.contains("performance_schema.events_transactions_current")
            && all.contains("PS_CURRENT_THREAD_ID()")
    );
    assert!(all.contains("RELEASE_LOCK(?)"));
    assert!(
        !all.contains("SESSION autocommit = 1")
            && !all.contains("CREATE TABLE t (id INT)")
            && !all.contains("CREATE DATABASE IF NOT EXISTS"),
        "an active caller transaction must remain untouched: {all}"
    );
}

#[compio::test]
async fn successful_apply_surfaces_restore_failure_after_releasing_lock() {
    let rec = RecordingSession::with_failure("SET SESSION sql_mode = ?");
    let backend = MysqlBackend::new_generic(&rec);
    let cfg = ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x"));
    let migration = trivial_migration();

    let result = zero_migrate::apply::executor::apply(
        zero_migrate::shipping_vendors(),
        &backend,
        &cfg,
        &[migration],
        zero_migrate::Approval::Approved,
        "tester",
    )
    .await;

    assert!(matches!(result, Err(ApplyError::Db(_))), "{result:?}");
    let log = rec.log.borrow();
    let author = log
        .iter()
        .position(|entry| entry == "batch: CREATE TABLE t (id INT)")
        .expect("author SQL ran");
    let restore = log
        .iter()
        .position(|entry| entry.contains("SET SESSION sql_mode = ?"))
        .expect("restore was attempted");
    let release = log
        .iter()
        .rposition(|entry| entry.contains("RELEASE_LOCK(?)"))
        .expect("project lock was released");
    assert!(author < restore && restore < release, "{log:?}");
}

#[compio::test]
async fn whole_plan_preflight_refuses_delete_before_earlier_insert() {
    let rec = RecordingSession::new();
    let backend = MysqlBackend::new_generic(&rec);
    let cfg = ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x"));
    let (insert, _) = plan_dml_step("insert user", false);
    let (delete, delete_version) = plan_dml_step("delete user", true);

    let result = zero_migrate::MigrationEngine::new(zero_migrate::shipping_vendors())
        .apply_plan_with_touched_and_depends_scoped(
            &[insert, delete],
            &["users".into()],
            &[],
            zero_migrate::approval::Approval::Approved,
            &zero_migrate::approval::ApprovalScope::Versions(Default::default()),
            &backend,
            &cfg,
            "tester",
            zero_migrate::apply::executor::LockMode::Acquire,
            None,
        )
        .await;

    assert!(matches!(
        result,
        Err(zero_migrate::engine::DeclarativeApplyError::Plain(
            zero_migrate::engine::EngineError::ApprovalNotScoped { ref version }
        )) if version == delete_version.as_str()
    ));
    let log = rec.log.borrow();
    assert!(
        !log.iter().any(|entry| {
            entry.contains("INSERT INTO `proj_x`.`users`")
                || entry.contains("DELETE FROM `proj_x`.`users`")
                || entry.contains("information_schema.TABLES")
        }),
        "scope preflight must refuse before either target inspection or mutation: {log:?}"
    );
}
