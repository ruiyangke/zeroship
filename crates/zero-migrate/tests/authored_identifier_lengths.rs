//! Bound every author-supplied constraint/index identifier that a target would
//! silently truncate.
//!
//! PostgreSQL caps identifiers at 63 bytes (NAMEDATALEN) and truncates anything
//! longer with only a NOTICE. The engine keeps the AUTHORED name while the catalog
//! keeps the TRUNCATED one, and everything downstream is keyed on the authored name.
//!
//! On the CREATE side that splits one identity into two: two authored names sharing
//! their first 63 bytes become one catalog object, the guarded create is a no-op that
//! reports success, and the snapshot records an object the catalog does not hold.
//!
//! On the DROP side it is worse. A guarded drop probes the AUTHORED name against the
//! INTROSPECTED snapshot; the truncated catalog name never matches, so the verdict is
//! a satisfied no-op, the executor skips the statement, and the journal records it
//! COMPLETED. The object is still in the database and the journal says it is gone.
//! An UNGUARDED PostgreSQL drop is resolved by the server's own truncation, so a
//! UNIQUE index is actually dropped without the approval gate firing, because the
//! uniqueness lookup keys on a live name the truncated one is never equal to.
//!
//! The dialect split below is load-bearing. The CREATE side is bounded on every
//! dialect because the authored name is what the engine will carry forward. The DROP
//! side is bounded on PostgreSQL ONLY: MySQL's limit is 64 CHARACTERS rather than
//! bytes and SQLite has no identifier cap at all, so a universal drop-side bound would
//! refuse a name that legitimately exists in those catalogs and strand the object.

use zero_migrate::model::validate::{validate_ir_scoped, AuthoringError, Dialect};
use zero_migrate::{
    ColType, IndexElement, IrColumn, IrConstraint, IrConstraintKind, IrIndex, MigrationIr, Op,
    SchemaScope,
};

const DIALECTS: [Dialect; 3] = [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite];

/// PostgreSQL's NAMEDATALEN-derived identifier bound, in bytes.
const MAX: usize = 63;

fn ir(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: "m".into(),
        owner_app: "app_idents".into(),
        ops,
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn validate(op: Op, dialect: Dialect) -> Result<(), AuthoringError> {
    validate_ir_scoped(&ir(vec![op]), dialect, &[], Some(&SchemaScope::Unconfined))
}

/// A name of exactly `bytes` ASCII bytes, opening with a letter.
fn ascii_name(bytes: usize) -> String {
    "c".repeat(bytes)
}

/// A name whose CHARACTER count is comfortably under the cap but whose BYTE count is
/// over it. This is the one deliberately non-ASCII fixture in the suite: the bound is
/// bytes, matching PostgreSQL's own NAMEDATALEN accounting, so a name that "looks
/// short" must still be refused.
fn multi_byte_name() -> String {
    // 'e' with acute accent is 2 bytes in UTF-8: 1 + 32 chars = 33 chars, 65 bytes.
    format!("c{}", "\u{e9}".repeat(32))
}

fn column() -> IrColumn {
    IrColumn {
        name: "c".into(),
        ty: ColType::Text,
        nullable: Some(false),
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

fn column_element() -> IndexElement {
    IndexElement::Column {
        name: "c".into(),
        order: None,
        opclass: None,
        collation: None,
    }
}

fn add_constraint(name: &str) -> Op {
    Op::AddConstraint {
        table: "t".into(),
        constraint: IrConstraint {
            name: Some(name.into()),
            kind: IrConstraintKind::Unique {
                columns: vec!["c".into()],
            },
        },
        schema: None,
        existence_guard: None,
    }
}

fn create_table(constraints: Vec<IrConstraint>, indexes: Vec<IrIndex>) -> Op {
    Op::CreateTable {
        name: "t".into(),
        columns: vec![column()],
        primary_key: Some(vec!["c".into()]),
        constraints,
        indexes,
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

/// A self-referencing foreign key: the one table-level constraint kind every
/// supported dialect accepts inside `createTable`, so the fixture isolates the name
/// bound from any vendor-capability refusal.
fn create_table_with_constraint_name(name: &str) -> Op {
    create_table(
        vec![IrConstraint {
            name: Some(name.into()),
            kind: IrConstraintKind::Fk {
                columns: vec!["c".into()],
                references_table: "t".into(),
                references_columns: vec!["c".into()],
                on_delete: None,
                on_update: None,
                deferrable: None,
                initially_deferred: None,
                not_valid: None,
            },
        }],
        vec![],
    )
}

fn create_table_with_index_name(name: &str) -> Op {
    create_table(
        vec![],
        vec![IrIndex {
            name: Some(name.into()),
            columns: vec![column_element()],
            unique: None,
            using: None,
            r#where: None,
            include: vec![],
            with: None,
            only: None,
            nulls_not_distinct: None,
        }],
    )
}

fn drop_index(name: &str) -> Op {
    Op::DropIndex {
        name: name.into(),
        table: Some("t".into()),
        unique: None,
        concurrently: None,
        schema: None,
        existence_guard: None,
    }
}

fn drop_constraint(name: &str) -> Op {
    Op::DropConstraint {
        table: "t".into(),
        name: name.into(),
        schema: None,
        existence_guard: None,
    }
}

fn validate_constraint(name: &str) -> Op {
    Op::ValidateConstraint {
        table: "t".into(),
        name: name.into(),
        schema: None,
        existence_guard: None,
    }
}

/// An op constructor under test, paired with the label its assertions report.
type NamedOpFactory = (&'static str, fn(&str) -> Op);

/// Every CREATE-side op factory that carries an author-supplied identifier.
fn create_side_factories() -> Vec<NamedOpFactory> {
    vec![
        ("addConstraint", add_constraint as fn(&str) -> Op),
        (
            "createTable inline constraint",
            create_table_with_constraint_name,
        ),
        ("createTable inline index", create_table_with_index_name),
    ]
}

/// Every DROP-side op factory that names an object PostgreSQL may already have
/// truncated.
fn drop_side_factories() -> Vec<NamedOpFactory> {
    vec![
        ("dropIndex", drop_index as fn(&str) -> Op),
        ("dropConstraint", drop_constraint),
        ("validateConstraint", validate_constraint),
    ]
}

fn assert_refused_for_length(label: &str, dialect: Dialect, op: Op) {
    let error = validate(op, dialect).expect_err(&format!(
        "{label} on {dialect:?} must refuse a truncatable name"
    ));
    assert!(
        error.reason.contains("truncates identifiers"),
        "{label} on {dialect:?} was refused for the wrong reason: {error:?}"
    );
    assert!(
        error.reason.contains(&MAX.to_string()),
        "{label} on {dialect:?} must name the byte cap: {error:?}"
    );
}

fn assert_not_refused_for_length(label: &str, dialect: Dialect, op: Op) {
    if let Err(error) = validate(op, dialect) {
        assert!(
            !error.reason.contains("truncates identifiers"),
            "{label} on {dialect:?} must not be refused for identifier length: {error:?}"
        );
    }
}

#[test]
fn add_constraint_refuses_a_64_byte_name_on_every_dialect() {
    let name = ascii_name(MAX + 1);
    for dialect in DIALECTS {
        assert_refused_for_length("addConstraint", dialect, add_constraint(&name));
    }
}

#[test]
fn create_table_refuses_a_64_byte_inline_constraint_name_on_every_dialect() {
    let name = ascii_name(MAX + 1);
    for dialect in DIALECTS {
        assert_refused_for_length(
            "createTable inline constraint",
            dialect,
            create_table_with_constraint_name(&name),
        );
    }
}

#[test]
fn create_table_refuses_a_64_byte_inline_index_name_on_every_dialect() {
    let name = ascii_name(MAX + 1);
    for dialect in DIALECTS {
        assert_refused_for_length(
            "createTable inline index",
            dialect,
            create_table_with_index_name(&name),
        );
    }
}

/// The drop side is bounded on PostgreSQL only. A PostgreSQL object that was
/// truncated has a physical name of at most 63 bytes by definition, so the bound can
/// never refuse a name that could have dropped a real object; the remedy for a legacy
/// over-long object is to name it as the catalog holds it.
#[test]
fn drop_side_refuses_a_64_byte_name_on_postgres() {
    let name = ascii_name(MAX + 1);
    for (label, factory) in drop_side_factories() {
        assert_refused_for_length(label, Dialect::Postgres, factory(&name));
    }
}

/// MySQL caps identifiers at 64 CHARACTERS and SQLite does not cap them at all, so a
/// 64-byte name can name a real object in either catalog. Refusing it would strand
/// the object, so the drop-side bound must not apply there.
#[test]
fn drop_side_accepts_a_64_byte_name_on_mysql_and_sqlite() {
    let name = ascii_name(MAX + 1);
    for dialect in [Dialect::Mysql, Dialect::Sqlite] {
        for (label, factory) in drop_side_factories() {
            assert_not_refused_for_length(label, dialect, factory(&name));
        }
    }
}

#[test]
fn a_63_byte_name_is_accepted_on_every_op_and_every_dialect() {
    let name = ascii_name(MAX);
    assert_eq!(
        name.len(),
        MAX,
        "the boundary fixture must be exactly {MAX} bytes"
    );
    for dialect in DIALECTS {
        for (label, factory) in create_side_factories() {
            validate(factory(&name), dialect).unwrap_or_else(|e| {
                panic!("{label} on {dialect:?} must accept {MAX} bytes: {e:?}")
            });
        }
        for (label, factory) in drop_side_factories() {
            assert_not_refused_for_length(label, dialect, factory(&name));
        }
    }
}

/// The bound is BYTES, not characters, because PostgreSQL's NAMEDATALEN is a byte
/// budget. A 33-character name of 65 bytes is truncated mid-name exactly as a
/// 65-byte ASCII name would be.
#[test]
fn a_multi_byte_name_over_63_bytes_is_refused_where_the_bound_applies() {
    let name = multi_byte_name();
    assert!(
        name.chars().count() < MAX && name.len() > MAX,
        "the fixture must be short in characters and long in bytes: {} chars, {} bytes",
        name.chars().count(),
        name.len()
    );
    for dialect in DIALECTS {
        for (label, factory) in create_side_factories() {
            assert_refused_for_length(label, dialect, factory(&name));
        }
    }
    for (label, factory) in drop_side_factories() {
        assert_refused_for_length(label, Dialect::Postgres, factory(&name));
    }
}
