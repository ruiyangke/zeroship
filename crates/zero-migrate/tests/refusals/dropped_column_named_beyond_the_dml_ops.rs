//! Five more sites that could name a column this migration dropped.
//!
//! The fourth consecutive defect from the same cause, and by now the cause is the
//! finding rather than the bug. `validate_no_op_references_a_dropped_column` is
//! driven by two accessors - `expression_column_references` and
//! `plain_column_references` - and each is a closed match. Every op absent from
//! those matches is invisible to a rule that covers the rest of the engine:
//!
//!     F761  `touched_table()` returns ONE name  -> ops naming a LIST escaped
//!     F762  the walk collected `IrColumn`s      -> ops with a bare type escaped
//!     F763  no collection existed at all        -> roles and schemas escaped
//!     F764  two closed matches                  -> the five ops below escaped
//!
//! MEASURED AGAINST LIVE POSTGRESQL, each after `ALTER TABLE a DROP COLUMN v`:
//!
//!     CREATE POLICY p ON a FOR ALL USING (v > 0)
//!         ERROR: column "v" does not exist
//!     COMMENT ON COLUMN a.v IS 'x'
//!         ERROR: column "v" of relation "a" does not exist
//!     ALTER TABLE a ADD PRIMARY KEY (v)
//!         ERROR: column "v" of relation "a" does not exist
//!     CREATE VIEW vw AS SELECT v FROM a
//!         ERROR: column "v" does not exist
//!     CREATE TRIGGER tg … WHEN (NEW.v > 0) …
//!         ERROR: column new.v does not exist
//!
//! THE VIEW ARM IS DELIBERATELY NARROW. A view body names columns of its source
//! relation, but in a JOINED select an unqualified column could belong to either
//! side, and attributing it to the `FROM` relation would refuse a view whose
//! column lives on the joined table. So the arm applies only to a join-free body,
//! and a qualified reference must name the `FROM` relation to count. Two controls
//! hold that line: a joined view naming a dropped column is ACCEPTED here - an
//! under-refusal, deliberately chosen over a wrong one - and a view qualified
//! with another relation is left alone.
//!
//! `alterPrimaryKey` also contributes its `expectedColumns` precondition, not
//! just the tuple being installed: those columns are emitted as a live-key check,
//! so a dropped one fails there just the same.

use crate::support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir_authorized, Dialect, VendorAuthority};

fn verdict(ops: &str) -> Result<(), String> {
    let policy = support::operator_charter("public");
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let authority = VendorAuthority {
        effective: &policy,
        default_schema: "public",
    };
    validate_ir_authorized(&ir, Dialect::Postgres, None, Some(authority))
        .map_err(|e| format!("{}: {}", e.code, e.reason))
}

fn expect_column_refusal(ops: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "this must be refused for naming the dropped column, not by the capability \
         gate - a denial would satisfy expect_err while proving nothing: {refusal}"
    );
    assert!(
        refusal.contains("column"),
        "the refusal must be about the column: {refusal}"
    );
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const B: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"int","nullable":false},{"name":"w","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const DROP_V: &str = r#"{"op":"dropColumn","table":"a","column":"v"}"#;

fn gt_zero(column: &str) -> String {
    format!(
        r#"{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"{column}"}},"rhs":{{"node":"literal","value":0}}}}"#
    )
}

fn policy_using(column: &str) -> String {
    format!(
        r#"{{"op":"createPolicy","name":"p","table":"a","forCmd":"all","using":{}}}"#,
        gt_zero(column)
    )
}

fn view_selecting(column: &str) -> String {
    format!(
        r#"{{"op":"createView","name":"vw","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"{column}"}}]}}}}}}"#
    )
}

fn trigger_when(column: &str) -> String {
    format!(
        r#"{{"op":"createTrigger","name":"tg","table":"a","timing":"after","events":["update"],"forEach":"row","when":{},"action":{{"kind":"executeFunction","name":"fn_a"}}}}"#,
        gt_zero(column)
    )
}

// ---------------------------------------------------------------------------
// Refusals - one per measured site.
// ---------------------------------------------------------------------------

#[test]
fn a_policy_predicate_naming_a_dropped_column_is_refused() {
    expect_column_refusal(
        &format!("{A},{DROP_V},{}", policy_using("v")),
        "the policy predicate reads a column that is gone",
    );
}

#[test]
fn a_policy_with_check_naming_a_dropped_column_is_refused() {
    // The second predicate on the same op. Reading only `using` passes the test
    // above and misses this.
    expect_column_refusal(
        &format!(
            r#"{A},{DROP_V},{{"op":"createPolicy","name":"p","table":"a","forCmd":"all","using":{},"withCheck":{}}}"#,
            gt_zero("c0"),
            gt_zero("v")
        ),
        "the WITH CHECK predicate reads a column that is gone",
    );
}

#[test]
fn a_comment_on_a_dropped_column_is_refused() {
    expect_column_refusal(
        &format!(
            r#"{A},{DROP_V},{{"op":"comment","target":{{"kind":"column","table":"a","name":"v"}},"comment":"x"}}"#
        ),
        "the comment names a column that is gone",
    );
}

#[test]
fn a_primary_key_installing_a_dropped_column_is_refused() {
    expect_column_refusal(
        &format!(
            r#"{A},{DROP_V},{{"op":"alterPrimaryKey","table":"a","action":{{"kind":"replace","expectedColumns":["c0"],"columns":["v"]}}}}"#
        ),
        "the new key tuple names a column that is gone",
    );
}

#[test]
fn a_primary_key_expecting_a_dropped_column_is_refused() {
    // The precondition side. It is emitted as a live-key check naming those
    // columns, so an implementation that only read the installed tuple passes
    // the test above and misses this.
    expect_column_refusal(
        &format!(
            r#"{A},{DROP_V},{{"op":"alterPrimaryKey","table":"a","action":{{"kind":"drop","expectedColumns":["v"]}}}}"#
        ),
        "the expected key tuple names a column that is gone",
    );
}

#[test]
fn a_view_selecting_a_dropped_column_is_refused() {
    expect_column_refusal(
        &format!("{A},{DROP_V},{}", view_selecting("v")),
        "the view body reads a column that is gone",
    );
}

#[test]
fn a_trigger_condition_naming_a_dropped_column_is_refused() {
    expect_column_refusal(
        &format!("{A},{DROP_V},{}", trigger_when("v")),
        "the trigger condition reads a column that is gone",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn each_site_is_still_allowed_on_a_live_column() {
    verdict(&format!("{A},{}", policy_using("v"))).expect("a live policy predicate is ordinary");
    verdict(&format!("{A},{}", view_selecting("v"))).expect("a live view body is ordinary");
    verdict(&format!("{A},{}", trigger_when("v"))).expect("a live trigger condition is ordinary");
    verdict(&format!(
        r#"{A},{{"op":"comment","target":{{"kind":"column","table":"a","name":"v"}},"comment":"x"}}"#
    ))
    .expect("a live column comment is ordinary");
    verdict(&format!(
        r#"{A},{{"op":"alterPrimaryKey","table":"a","action":{{"kind":"replace","expectedColumns":["c0"],"columns":["v"]}}}}"#
    ))
    .expect("a live key tuple is ordinary");
}

#[test]
fn dropping_a_different_column_disturbs_nothing() {
    verdict(&format!(
        r#"{A},{{"op":"dropColumn","table":"a","column":"c0"}},{}"#,
        policy_using("v")
    ))
    .expect("dropping one column says nothing about another");
}

#[test]
fn a_joined_view_is_deliberately_left_alone() {
    // THE BOUNDARY. In a joined select an unqualified column could belong to
    // either relation, so this arm does not run at all. That is an under-refusal
    // - the server would reject this - chosen over the alternative, which
    // refuses views whose column lives on the joined table.
    verdict(&format!(
        r#"{A},{B},{DROP_V},{{"op":"createView","name":"vw","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"v"}}],"joins":[{{"kind":"inner","table":{{"name":"b"}},"on":{{"node":"binOp","op":"eq","lhs":{{"node":"colRef","table":"a","name":"c0"}},"rhs":{{"node":"colRef","table":"b","name":"c0"}}}}}}]}}}}}}"#
    ))
    .expect("a joined body is outside this arm by design");
}

#[test]
fn a_view_column_qualified_with_another_relation_is_left_alone() {
    // Only a reference that resolves to the FROM relation counts. A qualifier
    // naming something else is not this table's column.
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"createView","name":"vw","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","table":"other","name":"v"}}]}}}}}}"#
    ))
    .expect("a foreign qualifier is not the FROM relation's column");
}

#[test]
fn recreating_the_column_before_use_is_still_allowed() {
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"addColumn","table":"a","column":"v","type":"int","nullable":true}},{}"#,
        policy_using("v")
    ))
    .expect("the column is back, so every later reference resolves");
}
