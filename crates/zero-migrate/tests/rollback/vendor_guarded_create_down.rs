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

// A role is CLUSTER-wide: not scoped to the schema, not even to the database. A
// guarded create-role against an existing role makes nothing, so a DROP ROLE down
// would remove a principal other databases depend on, cascading through every
// grant and ownership it holds. Same reasoning as the schema arm, wider blast.
#[test]
fn a_guarded_create_role_synthesises_no_down() {
    let op = Op::CreateRole {
        name: "app_reader".to_string(),
        login: None,
        password: None,
        bypass_rls: None,
        create_role: None,
        create_db: None,
        superuser: None,
        in_role: None,
        set_search_path: None,
        if_not_exists: Some(true),
    };
    let stmts = render_vendor_op(&op, "app").expect("render");
    assert!(
        stmts[0].down.is_none(),
        "a guarded create-role may have created nothing: {:?}",
        stmts[0].down
    );
}

#[test]
fn an_unguarded_create_role_keeps_its_down() {
    let op = Op::CreateRole {
        name: "app_reader".to_string(),
        login: None,
        password: None,
        bypass_rls: None,
        create_role: None,
        create_db: None,
        superuser: None,
        in_role: None,
        set_search_path: None,
        if_not_exists: None,
    };
    let stmts = render_vendor_op(&op, "app").expect("render");
    let down = stmts[0]
        .down
        .as_deref()
        .expect("an unguarded create owns its drop");
    assert!(down.contains("DROP ROLE"), "{down}");
}

// The guarded create-role pushes a SECOND statement for search_path, whose down
// is `RESET`. RESET discards whatever search_path the role carried before this
// migration, which is only an inverse if this migration created the role. Under
// the guard it may not have, so no statement in the batch may claim a down.
#[test]
fn a_guarded_create_role_with_search_path_synthesises_no_down_at_all() {
    let op = Op::CreateRole {
        name: "app_reader".to_string(),
        login: None,
        password: None,
        bypass_rls: None,
        create_role: None,
        create_db: None,
        superuser: None,
        in_role: None,
        set_search_path: Some(vec!["app".to_string()]),
        if_not_exists: Some(true),
    };
    let stmts = render_vendor_op(&op, "app").expect("render");
    assert!(stmts.len() >= 2, "expected the search_path statement too");
    for s in &stmts {
        assert!(
            s.down.is_none(),
            "{} must carry no down under the guard: {:?}",
            s.name,
            s.down
        );
    }
}

// Control: unguarded, the role really was created, so RESET is a true inverse.
#[test]
fn an_unguarded_create_role_with_search_path_keeps_both_downs() {
    let op = Op::CreateRole {
        name: "app_reader".to_string(),
        login: None,
        password: None,
        bypass_rls: None,
        create_role: None,
        create_db: None,
        superuser: None,
        in_role: None,
        set_search_path: Some(vec!["app".to_string()]),
        if_not_exists: None,
    };
    let stmts = render_vendor_op(&op, "app").expect("render");
    assert!(stmts.iter().all(|s| s.down.is_some()), "both downs survive");
}
