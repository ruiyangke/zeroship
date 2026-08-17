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

use zero_migrate::model::ir::{
    ColType, EmptyContainerKind, IndexElement, IrColumn, IrDefault, IrFlagsOverride, IrScalar, Op,
    ValueFormat,
};
use zero_migrate::model::validate::{validate_ir, Dialect};
use zero_migrate::{MigrationIr, CURRENT_IR_VERSION};

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

fn refused(migration: &MigrationIr, dialect: Dialect, what: &str) -> String {
    let error = validate_ir(migration, dialect).expect_err(what);
    assert_eq!(error.dialect, dialect, "{what}: {error}");
    format!("{error}")
}

fn accepted(migration: &MigrationIr, dialect: Dialect, what: &str) {
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
        Dialect::Mysql,
        "a literal DEFAULT on TEXT is fatal on MySQL",
    );
    assert!(
        rendered.contains("status"),
        "the refusal should name the column: {rendered}"
    );

    accepted(&migration, Dialect::Postgres, "a text default");
    accepted(&migration, Dialect::Sqlite, "a text default");
}

// (b) A bytes default renders `DEFAULT (X'..')`, an EXPRESSION MySQL accepts.
#[test]
fn mysql_accepts_a_bytes_default_that_renders_as_an_expression() {
    let mut blob = column("payload", ColType::Bytes);
    blob.default = Some(IrDefault::Literal {
        value: IrScalar::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
    });
    let migration = ir(vec![create_table("widgets", vec![blob])]);

    accepted(&migration, Dialect::Mysql, "a bytes default");
}

// (c) A JSON container default renders `DEFAULT (JSON_OBJECT())`.
#[test]
fn mysql_accepts_a_json_container_default_that_renders_as_an_expression() {
    let mut doc = column("doc", ColType::Json);
    doc.default = Some(IrDefault::Container {
        kind: EmptyContainerKind::Object,
    });
    let migration = ir(vec![create_table("widgets", vec![doc])]);

    accepted(&migration, Dialect::Mysql, "a json container default");
}

// (d) A bounded string renders VARCHAR(50), which takes a literal DEFAULT.
#[test]
fn mysql_accepts_a_literal_default_on_a_bounded_string_column() {
    let mut label = column("label", ColType::String { length: 50 });
    label.default = Some(IrDefault::Literal {
        value: IrScalar::Str("x".to_string()),
    });
    let migration = ir(vec![create_table("widgets", vec![label])]);

    accepted(&migration, Dialect::Mysql, "a bounded string default");
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
        Dialect::Mysql,
        "caseInsensitive renders bare TEXT, so the literal DEFAULT is fatal",
    );
    assert!(
        rendered.contains("label"),
        "the refusal should name the column: {rendered}"
    );

    accepted(&migration, Dialect::Postgres, "a citext default");
    accepted(&migration, Dialect::Sqlite, "a NOCASE text default");
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

    accepted(&migration, Dialect::Mysql, "a value-formatted text default");
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
        Dialect::Mysql,
        "MySQL 1170: a TEXT key needs a prefix length",
    );
    assert!(
        rendered.contains("sku"),
        "the refusal should name the column: {rendered}"
    );

    accepted(&migration, Dialect::Postgres, "a text index");
    accepted(&migration, Dialect::Sqlite, "a text index");
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

    accepted(&migration, Dialect::Mysql, "a bounded string index");
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

    accepted(&migration, Dialect::Mysql, "an id-prefixed text index");
}
