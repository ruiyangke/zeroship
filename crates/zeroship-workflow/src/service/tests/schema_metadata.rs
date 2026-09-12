use super::*;
use std::collections::{BTreeMap, BTreeSet};
use zeroship_data_orm::{
    orm::{Database, Output},
    sql::registration::{POSTGRES_FAMILY, SQLITE_FAMILY},
    value, OrmContext, Value,
};

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
async fn sqlite_journal_ids_and_scoped_unique_keys_bound_writes() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    assert_scoped_writes(store).await;
}

#[compio::test]
async fn postgres_journal_ids_and_scoped_unique_keys_bound_writes() {
    let fixture = PostgresFixture::start().await;
    assert_scoped_writes(fixture.store.clone()).await;
}

async fn assert_scoped_writes(store: OrmStore) {
    let context = OrmContext::new();
    context
        .with(|| super::super::models::install(&store.binding))
        .unwrap();
    let database = Database::new(context, store.binding.clone(), store.backend.clone());
    let a = AppId::mint();
    let b = AppId::mint();
    for app in [&a, &b] {
        database
            .collection("__zeroship_workflow_app_state")
            .unwrap()
            .insert(value!({"id":app.as_str(), "app_id":app.as_str(), "signal_epoch":0}))
            .await
            .unwrap();
    }
    let topics = database.collection("__zeroship_workflow_topics").unwrap();
    let mut identities = Vec::new();
    for (app, topic) in [(&a, "shared"), (&b, "shared"), (&a, "other")] {
        let id = typed_id::generate("wft");
        let inserted = topics
            .insert(
                value!({"id":id.clone(), "app_id":app.as_str(), "topic":topic, "signal_epoch":0}),
            )
            .await
            .unwrap();
        assert_eq!(rows(inserted).len(), 1);
        identities.push(id);
    }

    for document in [
        value!({"id":identities[0].clone(), "app_id":b.as_str(), "topic":"another", "signal_epoch":0}),
        value!({"id":typed_id::generate("wft"), "app_id":a.as_str(), "topic":"shared", "signal_epoch":0}),
        value!({"app_id":a.as_str(), "topic":"missing-id", "signal_epoch":0}),
        value!({"id":typed_id::generate("wft"), "app_id":AppId::mint().as_str(), "topic":"orphan", "signal_epoch":0}),
    ] {
        assert!(topics.insert(document).await.is_err());
    }

    let updated = rows(
        topics
            .update(
                value!({"app_id":a.as_str(), "topic":"shared"}),
                value!({"signal_epoch":1}),
            )
            .await
            .unwrap(),
    );
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0]["id"].as_str(), Some(identities[0].as_str()));
    assert_eq!(updated[0]["app_id"].as_str(), Some(a.as_str()));
    let records = rows(topics.find(value!({}), value!({})).await.unwrap());
    assert_eq!(records.len(), 3);
    for row in records {
        let expected = i64::from(
            row["app_id"].as_str() == Some(a.as_str()) && row["topic"].as_str() == Some("shared"),
        );
        assert_eq!(row["signal_epoch"].as_i64(), Some(expected));
    }

    // A filter matching different apps must still obey the single-row bound.
    let deleted = rows(topics.delete(value!({"topic":"shared"})).await.unwrap());
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0]["topic"].as_str(), Some("shared"));
    assert!(matches!(
        topics.count(value!({}), value!({})).await.unwrap(),
        Output::Count(2)
    ));

    let requests = database.collection("__zeroship_workflow_requests").unwrap();
    let request = RequestId::mint();
    for app in [&a, &b] {
        requests
            .insert(
                value!({"id":typed_id::generate("wfr"), "app_id":app.as_str(),
            "request_id":request.as_str(), "operation":"start", "digest":"same-input",
            "result":"null", "expires_at":1}),
            )
            .await
            .unwrap();
    }
    assert!(requests
        .insert(value!({"id":typed_id::generate("wfr"), "app_id":a.as_str(),
        "request_id":request.as_str(), "operation":"start", "digest":"same-input",
        "result":"null", "expires_at":1}))
        .await
        .is_err());
    assert!(matches!(
        requests
            .count(value!({"request_id":request.as_str()}), value!({}))
            .await
            .unwrap(),
        Output::Count(2)
    ));
}

fn rows(output: Output) -> Vec<Value> {
    let Output::Rows { rows, .. } = output else {
        panic!("expected returned journal rows");
    };
    rows
}

async fn assert_metadata(store: &OrmStore) {
    let descriptor: Value = serde_json::from_str(schema::RUNTIME_DESCRIPTOR).unwrap();
    let collections = descriptor["collections"].as_object().unwrap();
    assert!(!collections.is_empty());
    let namespace = store
        .backend
        .namespace(store.binding.app_id(), store.binding.schema());
    let family = store.backend.sql_registration().family();
    let sql = match family {
        POSTGRES_FAMILY => r#"
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
        SQLITE_FAMILY => format!(
            r#"
            SELECT m.name AS table_name, c.name AS column_name, lower(c.type) AS storage_type,
                   c."notnull" AS required, c.pk > 0 AS primary_key
            FROM {}.sqlite_master m JOIN pragma_table_info(m.name, $1) c
            WHERE m.type = 'table'
        "#,
            zeroship_data_orm::sql::mapping::quote_ident(namespace)
        ),
        other => panic!("unsupported journal SQL family: {other:?}"),
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
                primary.insert(name.as_str());
            }
            assert_eq!(
                field["storage"]["valueColumn"].as_str(),
                Some(name.as_str())
            );
            let expected_type = match (field["type"].as_str().unwrap(), family) {
                ("string", _) => "text",
                ("bigInt", POSTGRES_FAMILY) => "bigint",
                ("bigInt", SQLITE_FAMILY) => "integer",
                (other, _) => panic!("unverified journal field type {other}"),
            };
            assert_eq!(
                column["storage_type"].as_str(),
                Some(expected_type),
                "{table}.{name}"
            );
        }
        assert_eq!(primary, BTreeSet::from(["id"]), "{table}");
    }
    assert_unique_metadata(store, &descriptor).await;
}

async fn assert_unique_metadata(store: &OrmStore, descriptor: &Value) {
    let namespace = store
        .backend
        .namespace(store.binding.app_id(), store.binding.schema());
    let sql = match store.backend.sql_registration().family() {
        POSTGRES_FAMILY => r#"
            SELECT t.relname AS table_name, i.relname AS index_name, a.attname AS column_name
            FROM pg_index x
            JOIN pg_class t ON t.oid=x.indrelid
            JOIN pg_namespace n ON n.oid=t.relnamespace
            JOIN pg_class i ON i.oid=x.indexrelid
            JOIN LATERAL unnest(x.indkey) WITH ORDINALITY k(attnum, position) ON true
            JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=k.attnum
            WHERE n.nspname=$1 AND x.indisunique AND NOT x.indisprimary
            ORDER BY t.relname,i.relname,k.position
        "#
        .to_owned(),
        SQLITE_FAMILY => format!(
            r#"
            SELECT m.name AS table_name, i.name AS index_name, c.name AS column_name
            FROM {}.sqlite_master m
            JOIN pragma_index_list(m.name,$1) i
            JOIN pragma_index_info(i.name,$1) c
            WHERE m.type='table' AND i."unique"=1 AND i.origin!='pk'
            ORDER BY m.name,i.name,c.seqno
        "#,
            zeroship_data_orm::sql::mapping::quote_ident(namespace)
        ),
        other => panic!("unsupported journal SQL family: {other:?}"),
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
    assert!(!rows.is_empty(), "journal domain keys must be unique");
    let mut actual = BTreeMap::<(String, String), Vec<String>>::new();
    for row in rows {
        actual
            .entry((
                row["table_name"].as_str().unwrap().to_owned(),
                row["index_name"].as_str().unwrap().to_owned(),
            ))
            .or_default()
            .push(row["column_name"].as_str().unwrap().to_owned());
    }
    let mut expected = BTreeMap::new();
    for (table, collection) in descriptor["collections"].as_object().unwrap() {
        for index in collection["indexes"].as_array().unwrap() {
            if index["unique"].as_bool() != Some(true) {
                continue;
            }
            let fields = index["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            assert!(!fields.is_empty(), "unique keys need declared columns");
            assert!(expected
                .insert(
                    (table.clone(), index["name"].as_str().unwrap().to_owned()),
                    fields,
                )
                .is_none());
        }
    }
    assert_eq!(actual, expected);
}
