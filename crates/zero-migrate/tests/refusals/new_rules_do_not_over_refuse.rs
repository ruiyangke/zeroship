//! A regression guard for the refusal rules added across F712-F727.
//!
//! Roughly ten new rules were added to `validate_ir` in one session: shared
//! relation and type namespaces, per-table constraint and trigger names, indexes
//! in the relation namespace, use-after-drop for columns in expressions, second
//! relation names, and DML column references. Each shipped with controls chosen
//! by the person writing it - which is exactly the weakness. A control tests the
//! rule its author was thinking about; it cannot test the pattern its author did
//! not think of.
//!
//! THIS FIXTURE EXISTS FOR THE OTHER DIRECTION. Every case below is an ordinary
//! migration shape that a real project would author, and every one must keep
//! passing. A future tightening that breaks one of these breaks users, and the
//! failure would otherwise surface as a bug report rather than a red test.
//!
//! Nothing here asserts a refusal. That is deliberate: a file of `expect_err`
//! calls proves the rules fire, and the whole point of this one is to prove they
//! do NOT fire on legitimate work.

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

#[track_caller]
fn must_pass(what: &str, ops: &str) {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    if let Err(e) = validate_ir(&ir, Dialect::Postgres) {
        panic!(
            "{what} is an ordinary migration and must pass: [{}] {}",
            e.code, e.reason
        );
    }
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const VW_A: &str = r#"{"op":"createView","name":"vw","query":{"kind":"structured","select":{"from":{"name":"a"},"projection":[{"kind":"colRef","name":"c0"}]}}}"#;

#[test]
fn conventional_naming_around_one_table_is_fine() {
    // The commonest real shape: an enum, a table, an index, a view and a
    // sequence all named after the same entity. Five of this session's rules
    // touch these namespaces.
    must_pass(
        "a table with conventionally named companions",
        &format!(
            r#"{{"op":"createEnum","name":"a_kind","values":["x"]}},{A},{{"op":"createIndex","name":"a_v_idx","table":"a","columns":[{{"kind":"column","name":"v"}}]}},{{"op":"createView","name":"a_summary","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"c0"}}]}}}}}},{{"op":"createSequence","name":"a_seq"}}"#
        ),
    );
}

#[test]
fn per_table_names_may_repeat_across_tables() {
    must_pass(
        "two tables with parallel index names",
        &format!(
            r#"{A},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}},{{"op":"createIndex","name":"a_v_idx","table":"a","columns":[{{"kind":"column","name":"v"}}]}},{{"op":"createIndex","name":"b_v_idx","table":"b","columns":[{{"kind":"column","name":"v"}}]}}"#
        ),
    );
}

#[test]
fn the_same_table_name_in_two_schemas_is_fine() {
    must_pass(
        "multi-schema projects",
        r#"{"op":"createTable","name":"t","schema":"s1","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]},{"op":"createTable","name":"t","schema":"s2","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#,
    );
}

#[test]
fn add_a_column_then_index_and_constrain_it() {
    must_pass(
        "the standard add-and-harden sequence",
        &format!(
            r#"{A},{{"op":"addColumn","table":"a","column":"n","type":"int","nullable":true}},{{"op":"createIndex","name":"a_n_idx","table":"a","columns":[{{"kind":"column","name":"n"}}]}},{{"op":"addConstraint","table":"a","constraint":{{"name":"a_n_chk","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"n"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}"#
        ),
    );
}

#[test]
fn replace_an_index_definition_in_place() {
    must_pass(
        "drop an index and recreate it over different columns",
        &format!(
            r#"{A},{{"op":"dropIndex","name":"ix","table":"a"}},{{"op":"createIndex","name":"ix","table":"a","columns":[{{"kind":"column","name":"w"}}]}}"#
        ),
    );
}

#[test]
fn seed_and_backfill_after_creating_structure() {
    must_pass(
        "seed rows then backfill a new column from an existing one",
        &format!(
            r#"{A},{{"op":"insert","table":"a","columns":["c0","v"],"rows":[[1,2]]}},{{"op":"addColumn","table":"a","column":"n","type":"int","nullable":true}},{{"op":"backfill","table":"a","name":"bf","cursorColumns":["c0"],"cursorStability":{{"mode":"guardUpdates"}},"batchSize":100,"set":{{"n":{{"node":"colRef","name":"v"}}}}}}"#
        ),
    );
}

#[test]
fn a_view_may_outlive_a_rename_of_its_source() {
    must_pass(
        "create a view, then rename the table it reads",
        &format!(r#"{A},{VW_A},{{"op":"renameTable","table":"a","to":"a2"}}"#),
    );
}

#[test]
fn freed_names_may_be_reused_across_kinds() {
    must_pass(
        "rename a table away, then take its name for a view",
        &format!(
            r#"{A},{{"op":"renameTable","table":"a","to":"a2"}},{{"op":"createView","name":"a","query":{{"kind":"structured","select":{{"from":{{"name":"a2"}},"projection":[{{"kind":"colRef","name":"c0"}}]}}}}}}"#
        ),
    );
    must_pass(
        "drop a view, then take its name for a table",
        &format!(
            r#"{A},{VW_A},{{"op":"dropView","name":"vw"}},{{"op":"createTable","name":"vw","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
        ),
    );
}

#[test]
fn expressions_may_name_columns_created_in_the_same_envelope() {
    must_pass(
        "an inline CHECK over a column of its own createTable",
        r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"ck","kind":{"kind":"check","expr":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"v"},"rhs":{"node":"literal","value":0}}}}]}"#,
    );
}

#[test]
fn a_foreign_key_may_point_forward() {
    must_pass(
        "a forward FK alongside other work",
        &format!(
            r#"{A},{{"op":"addConstraint","table":"a","constraint":{{"name":"fk","kind":{{"kind":"fk","columns":["c0"],"referencesTable":"b","referencesColumns":["c0"]}}}}}},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}},{VW_A}"#
        ),
    );
}

#[test]
fn different_object_kinds_may_share_a_name_where_the_server_allows_it() {
    must_pass(
        "a trigger and a constraint with the same name on one table",
        &format!(
            r#"{A},{{"op":"addConstraint","table":"a","constraint":{{"name":"x","kind":{{"kind":"unique","columns":["v"]}}}}}},{{"op":"createTrigger","name":"x","table":"a","timing":"after","events":["insert"],"forEach":"row","action":{{"kind":"executeFunction","name":"f"}}}}"#
        ),
    );
}
