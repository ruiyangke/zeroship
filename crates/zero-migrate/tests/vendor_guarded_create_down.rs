//! A guarded create must not synthesise a down that destroys what it did not make.
//!
//! `CREATE SCHEMA IF NOT EXISTS` applied against an existing schema creates
//! nothing. A `DROP SCHEMA` down would then destroy a schema this migration never
//! owned, and everything in it. The engine cannot know whether the up created
//! anything, so the honest answer is no down at all - `plan_rollback` already
//! refuses an irreversible migration and already has the force path for an
//! operator who accepts the loss.

use zero_migrate::model::ir::Op;
use zero_migrate::render::vendor::render_vendor_op;

#[test]
fn a_guarded_create_schema_synthesises_no_down() {
    let op = Op::CreateSchema {
        name: "analytics".to_string(),
        if_not_exists: Some(true),
        authorization: None,
    };
    let stmts = render_vendor_op(&op, "app").expect("render");
    assert_eq!(stmts.len(), 1);
    assert!(
        stmts[0].down.is_none(),
        "a guarded create may have created nothing, so it cannot own a DROP: {:?}",
        stmts[0].down
    );
}

#[test]
fn a_guarded_create_extension_synthesises_no_down() {
    let op = Op::CreateExtension {
        name: "pgcrypto".to_string(),
        if_not_exists: Some(true),
        schema: None,
    };
    let stmts = render_vendor_op(&op, "app").expect("render");
    assert!(
        stmts[0].down.is_none(),
        "same reasoning as the schema arm: {:?}",
        stmts[0].down
    );
}

// The control: an UNGUARDED create did make the object, so it keeps its down.
// Without this, emitting `down: None` for every create would also pass.
#[test]
fn an_unguarded_create_schema_keeps_its_down() {
    let op = Op::CreateSchema {
        name: "analytics".to_string(),
        if_not_exists: None,
        authorization: None,
    };
    let stmts = render_vendor_op(&op, "app").expect("render");
    let down = stmts[0]
        .down
        .as_deref()
        .expect("an unguarded create owns its drop");
    assert!(down.contains("DROP SCHEMA"), "{down}");
}
