#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use thread-local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;

const UNUSUAL: &str = "{\"q\",\\x} é 🦀 , NULL";

fn fields() -> Value {
    value!({
        "key":{"type":"string", "required":true, "unique":true},
        "labels":{"type":"textArray", "required":true},
        "maybe":{"type":"textArray"}
    })
}

fn strings(items: &[&str]) -> Value {
    Value::Array(items.iter().copied().map(Value::from).collect())
}

fn cases() -> Vec<(&'static str, Value, Value)> {
    vec![
        ("a_empty", strings(&[]), Value::Null),
        ("b_ordered", strings(&["b", "a", "a"]), strings(&[])),
        ("c_null_text", strings(&["NULL"]), strings(&["NULL"])),
        ("d_blank", strings(&[""]), Value::Null),
        ("e_unusual", strings(&[UNUSUAL, ","]), Value::Null),
        ("f_forward", strings(&["a", "b"]), Value::Null),
        ("g_reverse", strings(&["b", "a"]), Value::Null),
        ("h_repeat", strings(&["a", "b", "b"]), Value::Null),
    ]
}

async fn rows(records: &Collection, filter: Value) -> Vec<Value> {
    let Output::Rows { rows, .. } = records
        .find(filter, value!({"orderBy":{"key":1}}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    rows
}

async fn keys(records: &Collection, filter: Value) -> Vec<String> {
    rows(records, filter)
        .await
        .iter()
        .map(|row| row["key"].as_str().unwrap().to_owned())
        .collect()
}

async fn labels_of(records: &Collection, key: &str) -> (Value, Value) {
    let rows = rows(records, value!({"key":key})).await;
    (rows[0]["labels"].clone(), rows[0]["maybe"].clone())
}

async fn update(records: &Collection, key: &str, patch: Value) -> Result<Value, DbError> {
    let Output::Rows { rows, .. } = records.update(value!({"key":key}), patch).await? else {
        panic!("update must return rows")
    };
    Ok(rows[0].clone())
}

fn assert_code(error: &DbError, code: &str) {
    assert_eq!(error.code(), code, "{error:?}");
    assert!(
        !error.message_str().contains("private_element"),
        "{error:?}"
    );
}

async fn insert_cases(records: &Collection) {
    let all = cases();
    let (batch, single) = all.split_at(2);
    let Output::Rows { rows, .. } = records
        .execute(Operation::InsertMany {
            documents: Value::Array(
                batch
                    .iter()
                    .map(|(key, labels, maybe)| {
                        value!({"key":key, "labels":labels.clone(), "maybe":maybe.clone()})
                    })
                    .collect(),
            ),
        })
        .await
        .unwrap()
    else {
        panic!("insert must return rows")
    };
    for ((key, labels, maybe), row) in batch.iter().zip(&rows) {
        assert_eq!(&row["labels"], labels, "{key}");
        assert_eq!(&row["maybe"], maybe, "{key}");
    }
    for (key, labels, maybe) in single.iter().cloned() {
        let Output::Rows { rows, .. } = records
            .insert(value!({"key":key, "labels":labels.clone(), "maybe":maybe.clone()}))
            .await
            .unwrap()
        else {
            panic!("insert must return rows")
        };
        assert_eq!(rows[0]["labels"], labels, "{key}");
        assert_eq!(rows[0]["maybe"], maybe, "{key}");
    }
    for (key, labels, maybe) in cases() {
        assert_eq!(labels_of(records, key).await, (labels, maybe), "{key}");
    }
}

async fn exercise(owner: &CollectionFixture) {
    let records = owner.database.collection("records").unwrap();
    assert_eq!(
        keys(&records, value!({"maybe":null})).await,
        [
            "a_empty",
            "d_blank",
            "e_unusual",
            "f_forward",
            "g_reverse",
            "h_repeat"
        ]
    );
    assert_eq!(keys(&records, value!({"maybe":[]})).await, ["b_ordered"]);
    assert_eq!(
        keys(&records, value!({"maybe":["NULL"]})).await,
        ["c_null_text"]
    );
    assert_eq!(keys(&records, value!({"labels":[]})).await, ["a_empty"]);
    assert_eq!(
        keys(&records, value!({"labels":["a","b"]})).await,
        ["f_forward"]
    );
    assert_eq!(
        keys(&records, value!({"labels":["b","a"]})).await,
        ["g_reverse"]
    );
    assert_eq!(
        keys(&records, value!({"labels":strings(&[UNUSUAL, ","])})).await,
        ["e_unusual"]
    );
    assert_eq!(
        keys(&records, value!({"labels":{"$in":[["a","b"],["NULL"]]}})).await,
        ["c_null_text", "f_forward"]
    );
    assert_eq!(
        keys(&records, value!({"labels":{"$ne":["a","b"]}}))
            .await
            .len(),
        cases().len() - 1
    );
    assert_eq!(
        keys(&records, value!({"labels":{"$nin":[["a","b"],["b","a"]]}}))
            .await
            .len(),
        cases().len() - 2
    );

    let before = rows(&records, value!({})).await;
    for invalid in [
        value!([null]),
        value!(["private_element\u{0}"]),
        value!([1]),
        value!([["private_element"]]),
    ] {
        let insert = records
            .insert(value!({"key":"invalid", "labels":invalid.clone()}))
            .await
            .unwrap_err();
        assert_code(&insert, "invalid_array_element");
        let set = update(
            &records,
            "b_ordered",
            value!({"labels":{"$set":invalid.clone()}}),
        )
        .await
        .unwrap_err();
        assert_code(&set, "invalid_array_element");
        let filter = records
            .find(value!({"labels":invalid}), value!({}))
            .await
            .unwrap_err();
        assert!(filter.message_str().contains("labels"), "{filter:?}");
    }
    for operand in [
        value!(null),
        value!("private_element\u{0}"),
        value!(1),
        value!(["a"]),
    ] {
        for operator in ["$push", "$pull", "$addToSet"] {
            let error = update(
                &records,
                "b_ordered",
                Value::Object(
                    [(
                        "labels".into(),
                        Value::Object([(operator.into(), operand.clone())].into()),
                    )]
                    .into(),
                ),
            )
            .await
            .unwrap_err();
            assert_code(&error, "invalid_array_element");
        }
    }
    assert!(records
        .insert(value!({"key":"invalid", "labels":"private_element"}))
        .await
        .is_err());
    assert!(update(&records, "b_ordered", value!({"labels":{"$inc":1}}))
        .await
        .is_err());
    assert!(records
        .find(value!({"labels":{"$gt":["a"]}}), value!({}))
        .await
        .is_err());
    assert_eq!(rows(&records, value!({})).await, before);

    let pushed = update(&records, "b_ordered", value!({"labels":{"$push":"a"}}))
        .await
        .unwrap();
    assert_eq!(pushed["labels"], strings(&["b", "a", "a", "a"]));
    let pulled = update(&records, "b_ordered", value!({"labels":{"$pull":"a"}}))
        .await
        .unwrap();
    assert_eq!(pulled["labels"], strings(&["b"]));
    let absent = update(&records, "b_ordered", value!({"labels":{"$pull":"z"}}))
        .await
        .unwrap();
    assert_eq!(absent["labels"], strings(&["b"]));
    for (operand, expected) in [
        ("b", strings(&["b"])),
        ("NULL", strings(&["b", "NULL"])),
        ("NULL", strings(&["b", "NULL"])),
        ("", strings(&["b", "NULL", ""])),
    ] {
        let added = update(
            &records,
            "b_ordered",
            value!({"labels":{"$addToSet":operand}}),
        )
        .await
        .unwrap();
        assert_eq!(added["labels"], expected, "{operand:?}");
    }
    let repeated = update(&records, "h_repeat", value!({"labels":{"$addToSet":"b"}}))
        .await
        .unwrap();
    assert_eq!(repeated["labels"], strings(&["a", "b", "b"]));
    let pulled_null_text = update(&records, "c_null_text", value!({"labels":{"$pull":"NULL"}}))
        .await
        .unwrap();
    assert_eq!(pulled_null_text["labels"], strings(&[]));
    for operator in ["$push", "$pull", "$addToSet"] {
        let unchanged = update(
            &records,
            "a_empty",
            Value::Object(
                [(
                    "maybe".into(),
                    Value::Object([(operator.into(), Value::from("x"))].into()),
                )]
                .into(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(unchanged["maybe"], Value::Null, "{operator}");
    }
    let empty = update(&records, "a_empty", value!({"maybe":{"$set":[]}}))
        .await
        .unwrap();
    assert_eq!(empty["maybe"], strings(&[]));
    let pushed_empty = update(&records, "a_empty", value!({"maybe":{"$push":"x"}}))
        .await
        .unwrap();
    assert_eq!(pushed_empty["maybe"], strings(&["x"]));
    let cleared = update(&records, "a_empty", value!({"maybe":null}))
        .await
        .unwrap();
    assert_eq!(cleared["maybe"], Value::Null);

    let Output::Rows { rows: upserted, .. } = records
        .execute(Operation::Upsert {
            document: value!({"key":"f_forward", "labels":["z"], "maybe":["y"]}),
            conflict_fields: value!(["key"]),
        })
        .await
        .unwrap()
    else {
        panic!("upsert must return rows")
    };
    assert_eq!(upserted[0]["labels"], strings(&["z"]));
    assert_eq!(upserted[0]["maybe"], strings(&["y"]));

    let committed = labels_of(&records, "g_reverse").await;
    let rolled_back: Result<(), DbError> = owner
        .database
        .transaction(|tx| async move {
            let records = tx.collection("records")?;
            records
                .update(
                    value!({"key":"g_reverse"}),
                    value!({"labels":{"$push":"c"}}),
                )
                .await?;
            records
                .update(
                    value!({"key":"g_reverse"}),
                    value!({"maybe":{"$set":["kept"]}}),
                )
                .await?;
            Err(DbError::validation("rollback_test", "abort"))
        })
        .await;
    assert!(rolled_back.is_err());
    assert_eq!(labels_of(&records, "g_reverse").await, committed);

    let Output::Count(count) = records
        .execute(Operation::Update {
            filter: value!({"labels":{"$ne":["never"]}}),
            patch: value!({"labels":{"$push":"tail"}}),
            many: true,
        })
        .await
        .unwrap()
    else {
        panic!("update many must return a count")
    };
    assert_eq!(usize::try_from(count).unwrap(), cases().len());
    assert_eq!(
        labels_of(&records, "h_repeat").await.0,
        strings(&["a", "b", "b", "tail"])
    );
}

#[compio::test]
async fn postgres_text_arrays_use_native_storage() {
    let owner = CollectionFixture::postgres("records", fields()).await;
    let catalog = owner
        .postgres_oracle(
            "records",
            "SELECT attname::text AS name, format_type(atttypid, atttypmod) AS type \
             FROM pg_attribute WHERE attrelid = '{table}'::regclass \
             AND attname IN ('labels', 'maybe') ORDER BY attname",
        )
        .await;
    assert_eq!(
        catalog
            .iter()
            .map(|row| (row.get::<_, String>("name"), row.get::<_, String>("type")))
            .collect::<Vec<_>>(),
        [
            ("labels".to_owned(), "text[]".to_owned()),
            ("maybe".to_owned(), "text[]".to_owned())
        ]
    );
    let records = owner.database.collection("records").unwrap();
    insert_cases(&records).await;
    let oracle = owner
        .postgres_oracle(
            "records",
            "SELECT key, labels, maybe, maybe IS NULL AS maybe_absent, \
             labels = ARRAY['b','a','a']::text[] AS ordered, array_ndims(labels) AS ndims, \
             array_lower(labels, 1) AS lower FROM {table} ORDER BY key",
        )
        .await;
    for ((key, labels, maybe), row) in cases().into_iter().zip(&oracle) {
        assert_eq!(row.get::<_, String>("key"), key);
        let stored: Vec<String> = row.get("labels");
        assert_eq!(
            strings(&stored.iter().map(String::as_str).collect::<Vec<_>>()),
            labels
        );
        let stored_maybe: Option<Vec<String>> = row.get("maybe");
        assert_eq!(
            stored_maybe.map_or(Value::Null, |items| strings(
                &items.iter().map(String::as_str).collect::<Vec<_>>()
            )),
            maybe
        );
        assert_eq!(row.get::<_, bool>("maybe_absent"), maybe.is_null());
        assert_eq!(row.get::<_, bool>("ordered"), key == "b_ordered");
        let empty = labels.as_array().unwrap().is_empty();
        assert_eq!(row.get::<_, Option<i32>>("ndims"), (!empty).then_some(1));
        assert_eq!(row.get::<_, Option<i32>>("lower"), (!empty).then_some(1));
    }
    exercise(&owner).await;
    let mutated = owner
        .postgres_oracle(
            "records",
            "SELECT labels = ARRAY['b','NULL','','tail']::text[] AS exact, \
             array_position(labels, NULL) IS NULL AS no_null_element \
             FROM {table} WHERE key = 'b_ordered'",
        )
        .await;
    assert!(mutated[0].get::<_, bool>("exact"));
    assert!(mutated[0].get::<_, bool>("no_null_element"));
    owner.close().await;
}

#[compio::test]
async fn sqlite_text_arrays_store_json_text_with_the_same_contract() {
    let owner = CollectionFixture::sqlite("records", fields()).await;
    insert_cases(&owner.database.collection("records").unwrap()).await;
    exercise(&owner).await;
    let oracle = rusqlite::Connection::open(owner.sqlite_file.as_ref().unwrap()).unwrap();
    let (kind, stored): (String, String) = oracle
        .query_row(
            "SELECT typeof(labels), labels FROM records WHERE key = 'b_ordered'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind, "text");
    assert_eq!(stored, r#"["b","NULL","","tail"]"#);
    drop(oracle);
    owner.close().await;
}

#[compio::test]
async fn postgres_text_array_null_elements_are_refused_on_read() {
    let owner = CollectionFixture::postgres("records", fields()).await;
    let records = owner.database.collection("records").unwrap();
    records
        .insert(value!({"key":"row", "labels":["a"]}))
        .await
        .unwrap();
    owner
        .postgres_oracle(
            "records",
            "UPDATE {table} SET labels = '{private_element,NULL}' WHERE key = 'row'",
        )
        .await;
    let error = records.find(value!({}), value!({})).await.unwrap_err();
    assert_eq!(error.code(), "row_decode_failed", "{error:?}");
    assert!(error.message_str().contains("labels"));
    assert!(!error.message_str().contains("private_element"));
    owner
        .postgres_oracle(
            "records",
            "UPDATE {table} SET labels = '{private_element,\"NULL\"}' WHERE key = 'row'",
        )
        .await;
    assert_eq!(
        rows(&records, value!({})).await[0]["labels"],
        strings(&["private_element", "NULL"])
    );
    owner.close().await;
}

#[compio::test]
async fn postgres_storage_mismatches_fail_before_writing() {
    for (declared, column) in [
        (
            value!({"type":"array", "items":"string", "required":true}),
            "TEXT[] NOT NULL",
        ),
        (
            value!({"type":"textArray", "required":true}),
            "JSONB NOT NULL",
        ),
    ] {
        let fields = value!({
            "id":{"type":"string", "required":true, "primaryKey":true},
            "labels":declared
        });
        let owner = CollectionFixture::postgres_from_table_definition(
            "records",
            fields,
            &format!("id TEXT PRIMARY KEY, labels {column}"),
        )
        .await;
        let records = owner.database.collection("records").unwrap();
        assert!(records
            .insert(value!({"id":"row", "labels":["a"]}))
            .await
            .is_err());
        let Output::Count(count) = records.count(value!({}), value!({})).await.unwrap() else {
            panic!("count must return a count")
        };
        assert_eq!(count, 0, "{column}");
        owner.close().await;
    }
}

fn text_array_migration() -> String {
    serde_json::json!({
        "ir_version": 1,
        "name": "text_array_fixture",
        "ops": [{
            "op": "createTable",
            "name": "records",
            "columns": [
                {"name": "key", "type": "text", "nullable": false},
                {"name": "scopes", "type": "textArray", "nullable": false},
                {"name": "amr", "type": "textArray"}
            ]
        }]
    })
    .to_string()
}

async fn round_trip_migrated(owner: &CollectionFixture) {
    let fields = owner
        .database
        .context
        .with(|| crate::descriptor::collection_schema(&owner.database.binding, "records").unwrap());
    for field in ["scopes", "amr"] {
        assert!(fields[field].has_native_array_storage(), "{field}");
        assert_eq!(fields[field].items, Some(LogicalType::Text));
    }
    assert!(fields["scopes"].required);
    assert!(!fields["amr"].required);
    let records = owner.database.collection("records").unwrap();
    let Output::Rows { rows, .. } = records
        .insert(value!({"key":"k", "scopes":["b","a","a"], "amr":null}))
        .await
        .unwrap()
    else {
        panic!("insert must return rows")
    };
    assert_eq!(rows[0]["scopes"], strings(&["b", "a", "a"]));
    assert_eq!(rows[0]["amr"], Value::Null);
    let added = update(
        &records,
        "k",
        value!({"scopes":{"$addToSet":"c"}, "amr":{"$set":["pwd"]}}),
    )
    .await
    .unwrap();
    assert_eq!(added["scopes"], strings(&["b", "a", "a", "c"]));
    assert_eq!(added["amr"], strings(&["pwd"]));
    assert_eq!(
        keys(&records, value!({"scopes":["b","a","a","c"]})).await,
        ["k"]
    );
    assert!(keys(&records, value!({"scopes":["a","b","a","c"]}))
        .await
        .is_empty());
}

#[compio::test]
async fn migrated_text_arrays_round_trip_through_postgres() {
    let mut owner =
        CollectionFixture::postgres("records", value!({"label":{"type":"string"}})).await;
    owner
        .replace_from_migration("records", &text_array_migration())
        .await;
    let catalog = owner
        .postgres_oracle(
            "records",
            "SELECT format_type(atttypid, atttypmod) AS type FROM pg_attribute \
             WHERE attrelid = '{table}'::regclass AND attname IN ('scopes', 'amr')",
        )
        .await;
    assert_eq!(catalog.len(), 2);
    assert!(catalog
        .iter()
        .all(|row| row.get::<_, String>("type") == "text[]"));
    round_trip_migrated(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn migrated_text_arrays_round_trip_through_sqlite() {
    let mut owner = CollectionFixture::sqlite("records", value!({"label":{"type":"string"}})).await;
    owner
        .replace_from_migration("records", &text_array_migration())
        .await;
    let oracle = rusqlite::Connection::open(owner.sqlite_file.as_ref().unwrap()).unwrap();
    let declared: Vec<String> = oracle
        .prepare("SELECT type FROM pragma_table_info('records') WHERE name IN ('scopes', 'amr')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(declared, ["TEXT", "TEXT"]);
    drop(oracle);
    round_trip_migrated(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn sqlite_text_arrays_refuse_distinct_and_ordering() {
    let owner = CollectionFixture::sqlite("records", fields()).await;
    let records = owner.database.collection("records").unwrap();
    records
        .insert(value!({"key":"k", "labels":["a"]}))
        .await
        .unwrap();
    assert!(records
        .execute(Operation::Distinct {
            field: "labels".into(),
            filter: value!({}),
            options: value!({}),
        })
        .await
        .is_err());
    assert!(records
        .find(value!({}), value!({"orderBy":{"labels":1}}))
        .await
        .is_err());
    assert!(records
        .execute(Operation::Distinct {
            field: "key".into(),
            filter: value!({}),
            options: value!({}),
        })
        .await
        .is_ok());
    owner.close().await;
}
