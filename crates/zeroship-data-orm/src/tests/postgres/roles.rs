//! PostgreSQL roles contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use crate::tests::fixtures::{self};

use compio_postgres::{NoTls, Pool};

use zeroship_data_orm::binding::DbBinding;

use crate::value::{value, Value};

/// Walk a compio-postgres Error's `source()` chain into one string —
/// without this, top-level Display is just "db error" and the
/// SQLSTATE-bearing inner DbError stays invisible.
fn err_chain(e: &dyn std::error::Error) -> String {
    let mut s = format!("{e}");
    let mut cur = e.source();
    while let Some(src) = cur {
        s.push_str(" | ");
        s.push_str(&format!("{src}"));
        cur = src.source();
    }
    s.to_lowercase()
}

#[test]
fn per_app_role_created_at_provision() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app = crate::tests::fixtures::test_app_id!();
            let app = app.as_str();
            let role = provision_app_with_role(&pool, app).await;

            // First provision creates the role.
            let first = crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .expect("provision per-app role");
            assert!(first.created_role, "first provision must create the role");

            // The role now exists in pg_roles.
            let exists = pool
                .query_text_params(
                    "SELECT 1 FROM pg_roles WHERE rolname = $1",
                    &[role.as_str()],
                )
                .await
                .unwrap();
            assert_eq!(exists.len(), 1, "role must exist after provision");

            // Idempotent: a second provision is a no-op create (GRANTs re-run
            // harmlessly).
            let second = crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .expect("re-provision per-app role");
            assert!(
                !second.created_role,
                "second provision must NOT re-create the role"
            );

            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn per_app_role_has_no_replication_attr() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app = crate::tests::fixtures::test_app_id!();
            let app = app.as_str();
            let role = provision_app_with_role(&pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .unwrap();

            // §17.5 NON-NEGOTIABLE: rolreplication MUST be false.
            let rows = pool
                .query_text_params(
                    "SELECT rolreplication FROM pg_roles WHERE rolname = $1",
                    &[role.as_str()],
                )
                .await
                .unwrap();
            let is_repl: bool = rows[0].get("rolreplication");
            assert!(
                !is_repl,
                "per-app role MUST NOT have the REPLICATION attribute (§17.5 \
         slot-ownership-stays-platform)"
            );

            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn per_app_role_grant_scoped_to_schema() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app = crate::tests::fixtures::test_app_id!();
            let app = app.as_str();
            let role = provision_app_with_role(&pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .unwrap();

            // Create a table in the app schema (as superuser), insert a row.
            pool.execute(
                &format!(r#"CREATE TABLE "{app}".widgets (id SERIAL PRIMARY KEY, name TEXT)"#),
                &[],
            )
            .await
            .unwrap();
            pool.execute(
                &format!(r#"INSERT INTO "{app}".widgets (name) VALUES ('seed')"#),
                &[],
            )
            .await
            .unwrap();
            fixtures::grant_all_runtime_table_columns(&pool, app, "widgets").await;

            // SET ROLE to the per-app role and CRUD its own schema — must work.
            pool.execute(&format!(r#"SET ROLE "{role}""#), &[])
                .await
                .unwrap();
            let sel = pool
                .query_text_params(&format!(r#"SELECT name FROM "{app}".widgets"#), &[])
                .await;
            assert!(
                sel.is_ok(),
                "per-app role must SELECT its own schema: {sel:?}"
            );
            let ins = pool
                .execute(
                    &format!(r#"INSERT INTO "{app}".widgets (name) VALUES ('by_role')"#),
                    &[],
                )
                .await;
            assert!(
                ins.is_ok(),
                "per-app role must INSERT its own schema: {ins:?}"
            );
            pool.execute("RESET ROLE", &[]).await.unwrap();

            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn per_app_role_cannot_read_sibling_schema_or_touch_slots() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app_a = crate::tests::fixtures::test_app_id!("a");
            let app_a = app_a.as_str();
            let app_b = crate::tests::fixtures::test_app_id!("b");
            let app_b = app_b.as_str();
            let role_a = provision_app_with_role(&pool, app_a).await;
            // Provision a sibling schema B (and its role) with a table.
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_b}\" CASCADE"), &[])
                .await
                .unwrap();
            let role_b = zeroship_core::database_role::per_app_role_name(app_b)
                .expect("sibling fixture app id must produce a valid PostgreSQL role name");
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role_b}\""), &[])
                .await;
            pool.execute(&format!("CREATE SCHEMA \"{app_b}\""), &[])
                .await
                .unwrap();

            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app_a)
                .await
                .unwrap();
            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app_b)
                .await
                .unwrap();
            pool.execute(
                &format!(r#"CREATE TABLE "{app_b}".secrets (id SERIAL PRIMARY KEY, val TEXT)"#),
                &[],
            )
            .await
            .unwrap();
            pool.execute(
                &format!(r#"INSERT INTO "{app_b}".secrets (val) VALUES ('app_b_secret')"#),
                &[],
            )
            .await
            .unwrap();

            // SET ROLE to app_a's role and attempt to read app_b's schema — must
            // be denied (no USAGE on the sibling schema).
            pool.execute(&format!(r#"SET ROLE "{role_a}""#), &[])
                .await
                .unwrap();
            let cross = pool
                .query_text_params(&format!(r#"SELECT val FROM "{app_b}".secrets"#), &[])
                .await;
            assert!(
                cross.is_err(),
                "per-app role A must NOT read sibling schema B; got Ok"
            );
            let cross_err = err_chain(&cross.unwrap_err());
            assert!(
                cross_err.contains("permission denied") || cross_err.contains("acl"),
                "expected permission-denied reading sibling schema, got: {cross_err}"
            );

            // While SET ROLE'd: cannot create a replication slot (NOREPLICATION).
            let slot_create = pool
        .execute(
            "SELECT pg_create_logical_replication_slot('p6a_fence_slot', 'pgoutput', false, false)",
            &[],
        )
        .await;
            assert!(
                slot_create.is_err(),
                "per-app role must NOT create a replication slot directly"
            );
            let slot_err = err_chain(&slot_create.unwrap_err());
            assert!(
                slot_err.contains("replication") || slot_err.contains("permission denied"),
                "expected REPLICATION-privilege error on slot create, got: {slot_err}"
            );

            // Cannot drop a slot either (pg_drop_replication_slot requires
            // REPLICATION). Use a name that doesn't exist — the privilege check
            // fires before the "no such slot" check.
            let slot_drop = pool
                .execute("SELECT pg_drop_replication_slot('does_not_exist')", &[])
                .await;
            assert!(
                slot_drop.is_err(),
                "per-app role must NOT drop a replication slot"
            );

            pool.execute("RESET ROLE", &[]).await.unwrap();

            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app_a}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app_b}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role_a}\""), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role_b}\""), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn client_sql_runs_under_per_app_role() {
    Host::test(|host| {
        host.run(async {
            // Proves the `SET LOCAL ROLE` shape `exec_begin`
            // issue actually switches the effective role for the rest of the tx,
            // and reverts at COMMIT/ROLLBACK.
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app = crate::tests::fixtures::test_app_id!();
            let app = app.as_str();
            let role = provision_app_with_role(&pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .unwrap();

            // Open a dedicated connection, BEGIN, then apply the SAME SET LOCAL
            // ROLE SQL the orchestrator emits.
            let (client, conn) = compio_postgres::connect(&url, NoTls).await.unwrap();
            compio::runtime::spawn(async move {
                let _ = conn.run().await;
            })
            .detach();

            client.execute("BEGIN", &[]).await.unwrap();
            let set_sql = crate::tests::fixtures::roles::set_local_role_sql(app)
                .expect("integration app id must produce valid SET LOCAL ROLE SQL");
            client.execute(&set_sql, &[]).await.unwrap();

            // current_user inside the tx must be the per-app role.
            let who = client
                .query_text_params("SELECT current_user AS u", &[])
                .await
                .unwrap();
            let current: String = who[0].get("u");
            assert_eq!(
                current, role,
                "client SQL inside the tx must run under the per-app role"
            );

            // COMMIT reverts SET LOCAL — current_user is back to the login role.
            client.execute("COMMIT", &[]).await.unwrap();
            let who2 = client
                .query_text_params("SELECT current_user AS u", &[])
                .await
                .unwrap();
            let after: String = who2[0].get("u");
            assert_ne!(
                after, role,
                "SET LOCAL ROLE must revert at COMMIT (no role leak to next stmt)"
            );

            drop(client);
            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn exec_autocommit_query_runs_under_per_app_role() {
    Host::test(|host| {
        host.run(async {
            // I2 regression: the shared autocommit exec path must switch to the
            // per-app role before running the statement, not just explicit/auto tx.
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let app = crate::tests::fixtures::test_app_id!();
            let app = app.as_str();
            let role = provision_app_with_role(&pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&pool, app)
                .await
                .unwrap();
            host.set_database_url(&url);

            let rows = host
                .exec_query(
                    app,
                    crate::sql::compiler::CompiledQuery {
                        sql: "SELECT current_user AS u".to_string(),
                        params: vec![],
                    },
                )
                .await
                .expect("autocommit exec query");
            let current = rows[0]
                .get("u")
                .and_then(Value::as_str)
                .expect("current_user string");
            assert_eq!(
                current, role,
                "autocommit exec query must run under the per-app role",
            );

            let who = pool
                .query_text_params("SELECT current_user AS u", &[])
                .await
                .unwrap();
            let after: String = who[0].get("u");
            assert_ne!(
                after, role,
                "RESET ROLE must run before the pooled autocommit connection returns",
            );

            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn vector_search_runs_under_per_app_role_via_rls() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::backend::VectorMetric;

            let (_postgres, url) = require_pg(host).await;
            let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
            require_pgvector(&admin_pool).await;

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let coll = "docs";
            let role = provision_app_with_role(&admin_pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&admin_pool, app)
                .await
                .unwrap();
            admin_pool
                .execute(
                    &format!(
                        "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               embedding vector(2) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
                    ),
                    &[],
                )
                .await
                .unwrap();
            crate::tests::fixtures::cache_schema(
                app,
                coll,
                value!({ "embedding": { "type": "vector", "vectorDims": 2 } }),
            );
            admin_pool
                .execute(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (embedding) VALUES ($1::text::vector)"
                    ),
                    &[&"[1,0]" as &(dyn compio_postgres::types::ToSql + Sync)],
                )
                .await
                .unwrap();
            fixtures::grant_all_runtime_table_columns(&admin_pool, app, coll).await;
            install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
            let login_role = "p6a_vector_login";
            let (login_url, login_pool) =
                provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app)
                    .await;

            let blocked = login_pool
                .query_text_params(&format!("SELECT id FROM \"{app}\".\"{coll}\""), &[])
                .await
                .unwrap();
            assert!(
                blocked.is_empty(),
                "login role must be blocked by FORCE RLS before vector_search proves the role fence"
            );

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                login_pool.clone(),
                login_url,
                host.key_source(),
            );
            let binding = DbBinding::cold_start(app);
            let schema = zeroship_data_orm::descriptor::collection_schema(&binding, coll)
                .expect("descriptor slice for the search fixture");
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::postgres();
            let rows = zeroship_data_orm::search::Search::vector_search(
                &backend,
                None,
                zeroship_data_orm::search::VectorSearch::compile(
                    &binding,
                    coll,
                    "embedding",
                    &[1.0, 0.0],
                    1,
                    VectorMetric::Cosine,
                    &Value::Null,
                    &schema,
                    &registration,
                )
                .unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("vector_search failed: {e:?}"));
            assert_eq!(rows.len(), 1, "vector_search must see the role-gated row");
            assert_eq!(rows[0]["id"], 1);

            drop(login_pool);
            let _ = admin_pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            drop(backend);
            release_pg(host, admin_pool).await;
        })
    })
}

#[test]
fn spatial_near_runs_under_per_app_role_via_rls() {
    Host::test(|host| {
        host.run(async {
            use zeroship_data_orm::backend::GeoPoint;

            let (_postgres, url) = require_pg(host).await;
            let admin_pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());
            require_postgis(&admin_pool).await;

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let coll = "places";
            let role = provision_app_with_role(&admin_pool, app).await;
            crate::tests::fixtures::roles::ensure_per_app_role(&admin_pool, app)
                .await
                .unwrap();
            admin_pool
                .execute(
                    &format!(
                        "CREATE TABLE \"{app}\".\"{coll}\" (\
               id SERIAL PRIMARY KEY, \
               location geography(POINT, 4326) NOT NULL, \
               created_at TIMESTAMPTZ DEFAULT NOW(), \
               updated_at TIMESTAMPTZ DEFAULT NOW(), \
               created_by TEXT, \
               updated_by TEXT, \
               version INTEGER NOT NULL DEFAULT 1, \
               deleted_at TIMESTAMPTZ\
             )"
                    ),
                    &[],
                )
                .await
                .unwrap();
            crate::tests::fixtures::cache_schema(
                app,
                coll,
                value!({ "location": { "type": "geoPoint" } }),
            );
            admin_pool
                .execute(
                    &format!(
                        "INSERT INTO \"{app}\".\"{coll}\" (location) \
             VALUES (ST_GeogFromText('POINT(-0.1278 51.5074)'))"
                    ),
                    &[],
                )
                .await
                .unwrap();
            fixtures::grant_all_runtime_table_columns(&admin_pool, app, coll).await;
            install_role_bound_select_policy(&admin_pool, app, coll, &role).await;
            let login_role = "p6a_spatial_login";
            let (login_url, login_pool) =
                provision_platform_login_pool(&admin_pool, &url, login_role, "test", &role, app)
                    .await;

            let blocked = login_pool
                .query_text_params(&format!("SELECT id FROM \"{app}\".\"{coll}\""), &[])
                .await
                .unwrap();
            assert!(
                blocked.is_empty(),
                "login role must be blocked by FORCE RLS before spatial_near proves the role fence"
            );

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                login_pool.clone(),
                login_url,
                host.key_source(),
            );
            let binding = DbBinding::cold_start(app);
            let schema = zeroship_data_orm::descriptor::collection_schema(&binding, coll).unwrap();
            let registration = zeroship_data_orm::sql::registration::SqlRegistration::postgres();
            let rows = zeroship_data_orm::search::Search::spatial_near(
                &backend,
                None,
                zeroship_data_orm::search::SpatialSearch::compile(
                    &binding,
                    coll,
                    "location",
                    GeoPoint {
                        lat: 51.5074,
                        lng: -0.1278,
                    },
                    1000.0,
                    &Value::Null,
                    Some(1),
                    &schema,
                    &registration,
                )
                .unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("spatial_near failed: {e:?}"));
            assert_eq!(rows.len(), 1, "spatial_near must see the role-gated row");
            assert_eq!(rows[0]["id"], 1);

            drop(login_pool);
            let _ = admin_pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{login_role}\""), &[])
                .await;
            let _ = admin_pool
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
                .await;
            drop(backend);
            release_pg(host, admin_pool).await;
        })
    })
}
