use super::*;
use std::collections::{BTreeMap, BTreeSet};
use zeroship_data_orm::{sql::compile::SqlDialect, Value};

#[compio::test]
async fn sqlite_model_metadata_matches_the_migrated_journal() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    assert_metadata(&store).await;
}

#[compio::test]
async fn postgres_model_metadata_matches_the_migrated_journal() {
    let fixture = PostgresFixture::start().await;
    assert_metadata(&fixture.store).await;
}

#[compio::test]
async fn sqlite_bounded_journal_writes_use_the_complete_key() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    assert_compound_writes(store).await;
}

#[compio::test]
async fn postgres_bounded_journal_writes_use_the_complete_key() {
    let fixture = PostgresFixture::start().await;
    assert_compound_writes(fixture.store.clone()).await;
}

async fn assert_compound_writes(store: OrmStore) {
    use zeroship_data_orm::{sql::compile, value};
    let store = Rc::new(store);
    let service = WorkflowService::open(store.clone(), Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    let a = AppId::mint();
    let b = AppId::mint();
    for app in [&a, &b] {
        service
            .register_app(app, configured_policy(1, AppPolicy::default()))
            .await
            .unwrap();
    }
    let descriptor: Value = serde_json::from_str(schema::RUNTIME_DESCRIPTOR).unwrap();
    let collection = "__zeroship_workflow_topics";
    let fields = &descriptor["collections"][collection]["fields"];
    let namespace = super::super::store::SchemaName::new(
        store
            .backend
            .namespace(store.binding.app_id(), store.binding.schema()),
    )
    .unwrap();
    let dialect = store.backend.dialect();
    let mut tx = store.begin().await.unwrap();
    for (app, topic) in [(&a, "shared"), (&b, "shared"), (&a, "other")] {
        let query = compile::build_insert_with_dialect(
            &namespace,
            collection,
            fields,
            &value!({"app_id":app.as_str(), "topic":topic, "signal_epoch":0}),
            dialect,
        )
        .unwrap();
        assert_eq!(tx.query(query.sql(), query.params()).await.unwrap().len(), 1);
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    let update = compile::build_update_one_with_dialect(
        &namespace,
        collection,
        fields,
        &value!({"app_id":a.as_str(), "topic":"shared"}),
        &value!({"signal_epoch":1}),
        dialect,
    )
    .unwrap();
    let updated = tx.query(update.sql(), update.params()).await.unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].text("app_id").unwrap(), a.as_str());
    let rows = tx
        .query(
            &format!(
                "SELECT app_id,topic,signal_epoch FROM {}",
                tx.table("topics")
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    for row in rows {
        let expected = i64::from(
            row.text("app_id").unwrap() == a.as_str() && row.text("topic").unwrap() == "shared",
        );
        assert_eq!(row.integer("signal_epoch").unwrap(), expected);
    }
    tx.commit().await.unwrap();

    // A filter matching different apps must still obey the single-row bound.
    let delete = compile::build_delete_one_with_dialect(
        &namespace,
        collection,
        fields,
        &value!({"topic":"shared"}),
        dialect,
    )
    .unwrap();
    let mut tx = store.begin().await.unwrap();
    let deleted = tx.query(delete.sql(), delete.params()).await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].text("topic").unwrap(), "shared");
    tx.commit().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let count = tx
        .query(
            &format!("SELECT COUNT(*) AS total FROM {}", tx.table("topics")),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(count[0].integer("total").unwrap(), 2);
    tx.commit().await.unwrap();
}

async fn assert_metadata(store: &OrmStore) {
    let descriptor: Value = serde_json::from_str(schema::RUNTIME_DESCRIPTOR).unwrap();
    let collections = descriptor["collections"].as_object().unwrap();
    assert!(!collections.is_empty());
    let namespace = store
        .backend
        .namespace(store.binding.app_id(), store.binding.schema());
    let sql = match store.backend.dialect() {
        SqlDialect::Postgres => r#"
            SELECT c.relname AS table_name, a.attname AS column_name,
                   format_type(a.atttypid, a.atttypmod) AS storage_type,
                   a.attnotnull AS required,
                   COALESCE(a.attnum = ANY(i.indkey), false) AS primary_key
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
            LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
            WHERE n.nspname = $1 AND c.relkind = 'r'
        "#
        .to_owned(),
        SqlDialect::Sqlite => format!(
            r#"
            SELECT m.name AS table_name, c.name AS column_name, lower(c.type) AS storage_type,
                   c."notnull" AS required, c.pk > 0 AS primary_key
            FROM {}.sqlite_master m JOIN pragma_table_info(m.name, $1) c
            WHERE m.type = 'table'
        "#,
            zeroship_data_orm::sql::compile::quote_ident(namespace)
        ),
    };
    let rows = store
        .backend
        .query(
            store.binding.app_id(),
            store.binding.schema(),
            &sql,
            &[namespace.into()],
        )
        .await
        .unwrap();
    let mut actual: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for row in rows {
        let table = row["table_name"].as_str().unwrap().to_owned();
        let column = row["column_name"].as_str().unwrap().to_owned();
        assert!(actual
            .entry(table)
            .or_default()
            .insert(column, row)
            .is_none());
    }
    assert_eq!(
        actual.keys().collect::<BTreeSet<_>>(),
        collections.keys().collect()
    );
    for (table, collection) in collections {
        let fields = collection["fields"].as_object().unwrap();
        let actual = &actual[table];
        assert!(!fields.is_empty(), "{table}");
        assert_eq!(
            actual.keys().collect::<BTreeSet<_>>(),
            fields.keys().collect(),
            "{table}"
        );
        let mut primary = BTreeSet::new();
        for (name, field) in fields {
            let column = &actual[name];
            let is_true =
                |value: &Value| value.as_bool() == Some(true) || value.as_i64() == Some(1);
            assert_eq!(
                is_true(&column["required"]),
                is_true(&field["required"]),
                "{table}.{name}"
            );
            assert_eq!(
                is_true(&column["primary_key"]),
                is_true(&field["primaryKey"]),
                "{table}.{name}"
            );
            if is_true(&field["primaryKey"]) {
                primary.insert(name);
            }
            assert_eq!(
                field["storage"]["valueColumn"].as_str(),
                Some(name.as_str())
            );
            let expected_type = match (field["type"].as_str().unwrap(), store.backend.dialect()) {
                ("string", _) => "text",
                ("bigInt", SqlDialect::Postgres) => "bigint",
                ("bigInt", SqlDialect::Sqlite) => "integer",
                (other, _) => panic!("unverified journal field type {other}"),
            };
            assert_eq!(
                column["storage_type"].as_str(),
                Some(expected_type),
                "{table}.{name}"
            );
        }
        assert!(!primary.is_empty(), "{table} requires a key");
    }
}
