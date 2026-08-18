//! A COMMENT target that names the project schema in another casing must render the
//! canonical schema, not the casing the author wrote.
//!
//! `SchemaScope::permits` matches case-INsensitively, so a Confined project on `app`
//! accepts a qualifier of `APP`. The render seam quotes byte-verbatim, and PostgreSQL
//! treats `"APP"` and `"app"` as different schemas. So an op that renders the author's
//! casing lands somewhere the confinement gate never blessed - the gate decided about
//! `app` and the statement addressed `APP`.
//!
//! `IrAuthor::effective_schema` is the canonicalization that keeps the two in step, and
//! the comment renderer used to re-read the target's own schema instead of the
//! already-canonicalized value handed to it.

use crate::support;

use std::collections::BTreeSet;
use zero_migrate::render::lower::IrAuthor;
use zero_migrate::{CommentTarget, LiveSchema, MigrationIr, Op, SqlDialect};

const SCHEMA: &str = "app";
const OWNER: &str = "app_test";

fn ir(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: 1,
        name: "comments".into(),
        owner_app: OWNER.into(),
        ops,
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn comment_sql(schema: Option<&str>) -> String {
    let author = IrAuthor::new(
        SCHEMA,
        OWNER,
        SqlDialect::Postgres,
        &support::no_inject("app"),
    );
    let ops = vec![Op::Comment {
        target: CommentTarget::Table {
            schema: schema.map(str::to_string),
            name: "accounts".into(),
        },
        comment: Some("hello".into()),
    }];
    let migrations = author
        .lower(&ir(ops), &LiveSchema::from(&BTreeSet::new()))
        .expect("a comment on the project's own table lowers");
    migrations
        .first()
        .expect("the comment produces one migration")
        .up
        .clone()
}

#[test]
fn a_case_variant_target_schema_renders_canonically() {
    assert_eq!(
        comment_sql(Some("APP")),
        comment_sql(Some("app")),
        "a target qualified APP under project app must render the same statement as app"
    );
}

#[test]
fn the_canonical_and_absent_forms_already_agree() {
    // The control. Both were correct before, so a renderer that simply ignored the
    // target's schema entirely would pass the test above while breaking these.
    assert_eq!(
        comment_sql(Some("app")),
        r#"COMMENT ON TABLE "app"."accounts" IS 'hello'"#
    );
    assert_eq!(
        comment_sql(None),
        r#"COMMENT ON TABLE "app"."accounts" IS 'hello'"#
    );
}
