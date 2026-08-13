//! What generated types say about a table caught mid-online-rename.
//!
//! `fold.rs`'s `RenameColumn` arm flags this gap in its own words:
//!
//! > the IR rename lowers to an online expand-contract whose CONTRACT (drop the
//! > `from` column) is a SEPARATE later deploy. Between expand and contract, live
//! > PG carries BOTH the `from` and `to` columns while this fold [...] shows only
//! > `to`. [...] in the migration-first model the fold is the SOLE source of truth
//! > for gen-types - so generated types over a mid-expand migration set reflect the
//! > POST-EXPAND logical shape (final `to` name). A live mid-expand DB should be
//! > exercised e2e to confirm gen-types reads/writes resolve. No action here.
//!
//! So the authored position is: the types show only the destination, and whether
//! that is USABLE against a live mid-expand database was never checked.
//!
//! It is usable, and this file records why - by pairing what the fold generates
//! against what `online-rename-coexistence.test.ts` measured on live PostgreSQL.
//!
//! The two halves fit together in a way that is easy to get backwards. The live
//! CATALOG mid-expand says the destination is NULLABLE and carries no UNIQUE - the
//! rename transfers neither. But the dual-write trigger copies every write to the
//! source, so the SOURCE's `NOT NULL` and `UNIQUE` still reject writes made
//! through the destination name. An application therefore cannot write a NULL or a
//! duplicate through the new column, even though the catalog column permits both.
//!
//! Generated types that reported the destination as nullable and unconstrained -
//! reading the catalog literally - would tell an application it may do things the
//! database will refuse. Reporting the pre-rename constraints under the new name
//! is the accurate description of what writes will succeed, and that is what the
//! fold does.

mod support;

use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::{render_artifacts, SqlDialect};

const SCHEMA: &str = "public";

/// `users(id, display_name NOT NULL)` with a UNIQUE over the renamed column, then
/// the rename to `full_name`. This is the mid-expand migration set: the contract
/// that drops `display_name` is a later deploy that has not happened.
fn mid_expand_ops() -> Vec<Op> {
    let ir: MigrationIr = serde_json::from_str(
        r#"{
          "ir_version": 1,
          "name": "mid_expand",
          "owner_app": "app_gen_mid_expand",
          "ops": [
            {"op":"createTable","name":"users","columns":[
              {"name":"id","type":"int","nullable":false},
              {"name":"display_name","type":"text","nullable":false}
            ],
            "primaryKey":["id"],
            "constraints":[
              {"name":"users_display_name_key",
               "kind":{"kind":"unique","columns":["display_name"]}}
            ]},
            {"op":"renameColumn","table":"users","from":"display_name",
             "to":"full_name","type":"text"}
          ]
        }"#,
    )
    .expect("the mid-expand fixture parses");
    ir.ops
}

#[test]
fn generated_types_name_only_the_destination_column() {
    let artifacts = render_artifacts(
        &mid_expand_ops(),
        SqlDialect::Postgres,
        SCHEMA,
        &support::no_inject(SCHEMA),
    )
    .expect("the mid-expand set renders");

    let dts = &artifacts.env_db_ts;
    assert!(
        dts.contains("full_name: t.text().notNull()"),
        "the destination column must appear, carrying the source's nullability: {dts}"
    );

    // The authored claim: only `to`. Mid-expand the live database still carries
    // `display_name`, and an application compiled against these types must not be
    // offered a column the contract deploy is about to remove.
    //
    // The check is on the COLUMN, not on the string. A constraint NAME may still
    // contain `display_name` and legitimately does - see the next assertion.
    assert!(
        !dts.contains("display_name:"),
        "the source COLUMN must not appear: the pending contract removes it: {dts}"
    );

    // And the constraint keeps its original NAME while its columns follow the
    // rename. That is not a leak of the old column - it is the constraint's
    // identity, and PostgreSQL does not rename a constraint when a column it
    // covers is renamed, so the live catalog reads the same way. Generated types
    // that renamed it would disagree with the database.
    assert!(
        dts.contains(r#"{ name: "users_display_name_key", columns: ["full_name"] }"#),
        "the unique must keep its name and follow the rename in its columns: {dts}"
    );
}

#[test]
fn the_destination_carries_the_source_constraints_which_is_what_writes_obey() {
    let artifacts = render_artifacts(
        &mid_expand_ops(),
        SqlDialect::Postgres,
        SCHEMA,
        &support::no_inject(SCHEMA),
    )
    .expect("the mid-expand set renders");
    let dts = &artifacts.env_db_ts;

    // A pure rename keeps type and nullability and only changes the NAME, so the
    // destination is reported NOT NULL. That disagrees with the mid-expand
    // CATALOG, where the destination column is nullable - and agrees with the
    // BEHAVIOUR, because the dual-write trigger sends every write to the source,
    // whose NOT NULL rejects it. `online-rename-coexistence.test.ts` measures that
    // rejection against live PostgreSQL.
    //
    // Asserting the absence of a nullable marker is the only way to state this in
    // the generated surface, so the assertion is written to name WHY rather than
    // to match a spelling that may change.
    assert!(
        !dts.contains("full_name?"),
        "the destination must not be reported optional: a NULL written through it \
         is rejected by the source's NOT NULL during coexistence: {dts}"
    );

    // The UNIQUE follows the rename too. Live mid-expand it still stands on
    // `display_name`, and a duplicate written through `full_name` is refused by it -
    // so naming the constraint over the destination describes the writes that
    // succeed rather than the catalog row.
    let runtime = &artifacts.runtime_json;
    assert!(
        runtime.contains("full_name"),
        "the runtime descriptor must carry the destination column: {runtime}"
    );
    assert!(
        !runtime.contains(r#""display_name""#),
        "and must not carry the source COLUMN the contract deploy removes: {runtime}"
    );
}
