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

/// The binding proper: three crates, one name.
#[test]
fn every_declaration_of_the_audit_table_name_is_the_one_this_file_states() {
    assert_eq!(
        WRITER, AUDIT_TABLE,
        "the ORM audit INSERT target must match the relation created by both apply hosts",
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

/// Tables in the bound creator schema use the same ORM path regardless of name.
#[test]
fn the_audit_relation_is_addressable_as_an_ordinary_collection() {
    assert!(
        zeroship_data_orm::sql::mapping::validate_collection(AUDIT_TABLE).is_ok(),
        "the bound creator schema must expose {AUDIT_TABLE} through the ordinary ORM path",
    );
}
