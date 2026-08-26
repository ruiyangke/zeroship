//! An attribute a backend DECLARES must reach that backend's `CREATE TABLE`.
//!
//! Declaring a knob, exporting it to the vocabulary artifact and generating a TypeScript
//! field for it produces a surface an author can write against. None of that emits SQL.
//! Until a renderer reads the carried attributes, `postgres: { fillfactor: 85 }` type-checks,
//! travels on the wire, survives the checksum — and silently does nothing to the database.
//!
//! That is the failure this file exists to make loud, and it is a fail-OPEN one: the
//! author gets no error, the migration applies, and the table simply does not have the
//! storage option they asked for.
//!
//! # Why one test per vendor rather than a shared assertion
//!
//! There is no neutral spelling to assert. PostgreSQL says `WITH (fillfactor='85')`,
//! MySQL says `ROW_FORMAT=DYNAMIC` as a bare table option, and SQLite says `STRICT` as a
//! trailing keyword with no value at all. A single shared assertion would have to pick one
//! grammar and would then be testing that vendor twice and the others never — so each
//! backend asserts its OWN spelling, which is the same reason the renderers are per-vendor
//! in the first place.

use crate::support;

use zeroship_migrate::{
    ColType, IrAuthor, IrColumn, IrFlagsOverride, IrScalar, LiveSchema, MigrationIr, Op,
    CURRENT_IR_VERSION,
};
use zeroship_migrate_ir::attribute::{AttrKey, Attributes, CreateTableAttributes};

const SCHEMA: &str = "app";
const OWNER: &str = "app_a";

fn col(name: &str) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty: ColType::Int,
        nullable: Some(true),
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

/// One `CreateTable` carrying exactly the attributes given, keyed by their full wire
/// spelling (`<dialect>.<name>`) — the same form `flattenVendorAttributes` produces in
/// TypeScript, so this test authors what the DSL authors rather than a Rust-only shape.
fn create_table_with(attrs: &[(&str, IrScalar)]) -> Op {
    let mut carried = Attributes::new();
    for (key, value) in attrs {
        carried.insert(
            AttrKey::parse(key).expect("a well-formed attribute key"),
            value.clone(),
        );
    }
    Op::CreateTable {
        name: "widgets".to_string(),
        columns: vec![col("id")],
        primary_key: None,
        constraints: Vec::new(),
        indexes: Vec::new(),
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
        attributes: CreateTableAttributes::from(carried),
    }
}

fn create_table_sql(dialect: &zeroship_migrate::DialectId, op: Op) -> String {
    let author = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        SCHEMA,
        OWNER,
        dialect,
        &support::no_inject("app"),
    );
    let ir = MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "an_authored_attribute".to_string(),
        owner_app: OWNER.to_string(),
        ops: vec![op],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    };
    author
        .lower(&ir, &LiveSchema::default())
        .expect("the create lowers")
        .into_iter()
        .map(|m| m.up)
        .find(|sql| sql.contains("CREATE TABLE"))
        .expect("a CREATE TABLE was rendered")
}

#[test]
fn postgres_renders_a_declared_storage_parameter_into_the_create() {
    let sql = create_table_sql(
        &zeroship_migrate_postgres::DIALECT,
        create_table_with(&[("postgres.fillfactor", IrScalar::Int(85))]),
    );
    assert!(
        sql.contains("fillfactor"),
        "the authored `postgres.fillfactor` never reached the DDL, so the table is \
         created without the storage parameter the author asked for and nothing \
         reported it. Rendered:\n{sql}"
    );
    assert!(
        sql.contains("WITH (fillfactor='85')"),
        "PostgreSQL spells a storage parameter `WITH (fillfactor='85')`. Rendered:\n{sql}"
    );
}

#[test]
fn postgres_renders_every_declared_table_attribute_it_was_given() {
    // More than one, because a renderer that handles a single knob by special-casing it
    // passes the test above and fails the moment a second is authored.
    let sql = create_table_sql(
        &zeroship_migrate_postgres::DIALECT,
        create_table_with(&[
            ("postgres.fillfactor", IrScalar::Int(70)),
            ("postgres.autovacuum_enabled", IrScalar::Bool(false)),
            ("postgres.parallel_workers", IrScalar::Int(4)),
        ]),
    );
    for expected in [
        "fillfactor='70'",
        "autovacuum_enabled='false'",
        "parallel_workers='4'",
    ] {
        assert!(
            sql.contains(expected),
            "`{expected}` is missing from the rendered create. Rendered:\n{sql}"
        );
    }
}

#[test]
fn mysql_renders_a_declared_table_option_in_its_own_grammar() {
    let sql = create_table_sql(
        &zeroship_migrate_mysql::DIALECT,
        create_table_with(&[
            ("mysql.row_format", IrScalar::Str("DYNAMIC".to_string())),
            ("mysql.engine", IrScalar::Str("InnoDB".to_string())),
        ]),
    );
    // MySQL table options are `NAME=value` after the closing paren, NOT a `WITH (...)`
    // list — asserting PostgreSQL's spelling here would pass a renderer that emitted
    // syntactically invalid MySQL.
    assert!(
        sql.contains("ROW_FORMAT=DYNAMIC"),
        "MySQL spells this `ROW_FORMAT=DYNAMIC`. Rendered:\n{sql}"
    );
    assert!(
        sql.contains("ENGINE=InnoDB"),
        "MySQL spells this `ENGINE=InnoDB`. Rendered:\n{sql}"
    );
    assert!(
        !sql.contains("WITH ("),
        "a `WITH (...)` list is PostgreSQL's grammar and is a syntax error in MySQL. \
         Rendered:\n{sql}"
    );
}

#[test]
fn sqlite_renders_a_declared_valueless_clause() {
    let sql = create_table_sql(
        &zeroship_migrate_sqlite::DIALECT,
        create_table_with(&[("sqlite.strict", IrScalar::Bool(true))]),
    );
    // SQLite's STRICT is a bare trailing keyword. A renderer that emitted `strict='true'`
    // would satisfy a naive "contains strict" check and be rejected by the server, so the
    // assertion pins the keyword form and refuses the assignment form.
    assert!(
        sql.contains("STRICT"),
        "the authored `sqlite.strict` never reached the DDL. Rendered:\n{sql}"
    );
    assert!(
        !sql.contains("strict='true'") && !sql.contains("strict=1"),
        "SQLite's STRICT takes no value; an assignment form is a syntax error. \
         Rendered:\n{sql}"
    );
}

#[test]
fn a_table_authored_with_no_attributes_renders_exactly_as_before() {
    // The regression direction. Every existing create in the suite carries an empty
    // attribute map, so a renderer that appends an empty `WITH ()` or a stray space
    // would redden hundreds of byte-pinned tests elsewhere — but only if something
    // pins the empty case directly, which is what this does.
    let sql = create_table_sql(&zeroship_migrate_postgres::DIALECT, create_table_with(&[]));
    assert!(
        !sql.contains("WITH ("),
        "a create carrying no attributes must emit no storage-parameter list at all. \
         Rendered:\n{sql}"
    );
}

/// The refusal half. Rendering an attribute is only safe if a WRONG one is refused before
/// a connection is opened; otherwise the fix for "silently does nothing" is "fails
/// halfway through an apply", which is worse.
mod a_wrong_attribute_is_refused_before_any_connection {
    use super::{create_table_with, OWNER, SCHEMA};
    use zeroship_migrate::{IrFlagsOverride, IrScalar, MigrationIr, Op, CURRENT_IR_VERSION};

    fn validate(dialect: &zeroship_migrate::DialectId, op: Op) -> Result<(), String> {
        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: CURRENT_IR_VERSION,
            name: "refusal".to_string(),
            owner_app: OWNER.to_string(),
            ops: vec![op],
            flags: IrFlagsOverride::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        let _ = SCHEMA;
        zeroship_migrate::validate_ir(zeroship_migrate::shipping_vendors(), &ir, dialect)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn a_misspelled_key_is_refused_and_the_message_names_it() {
        let err = validate(
            &zeroship_migrate_postgres::DIALECT,
            create_table_with(&[("postgres.filfactor", IrScalar::Int(85))]),
        )
        .expect_err(
            "a key PostgreSQL never declared must not reach the renderer, which would \
             spell it verbatim into `WITH (…)` and fail at the server mid-apply",
        );
        assert!(
            err.contains("filfactor"),
            "the refusal must name the key the author actually wrote: {err}"
        );
    }

    #[test]
    fn a_value_outside_the_declared_range_is_refused() {
        // `fillfactor` is declared 10..=100. A declaration that carries a range and never
        // enforces it is decoration.
        let err = validate(
            &zeroship_migrate_postgres::DIALECT,
            create_table_with(&[("postgres.fillfactor", IrScalar::Int(5))]),
        )
        .expect_err("5 is below the declared minimum of 10");
        assert!(err.contains('5'), "{err}");
    }

    #[test]
    fn a_declared_key_used_on_the_wrong_op_is_refused() {
        // `pages_per_range` is declared for `createIndex` only. Carrying it on a
        // `createTable` is exactly the mistake the (key, op) identity exists to catch, and
        // it is INVISIBLE to a key-only check.
        let err = validate(
            &zeroship_migrate_postgres::DIALECT,
            create_table_with(&[("postgres.pages_per_range", IrScalar::Int(64))]),
        )
        .expect_err("pages_per_range is not legal on createTable");
        assert!(err.contains("pages_per_range"), "{err}");
    }

    #[test]
    fn another_backends_namespace_is_carried_rather_than_refused() {
        // The portability property, stated as a test because it is the one that makes the
        // refusals above safe to add. A table authored for three backends must validate on
        // each of them; judging every namespace against every backend would refuse a plan
        // for a target the author never deploys to.
        validate(
            &zeroship_migrate_postgres::DIALECT,
            create_table_with(&[
                ("postgres.fillfactor", IrScalar::Int(85)),
                ("mysql.engine", IrScalar::Str("InnoDB".to_string())),
                ("sqlite.strict", IrScalar::Bool(true)),
            ]),
        )
        .expect("a PostgreSQL target must not judge MySQL's or SQLite's namespace");
    }

    #[test]
    fn a_correctly_declared_attribute_validates() {
        validate(
            &zeroship_migrate_postgres::DIALECT,
            create_table_with(&[("postgres.fillfactor", IrScalar::Int(85))]),
        )
        .expect("85 is inside the declared 10..=100");
    }
}

/// The INDEX surface, which the generated TypeScript has advertised since vendor
/// attributes landed and which nothing could reach.
///
/// `PostgresCreateIndexAttributes` is generated, exported and documented; until now
/// `Op::CreateIndex` had no attribute field at all, so the typings described an authoring
/// surface that did not exist. The two PostgreSQL index declarations existed only to
/// retire [`IndexStorageParams`], which held `fillfactor` and `pages_per_range` as named
/// fields in the NEUTRAL IR -- and which core's own drift pass then formatted by those
/// two PostgreSQL spellings, inside the crate whose rule is to name no vendor.
mod an_authored_index_attribute_reaches_the_ddl {
    use super::{col, OWNER, SCHEMA};
    use zeroship_migrate::{
        IndexElement, IrAuthor, IrFlagsOverride, IrScalar, LiveSchema, MigrationIr, Op,
        CURRENT_IR_VERSION,
    };
    use zeroship_migrate_ir::attribute::{AttrKey, Attributes, CreateIndexAttributes};

    fn attrs(pairs: &[(&str, IrScalar)]) -> CreateIndexAttributes {
        let mut carried = Attributes::new();
        for (key, value) in pairs {
            carried.insert(
                AttrKey::parse(key).expect("a well-formed attribute key"),
                value.clone(),
            );
        }
        CreateIndexAttributes::from(carried)
    }

    fn create_index_sql(dialect: &zeroship_migrate::DialectId, index: Op) -> String {
        let author = IrAuthor::new(
            zeroship_migrate::shipping_vendors(),
            SCHEMA,
            OWNER,
            dialect,
            &crate::support::no_inject("app"),
        );
        let table = Op::CreateTable {
            name: "widgets".to_string(),
            columns: vec![col("id")],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
            attributes: Default::default(),
        };
        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: CURRENT_IR_VERSION,
            name: "index_attribute".to_string(),
            owner_app: OWNER.to_string(),
            ops: vec![table, index],
            flags: IrFlagsOverride::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        author
            .lower(&ir, &LiveSchema::default())
            .expect("the index lowers")
            .into_iter()
            .map(|m| m.up)
            .find(|sql| sql.contains("CREATE INDEX") || sql.contains("CREATE UNIQUE INDEX"))
            .expect("a CREATE INDEX was rendered")
    }

    fn index_op(attributes: CreateIndexAttributes) -> Op {
        Op::CreateIndex {
            table: "widgets".to_string(),
            columns: vec![IndexElement::Column {
                name: "id".to_string(),
                order: None,
                opclass: None,
                collation: None,
            }],
            name: Some("widgets_id_idx".to_string()),
            unique: None,
            using: None,
            r#where: None,
            concurrently: None,
            include: Vec::new(),
            only: None,
            nulls_not_distinct: None,
            schema: None,
            existence_guard: None,
            attributes,
        }
    }

    #[test]
    fn postgres_renders_a_declared_index_storage_parameter() {
        let sql = create_index_sql(
            &zeroship_migrate_postgres::DIALECT,
            index_op(attrs(&[("postgres.fillfactor", IrScalar::Int(90))])),
        );
        assert!(
            sql.contains("WITH (fillfactor='90')"),
            "the authored `postgres.fillfactor` never reached the CREATE INDEX. \
             Rendered:\n{sql}"
        );
    }

    #[test]
    fn both_declared_index_parameters_render_in_one_with_clause() {
        // BRIN accepts both. The ORDER is now the attribute map's canonical one
        // (alphabetical by full key, so `fillfactor` before `pages_per_range`) rather
        // than the order two struct fields happened to be declared in. Pinned because a
        // canonical order is the property that keeps rendered DDL reproducible.
        let sql = create_index_sql(
            &zeroship_migrate_postgres::DIALECT,
            index_op(attrs(&[
                ("postgres.pages_per_range", IrScalar::Int(64)),
                ("postgres.fillfactor", IrScalar::Int(90)),
            ])),
        );
        assert!(
            sql.contains("WITH (fillfactor='90', pages_per_range='64')"),
            "expected one WITH clause in canonical key order. Rendered:\n{sql}"
        );
    }

    #[test]
    fn an_index_carrying_no_attributes_emits_no_with_clause() {
        let sql = create_index_sql(
            &zeroship_migrate_postgres::DIALECT,
            index_op(CreateIndexAttributes::new()),
        );
        assert!(
            !sql.contains("WITH ("),
            "an index with no storage parameters must emit no WITH clause. \
             Rendered:\n{sql}"
        );
    }

    #[test]
    fn a_table_only_key_is_refused_on_an_index() {
        // `postgres.tablespace` IS declared -- for createTable, createPartition and
        // setTableOptions, NOT for createIndex. This is the (key, op) identity earning
        // its keep: a key-only check would accept it.
        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: CURRENT_IR_VERSION,
            name: "index_refusal".to_string(),
            owner_app: OWNER.to_string(),
            ops: vec![index_op(attrs(&[(
                "postgres.tablespace",
                IrScalar::Str("fast".to_string()),
            )]))],
            flags: IrFlagsOverride::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };
        let err = zeroship_migrate::validate_ir(
            zeroship_migrate::shipping_vendors(),
            &ir,
            &zeroship_migrate_postgres::DIALECT,
        )
        .expect_err("tablespace is not declared on createIndex");
        assert!(err.to_string().contains("tablespace"), "{err}");
    }
}
