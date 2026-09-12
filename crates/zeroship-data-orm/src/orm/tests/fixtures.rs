use super::*;

/// A migrated collection and the resources that must outlive its queries.
pub(super) struct CollectionFixture {
    pub database: Database,
    pub sqlite_file: Option<std::path::PathBuf>,
    directory: Option<tempfile::TempDir>,
    server: Option<crate::tests::fixtures::postgres::Postgres>,
    postgres: Option<(
        Rc<crate::backend::postgres::PostgresBackend>,
        String,
        String,
    )>,
}

impl CollectionFixture {
    pub async fn wait_for_upsert_conflict(&self) {
        let backend = &self.postgres.as_ref().expect("PostgreSQL fixture").0;
        let app = self.database.binding.app_id();
        compio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let rows = backend.pool().query(
                    "SELECT EXISTS(SELECT FROM pg_stat_activity WHERE wait_event = 'transactionid' AND query LIKE '%ON CONFLICT%' AND query LIKE '%' || $1 || '%')",
                    &[&app],
                ).await.unwrap();
                if rows[0].get::<_, bool>(0) { break; }
                compio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("upsert must wait for the uncommitted conflicting row");
    }

    pub async fn sqlite(collection: &str, fields: Value) -> Self {
        Self::sqlite_with_keys(collection, fields, ProjectKeySource::unavailable()).await
    }

    pub async fn sqlite_with_keys(
        collection: &str,
        fields: Value,
        key_source: ProjectKeySource,
    ) -> Self {
        let (original, directory) = database_with_keys(key_source).await;
        let file = directory
            .path()
            .join(format!("zs-{}.sqlite", original.binding.app_id()));
        let fixture = rusqlite::Connection::open(&file).unwrap();
        for statement in zeroship_migrate_sqlite::backend::audit_unmask_ddl("main") {
            fixture.execute_batch(&statement).unwrap();
        }
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
            vec![(
                collection.into(),
                crate::tests::fixtures::schema::generated_fields(fields),
            )],
        )
        .unwrap();
        Self {
            database,
            sqlite_file: Some(file),
            directory: Some(directory),
            postgres: None,
            server: None,
        }
    }

    pub async fn postgres(collection: &str, fields: Value) -> Self {
        Self::postgres_with_keys(collection, fields, ProjectKeySource::unavailable()).await
    }

    pub async fn sqlite_from_table_definition(
        collection: &str,
        fields: Value,
        columns: &str,
    ) -> Self {
        crate::tests::fixtures::reset_engine();
        let directory = tempfile::tempdir().unwrap();
        let binding = DbBinding::cold_start("orm_internal_fixture");
        let file = directory
            .path()
            .join(format!("zs-{}.sqlite", binding.app_id()));
        rusqlite::Connection::open(&file)
            .unwrap()
            .execute_batch(&format!(
                "CREATE TABLE {} ({columns})",
                crate::sql::mapping::quote_ident(collection)
            ))
            .unwrap();
        let database = Database::connect(
            binding,
            crate::ConnectOptions::new(
                directory.path().join("control.sqlite").to_string_lossy(),
                ProjectKeySource::unavailable(),
            ),
            vec![(collection.into(), fields)],
        )
        .await
        .unwrap();
        Self {
            database,
            sqlite_file: Some(file),
            directory: Some(directory),
            postgres: None,
            server: None,
        }
    }

    pub async fn postgres_from_table_definition(
        collection: &str,
        fields: Value,
        columns: &str,
    ) -> Self {
        let server = crate::tests::fixtures::postgres::Postgres::start();
        crate::tests::fixtures::reset_engine();
        let backend = Rc::new(
            crate::backend::postgres::PostgresBackend::connect(
                &server.url(),
                4,
                ProjectKeySource::unavailable(),
            )
            .await
            .unwrap(),
        );
        let app = format!("zsorm_{}", uuid::Uuid::new_v4().simple());
        let schema = crate::sql::mapping::quote_ident(&app);
        let table = crate::sql::mapping::quote_ident(collection);
        backend
            .pool()
            .batch_execute(&format!(
                "CREATE SCHEMA {schema}; CREATE TABLE {schema}.{table} ({columns})"
            ))
            .await
            .unwrap();
        crate::tests::fixtures::roles::ensure_per_app_role(backend.pool(), &app)
            .await
            .unwrap();
        let role = crate::sql::mapping::quote_ident(
            &zeroship_core::database_role::per_app_role_name(&app).unwrap(),
        );
        backend
            .pool()
            .batch_execute(&format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON {schema}.{table} TO {role}"
            ))
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
            server: Some(server),
        }
    }

    pub async fn postgres_with_keys(
        collection: &str,
        fields: Value,
        key_source: ProjectKeySource,
    ) -> Self {
        let server = crate::tests::fixtures::postgres::Postgres::start();
        crate::tests::fixtures::reset_engine();
        let backend = Rc::new(
            crate::backend::postgres::PostgresBackend::connect(&server.url(), 4, key_source)
                .await
                .unwrap(),
        );
        let app = format!("zsorm_{}", uuid::Uuid::new_v4().simple());
        let schema = crate::sql::mapping::quote_ident(&app);
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
        backend
            .pool()
            .batch_execute(&zeroship_migrate_server::provisioning::audit_unmask_table_sql(&app))
            .await
            .unwrap();
        crate::tests::fixtures::roles::ensure_per_app_role(backend.pool(), &app)
            .await
            .unwrap();
        let role = crate::sql::mapping::quote_ident(
            &zeroship_core::database_role::per_app_role_name(&app).unwrap(),
        );
        backend
            .execute_fixture(
                &format!(
                    "GRANT SELECT, INSERT, UPDATE, DELETE ON {schema}.{} TO {role}",
                    crate::sql::mapping::quote_ident(collection)
                ),
                &[],
            )
            .await
            .unwrap();
        let database = Database::from_schema(
            DbBinding::cold_start(&app),
            crate::backend_handle::BackendHandle::new(backend.clone()),
            vec![(
                collection.into(),
                crate::tests::fixtures::schema::generated_fields(fields),
            )],
        )
        .unwrap();
        Self {
            database,
            sqlite_file: None,
            directory: None,
            postgres: Some((backend, schema, role)),
            server: Some(server),
        }
    }

    pub async fn replace_from_migration(&mut self, collection: &str, migration: &str) {
        let migration: zeroship_migrate::model::ir::MigrationIr =
            serde_json::from_str(migration).unwrap();
        let policy = zeroship_migrate::effective_policy_from_charter_toml(
            zeroship_migrate_server::policy::CONFINED_CEILING_TOML,
        )
        .unwrap();
        let namespace = if self.sqlite_file.is_some() {
            "main"
        } else {
            self.database.binding.schema().as_str()
        };
        let dialect = if self.sqlite_file.is_some() {
            &zeroship_migrate_sqlite::DIALECT
        } else {
            &zeroship_migrate_postgres::DIALECT
        };
        let artifacts = zeroship_migrate::render_artifacts(
            zeroship_migrate::shipping_vendors(),
            &migration.ops,
            dialect,
            namespace,
            &policy,
        )
        .unwrap();
        let (_, statements) = zeroship_migrate::render_ir_envelope_sql_statements(
            zeroship_migrate::shipping_vendors(),
            &serde_json::to_string(&migration).unwrap(),
            dialect,
            &zeroship_migrate::PreviewOpts {
                default_schema: namespace.into(),
                owner_app: namespace.into(),
                effective_policy: policy,
            },
        )
        .unwrap();
        let table = format!(
            "{}.{}",
            crate::sql::mapping::quote_ident(namespace),
            crate::sql::mapping::quote_ident(collection)
        );
        let ddl = format!("DROP TABLE {table};{}", statements.join(";"));
        if let Some(file) = &self.sqlite_file {
            rusqlite::Connection::open(file)
                .unwrap()
                .execute_batch(&ddl)
                .unwrap();
        } else {
            let (backend, _, role) = self.postgres.as_ref().unwrap();
            backend.pool().batch_execute(&ddl).await.unwrap();
            backend.pool().batch_execute(&format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}; GRANT USAGE ON ALL SEQUENCES IN SCHEMA {} TO {role}",
                crate::sql::mapping::quote_ident(namespace),
            )).await.unwrap();
        }
        let runtime: Value = serde_json::from_str(&artifacts.runtime_json).unwrap();
        self.database = Database::from_schema(
            self.database.binding.clone(),
            self.database.backend.clone(),
            vec![(
                collection.into(),
                runtime["collections"][collection]["fields"].clone(),
            )],
        )
        .unwrap();
    }

    pub async fn rename_fields(&mut self, collection: &str, names: &[(&str, &str)]) {
        let fields = self.database.context.with(|| {
            crate::descriptor::collection_schema(&self.database.binding, collection).unwrap()
        });
        let mut fields = fields.as_ref().clone();
        for (old, new) in names {
            let table = crate::sql::mapping::quote_ident(collection);
            let column = crate::sql::mapping::quote_ident(old);
            let renamed = crate::sql::mapping::quote_ident(new);
            if let Some(file) = &self.sqlite_file {
                rusqlite::Connection::open(file)
                    .unwrap()
                    .execute_batch(&format!(
                        "ALTER TABLE {table} RENAME COLUMN {column} TO {renamed}"
                    ))
                    .unwrap();
            } else {
                let (backend, namespace, _) = self.postgres.as_ref().unwrap();
                backend
                    .execute_fixture(
                        &format!(
                            "ALTER TABLE {namespace}.{table} RENAME COLUMN {column} TO {renamed}"
                        ),
                        &[],
                    )
                    .await
                    .unwrap();
            }
            let mut definition = fields.as_object_mut().unwrap().shift_remove(*old).unwrap();
            definition["storage"] = value!({"valueColumn":new});
            fields
                .as_object_mut()
                .unwrap()
                .insert((*new).into(), definition);
        }
        self.database = Database::from_schema(
            self.database.binding.clone(),
            self.database.backend.clone(),
            vec![(collection.into(), fields)],
        )
        .unwrap();
    }

    pub async fn add_unique_index(&self, collection: &str, fields: &[&str]) {
        let namespace = self.database.binding.schema().as_str();
        let name = format!("{collection}_{}_fixture", fields.join("_"));
        let columns = fields
            .iter()
            .map(|field| crate::sql::mapping::quote_ident(field))
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(file) = &self.sqlite_file {
            let sql = format!(
                "CREATE UNIQUE INDEX {} ON {} ({columns})",
                crate::sql::mapping::quote_ident(&name),
                crate::sql::mapping::quote_ident(collection),
            );
            rusqlite::Connection::open(file)
                .unwrap()
                .execute_batch(&sql)
                .unwrap();
        } else {
            let sql = format!(
                "CREATE UNIQUE INDEX {} ON {}.{} ({columns})",
                crate::sql::mapping::quote_ident(&name),
                crate::sql::mapping::quote_ident(namespace),
                crate::sql::mapping::quote_ident(collection),
            );
            self.postgres
                .as_ref()
                .unwrap()
                .0
                .pool()
                .batch_execute(&sql)
                .await
                .unwrap();
        }
    }

    pub async fn close(self) {
        let Self {
            database,
            directory,
            postgres,
            server,
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
        drop(server);
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
    let authored = Value::Object(
        fields
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, definition)| definition.get("assign").is_none())
            .map(|(name, definition)| (name.clone(), definition.clone()))
            .collect(),
    );
    let fields = serde_json::to_value(authored).unwrap();
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
