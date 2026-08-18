//! An exclusion constraint's expressions and column targets reach the
//! dropped-column walk.
//!
//! F765 made `expression_column_references` exhaustive over `Op` and that was
//! described - by me - as closing the accessor. It closed ONE of its three
//! matches. The inner match on `IrConstraintKind` still read
//!
//!     IrConstraintKind::Check { expr, .. } => refs(table, expr),
//!     _ => Vec::new(),
//!
//! and `Exclusion` carries a `where_predicate` plus element targets that may be
//! expressions. So an exclusion constraint could name a column the migration had
//! just dropped and be accepted.
//!
//! MEASURED before the fix, with the baseline that makes it attributable:
//!
//!     createTable a(c0,v); addConstraint EXCLUDE (c0 WITH =) WHERE (v > 0)
//!         ACCEPTED - the envelope is valid
//!     ...the same with dropColumn v FIRST
//!         ALSO ACCEPTED - the defect
//!
//! MAKING AN OUTER MATCH EXHAUSTIVE DOES NOT CLOSE A WALKER. A walker holds as
//! many matches as it has nested closed enums, and the compiler only forces the
//! one you removed the catch-all from. Both of this accessor's constraint-kind
//! matches are now exhaustive; `IrValue` and `BackfillSetValue` remain.

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const DROP_V: &str = r#"{"op":"dropColumn","table":"a","column":"v"}"#;

/// `EXCLUDE (c0 WITH =) WHERE (v > 0)` - the predicate names `v`.
const EXCL_WHERE_V: &str = r#"{"op":"addConstraint","table":"a","constraint":{"name":"ex1","kind":{"kind":"exclusion","elements":[{"target":{"kind":"column","name":"c0"},"operator":"="}],"wherePredicate":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"v"},"rhs":{"node":"literal","value":0}}}}}"#;
/// `EXCLUDE (v WITH =)` - the ELEMENT names `v`, as a bare column.
const EXCL_ON_V: &str = r#"{"op":"addConstraint","table":"a","constraint":{"name":"ex2","kind":{"kind":"exclusion","elements":[{"target":{"kind":"column","name":"v"},"operator":"="}]}}}"#;

#[test]
fn the_envelope_is_valid_without_the_drop() {
    // The precondition. Without it, a refusal below could be the exclusion
    // constraint being rejected for some unrelated reason.
    verdict(&format!("{A},{EXCL_WHERE_V}")).expect("an exclusion with a WHERE is ordinary");
    verdict(&format!("{A},{EXCL_ON_V}")).expect("an exclusion on a live column is ordinary");
}

#[test]
fn an_exclusion_predicate_naming_a_dropped_column_is_refused() {
    let refusal = verdict(&format!("{A},{DROP_V},{EXCL_WHERE_V}"))
        .expect_err("the WHERE reads a column the drop removed");
    assert!(
        refusal.contains("names column \"v\""),
        "the refusal must name the column the predicate reads: {refusal}"
    );
}

#[test]
fn an_exclusion_element_naming_a_dropped_column_is_refused() {
    // The other half: a bare column target is a plain reference, read by the
    // sibling accessor. A fix written only into the expression side passes the
    // test above and misses this.
    let refusal = verdict(&format!("{A},{DROP_V},{EXCL_ON_V}"))
        .expect_err("the element names a column the drop removed");
    assert!(
        refusal.contains("names column \"v\""),
        "the refusal must name the excluded column: {refusal}"
    );
}
