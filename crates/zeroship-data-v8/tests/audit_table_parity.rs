//! Migration provisioning and ORM audit writes must name the same table.
//! This adapter test compares their contracts through development dependencies.

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
        zeroship_data_orm::sql::mapping::validate_collection(AUDIT_TABLE).is_err(),
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
        zeroship_data_orm::sql::mapping::validate_collection(unreserved).is_ok(),
        "the control name {unreserved} is refused too, so the arm above says \
         nothing about the reserved prefix",
    );
}
