//! `__zeroship_audit_unmask` - one relation, three crates, previously nothing
//! holding them together.
//!
//! # What was unbound, and what it would have cost
//!
//! The unmask audit row is the ONLY durable record that someone read PII, PHI
//! or PCI, and the data plane is its only writer while holding no authority to
//! create it. Three crates therefore have to agree on the relation's name:
//!
//! * `zeroship_migrate_sqlite::backend::AUDIT_UNMASK_TABLE` - the dev tier's
//!   creator, called by the SQLite apply host.
//! * `zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE` - the
//!   PostgreSQL creator, called by the managed-policy apply service.
//! * `zeroship_data_orm::backend_handle::AUDIT_UNMASK_TABLE` - the WRITER.
//!
//! If a creator and the writer part company the INSERT targets a relation
//! nothing made. That is not a silent audit gap: `crud/unmask.rs` refuses to
//! return plaintext when the audit append fails, so EVERY unmask on the app
//! fails - loudly, in production, on a path no test in the tree exercised
//! without a live server.
//!
//! Until 2026-09-04 nothing compared them. Each crate pinned its own constant
//! against its own DDL, and the migration server's constant carried a comment
//! NAMING the SQLite peer - a citation, which reads like a guard and is not
//! one. This file is the guard.
//!
//! # Why a comparison and not one shared constant
//!
//! A shared constant would have to live somewhere all three can see, and there
//! is no such crate. `zeroship-data-orm` runs creator code and must not link
//! the migration engine: privilege follows the PROCESS, so the tier that
//! executes app JS does not gain a dependency on the tier that changes schema.
//! Pushing the name down into `zeroship-data-sql` (the one leaf both sides
//! already share) would put a MIGRATION-OWNED relation name in the vendor-
//! neutral schema authority, and the vendor crates do not depend on it - which
//! is exactly why `zeroship-data-sql`'s own `cross_codec_parity` /
//! `raw_column_parity` modules exist rather than a shared codec.
//!
//! So the answer is the same one those modules reached: compare, in the one
//! place all three are nameable. `zeroship-plugin-db` already dev-depends on
//! all three (`Cargo.toml`: `zeroship-data-orm`, `zeroship-migrate-server`,
//! `zeroship-migrate-sqlite`), so this costs no new dependency edge at all.
//!
//! # No database
//!
//! Every arm here is a string comparison over constants and generated DDL, so
//! this target carries no `required-features` and runs under a bare
//! `cargo test -p zeroship-plugin-db`. The live half - that a real apply leaves
//! the runtime role able to write the row - is
//! `zeroship-migrate-server`'s `apply_api_test::
//! a_real_apply_leaves_the_runtime_role_able_to_write_the_unmask_audit_row_pg`,
//! and it needs PostgreSQL. Neither replaces the other: that one proves the
//! grant, this one proves the NAME, and a name divergence would make that test
//! fail for a reason it does not name.

use zeroship_data_orm::backend_handle::AUDIT_UNMASK_TABLE as WRITER;
use zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE as POSTGRES_CREATOR;
use zeroship_migrate_sqlite::backend::AUDIT_UNMASK_TABLE as SQLITE_CREATOR;

/// The relation name, stated here as a literal.
///
/// A fourth copy on purpose, and the reason this file can catch a COORDINATED
/// rename as well as a one-sided one. Comparing the three constants to each
/// other would go green the moment somebody renamed all three - which, given
/// they sit in three crates a single change rarely spans, is the less likely
/// failure but the one that would ship silently.
const AUDIT_TABLE: &str = "__zeroship_audit_unmask";

/// The prefix that makes this name unreachable for a creator collection.
const RESERVED_PREFIX: &str = "__zeroship_";

/// The binding proper: three crates, one name.
#[test]
fn every_declaration_of_the_audit_table_name_is_the_one_this_file_states() {
    assert_eq!(
        WRITER, AUDIT_TABLE,
        "the data plane's unmask-audit INSERT target moved; it is the only writer \
         and creates nothing, so it would be INSERTing into a relation neither \
         apply host makes",
    );
    assert_eq!(
        SQLITE_CREATOR, AUDIT_TABLE,
        "the SQLite apply host's audit table name moved away from the writer's",
    );
    assert_eq!(
        POSTGRES_CREATOR, AUDIT_TABLE,
        "the PostgreSQL apply host's audit table name moved away from the writer's",
    );
}

/// The constants agreeing is necessary and not sufficient: what matters is that
/// the emitted DDL creates the relation the WRITER targets.
///
/// Both arms below drive the production generator and search its output for
/// [`WRITER`] - the data plane's constant, never the migration crate's own - so
/// a creator whose constant agrees while its DDL names something else still
/// fails. That is a real shape: both generators format the table name into
/// index names as well, and only the `CREATE TABLE` decides where a row lands.
#[test]
fn the_sqlite_apply_host_creates_the_relation_the_writer_targets() {
    // `main` is the qualifier the dev-tier apply host passes: it opens the
    // tenant's app file directly. The worker reaches the SAME physical table
    // under the `<app_id>` ATTACH alias, which is why the generator takes the
    // qualifier rather than baking one in.
    let ddl = zeroship_migrate_sqlite::backend::audit_unmask_ddl("main");
    let create = ddl
        .first()
        .expect("the SQLite generator emits a CREATE TABLE");
    assert!(
        create.contains(&format!(r#""main"."{WRITER}""#)),
        "the SQLite CREATE TABLE does not name the relation the data plane writes \
         ({WRITER}): {create}",
    );
}

#[test]
fn the_postgres_apply_host_creates_the_relation_the_writer_targets() {
    let sql = zeroship_migrate_server::provisioning::audit_unmask_table_sql("app");
    assert!(
        sql.contains(&format!(r#""app"."{WRITER}""#)),
        "the PostgreSQL CREATE TABLE does not name the relation the data plane \
         writes ({WRITER}): {sql}",
    );
}

/// Why the name is safe to hardcode at all, held against the fence that makes
/// it so.
///
/// This is the arm the three above cannot cover: they would all stay green if
/// the shared name were renamed to something a creator can also declare. The
/// relation lives in the creator's OWN schema alongside their tables, so the
/// only thing stopping a creator from declaring a colliding collection - and
/// thereby handing the worker a relation the creator controls to write its
/// audit log into - is that the schema authority refuses the `__zeroship_`
/// prefix on an inbound collection name.
#[test]
fn the_audit_relation_sits_in_a_namespace_a_creator_cannot_declare() {
    assert!(
        AUDIT_TABLE.starts_with(RESERVED_PREFIX),
        "{AUDIT_TABLE} left the reserved platform namespace {RESERVED_PREFIX}",
    );
    assert!(
        zeroship_data_sql::compile::validate_collection(AUDIT_TABLE).is_err(),
        "the schema authority now ACCEPTS {AUDIT_TABLE} as a creator collection; a \
         creator could declare the relation their own audit log is written into",
    );
    // The control, differing in one variable: the same name without the
    // reserved prefix is an ordinary collection. Without this the assertion
    // above would also pass if `validate_collection` had started refusing
    // everything.
    let unreserved = AUDIT_TABLE.trim_start_matches('_');
    assert_ne!(
        unreserved, AUDIT_TABLE,
        "the control is the same string as the subject, so it varies nothing",
    );
    assert!(
        zeroship_data_sql::compile::validate_collection(unreserved).is_ok(),
        "the control name {unreserved} is refused too, so the arm above says \
         nothing about the reserved prefix",
    );
}
