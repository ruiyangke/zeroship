//! What the declarative differ emits for a MySQL column type / nullability change.
//!
//! The existing-table branch of `diff` special-cases SQLite and fails closed there,
//! because SQLite has no `ALTER COLUMN` at all and its rebuild detector is supposed
//! to have caught the change earlier. MySQL has no such branch: it falls through to
//! the same renderers PostgreSQL uses, which emit `ALTER COLUMN ... TYPE ... USING
//! ...::...` and `ALTER COLUMN ... SET NOT NULL`. MySQL accepts neither spelling -
//! it uses `MODIFY COLUMN`, and has no `USING` cast - and quotes identifiers with
//! backticks rather than double quotes.
//!
//! This measures what comes out rather than asserting from the renderer's source, so
//! the record is the emitted string. MEASURED, with the MySQL author:
//!
//!     ALTER TABLE "app"."accounts" ALTER COLUMN "nickname" TYPE INT USING "nickname"::INT
//!     ALTER TABLE "app"."accounts" ALTER COLUMN "nickname" SET NOT NULL
//!
//! Note the TABLE is double-quoted as well, not just the column: the statement is
//! PostgreSQL end to end rather than a mix of the two dialects. Reading the renderer
//! alone suggested otherwise, because the qualifier helper does have a MySQL arm -
//! which is a reason to measure rather than to reason about it.
//!
//! It is a pin on a KNOWN DEFECT, not an endorsement: the assertions below say what
//! is emitted today and name what MySQL would need instead, so whoever fixes it sees
//! the test fail and reads why.
//!
//! Note what this is NOT. Refusing this shape would not reject a migration that works
//! today, because the emitted statement cannot execute on MySQL at all. That is why
//! the usual objection to tightening a gate does not apply here.

mod support;

use std::collections::HashMap;

use zero_migrate::model::snapshot::SchemaSnapshot;
use zero_migrate::render::declarative::{
    desired_snapshot_for_dialect, CollectionDescriptor, DeclarativeAuthor, FieldDescriptor,
};
use zero_migrate::SqlDialect;

const PROJECT: &str = "app";
const APP: &str = "app_test";

fn descriptor(ty: &str, required: bool) -> CollectionDescriptor {
    CollectionDescriptor {
        name: "accounts".to_string(),
        owner_app: APP.to_string(),
        fields: vec![FieldDescriptor {
            name: "nickname".to_string(),
            ty: ty.to_string(),
            required,
            ..Default::default()
        }],
        indexes: vec![],
        runtime_options: Default::default(),
    }
}

/// The live snapshot for a table that already exists with `ty`, built through the
/// SAME desired-schema construction the differ uses, so the two sides differ in
/// exactly the facet under test rather than in how they were assembled.
fn live_with(
    ty: &str,
    required: bool,
    effective: &zero_migrate_policy::EffectivePolicy,
) -> SchemaSnapshot {
    let desired = desired_snapshot_for_dialect(
        PROJECT,
        &[descriptor(ty, required)],
        SqlDialect::Mysql,
        effective,
    )
    .expect("live-side desired snapshot");
    desired.snapshot
}

fn mysql_author() -> DeclarativeAuthor {
    DeclarativeAuthor::new_for_dialect(PROJECT, APP, SqlDialect::Mysql)
}

#[test]
fn a_mysql_column_type_change_emits_postgres_alter_column_syntax() {
    let effective = support::confined_charter();
    let desired = desired_snapshot_for_dialect(
        PROJECT,
        &[descriptor("integer", true)],
        SqlDialect::Mysql,
        &effective,
    )
    .expect("desired snapshot");
    let live = live_with("string", true, &effective);

    let plan = mysql_author()
        .diff(&desired, &live, &HashMap::new(), &[], &effective)
        .expect("the differ accepts a MySQL type change rather than refusing it");

    let alter = plan
        .all_migrations()
        .into_iter()
        .find(|m| m.name.starts_with("alter_column_type_"))
        .expect("a type change emits an alter-column migration on MySQL");

    // MEASURED, and wrong for this dialect on three counts.
    assert!(
        alter.up.contains("ALTER COLUMN"),
        "today's emitted SQL, for the record: {}",
        alter.up
    );
    assert!(
        alter.up.contains("TYPE ") && alter.up.contains("USING "),
        "MySQL has no `ALTER COLUMN ... TYPE ... USING`; it needs MODIFY COLUMN and no \
         cast clause. Emitted: {}",
        alter.up
    );
    assert!(
        alter.up.contains('"'),
        "MySQL quotes identifiers with backticks, not double quotes. Emitted: {}",
        alter.up
    );
    assert!(
        !alter.up.contains("MODIFY COLUMN"),
        "when this starts failing the renderer has been taught MySQL syntax, which is \
         the fix this pin is waiting for: {}",
        alter.up
    );
}

#[test]
fn a_mysql_nullability_change_emits_postgres_set_not_null_syntax() {
    let effective = support::confined_charter();
    let desired = desired_snapshot_for_dialect(
        PROJECT,
        &[descriptor("string", true)],
        SqlDialect::Mysql,
        &effective,
    )
    .expect("desired snapshot");
    let live = live_with("string", false, &effective);

    let plan = mysql_author()
        .diff(&desired, &live, &HashMap::new(), &[], &effective)
        .expect("the differ accepts a MySQL nullability change rather than refusing it");

    let alter = plan
        .all_migrations()
        .into_iter()
        .find(|m| m.name.starts_with("alter_column_null_"))
        .or_else(|| {
            plan.all_migrations()
                .into_iter()
                .find(|m| m.up.contains("NOT NULL") && m.up.contains("ALTER COLUMN"))
        })
        .expect("a nullability change emits an alter-column migration on MySQL");

    assert!(
        alter.up.contains("SET NOT NULL") || alter.up.contains("DROP NOT NULL"),
        "today's emitted SQL, for the record: {}",
        alter.up
    );
    assert!(
        !alter.up.contains("MODIFY COLUMN"),
        "when this starts failing the renderer has been taught MySQL syntax: {}",
        alter.up
    );
}
