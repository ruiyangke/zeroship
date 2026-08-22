//! MySQL refuses two storage shapes the engine renders without complaint, and
//! `lint` reports both green because `render_ir_ops` degrades every lowering
//! error to a `-- [runtime-resolved]` comment. The load-and-validate gate is the
//! only seam whose verdict lint prints, and apply runs the same gate, so both
//! rules live there, dialect-scoped to MySQL.
//!
//! 1. A BARE LITERAL `DEFAULT` on a column whose RENDERED MySQL storage is
//!    TEXT/BLOB/JSON/GEOMETRY. MySQL 8 refuses that unconditionally, but it
//!    ACCEPTS a parenthesized expression default, which is how this engine
//!    already renders bytes (`DEFAULT (X'..')`) and JSON container defaults.
//! 2. A key (index / primary key / unique) over a bare TEXT column with no
//!    prefix length, MySQL error 1170.
//!
//! Both rules key on the RENDERED storage, never the authored type name. An
//! authored `t.text()` carrying a value format or a legacy id prefix renders
//! `VARCHAR(191)`, so it takes a literal DEFAULT and indexes without a prefix
//! length; a name-keyed rule would wrongly refuse it. The mirror case (an
//! authored name that says "bounded" over a rendered bare `TEXT`) is only
//! expressible on the descriptor path and is covered by the agreement test
//! beside the renderer.

use std::collections::HashMap;

use crate::support;
use zero_migrate::model::ir::{
    AlterPrimaryKeyAction, ColType, ColumnReference, EmptyContainerKind, IndexElement, IrColumn,
    IrConstraint, IrConstraintKind, IrDefault, IrFlagsOverride, IrIndex, IrScalar, Op, ValueFormat,
};
use zero_migrate::model::validate::validate_ir;
use zero_migrate::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor,
    IndexDescriptor, MigrationIr, SchemaSnapshot, CURRENT_IR_VERSION,
};

const OWNER: &str = "app_mysql_storage";

fn ir(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "mysql_storage_shapes".to_string(),
        owner_app: OWNER.to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn column(name: &str, ty: ColType) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: None,
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        vector_metric: None,
        case_sensitive: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

fn create_table(name: &str, columns: Vec<IrColumn>) -> Op {
    Op::CreateTable {
        name: name.to_string(),
        columns,
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

fn create_index(table: &str, name: &str, column: &str) -> Op {
    Op::CreateIndex {
        table: table.to_string(),
        columns: vec![IndexElement::Column {
            name: column.to_string(),
            order: None,
            opclass: None,
            collation: None,
        }],
        name: Some(name.to_string()),
        unique: None,
        using: None,
        r#where: None,
        concurrently: None,
        include: vec![],
        with: None,
        only: None,
        nulls_not_distinct: None,
        schema: None,
        existence_guard: None,
    }
}

fn key_element(name: &str) -> IndexElement {
    IndexElement::Column {
        name: name.to_string(),
        order: None,
        opclass: None,
        collation: None,
    }
}

fn inline_index(name: &str, column: &str) -> IrIndex {
    IrIndex {
        name: Some(name.to_string()),
        columns: vec![key_element(column)],
        unique: None,
        using: None,
        r#where: None,
        include: vec![],
        with: None,
        only: None,
        nulls_not_distinct: None,
    }
}

fn unique_constraint(name: &str, column: &str) -> IrConstraint {
    IrConstraint {
        name: Some(name.to_string()),
        kind: IrConstraintKind::Unique {
            columns: vec![column.to_string()],
        },
    }
}

fn fk_constraint(
    name: &str,
    column: &str,
    references_table: &str,
    references_column: &str,
) -> IrConstraint {
    IrConstraint {
        name: Some(name.to_string()),
        kind: IrConstraintKind::Fk {
            columns: vec![column.to_string()],
            references_table: references_table.to_string(),
            references_columns: vec![references_column.to_string()],
            on_delete: None,
            on_update: None,
            deferrable: None,
            initially_deferred: None,
            not_valid: None,
        },
    }
}

fn assert_mysql_key_refusal(migration: &MigrationIr, position: &str, column: &str) {
    let rendered = refused(
        migration,
        &zero_migrate::MYSQL,
        "MySQL 1170: an unbounded TEXT key needs a prefix length",
    );
    assert!(
        rendered.contains(position),
        "the refusal should name the key carrier {position:?}: {rendered}"
    );
    assert!(
        rendered.contains(column),
        "the refusal should name the keyed column {column:?}: {rendered}"
    );
}

fn refused(migration: &MigrationIr, dialect: &zero_migrate::DialectId, what: &str) -> String {
    let error = validate_ir(migration, dialect).expect_err(what);
    assert_eq!(&error.dialect, dialect, "{what}: {error}");
    format!("{error}")
}

fn accepted(migration: &MigrationIr, dialect: &zero_migrate::DialectId, what: &str) {
    validate_ir(migration, dialect)
        .unwrap_or_else(|error| panic!("{what} should validate on {dialect:?}: {error}"));
}

// (a) The shape the `create_widgets` host fixture used to carry until it was
// bounded: `t.text().notNull().default("new")`.
#[test]
fn mysql_refuses_a_bare_literal_default_on_a_text_column() {
    let mut status = column("status", ColType::Text);
    status.nullable = Some(false);
    status.default = Some(IrDefault::Literal {
        value: IrScalar::Str("new".to_string()),
    });
    let migration = ir(vec![create_table("widgets", vec![status])]);

    let rendered = refused(
        &migration,
        &zero_migrate::MYSQL,
        "a literal DEFAULT on TEXT is fatal on MySQL",
    );
    assert!(
        rendered.contains("status"),
        "the refusal should name the column: {rendered}"
    );

    accepted(&migration, &zero_migrate::POSTGRES, "a text default");
    accepted(&migration, &zero_migrate::SQLITE, "a text default");
}

// (b) A bytes default renders `DEFAULT (X'..')`, an EXPRESSION MySQL accepts.
#[test]
fn mysql_accepts_a_bytes_default_that_renders_as_an_expression() {
    let mut blob = column("payload", ColType::Bytes);
    blob.default = Some(IrDefault::Literal {
        value: IrScalar::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
    });
    let migration = ir(vec![create_table("widgets", vec![blob])]);

    accepted(&migration, &zero_migrate::MYSQL, "a bytes default");
}

// (c) A JSON container default renders `DEFAULT (JSON_OBJECT())`.
#[test]
fn mysql_accepts_a_json_container_default_that_renders_as_an_expression() {
    let mut doc = column("doc", ColType::Json);
    doc.default = Some(IrDefault::Container {
        kind: EmptyContainerKind::Object,
    });
    let migration = ir(vec![create_table("widgets", vec![doc])]);

    accepted(&migration, &zero_migrate::MYSQL, "a json container default");
}

// (d) A bounded string renders VARCHAR(50), which takes a literal DEFAULT.
#[test]
fn mysql_accepts_a_literal_default_on_a_bounded_string_column() {
    let mut label = column("label", ColType::String { length: 50 });
    label.default = Some(IrDefault::Literal {
        value: IrScalar::Str("x".to_string()),
    });
    let migration = ir(vec![create_table("widgets", vec![label])]);

    accepted(&migration, &zero_migrate::MYSQL, "a bounded string default");
}

// (e) A case-insensitive text column also renders a bare MySQL TEXT.
#[test]
fn mysql_refuses_a_literal_default_on_a_case_insensitive_text_column() {
    let mut label = column("label", ColType::Text);
    label.case_sensitive = Some(false);
    label.default = Some(IrDefault::Literal {
        value: IrScalar::Str("x".to_string()),
    });
    let migration = ir(vec![create_table("widgets", vec![label])]);

    let rendered = refused(
        &migration,
        &zero_migrate::MYSQL,
        "caseInsensitive renders bare TEXT, so the literal DEFAULT is fatal",
    );
    assert!(
        rendered.contains("label"),
        "the refusal should name the column: {rendered}"
    );

    accepted(&migration, &zero_migrate::POSTGRES, "a citext default");
    accepted(&migration, &zero_migrate::SQLITE, "a NOCASE text default");
}

// (e') The rule keys on RENDERED storage, not the authored type name: a value
// format takes the column off the unbounded-text arm and onto VARCHAR(191).
#[test]
fn mysql_accepts_a_literal_default_on_a_value_formatted_text_column() {
    let mut ticket = column("ticket", ColType::Text);
    ticket.value_format = Some(ValueFormat::Ulid);
    ticket.default = Some(IrDefault::Literal {
        value: IrScalar::Str("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()),
    });
    let migration = ir(vec![create_table("widgets", vec![ticket])]);

    accepted(
        &migration,
        &zero_migrate::MYSQL,
        "a value-formatted text default",
    );
}

// (f) The shape the `create_gadgets` host fixture used to carry until it was
// bounded: an index over a bare text column declared in the SAME envelope.
#[test]
fn mysql_refuses_an_index_over_a_bare_text_column() {
    let migration = ir(vec![
        create_table("gadgets", vec![column("sku", ColType::Text)]),
        create_index("gadgets", "gadgets_sku_idx", "sku"),
    ]);

    let rendered = refused(
        &migration,
        &zero_migrate::MYSQL,
        "MySQL 1170: a TEXT key needs a prefix length",
    );
    assert!(
        rendered.contains("sku"),
        "the refusal should name the column: {rendered}"
    );

    accepted(&migration, &zero_migrate::POSTGRES, "a text index");
    accepted(&migration, &zero_migrate::SQLITE, "a text index");
}

// (g) A bounded string renders VARCHAR(50), which indexes without a prefix.
#[test]
fn mysql_accepts_an_index_over_a_bounded_string_column() {
    let migration = ir(vec![
        create_table(
            "gadgets",
            vec![column("sku", ColType::String { length: 50 })],
        ),
        create_index("gadgets", "gadgets_sku_idx", "sku"),
    ]);

    accepted(&migration, &zero_migrate::MYSQL, "a bounded string index");
}

// (g') An authored `t.text()` carrying a legacy id prefix renders VARCHAR(191),
// so it indexes without a prefix length even though its type name says text.
#[test]
fn mysql_accepts_an_index_over_an_id_prefixed_text_column() {
    let mut id = column("id", ColType::Text);
    id.id_prefix = Some("gdg".to_string());
    let migration = ir(vec![
        create_table("gadgets", vec![id]),
        create_index("gadgets", "gadgets_id_idx", "id"),
    ]);

    accepted(
        &migration,
        &zero_migrate::MYSQL,
        "an id-prefixed text index",
    );
}

#[test]
fn mysql_refuses_a_primary_key_over_unbounded_text() {
    let mut table = create_table("documents", vec![column("slug", ColType::Text)]);
    let Op::CreateTable { primary_key, .. } = &mut table else {
        unreachable!("the helper returns createTable")
    };
    *primary_key = Some(vec!["slug".to_string()]);

    assert_mysql_key_refusal(&ir(vec![table]), "createTable.primaryKey", "documents.slug");
}

#[test]
fn mysql_refuses_a_column_unique_key_over_unbounded_text() {
    let mut slug = column("slug", ColType::Text);
    slug.unique = Some(true);

    assert_mysql_key_refusal(
        &ir(vec![create_table("documents", vec![slug])]),
        "createTable column unique",
        "documents.slug",
    );
}

#[test]
fn mysql_refuses_an_inline_index_over_unbounded_text() {
    let mut table = create_table("documents", vec![column("body", ColType::Text)]);
    let Op::CreateTable { indexes, .. } = &mut table else {
        unreachable!("the helper returns createTable")
    };
    indexes.push(inline_index("documents_body_idx", "body"));

    assert_mysql_key_refusal(&ir(vec![table]), "createTable.indexes", "documents.body");
}

#[test]
fn mysql_refuses_a_table_unique_constraint_over_unbounded_text() {
    let mut table = create_table("documents", vec![column("slug", ColType::Text)]);
    let Op::CreateTable { constraints, .. } = &mut table else {
        unreachable!("the helper returns createTable")
    };
    constraints.push(unique_constraint("documents_slug_key", "slug"));

    assert_mysql_key_refusal(
        &ir(vec![table]),
        "createTable.constraints unique",
        "documents.slug",
    );
}

#[test]
fn mysql_refuses_an_alter_primary_key_target_over_unbounded_text() {
    let migration = ir(vec![
        create_table("documents", vec![column("slug", ColType::Text)]),
        Op::AlterPrimaryKey {
            table: "documents".to_string(),
            action: AlterPrimaryKeyAction::Add {
                columns: vec!["slug".to_string()],
            },
            schema: None,
        },
    ]);

    assert_mysql_key_refusal(&migration, "alterPrimaryKey target", "documents.slug");
}

#[test]
fn mysql_refuses_a_column_reference_that_would_synthesize_a_text_key() {
    let mut parent_id = column("id", ColType::String { length: 64 });
    parent_id.unique = Some(true);
    let mut parent_id_ref = column("parent_id", ColType::Text);
    parent_id_ref.references = Some(ColumnReference {
        table: "parents".to_string(),
        column: "id".to_string(),
        on_delete: None,
        on_update: None,
        name: Some("children_parent_fkey".to_string()),
    });
    let migration = ir(vec![
        create_table("parents", vec![parent_id]),
        create_table("children", vec![parent_id_ref]),
    ]);

    assert_mysql_key_refusal(
        &migration,
        "createTable column reference local key",
        "children.parent_id",
    );
}

#[test]
fn mysql_refuses_an_inline_foreign_key_that_would_synthesize_a_text_key() {
    let mut parent_id = column("id", ColType::String { length: 64 });
    parent_id.unique = Some(true);
    let mut child = create_table("children", vec![column("parent_id", ColType::Text)]);
    let Op::CreateTable { constraints, .. } = &mut child else {
        unreachable!("the helper returns createTable")
    };
    constraints.push(fk_constraint(
        "children_parent_fkey",
        "parent_id",
        "parents",
        "id",
    ));
    let migration = ir(vec![create_table("parents", vec![parent_id]), child]);

    assert_mysql_key_refusal(
        &migration,
        "createTable.constraints foreign key local key",
        "children.parent_id",
    );
}

#[test]
fn mysql_refuses_an_added_foreign_key_that_would_synthesize_a_text_key() {
    let mut parent_id = column("id", ColType::String { length: 64 });
    parent_id.unique = Some(true);
    let migration = ir(vec![
        create_table("parents", vec![parent_id]),
        create_table("children", vec![column("parent_id", ColType::Text)]),
        Op::AddConstraint {
            table: "children".to_string(),
            constraint: fk_constraint("children_parent_fkey", "parent_id", "parents", "id"),
            schema: None,
            existence_guard: None,
        },
    ]);

    assert_mysql_key_refusal(
        &migration,
        "addConstraint foreign key local key",
        "children.parent_id",
    );
}

#[test]
fn mysql_refuses_a_foreign_key_target_over_unbounded_text() {
    let mut target_slug = column("slug", ColType::Text);
    target_slug.unique = Some(true);
    let mut child = create_table(
        "children",
        vec![column("parent_slug", ColType::String { length: 64 })],
    );
    let Op::CreateTable { constraints, .. } = &mut child else {
        unreachable!("the helper returns createTable")
    };
    constraints.push(fk_constraint(
        "children_parent_fkey",
        "parent_slug",
        "parents",
        "slug",
    ));
    // Put the FK first so the target-key refusal is the first MySQL key-storage
    // verdict; declaration collection is deliberately two-pass and still sees the
    // later parent table.
    let migration = ir(vec![child, create_table("parents", vec![target_slug])]);

    assert_mysql_key_refusal(
        &migration,
        "createTable.constraints foreign key target key",
        "parents.slug",
    );
}

#[test]
fn mysql_declarative_refuses_the_live_fixture_shape_of_a_widthless_indexed_string() {
    let descriptor = CollectionDescriptor {
        name: "people".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![FieldDescriptor {
            name: "id".to_string(),
            ty: "string".to_string(),
            required: true,
            ..FieldDescriptor::default()
        }],
        indexes: vec![IndexDescriptor {
            name: "people_id_key".to_string(),
            columns: vec!["id".to_string()],
            unique: true,
        }],
        runtime_options: Default::default(),
    };
    let policy = support::no_inject("mysql_key_gate");
    let desired = desired_snapshot_for_dialect(
        "mysql_key_gate",
        &[descriptor],
        &zero_migrate::MYSQL,
        &policy,
    )
    .expect("the descriptor compiles before the storage gate runs");
    let error = DeclarativeAuthor::new_for_dialect("mysql_key_gate", OWNER, zero_migrate::MYSQL)
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &policy,
        )
        .expect_err("a widthless string renders TEXT and cannot back a MySQL index");
    let rendered = format!("{error}");
    assert!(rendered.contains("people_id_key"), "{rendered}");
    assert!(rendered.contains("people.id"), "{rendered}");
}

#[test]
fn mysql_declarative_refuses_an_implicit_foreign_key_index_over_widthless_string() {
    let parent = CollectionDescriptor {
        name: "parents".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![FieldDescriptor {
            name: "id".to_string(),
            ty: "string".to_string(),
            max_length: Some(64),
            required: true,
            unique: true,
            ..FieldDescriptor::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let child = CollectionDescriptor {
        name: "children".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![FieldDescriptor {
            name: "parent_id".to_string(),
            ty: "string".to_string(),
            required: true,
            references: Some("parents".to_string()),
            reference_column: Some("id".to_string()),
            ..FieldDescriptor::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    };
    let policy = support::no_inject("mysql_key_gate");
    let desired = desired_snapshot_for_dialect(
        "mysql_key_gate",
        &[parent, child],
        &zero_migrate::MYSQL,
        &policy,
    )
    .expect("the descriptors compile before the storage gate runs");
    let error = DeclarativeAuthor::new_for_dialect("mysql_key_gate", OWNER, zero_migrate::MYSQL)
        .diff(
            &desired,
            &SchemaSnapshot::default(),
            &HashMap::new(),
            &[],
            &policy,
        )
        .expect_err("InnoDB would synthesize an illegal index over the TEXT child column");
    let rendered = format!("{error}");
    assert!(rendered.contains("foreign key"), "{rendered}");
    assert!(rendered.contains("children.parent_id"), "{rendered}");
}
