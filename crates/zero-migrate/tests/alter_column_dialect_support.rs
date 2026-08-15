//! F674: the dialect table must not call an operation `portable` on a dialect
//! whose lowerer refuses it outright.
//!
//! `dialect-support.toml` is the hand-authored source of truth for per-dialect
//! op support, and the GATE reads the table generated from it. It declared
//!
//!     setColumnNotNull    sqlite = portable    mysql = portable
//!     dropColumnNotNull   sqlite = portable    mysql = portable
//!     dropColumnDefault   sqlite = portable
//!
//! while the lowerer refuses every one of those combinations as the FIRST
//! statement of the matching arm:
//!
//!     setColumnNotNull   require_alter_column_rendering(..)
//!     dropColumnNotNull  require_alter_column_rendering(..)
//!     dropColumnDefault  require_capability_for(NativeAlterColumn, ..)
//!
//! `require_alter_column_rendering` is the `NativeAlterColumn` capability check —
//! false for SQLite — plus an UNCONDITIONAL MySQL refusal. `dropColumnDefault`
//! takes only the capability half, which is why it is refused on SQLite and
//! accepted on MySQL. No live schema and no envelope shape reaches past those
//! calls, so the refusals are categorical and the table was simply wrong.
//!
//! WHY NO EXISTING GUARD CAUGHT IT. `dialect_table_faithfulness.rs` proves the
//! generated table matches `Support::decision()` and matches the sidecar
//! row-for-row. Both hold. The gate READS the table, so the two agree by
//! construction; nothing compared either against the lowerer, and the lowerer is
//! where these ops die. The consequence was a refusal arriving at lower time that
//! the gate had already waved through.
//!
//! The PostgreSQL arm is the control: these ops are genuinely portable there, and
//! a "fix" that refused them everywhere would satisfy the SQLite and MySQL
//! assertions while breaking the dialect that supports them.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

/// The three operations whose alter-column rendering the lowerer gates, paired
/// with the dialects that refuse them.
const REFUSED: &[(&str, &[Dialect], &str)] = &[
    (
        "setColumnNotNull",
        &[Dialect::Sqlite, Dialect::Mysql],
        r#"{"op":"setColumnNotNull","table":"t","column":"c1"}"#,
    ),
    (
        "dropColumnNotNull",
        &[Dialect::Sqlite, Dialect::Mysql],
        r#"{"op":"dropColumnNotNull","table":"t","column":"c1"}"#,
    ),
    (
        "dropColumnDefault",
        &[Dialect::Sqlite],
        r#"{"op":"dropColumnDefault","table":"t","column":"c1"}"#,
    ),
];

fn gate(op: &str, dialect: Dialect) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{op}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("envelope parses");
    validate_ir(&ir, dialect, &[]).map_err(|e| e.code)
}

#[test]
fn the_gate_refuses_alter_column_ops_on_dialects_whose_lowerer_refuses_them() {
    for (name, dialects, op) in REFUSED {
        for dialect in *dialects {
            let verdict = gate(op, *dialect);
            assert!(
                verdict.is_err(),
                "{name} on {dialect:?}: the gate accepted an operation the lowerer refuses \
                 outright (require_alter_column_rendering is the first statement of the arm). \
                 The dialect table called this cell `portable`, which that file defines as \
                 \"renders/validates on this dialect\""
            );
        }
    }
}

#[test]
fn postgresql_still_accepts_every_one_of_them() {
    // CONTROL. These operations are genuinely portable on PostgreSQL. Without
    // this arm, refusing them on all three dialects would pass the test above.
    for (name, _, op) in REFUSED {
        gate(op, Dialect::Postgres)
            .unwrap_or_else(|code| panic!("{name} must still pass the gate on PostgreSQL: {code}"));
    }
}
