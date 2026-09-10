use super::*;

/// A migrated collection and the resources that must outlive its queries.
pub(super) struct CollectionFixture {
    pub database: Database,
    pub sqlite_file: Option<std::path::PathBuf>,
    directory: Option<tempfile::TempDir>,
    postgres: Option<(
        Rc<crate::backend::postgres::PostgresBackend>,
        String,
        String,
    )>,
}

impl CollectionFixture {
    pub async fn sqlite(collection: &str, fields: Value) -> Self {
        let (original, directory) = database().await;
        let file = directory
            .path()
            .join(format!("zs-{}.sqlite", original.binding.app_id()));
        let fixture = rusqlite::Connection::open(&file).unwrap();
        fixture
            .execute_batch(&table_sql(
                "main",
                collection,
                &fields,
                &zeroship_migrate_sqlite::DIALECT,
            ))
            .unwrap();
        drop(fixture);
        let database = Database::from_schema(
            original.binding.clone(),
            original.backend.clone(),
            vec![(collection.into(), fields)],
        )
        .unwrap();
        Self {
            database,
            sqlite_file: Some(file),
            directory: Some(directory),
            postgres: None,
        }
    }

    pub async fn postgres(collection: &str, fields: Value) -> Self {
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
        let app = format!("zsorm_{}", uuid::Uuid::new_v4().simple());
        let schema = crate::compile::quote_ident(&app);
        backend
            .execute_fixture(&format!("CREATE SCHEMA {schema}"), &[])
            .await
            .unwrap();
        backend
            .pool()
            .batch_execute(&table_sql(
                &app,
                collection,
                &fields,
                &zeroship_migrate_postgres::DIALECT,
            ))
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
                &format!(
                    "GRANT SELECT, INSERT, UPDATE, DELETE ON {schema}.{} TO {role}",
                    crate::compile::quote_ident(collection)
                ),
                &[],
            )
            .await
            .unwrap();
        let database = Database::from_schema(
            DbBinding::cold_start(&app),
            crate::backend_handle::BackendHandle::new(backend.clone()),
            vec![(collection.into(), fields)],
        )
        .unwrap();
        Self {
            database,
            sqlite_file: None,
            directory: None,
            postgres: Some((backend, schema, role)),
        }
    }

    pub async fn close(self) {
        let Self {
            database,
            directory,
            postgres,
            ..
        } = self;
        drop(database);
        if let Some((backend, schema, role)) = postgres {
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
        drop(directory);
    }
}

fn table_sql(
    schema: &str,
    collection: &str,
    fields: &Value,
    dialect: &zeroship_migrate::DialectId,
) -> String {
    let policy = zeroship_migrate::effective_policy_from_charter_toml(
        zeroship_migrate_server::policy::CONFINED_CEILING_TOML,
    )
    .unwrap();
    let fields = serde_json::to_value(fields).unwrap();
    let mut sql = zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect(
        zeroship_migrate::shipping_vendors(),
        schema,
        collection,
        &fields,
        &zeroship_migrate::schema::query::FkEmission::Inline,
        dialect,
        &policy,
    )
    .unwrap();
    let indexes: Vec<_> = fields
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(field, definition)| {
            let unique = definition["unique"].as_bool() == Some(true);
            (unique || definition["index"].as_bool() == Some(true)).then(|| {
                serde_json::json!({
                    "op":"createIndex", "table":collection, "schema":schema,
                    "name":format!("{collection}_{field}_fixture"),
                    "columns":[{"kind":"column", "name":field}], "unique":unique,
                })
            })
        })
        .collect();
    if !indexes.is_empty() {
        let envelope = serde_json::json!({
            "ir_version":1, "name":"fixture_indexes", "owner_app":schema, "ops":indexes,
        });
        let (_, statements) = zeroship_migrate::render_ir_envelope_sql_statements(
            zeroship_migrate::shipping_vendors(),
            &envelope.to_string(),
            dialect,
            &zeroship_migrate::PreviewOpts {
                default_schema: schema.into(),
                owner_app: schema.into(),
                effective_policy: policy,
            },
        )
        .unwrap();
        assert_eq!(
            statements
                .iter()
                .filter(|statement| statement.starts_with("CREATE "))
                .count(),
            indexes.len()
        );
        for statement in statements {
            sql.push(';');
            sql.push_str(&statement);
        }
    }
    sql
}
