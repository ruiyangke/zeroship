//! Usage attribution for creator bindings beside bindings the host does not meter.
use super::fixtures::*;
use crate::tests::fixtures::parity;
use std::collections::BTreeMap;
use std::sync::Arc;
use zeroship_core::AppId;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::encryption::ProjectKeySource;
use zeroship_data_orm::error::DbError;
use zeroship_data_orm::orm::Database;
use zeroship_data_orm::schema::Schema;
use zeroship_data_orm::sql::SchemaName;
use zeroship_data_orm::value::{value, Value};
use zeroship_data_orm::ConnectOptions;

const NOTES: &str = "notes";

fn notes_source() -> SqliteRuntimeSource {
    sqlite_runtime_source(
        NOTES,
        &value!({"title": {"type": "string", "required": true}}),
        r#"
        const _procedures = {
            async write() {
                const notes = env.db.collection(COLLECTION);
                try {
                    await notes.insert({title: 'metered'});
                    const rows = await notes.find({}, {});
                    return {titles: rows.map(row => row.title)};
                } catch (error) {
                    return {refused: error.code};
                }
            },
        };
        "#,
    )
}

fn notes_ddl(database: &str) -> String {
    format!("CREATE TABLE \"{database}\".{NOTES} ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL);")
}

/// Usage recorded for `app`, keyed by metric.
fn usage_for(
    events: &[zeroship_core::usage_event::UsageEvent],
    app: &AppId,
) -> BTreeMap<String, u64> {
    events
        .iter()
        .filter(|event| event.subject.app.as_ref() == Some(app))
        .map(|event| (event.meter.clone(), event.value))
        .collect()
}

/// Write and read through a platform binding the host attached no sink to.
fn platform_round_trip(directory: &std::path::Path) -> Result<Vec<Value>, DbError> {
    const PLATFORM: &str = "platform";
    crate::tests::fixtures::tables::create_sqlite_table(
        directory,
        PLATFORM,
        &format!("CREATE TABLE \"{PLATFORM}\".{NOTES} (id TEXT PRIMARY KEY, title TEXT NOT NULL);"),
    );
    let url = format!("sqlite:{}", directory.join("platform.sqlite").display());
    parity::block_on(async move {
        let database = Database::connect(
            DbBinding::platform(PLATFORM, "platform-deploy", SchemaName::new(PLATFORM)?),
            ConnectOptions::new(url, ProjectKeySource::unavailable()),
            Schema::from_collections(vec![(
                NOTES.into(),
                value!({
                    "id": {"type": "string", "primaryKey": true, "required": true},
                    "title": {"type": "string", "required": true}
                }),
            )])?,
        )
        .await?;
        let notes = database.collection(NOTES)?;
        notes
            .insert(value!({"id": "n1", "title": "platform"}))
            .await?;
        let zeroship_data_orm::orm::Output::Rows { rows, .. } =
            notes.find(value!({}), value!({})).await?
        else {
            return Err(DbError::internal("find returned a count"));
        };
        Ok(rows
            .into_iter()
            .map(|row| row.get("title").cloned().unwrap_or(Value::Null))
            .collect())
    })
}

/// A creator isolate that registers its meter on a thread does not make that
/// meter the attribution for other bindings the thread serves.
#[test]
fn a_creator_meter_on_the_thread_leaves_platform_bindings_unmetered() {
    crate::tests::fixtures::reset_context();
    let creator_dir = tempfile::tempdir().unwrap();
    apply_schema_ahead_of_runtime(
        &creator_dir,
        &notes_ddl(&crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID)),
    );
    let meter = Arc::new(zeroship_metering::Meter::new());
    let source = notes_source();

    let (status, body) = parity::dispatch_zs_metered(
        &parity::sqlite_url(&creator_dir),
        &source.source,
        "write",
        LOCAL_DEV_APP_ID,
        &source.descriptor,
        Some(Arc::clone(&meter)),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, value!({"json": {"titles": ["metered"]}}));
    let creator = AppId::parse(LOCAL_DEV_APP_ID).unwrap();
    let events = meter.drain();
    assert_eq!(
        usage_for(&events, &creator),
        BTreeMap::from([
            ("db_reads".to_owned(), 1),
            ("db_rows_written".to_owned(), 1),
            ("db_writes".to_owned(), 1),
        ]),
        "the creator binding records each operation once: {events:?}"
    );

    let platform_dir = tempfile::tempdir().unwrap();
    let titles = platform_round_trip(platform_dir.path()).expect(
        "a binding with no usage sink must not be attributed to the thread's creator meter",
    );
    assert_eq!(titles, vec![Value::from("platform")]);
    let unattributed = meter.drain();
    assert!(
        unattributed.is_empty(),
        "a binding without a usage sink records nothing: {unattributed:?}"
    );
}

/// A creator binding whose app id cannot be attributed is refused rather than
/// served unmetered, beside the attributable control above.
#[test]
fn a_metered_creator_binding_without_a_valid_app_id_is_refused() {
    const UNATTRIBUTABLE: &str = "app_not_a_typed_id";
    assert!(
        AppId::parse(UNATTRIBUTABLE).is_err(),
        "fixture must not be an app id"
    );
    assert!(
        SchemaName::new(UNATTRIBUTABLE).is_ok(),
        "fixture must still name a schema"
    );
    crate::tests::fixtures::reset_context();
    let dir = tempfile::tempdir().unwrap();
    let alias = crate::tests::fixtures::harness_alias(UNATTRIBUTABLE);
    crate::tests::fixtures::tables::create_sqlite_table(dir.path(), &alias, &notes_ddl(&alias));
    let meter = Arc::new(zeroship_metering::Meter::new());
    let source = notes_source();

    let (status, body) = parity::dispatch_zs_metered(
        &parity::sqlite_url(&dir),
        &source.source,
        "write",
        UNATTRIBUTABLE,
        &source.descriptor,
        Some(Arc::clone(&meter)),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, value!({"json": {"refused": "invalid_meter_app_id"}}));
    let recorded = meter.drain();
    assert!(
        recorded.is_empty(),
        "a refused operation records nothing: {recorded:?}"
    );

    let (status, body) = parity::dispatch_zs_metered(
        &parity::sqlite_url(&dir),
        &source.source,
        "write",
        UNATTRIBUTABLE,
        &source.descriptor,
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        value!({"json": {"titles": ["metered"]}}),
        "the same binding is served when the host meters nothing"
    );
}
