use super::*;

fn fields() -> Value {
    value!({"payload":{"type":"json", "nullable":true}})
}

fn table_sql(schema: &str, dialect: &zeroship_migrate::DialectId) -> String {
    let policy = zeroship_migrate::effective_policy_from_charter_toml(
        zeroship_migrate_server::policy::CONFINED_CEILING_TOML,
    )
    .unwrap();
    zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect(
        zeroship_migrate::shipping_vendors(),
        schema,
        "documents",
        &serde_json::to_value(fields()).unwrap(),
        &zeroship_migrate::schema::query::FkEmission::Inline,
        dialect,
        &policy,
    )
    .unwrap()
}

#[compio::test]
async fn sqlite_json_values_round_trip_through_the_orm() {
    let (original, directory) = database().await;
    let fixture = rusqlite::Connection::open(
        directory
            .path()
            .join(format!("zs-{}.sqlite", original.binding.app_id())),
    )
    .unwrap();
    fixture
        .execute_batch(&table_sql("main", &zeroship_migrate_sqlite::DIALECT))
        .unwrap();
    drop(fixture);
    let db = Database::from_schema(
        original.binding.clone(),
        original.backend.clone(),
        vec![("documents".into(), fields())],
    )
    .unwrap();
    exercise_json_values(&db).await;

    // Model a file whose JSON constraint was bypassed by an external writer.
    let fixture = rusqlite::Connection::open(
        directory
            .path()
            .join(format!("zs-{}.sqlite", db.binding.app_id())),
    )
    .unwrap();
    fixture.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE documents SET payload = 'secret_invalid_json'").unwrap();
    let error = db
        .collection("documents")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap_err();
    let DbError::Coded { code, message, .. } = error else {
        panic!("{error}")
    };
    assert_eq!(code, "row_decode_failed");
    assert!(message.contains("payload"));
    assert!(!message.contains("secret_invalid_json"));

    fixture
        .execute("UPDATE documents SET payload = ?1", ["\"true\""])
        .unwrap();
    drop(fixture);
    let Output::Rows { rows, .. } = db
        .collection("documents")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| row["payload"] == Value::String("true".into()))
    );
}

#[compio::test]
async fn postgres_json_values_round_trip_through_the_orm() {
    crate::reset_engine_for_tests();
    let backend = Rc::new(
        crate::backend::postgres::PostgresBackend::connect(
            &zeroship_core::config::test_database_url(),
            4,
            LocalKeySource::EnvVar,
        )
        .await
        .unwrap(),
    );
    let app = format!("zsjson_{}", uuid::Uuid::new_v4().simple());
    let schema = crate::compile::quote_ident(&app);
    backend
        .execute_fixture(&format!("CREATE SCHEMA {schema}"), &[])
        .await
        .unwrap();
    backend
        .pool()
        .batch_execute(&table_sql(&app, &zeroship_migrate_postgres::DIALECT))
        .await
        .unwrap();
    crate::auth::bootstrap::ensure_per_app_role(backend.pool(), &app)
        .await
        .unwrap();
    let role = crate::compile::quote_ident(
        &zeroship_core::database_role::per_app_role_name(&app).unwrap(),
    );
    backend
        .execute_fixture(
            &format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {schema}.documents TO {role}"),
            &[],
        )
        .await
        .unwrap();
    let db = Database::from_schema(
        DbBinding::cold_start(&app),
        crate::backend_handle::BackendHandle::new(backend.clone()),
        vec![("documents".into(), fields())],
    )
    .unwrap();
    exercise_json_values(&db).await;
    drop(db);
    backend
        .execute_fixture(&format!("DROP SCHEMA {schema} CASCADE"), &[])
        .await
        .unwrap();
    backend
        .execute_fixture(&format!("DROP OWNED BY {role}"), &[])
        .await
        .unwrap();
    backend
        .execute_fixture(&format!("DROP ROLE {role}"), &[])
        .await
        .unwrap();
}

async fn exercise_json_values(db: &Database) {
    let values = value!([
        "true", "null", "42", "[1]", "{\"key\":1}", "\"nested\"", "plain text",
        true, false, 42, 1.5, null, {"key":"true"}, [false,"null"],
    ]);
    for expected in values.as_array().unwrap() {
        let payload = expected.clone();
        let id = db
            .transaction(|tx| async move {
                let collection = tx.collection("documents")?;
                let Output::Rows { rows, .. } = collection
                    .insert(value!({"payload":payload.clone()}))
                    .await?
                else {
                    panic!("insert must return rows")
                };
                assert_eq!(
                    rows[0]["payload"], payload,
                    "insert must preserve the JSON type"
                );
                let id = rows[0]["id"].clone();
                let Output::Rows { rows, .. } = collection
                    .update(
                        value!({"id":id.clone()}),
                        value!({"payload":{"$set":payload.clone()}}),
                    )
                    .await?
                else {
                    panic!("update must return rows")
                };
                assert_eq!(
                    rows[0]["payload"], payload,
                    "update must preserve the JSON type"
                );
                Ok(id)
            })
            .await
            .unwrap();
        let Output::Rows { rows, .. } = db
            .collection("documents")
            .unwrap()
            .find(value!({"id":id}), value!({}))
            .await
            .unwrap()
        else {
            panic!("find must return rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(
            &rows[0]["payload"], expected,
            "find must preserve the JSON type"
        );
    }
}
