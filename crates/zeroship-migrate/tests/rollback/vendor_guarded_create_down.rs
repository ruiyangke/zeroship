//! A guarded create must not synthesise a down that destroys what it did not make.
//!
//! `CREATE SCHEMA IF NOT EXISTS` applied against an existing schema creates
//! nothing. A `DROP SCHEMA` down would then destroy a schema this migration never
//! owned, and everything in it. The engine cannot know whether the up created
//! anything, so the honest answer is no down at all - `plan_rollback` already
//! refuses an irreversible migration and already has the force path for an
//! operator who accepts the loss.

use zeroship_migrate::model::ir::Op;
use zeroship_migrate::render::vendor::{VendorError, VendorStatement};

/// PostgreSQL's vendor-op rendering, reached through that vendor's REGISTERED
/// renderer.
///
/// This file used to `use zeroship_migrate::render::vendor::render_vendor_op`, a
/// re-export of `zeroship_migrate_postgres::render_vendor_op` at the engine's crate root.
/// Both are gone: the function is `pub(crate)` behind a private module now, so no
/// caller outside `zeroship-migrate-postgres` can name it, and the only door is
/// `DmlRenderer::render_vendor_op`.
///
/// A test naming the vendor whose spelling it asserts on is the legitimate case — the
/// seven assertions below are all about PostgreSQL's `CREATE SCHEMA` / `CREATE
/// EXTENSION` output. The shim keeps every call site below byte-identical, so this
/// commit changed the ROUTE these tests take and not one thing they assert.
fn render_vendor_op(op: &Op, eff_schema: &str) -> Result<Vec<VendorStatement>, VendorError> {
    zeroship_migrate_postgres::VENDOR
        .dml
        .render_vendor_op(op, eff_schema)
}

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
