//! Bindings on schema `main` address the file the backend opened.
//!
//! Every other binding addresses its app's own file, attached under the app id.
//! The pair below differs only in the binding's schema.

use std::path::Path;

use crate::binding::DbBinding;
use crate::encryption::ProjectKeySource;
use crate::error::DbError;
use crate::orm::{Database, Output};
use crate::schema::Schema;
use crate::sql::SchemaName;
use crate::value::{value, Value};

const MAIN: &str = "main";
const APP: &str = "app_attached";

/// Every per-app database file in `directory`.
fn app_files(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .expect("read the database directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.starts_with("zs-") && name.ends_with(".sqlite"))
        .collect();
    names.sort();
    names
}

fn id_field() -> Value {
    value!({"type":"string", "primaryKey":true, "required":true})
}

/// `notes` records no protection. The catalog records `people.ssn` as masked,
/// and this descriptor omits the mask, so writes to `people` must be refused.
fn schema() -> Schema {
    Schema::from_collections(vec![
        (
            "notes".into(),
            value!({"id": id_field(), "title": {"type":"string", "required":true}}),
        ),
        (
            "people".into(),
            value!({"id": id_field(), "ssn": {"type":"string"}}),
        ),
    ])
    .expect("fixture schema")
}

/// Create the fixture tables in `file`, opened as its own `main`.
fn create_tables(file: &Path) {
    let raw = crate::sql::mapping::raw_column_name("ssn");
    rusqlite::Connection::open(file)
        .expect("open the fixture file")
        .execute_batch(&format!(
            r#"CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT NOT NULL);
               CREATE TABLE people (
                   id TEXT PRIMARY KEY,
                   "{raw}" TEXT,
                   "ssn" TEXT /* zero-migrate:mask:kind=last4,classification=spi */
               );"#
        ))
        .expect("create the fixture tables");
}

/// Titles stored in `file`'s own `notes` table, read without the ORM.
fn stored_titles(file: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(file).expect("open the stored file");
    let mut statement = connection
        .prepare("SELECT title FROM notes ORDER BY id")
        .expect("the stored file holds notes");
    statement
        .query_map([], |row| row.get(0))
        .expect("read stored notes")
        .collect::<rusqlite::Result<_>>()
        .expect("stored titles")
}

async fn connect(binding: DbBinding, file: &Path) -> Database {
    crate::tests::fixtures::reset_engine();
    Database::connect(
        binding,
        crate::ConnectOptions::new(
            format!("sqlite:{}", file.display()),
            ProjectKeySource::unavailable(),
        ),
        schema(),
    )
    .await
    .expect("connect the fixture database")
}

/// Autocommit and transaction writes, then a read, through `database`.
async fn write_notes(database: &Database) -> Result<Vec<Value>, DbError> {
    let notes = database.collection("notes")?;
    notes.insert(value!({"id":"n1", "title":"first"})).await?;
    database
        .transaction(|tx| async move {
            tx.collection("notes")?
                .insert(value!({"id":"n2", "title":"second"}))
                .await?;
            Ok::<_, DbError>(())
        })
        .await?;
    let Output::Rows { rows, .. } = notes
        .find(value!({}), value!({"orderBy": {"id": 1}}))
        .await?
    else {
        return Err(DbError::internal("find returned a count"));
    };
    Ok(rows
        .into_iter()
        .map(|row| row.get("title").cloned().unwrap_or(Value::Null))
        .collect())
}

fn main_binding() -> DbBinding {
    DbBinding::new(
        "platform",
        "platform-deploy",
        SchemaName::new(MAIN).expect("main is a schema name"),
    )
}

#[compio::test]
async fn a_main_schema_binding_attaches_no_app_file() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("platform.sqlite");
    create_tables(&file);
    let database = connect(main_binding(), &file).await;

    let titles = write_notes(&database).await.expect("main binding writes");
    assert_eq!(titles, vec![Value::from("first"), Value::from("second")]);
    assert_eq!(
        app_files(directory.path()),
        Vec::<String>::new(),
        "a binding on schema main must not attach an app file"
    );
    assert_eq!(
        stored_titles(&file),
        ["first", "second"],
        "the rows land in the file the backend opened"
    );
}

/// Control for the arm above: the same work through an app binding addresses
/// the app's attached file, not the file the backend opened.
#[compio::test]
async fn an_app_binding_addresses_its_attached_file() {
    let directory = tempfile::tempdir().unwrap();
    let app_file = directory.path().join(format!("zs-{APP}.sqlite"));
    create_tables(&app_file);
    let opened = directory.path().join("control.sqlite");
    let database = connect(
        DbBinding::new(APP, "app-deploy", SchemaName::new(APP).unwrap()),
        &opened,
    )
    .await;

    let titles = write_notes(&database).await.expect("app binding writes");
    assert_eq!(titles, vec![Value::from("first"), Value::from("second")]);
    assert_eq!(
        app_files(directory.path()),
        vec![format!("zs-{APP}.sqlite")]
    );
    assert_eq!(stored_titles(&app_file), ["first", "second"]);
    let opened_tables: i64 = rusqlite::Connection::open(&opened)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'notes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(opened_tables, 0, "the opened file holds no app table");
}

/// The protection floor of a binding on schema `main` comes from the catalog
/// of the file the backend opened, so a descriptor that drops a recorded mask is
/// refused there too.
#[compio::test]
async fn a_main_schema_binding_reads_its_protection_floor_from_the_opened_file() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("platform.sqlite");
    create_tables(&file);
    let database = connect(main_binding(), &file).await;

    let refused = database
        .collection("people")
        .unwrap()
        .insert(value!({"id":"p1", "ssn":"123-45-6789"}))
        .await;
    assert!(
        matches!(
            refused,
            Err(DbError::Configuration {
                code: "protection_removed_from_descriptor",
                ..
            })
        ),
        "the recorded mask must be enforced: {refused:?}"
    );
    database
        .collection("notes")
        .unwrap()
        .insert(value!({"id":"n1", "title":"unprotected"}))
        .await
        .expect("a table recording no protection accepts the same kind of write");
}
