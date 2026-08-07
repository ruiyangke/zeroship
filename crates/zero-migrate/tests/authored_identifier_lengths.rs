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
//!
//! The bound is enforced at three seams, because each one is reachable without the
//! others:
//!
//! - the LOAD gate (`validate_ir_scoped`), the authoring entry point;
//! - the LOWER seam (`IrAuthor::lower` / `lower_steps` / `lower_plan`), which no
//!   caller is obliged to reach through the load gate, and which is where a
//!   `dialectal` leg is finally selected;
//! - the executor's existence PROBE (`decide`), because `Migration::existence_guard`
//!   is a public field on a struct a consumer can build directly, so a probe can
//!   reach the executor without lowering ever having run.

use std::collections::BTreeMap;

use zero_migrate::model::probe::{GuardDir, GuardProbe};
use zero_migrate::model::snapshot::{
    ConstraintSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};
use zero_migrate::model::validate::{validate_ir_scoped, AuthoringError, Dialect};
use zero_migrate::render::existence_probe::{decide, GuardVerdict};
use zero_migrate::{
    ColType, IndexElement, IrAuthor, IrColumn, IrConstraint, IrConstraintKind, IrIndex, LiveSchema,
    MigrationIr, Op, SchemaScope, SqlDialect,
};

mod support;

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

// Bounded rather than `t.text()` so the column is keyable on every dialect:
// MySQL refuses a key over a bare TEXT column with no prefix length, and these
// fixtures key `c` from a primary key, an index, and a unique constraint.
fn column() -> IrColumn {
    IrColumn {
        name: "c".into(),
        ty: ColType::String { length: 64 },
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

// - The lower seam -
//
// `IrAuthor::lower` / `lower_steps` / `lower_plan` are public entry points that
// no caller is obliged to reach through the load gate, so the bound has to run
// there too.

/// Lower one op through the real `IrAuthor`, returning the rendered error text.
fn lower_error(op: Op, dialect: SqlDialect) -> Option<String> {
    let author = IrAuthor::new("app", "app_idents", dialect, &support::no_inject("app"));
    author
        .lower(&ir(vec![op]), &LiveSchema::default())
        .err()
        .map(|error| error.to_string())
}

#[track_caller]
fn assert_lower_refused_for_length(label: &str, dialect: SqlDialect, op: Op) {
    let error = lower_error(op, dialect).unwrap_or_else(|| {
        panic!("{label} on {dialect:?} must refuse a truncatable name at lower")
    });
    assert!(
        error.contains("truncates identifiers"),
        "{label} on {dialect:?} was refused at lower for the wrong reason: {error}"
    );
}

#[track_caller]
fn assert_lower_not_refused_for_length(label: &str, dialect: SqlDialect, op: Op) {
    if let Some(error) = lower_error(op, dialect) {
        assert!(
            !error.contains("truncates identifiers"),
            "{label} on {dialect:?} must not be refused at lower for identifier length: {error}"
        );
    }
}

/// The gap commit ddd679d left: the bound was a load-time gate only, so lowering an
/// over-long authored name still carried it verbatim into the rendered DDL.
#[test]
fn lower_refuses_a_64_byte_authored_constraint_name() {
    let name = ascii_name(MAX + 1);
    assert_lower_refused_for_length("addConstraint", SqlDialect::Postgres, add_constraint(&name));
    assert_lower_refused_for_length(
        "dropConstraint",
        SqlDialect::Postgres,
        drop_constraint(&name),
    );
}

#[test]
fn lower_accepts_a_63_byte_authored_constraint_name() {
    let name = ascii_name(MAX);
    assert_lower_not_refused_for_length(
        "addConstraint",
        SqlDialect::Postgres,
        add_constraint(&name),
    );
    assert_lower_not_refused_for_length(
        "dropConstraint",
        SqlDialect::Postgres,
        drop_constraint(&name),
    );
}

// - The `dialectal` wrapper -

/// A `dropConstraint` buried in the PostgreSQL leg of a `dialectal` wrapper.
fn dialectal_pg_drop_constraint(name: &str) -> Op {
    Op::Dialectal {
        default: None,
        pg: Some(vec![drop_constraint(name)]),
        sqlite: None,
        mysql: None,
    }
}

/// The same op in the `default` leg, which every dialect falls back to.
fn dialectal_default_drop_constraint(name: &str) -> Op {
    Op::Dialectal {
        default: Some(vec![drop_constraint(name)]),
        pg: None,
        sqlite: None,
        mysql: None,
    }
}

/// A `dialectal` wrapper is not a hiding place. The load gate walked only top-level
/// ops, so a nested over-long name escaped it even on the fully routed production path.
#[test]
fn the_load_gate_refuses_a_64_byte_name_nested_in_a_dialectal_leg() {
    let name = ascii_name(MAX + 1);
    assert_refused_for_length(
        "dialectal pg dropConstraint",
        Dialect::Postgres,
        dialectal_pg_drop_constraint(&name),
    );
    assert_refused_for_length(
        "dialectal default dropConstraint",
        Dialect::Postgres,
        dialectal_default_drop_constraint(&name),
    );
}

#[test]
fn the_lower_seam_refuses_a_64_byte_name_nested_in_a_dialectal_leg() {
    let name = ascii_name(MAX + 1);
    assert_lower_refused_for_length(
        "dialectal pg dropConstraint",
        SqlDialect::Postgres,
        dialectal_pg_drop_constraint(&name),
    );
    assert_lower_refused_for_length(
        "dialectal default dropConstraint",
        SqlDialect::Postgres,
        dialectal_default_drop_constraint(&name),
    );
}

/// The leg that is NOT selected for the target dialect is not the engine's business:
/// it never lowers, so bounding it would refuse an IR that is correct on this target.
#[test]
fn an_unselected_dialectal_leg_is_not_bounded() {
    let name = ascii_name(MAX + 1);
    let op = Op::Dialectal {
        default: None,
        pg: None,
        sqlite: Some(vec![drop_constraint(&name)]),
        mysql: None,
    };
    assert_not_refused_for_length("dialectal sqlite leg on postgres", Dialect::Postgres, op);
}

// - The executor's existence probe -
//
// `Migration::existence_guard` is a public field on a struct that is not
// `#[non_exhaustive]`, in a crate a consumer can depend on directly, so a migration
// carrying a probe can reach the executor without lowering ever having run. The
// backstop is deliberately narrow: PostgreSQL only, and direction-aware.

/// PostgreSQL's own truncation of `name`: the longest prefix of WHOLE characters that
/// fits in [`MAX`] bytes. Verified against a live PostgreSQL 18 server - a name of 62
/// ASCII bytes plus a two-byte character truncates to the 62 ASCII bytes, never to a
/// split codepoint.
fn pg_truncation(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if out.len() + ch.len_utf8() > MAX {
            break;
        }
        out.push(ch);
    }
    out
}

fn constraint_snapshot(name: &str) -> ConstraintSnapshot {
    ConstraintSnapshot {
        name: name.to_string(),
        kind: "UNIQUE".to_string(),
        definition: "UNIQUE (c)".to_string(),
        comment: None,
    }
}

fn live_schema(
    constraints: Vec<ConstraintSnapshot>,
    indexes: Vec<IndexSnapshot>,
) -> SchemaSnapshot {
    let table = TableSnapshot {
        columns: Vec::new(),
        indexes,
        constraints,
        runtime_options: Default::default(),
        partition_by: None,
        comment: None,
        stored_create_sql: None,
    };
    let mut tables = BTreeMap::new();
    tables.insert("t".to_string(), table);
    SchemaSnapshot {
        tables,
        ..Default::default()
    }
}

fn constraint_probe(name: &str, direction: GuardDir) -> GuardProbe {
    GuardProbe::Constraint {
        schema: "app".into(),
        table: "t".into(),
        name: name.into(),
        direction,
        expect_kind: None,
        expect_definition: None,
    }
}

fn index_probe(name: &str, direction: GuardDir) -> GuardProbe {
    GuardProbe::Index {
        schema: "app".into(),
        table: "t".into(),
        name: name.into(),
        direction,
        expect: None,
    }
}

/// One probe kind under test: a label, the probe builder, and the builder that puts a
/// live object of that kind under the given name into the snapshot.
type ProbeCase = (
    &'static str,
    fn(&str, GuardDir) -> GuardProbe,
    fn(&str) -> SchemaSnapshot,
);

fn live_with_constraint(name: &str) -> SchemaSnapshot {
    live_schema(vec![constraint_snapshot(name)], Vec::new())
}

fn live_with_index(name: &str) -> SchemaSnapshot {
    live_schema(
        Vec::new(),
        vec![IndexSnapshot::btree(
            name.to_string(),
            false,
            vec!["c".to_string()],
        )],
    )
}

fn probe_cases() -> Vec<ProbeCase> {
    vec![
        (
            "constraint",
            constraint_probe as fn(&str, GuardDir) -> GuardProbe,
            live_with_constraint as fn(&str) -> SchemaSnapshot,
        ),
        ("index", index_probe, live_with_index),
    ]
}

fn empty_live() -> SchemaSnapshot {
    live_schema(Vec::new(), Vec::new())
}

/// The defect at its last seam. An over-long `ifExists` name whose TRUNCATED spelling
/// is what the catalog holds must never read as "already gone": that verdict skips the
/// statement and journals it completed while the object survives.
#[test]
fn an_over_long_if_exists_name_whose_truncation_is_live_fails_closed_on_postgres() {
    let authored = ascii_name(MAX + 1);
    let truncated = pg_truncation(&authored);
    for (label, probe, live) in probe_cases() {
        match decide(
            &probe(&authored, GuardDir::IfExists),
            &live(&truncated),
            SqlDialect::Postgres,
        ) {
            GuardVerdict::FailDrift(divergence) => assert_eq!(
                divergence.actual, truncated,
                "{label} must name the truncated spelling the catalog holds"
            ),
            verdict => panic!(
                "{label}: an over-long ifExists name whose truncation is live must fail \
                 closed, got {verdict:?}"
            ),
        }
    }
}

/// The truncation the backstop derives must be PostgreSQL's own, which clips on a
/// CHARACTER boundary. A 62-ASCII-byte prefix plus one two-byte character is 64 bytes
/// and truncates to the 62 ASCII bytes, not to a 63rd byte that would split the
/// codepoint.
#[test]
fn the_derived_truncation_clips_on_a_character_boundary() {
    let authored = format!("{}\u{e9}", ascii_name(MAX - 1));
    assert_eq!(authored.len(), MAX + 1, "the fixture must be one byte over");
    let truncated = pg_truncation(&authored);
    assert_eq!(
        truncated,
        ascii_name(MAX - 1),
        "PostgreSQL drops the whole trailing character rather than splitting it"
    );
    for (label, probe, live) in probe_cases() {
        match decide(
            &probe(&authored, GuardDir::IfExists),
            &live(&truncated),
            SqlDialect::Postgres,
        ) {
            GuardVerdict::FailDrift(divergence) => assert_eq!(divergence.actual, truncated),
            verdict => panic!("{label}: expected a fail-closed verdict, got {verdict:?}"),
        }
    }
}

/// The narrow half of the backstop. When even the TRUNCATED spelling is absent, the
/// drop's postcondition genuinely holds, so the satisfied no-op is correct and must
/// survive: refusing here would break migrations that are correct today.
#[test]
fn an_over_long_if_exists_name_absent_in_every_spelling_still_noops() {
    let authored = ascii_name(MAX + 1);
    for (label, probe, _live) in probe_cases() {
        assert_eq!(
            decide(
                &probe(&authored, GuardDir::IfExists),
                &empty_live(),
                SqlDialect::Postgres
            ),
            GuardVerdict::SatisfiedNoop,
            "{label}: a genuinely absent object is what ifExists is for"
        );
    }
}

/// The create side. `RunBare` on an over-long name would CREATE a truncated identity
/// the engine then carries under the authored name, so it is refused before the lookup.
#[test]
fn an_over_long_if_not_exists_name_fails_closed_on_postgres() {
    let authored = ascii_name(MAX + 1);
    for (label, probe, _live) in probe_cases() {
        match decide(
            &probe(&authored, GuardDir::IfNotExists),
            &empty_live(),
            SqlDialect::Postgres,
        ) {
            GuardVerdict::FailDrift(_) => {}
            verdict => panic!(
                "{label}: an over-long ifNotExists name must not create a truncated \
                 identity, got {verdict:?}"
            ),
        }
    }
}

/// A name WITHIN the bound keeps every verdict it has today, on every dialect and in
/// both directions. This is the common correct case and the backstop must be invisible
/// to it.
#[test]
fn a_within_bound_name_keeps_its_verdict_on_every_dialect() {
    let name = ascii_name(MAX);
    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
        for (label, probe, live) in probe_cases() {
            assert_eq!(
                decide(&probe(&name, GuardDir::IfExists), &live(&name), dialect),
                GuardVerdict::RunBare,
                "{label} ifExists on {dialect:?}: a live object still runs the drop"
            );
            assert_eq!(
                decide(&probe(&name, GuardDir::IfExists), &empty_live(), dialect),
                GuardVerdict::SatisfiedNoop,
                "{label} ifExists on {dialect:?}: an absent object still no-ops"
            );
            assert_eq!(
                decide(&probe(&name, GuardDir::IfNotExists), &empty_live(), dialect),
                GuardVerdict::RunBare,
                "{label} ifNotExists on {dialect:?}: an absent object still creates"
            );
        }
    }
}

/// MySQL caps identifiers at 64 CHARACTERS rather than bytes and SQLite does not cap
/// them at all, so an over-long name can name a real object in either catalog. The byte
/// rule must never reach them.
#[test]
fn an_over_long_name_keeps_its_verdict_on_mysql_and_sqlite() {
    let authored = ascii_name(MAX + 1);
    let truncated = pg_truncation(&authored);
    for dialect in [SqlDialect::Mysql, SqlDialect::Sqlite] {
        for (label, probe, live) in probe_cases() {
            assert_eq!(
                decide(
                    &probe(&authored, GuardDir::IfExists),
                    &live(&authored),
                    dialect
                ),
                GuardVerdict::RunBare,
                "{label} ifExists on {dialect:?}: the catalog can hold the full name"
            );
            assert_eq!(
                decide(
                    &probe(&authored, GuardDir::IfExists),
                    &live(&truncated),
                    dialect
                ),
                GuardVerdict::SatisfiedNoop,
                "{label} ifExists on {dialect:?}: no truncated spelling is derived"
            );
            assert_eq!(
                decide(
                    &probe(&authored, GuardDir::IfNotExists),
                    &empty_live(),
                    dialect
                ),
                GuardVerdict::RunBare,
                "{label} ifNotExists on {dialect:?}: an absent object still creates"
            );
        }
    }
}
