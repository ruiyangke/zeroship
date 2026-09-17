//! The `ALTER COLUMN` family is spelled by the BACKEND, not by the engine.
//!
//! # What this file pins
//!
//! [`zeroship_migrate_backend::ddl::DdlEmitter`] covers the table-level and column
//! add/drop verbs, and the `ALTER COLUMN` family is the backend's too:
//!
//! * `ALTER TABLE … ALTER COLUMN … TYPE …` — the verb,
//! * ` USING <col>::<type>` — the cast clause AND the `::` cast operator,
//! * `SET NOT NULL` / `DROP NOT NULL`,
//! * `SET DEFAULT <expr>` / `DROP DEFAULT`.
//!
//! These are PostgreSQL decisions with no other spelling: MySQL retypes with
//! `MODIFY COLUMN` and restates the whole definition, and SQLite has no `ALTER COLUMN`
//! at all. A backend that disagrees must have a method to override; when the engine
//! wrote these itself, it gave a disagreeing backend nowhere to say so.
//!
//! # Why this needs a FOURTH backend and cannot be shown on the three that ship
//!
//! When vendors agree, absent routing emits the right bytes and an assertion about
//! emitted SQL cannot see the missing dispatch. The shipping backends do not
//! disagree here in any way an assertion can catch —
//!
//! * PostgreSQL wants exactly what the engine wrote, so its leg is silently correct.
//! * SQLite never arrives: `Capability::NativeAlterColumn` is false, and the differ
//!   routes a retype through the table rebuild.
//! * MySQL never arrives for a retype or a nullability change either, but for a
//!   DIFFERENT reason and through a DIFFERENT door — `restates_column_type_at_apply`
//!   for the first, `CatalogFoldPolicy::alter_column_refusal` for the second.
//!
//! What is left is a backend that answers YES to the capability and does not take
//! either of MySQL's two exits. There is no such backend among the three, so this file
//! composes one. It is not a hypothetical: those three answers are the ENTIRE gate,
//! and every one of them is a question a vendor answers about itself.
//!
//! # What the fixture is, exactly, and what it is not
//!
//! `FOURTH_VENDOR` is a real [`zeroship_migrate_backend::registry::BackendVendor`] resolved
//! through the real [`zeroship_migrate_backend::registry::VendorSet`] by the real
//! `IrAuthor`, and it supplies its OWN [`zeroship_migrate_backend::ddl::DdlEmitter`] —
//! the seam under test, and the only surface a backend has for spelling DDL.
//!
//! Every OTHER surface is PostgreSQL's, and that is deliberate rather than a
//! shortcut. Those three gate answers — the capability, `restates_column_type_at_apply`
//! and `alter_column_refusal` — are what a backend must pass to ARRIVE at the render
//! at all, and borrowing a set that passes them is what puts the fixture on the live
//! path instead of an argument about whether one could exist. **The grammar is not
//! among the borrowed answers.** `SchemaRenderer` contributes `quote_ident` and
//! `column_type`; it has no `ALTER COLUMN` method, because the trait that would own
//! one is `DdlEmitter`, and `DdlEmitter` here is this file's.
//!
//! So the test is exact about what it proves: the emitter is the ONLY thing that
//! could have spelled the statement, this emitter can be asked nothing, and a
//! statement comes out anyway. In GREEN the same emitter answers and the assertion
//! reads its bytes back — including its own `app::widgets` table reference, which no
//! borrowed renderer produces.
//!
//! It is NOT a claim about any real product, and the spellings below are nobody's
//! dialect. They are chosen to be UNMISTAKABLY not PostgreSQL's — `SET DATA TYPE`
//! with no `USING` and no `::` — so the assertion distinguishes "the backend was
//! asked" from "the backend happened to agree".

use std::collections::BTreeSet;
use std::sync::LazyLock;

use zeroship_migrate::model::ir::{ColType, IrDefault, IrFlagsOverride, Op};
use zeroship_migrate::{IrAuthor, LiveSchema, Migration, MigrationIr, CURRENT_IR_VERSION};
use zeroship_migrate_backend::attribute::AttributeVocabulary;
use zeroship_migrate_backend::ddl::{CreateTableRequest, DdlEmitter};
use zeroship_migrate_backend::registry::{BackendVendor, VendorSet};
use zeroship_migrate_backend::snapshot::{ColumnSnapshot, ConstraintSnapshot, IndexSnapshot};
use zeroship_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_ir::ir::PartitionBounds;

use crate::support;

const SCHEMA: &str = "app";
const OWNER: &str = "app_fourth";
const TABLE: &str = "widgets";
const COLUMN: &str = "qty";

/// The fourth backend's identity. A test binary is a host and may compose a backend.
const FOURTH_ID: DialectId = DialectId::new("fourthdb");

/// The fourth backend's own capability row.
///
/// `NativeAlterColumn` is the ONE capability this file needs and the whole reason the
/// fixture exists. It is a claim about the DATABASE — "this server alters a column in
/// place rather than rebuilding the table" — and it is true of far more servers than
/// the three that ship.
static FOURTH_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: FOURTH_ID,
    display_name: "FourthDB",
    capabilities: CapabilitySet::empty().with(Capability::NativeAlterColumn),
    limits: Limits {
        identifier: IdentifierLimit::Unbounded,
        reserved_identifier_prefixes: &[],
    },
};

/// The fourth backend's `ALTER COLUMN` spellings — the answers the seam exists to
/// carry.
///
/// Every OTHER method is `unreachable!`, and that is an assertion rather than a
/// shortcut: this path must reach the alter-column methods and nothing else. A call to
/// any other one fails the test by name instead of quietly returning a plausible
/// string.
#[derive(Debug)]
struct FourthEmitter {
    project_schema: String,
}

impl FourthEmitter {
    /// This backend's identifier spelling. Deliberately its own — a bare, unquoted,
    /// schema-qualified reference — so that a byte comparison cannot pass by accident
    /// through a shared quoting convention.
    fn refs(&self, table: &str, column: &str) -> (String, String) {
        (
            format!("{}::{table}", self.project_schema),
            column.to_string(),
        )
    }
}

fn fourth_emitter(project_schema: &str) -> Box<dyn DdlEmitter> {
    Box::new(FourthEmitter {
        project_schema: project_schema.to_string(),
    })
}

impl DdlEmitter for FourthEmitter {
    fn dialect(&self) -> DialectId {
        FOURTH_ID
    }

    fn alter_column_type_up(
        &self,
        table: &str,
        column: &str,
        ty: &str,
        _cast_value: bool,
    ) -> Option<String> {
        let (table_ref, col_ref) = self.refs(table, column);
        Some(format!(
            "ALTER TABLE {table_ref} ALTER {col_ref} SET DATA TYPE {ty}"
        ))
    }

    fn alter_column_nullability(
        &self,
        table: &str,
        column: &str,
        nullable: bool,
    ) -> Option<(String, String)> {
        let (table_ref, col_ref) = self.refs(table, column);
        let (verb, reverse) = if nullable {
            ("DROP NOT NULL", "SET NOT NULL")
        } else {
            ("SET NOT NULL", "DROP NOT NULL")
        };
        Some((
            format!("ALTER TABLE {table_ref} ALTER {col_ref} {verb}"),
            format!("ALTER TABLE {table_ref} ALTER {col_ref} {reverse}"),
        ))
    }

    fn alter_column_default(
        &self,
        table: &str,
        column: &str,
        default_sql: Option<&str>,
    ) -> Option<String> {
        let (table_ref, col_ref) = self.refs(table, column);
        let action = match default_sql {
            Some(default_sql) => format!("SET DEFAULT {default_sql}"),
            None => "DROP DEFAULT".to_string(),
        };
        Some(format!("ALTER TABLE {table_ref} ALTER {col_ref} {action}"))
    }

    fn create_table(&self, _req: &CreateTableRequest<'_>) -> Vec<String> {
        unreachable!("the alter-column path must not reach create_table")
    }
    fn fk_clause(&self, _fk: &ConstraintSnapshot) -> String {
        unreachable!("the alter-column path must not reach fk_clause")
    }
    fn alter_table_ref(&self, _table: &str) -> String {
        unreachable!("the alter-column path must not reach alter_table_ref")
    }
    fn drop_foreign_key_up(&self, _table: &str, _name: &str) -> Option<String> {
        unreachable!("the alter-column path must not reach drop_foreign_key_up")
    }
    fn indexes_inlined_by_create(&self, _req: &CreateTableRequest<'_>) -> Vec<String> {
        unreachable!("the alter-column path must not reach indexes_inlined_by_create")
    }
    fn add_column(&self, _table: &str, _c: &ColumnSnapshot) -> (Vec<String>, Option<String>) {
        unreachable!("the alter-column path must not reach add_column")
    }
    fn create_index(&self, _table: &str, _idx: &IndexSnapshot) -> (String, String) {
        unreachable!("the alter-column path must not reach create_index")
    }
    fn drop_table_up(&self, _table: &str) -> String {
        unreachable!("the alter-column path must not reach drop_table_up")
    }
    fn rename_table(&self, _table: &str, _to: &str) -> (String, String) {
        unreachable!("the alter-column path must not reach rename_table")
    }
    fn drop_column_up(&self, _table: &str, _col: &str) -> String {
        unreachable!("the alter-column path must not reach drop_column_up")
    }
    fn drop_index_up(&self, _table: Option<&str>, _idx: &str) -> String {
        unreachable!("the alter-column path must not reach drop_index_up")
    }
    fn create_partition(
        &self,
        _name: &str,
        _of: &str,
        _bounds: &PartitionBounds,
    ) -> Option<(String, String)> {
        None
    }
    fn attach_partition(
        &self,
        _parent: &str,
        _name: &str,
        _bounds: &PartitionBounds,
    ) -> Option<(String, String)> {
        None
    }
    fn detach_partition(&self, _parent: &str, _name: &str, _concurrently: bool) -> Option<String> {
        None
    }
    fn drop_partition(&self, _name: &str, _cascade: bool) -> Option<String> {
        None
    }
}

/// The composed fourth vendor.
///
/// Leaked rather than declared `static` because `BackendVendor`'s renderer fields are
/// `&'static dyn`, and a `static` initializer may not READ another `static` to borrow
/// SQLite's. Leaking once at first use is the same lifetime with a different birth.
static FOURTH_VENDOR: LazyLock<&'static BackendVendor> = LazyLock::new(|| {
    let borrowed = &zeroship_migrate_postgres::VENDOR;
    Box::leak(Box::new(BackendVendor {
        descriptor: &FOURTH_DESCRIPTOR,
        dml: borrowed.dml,
        schema: borrowed.schema,
        value_format: borrowed.value_format,
        existence_probe: borrowed.existence_probe,
        catalog_fold: borrowed.catalog_fold,
        validation: borrowed.validation,
        // THE ONE SURFACE THAT IS THIS BACKEND'S OWN, and the seam under test.
        ddl: fourth_emitter,
        guard: borrowed.guard,
        advisor: borrowed.advisor,
        // Written out, not borrowed from PostgreSQL: a vocabulary is the one field where
        // inheriting another backend's answer would be actively wrong, since every key in
        // it names that other backend's dialect. This fourth backend declares nothing,
        // and says so.
        attributes: AttributeVocabulary::empty(),
    }))
});

/// A shipping set with the fourth backend in it, composed the way a host composes one.
fn four_vendors() -> VendorSet {
    static SET: LazyLock<&'static [&'static BackendVendor]> = LazyLock::new(|| {
        Box::leak(Box::new([
            &zeroship_migrate_postgres::VENDOR,
            &zeroship_migrate_sqlite::VENDOR,
            &zeroship_migrate_mysql::VENDOR,
            *FOURTH_VENDOR,
        ])) as &'static [&'static BackendVendor]
    });
    VendorSet::new(*SET)
}

fn ir(op: Op) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "m".into(),
        owner_app: OWNER.into(),
        ops: vec![op],
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// Lower one op on the fourth backend, through the real `IrAuthor` and the real
/// registry.
fn lower_on_fourth(op: Op) -> Vec<Migration> {
    let mut live = BTreeSet::new();
    live.insert(TABLE.to_string());
    IrAuthor::new(
        four_vendors(),
        SCHEMA,
        OWNER,
        &FOURTH_ID,
        &support::no_inject(SCHEMA),
    )
    .lower(&ir(op), &LiveSchema::from(&live))
    .expect("the fourth backend declares NativeAlterColumn, so this op lowers")
}

fn only_up(migrations: &[Migration]) -> String {
    assert_eq!(
        migrations.len(),
        1,
        "one op lowers to one migration; got {migrations:#?}"
    );
    migrations[0].up.clone()
}

/// A retype on a backend that is not PostgreSQL must carry that backend's verb, and
/// must NOT carry PostgreSQL's `USING <col>::<type>` cast.
///
/// The cast is the sharpest half. `USING` is a PostgreSQL clause and `::` is a
/// PostgreSQL operator; a server without either receives a statement it cannot parse,
/// and it receives it from a crate that never named it.
#[test]
fn a_retype_is_spelled_by_the_backend_and_not_by_the_engine() {
    let up = only_up(&lower_on_fourth(Op::SetColumnType {
        table: TABLE.into(),
        column: COLUMN.into(),
        to_type: ColType::Double,
        using: None,
        schema: None,
        existence_guard: None,
    }));

    assert!(
        up.contains("SET DATA TYPE"),
        "the fourth backend's own retype verb must reach the output; got: {up}"
    );
    assert!(
        !up.contains("USING"),
        "PostgreSQL's USING clause must not be written for a backend that never \
         asked for it; got: {up}"
    );
    assert!(
        !up.contains("::vector") && !up.contains(&format!("\"{COLUMN}\"::")),
        "PostgreSQL's `::` cast must not be written for a backend that never asked \
         for it; got: {up}"
    );
}

/// The two nullability verbs are the backend's, and so is the `down` that inverts
/// them.
#[test]
fn a_nullability_change_is_spelled_by_the_backend_in_both_directions() {
    let migrations = lower_on_fourth(Op::SetColumnNotNull {
        table: TABLE.into(),
        column: COLUMN.into(),
        schema: None,
        existence_guard: None,
    });
    assert_eq!(migrations.len(), 1, "one op, one migration");
    let up = migrations[0].up.clone();
    let down = migrations[0].down.clone().expect("a tightening has a down");

    assert_eq!(
        up,
        format!("ALTER TABLE {SCHEMA}::{TABLE} ALTER {COLUMN} SET NOT NULL"),
        "the fourth backend's own nullability spelling must reach the up"
    );
    assert_eq!(
        down,
        format!("ALTER TABLE {SCHEMA}::{TABLE} ALTER {COLUMN} DROP NOT NULL"),
        "and its own inverse must reach the down"
    );
}

/// The default verbs likewise. This is the one member of the family that a SHIPPING
/// backend other than PostgreSQL already reaches: `Op::SetColumnDefault` gates on
/// `Capability::NativeAlterColumn` alone, with no `alter_column_refusal` exit, so
/// MySQL arrives here today and receives an engine-authored statement.
#[test]
fn a_default_change_is_spelled_by_the_backend() {
    let set = only_up(&lower_on_fourth(Op::SetColumnDefault {
        table: TABLE.into(),
        column: COLUMN.into(),
        value: IrDefault::Literal {
            value: zeroship_migrate::model::ir::IrScalar::Int(7),
        },
        schema: None,
        existence_guard: None,
    }));
    assert_eq!(
        set,
        format!("ALTER TABLE {SCHEMA}::{TABLE} ALTER {COLUMN} SET DEFAULT 7"),
        "the fourth backend's own SET DEFAULT spelling must reach the up"
    );

    let drop = only_up(&lower_on_fourth(Op::DropColumnDefault {
        table: TABLE.into(),
        column: COLUMN.into(),
        schema: None,
        existence_guard: None,
    }));
    assert_eq!(
        drop,
        format!("ALTER TABLE {SCHEMA}::{TABLE} ALTER {COLUMN} DROP DEFAULT"),
        "and its own DROP DEFAULT spelling"
    );
}
