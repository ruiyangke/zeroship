//! Where a host's journal storage opens the journal.

use super::*;
use crate::service::{
    models,
    store::{journal_binding, HostStorage, JOURNAL_SCHEMA},
};
use zeroship_data_orm::connection::ConnectionFactory;

/// `zeroship serve` and the production service both open their journal
/// through [`HostStorage`], so this is where the one binding they share is
/// decided. The store it opens writes through the production journal's
/// binding, and on `SQLite`, where that binding's schema names the attached
/// file, the installed journal lands in the production journal's file and in
/// no other.
#[compio::test]
async fn host_storage_opens_the_journal_on_the_production_binding() {
    let directory = tempfile::tempdir().unwrap();
    let session = directory.path().join("session.sqlite");
    let store = HostStorage::new(
        ConnectionFactory::for_platform_url(&format!("sqlite:{}", session.display())).unwrap(),
    )
    .open()
    .await
    .unwrap();

    assert_eq!(
        store.binding,
        journal_binding(),
        "the opened store must write through the production journal's binding"
    );

    schema::initialize_local(&store).await.unwrap();
    // Database files only: the journal runs in WAL, so its `-wal` and `-shm`
    // siblings are the same database rather than another place it is kept.
    let mut files = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("zs-") && name.ends_with(".sqlite"))
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(
        files,
        [format!("zs-{JOURNAL_SCHEMA}.sqlite")],
        "the journal must be kept in the production journal's file and in no other"
    );
    let journal = rusqlite::Connection::open(directory.path().join(&files[0])).unwrap();
    assert_eq!(
        journal
            .query_row(
                "SELECT version FROM __zeroship_workflow_schema_version WHERE id='workflow'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        i64::from(zeroship_workflow_schema::VERSION),
        "the installed journal's stamp must be in that file"
    );
}

/// [`HostStorage`] opens its store without a project key, which is sound only
/// while the journal declares no encrypted column. A journal field that gained
/// one would be refused at its first read with `column_key_not_configured`;
/// this names the premise where the schema is declared instead.
#[test]
fn the_journal_declares_no_column_a_project_key_would_decrypt() {
    let schema = models::journal::schema();
    let mut columns = 0;
    for (collection, fields) in schema.collections() {
        for (name, column) in fields.fields() {
            columns += 1;
            assert!(
                !column.encrypted,
                "{collection}.{name} is encrypted, so HostStorage must supply the project key"
            );
        }
    }
    assert!(
        columns > 0,
        "the journal schema must declare columns for this check to judge"
    );
}
