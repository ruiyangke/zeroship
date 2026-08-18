//! Every cell `dialect-support.toml` declares `unsupported` must refuse with a
//! reason written FOR AN OPERATOR, never with the engine's internal placeholder.
//!
//! WHY THIS FILE EXISTS. `Op::support()` reads the generated dialect table, so
//! declaring a cell `unsupported` makes `validate` refuse on the table's own
//! say-so. The refusal itself is therefore satisfied BY CONSTRUCTION - it cannot
//! fail, and `dialect_conformance_live.rs` records the demonstration (flipping
//! `dropTable/base` on PostgreSQL, an op PostgreSQL obviously supports, left that
//! suite GREEN). The ONE observable a too-conservative declaration still moves is
//! the MESSAGE, because the reason is looked up separately in
//! `op_support.rs::unsupported_reason`, a match over `(Op, dialect, variant)`.
//!
//! When those two are edited apart - a sidecar cell flipped without the matching
//! reason arm - the operator is handed the literal string
//! [`INTERNAL_NO_REFUSAL_REASON`] for an ordinary, well-understood dialect limit.
//! That is a user-facing defect, and it shipped: four rows carried it on SQLite,
//! three of them also on MySQL.
//!
//! WHY IT IS NOT PART OF THE LIVE SUITE. `dialect_conformance_live.rs` covers
//! PostgreSQL and SQLite only - it opens no MySQL connection, and its own header
//! says so. Three of the shipping placeholder cells were MySQL cells, invisible
//! to it for that reason alone. This sweep needs no server: the reason is a pure
//! function of `(op, dialect, variant)`, so it can cover ALL THREE dialects
//! offline and runs in an ordinary `cargo test`.
//!
//! It drives `validate_op`, the authoring gate an operator actually hits, and
//! reads the `reason` that gate produces - not `unsupported_reason` directly, so
//! a refusal that is re-worded or intercepted upstream is judged on what the
//! operator would really be shown.

mod dialect_corpus;

use dialect_corpus::corpus;
use zero_migrate::model::dialect_table::{Disposition, DIALECT_TABLE};
use zero_migrate::model::op_support::INTERNAL_NO_REFUSAL_REASON;
use zero_migrate::model::support::Dialect as TableDialect;
use zero_migrate::model::validate::{validate_op, Dialect as ValidateDialect};

/// The three dialects the sidecar declares. `dialect_table` and `validate` carry
/// SEPARATE `Dialect` enums (engine-side support vs. the IR wire contract), so
/// each row names both rather than converting one into the other silently.
const DIALECTS: [(&str, TableDialect, ValidateDialect); 3] = [
    (
        "postgres",
        TableDialect::Postgres,
        ValidateDialect::Postgres,
    ),
    ("sqlite", TableDialect::Sqlite, ValidateDialect::Sqlite),
    ("mysql", TableDialect::Mysql, ValidateDialect::Mysql),
];

#[test]
fn no_unsupported_cell_shows_the_operator_an_internal_placeholder() {
    let corpus = corpus();
    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0_usize;

    for (kind, variant, op) in &corpus {
        let row = DIALECT_TABLE
            .iter()
            .find(|r| r.kind == *kind && r.variant == *variant)
            .unwrap_or_else(|| panic!("no DIALECT_TABLE row for {kind}/{variant}"));

        for (name, table_dialect, validate_dialect) in DIALECTS {
            if row.disposition(table_dialect) != Disposition::Unsupported {
                continue;
            }
            checked += 1;

            // The cell is declared unsupported, so the authoring gate must refuse.
            // This half is the tautology; it is asserted anyway so a refusal that
            // stops happening is not silently skipped by the message check below.
            let Err(err) = validate_op(op, validate_dialect, 0) else {
                offenders.push(format!(
                    "  {kind}/{variant} [{name}] is declared `unsupported` but validate_op \
                     ACCEPTED it, so no message was produced to check"
                ));
                continue;
            };

            if err.reason.contains(INTERNAL_NO_REFUSAL_REASON) {
                offenders.push(format!(
                    "  {kind}/{variant} [{name}] refuses with the INTERNAL placeholder \
                     {INTERNAL_NO_REFUSAL_REASON:?} instead of an operator-facing reason. \
                     dialect-support.toml declares this cell `unsupported` and \
                     op_support.rs::unsupported_reason has no arm for it."
                ));
            } else if err.reason.trim().is_empty() {
                offenders.push(format!(
                    "  {kind}/{variant} [{name}] refuses with an EMPTY reason"
                ));
            }
        }
    }

    // A guard whose subject set is empty is a guard that cannot fail. The sidecar
    // declares 117 unsupported cells today; the floor keeps a sweep that silently
    // stops finding them from passing.
    assert!(
        checked >= 100,
        "expected the sidecar to declare at least 100 unsupported cells for this sweep to \
         judge, found {checked} - the corpus or the table lookup is broken, not the reasons"
    );

    assert!(
        offenders.is_empty(),
        "{} of {checked} declared-unsupported cells hand the operator a non-reason:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}
