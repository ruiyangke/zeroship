//! PostgreSQL column-grant behavior for registered ORM writes.

use crate::{
    sql::{
        mapping::{quote_ident, raw_column_name, WriteAssignments},
        registration::SqlRegistration,
        SchemaName,
    },
    tests::fixtures::{
        schema::{fixture_table_sql, generated_fields},
        Host,
    },
    value::{value, Value},
};
use compio_postgres::{Client, NoTls};
use std::collections::BTreeSet;
use zeroship_migrate::schema::query::FkEmission;

const COLLECTION: &str = "people";

fn people_schema() -> Value {
    generated_fields(value!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "full", "classification": "pci" }
        },
        "nickname": { "type": "string" }
    }))
}

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to PostgreSQL fixture");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
        .to_string()
}

fn readable_columns(schema: &Value) -> Vec<String> {
    crate::sql::mapping::implicit_read_fields(schema)
        .expect("readable fixture fields")
        .into_iter()
        .map(str::to_owned)
        .collect()
}

async fn fixture(admin: &Client, suffix: &str) -> (String, String) {
    let app = format!("colgrant_{suffix}");
    let role = format!("colgrant_role_{suffix}");
    let schema = people_schema();
    admin
        .batch_execute(&format!("CREATE SCHEMA {}", quote_ident(&app)))
        .await
        .expect("create app schema");
    let ddl = fixture_table_sql(
        &SchemaName::new(&app).expect("fixture schema"),
        COLLECTION,
        &schema,
        &FkEmission::Inline,
    )
    .expect("build fixture table");
    admin
        .batch_execute(&ddl)
        .await
        .expect("create fixture table");
    admin
        .batch_execute(&format!("CREATE ROLE {} NOLOGIN", quote_ident(&role)))
        .await
        .expect("create fixture role");

    let table = format!("{}.{}", quote_ident(&app), quote_ident(COLLECTION));
    let readable = readable_columns(&schema);
    let readable_list = readable
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let mut writable = readable;
    writable.push(raw_column_name("ssn"));
    let writable_list = writable
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    admin
        .batch_execute(&format!(
            "GRANT USAGE ON SCHEMA {schema} TO {role}; \
             GRANT SELECT ({readable}) ON {table} TO {role}; \
             GRANT INSERT ({writable}) ON {table} TO {role}; \
             GRANT UPDATE ({writable}) ON {table} TO {role}; \
             GRANT DELETE ON {table} TO {role}",
            schema = quote_ident(&app),
            role = quote_ident(&role),
            readable = readable_list,
            writable = writable_list,
        ))
        .await
        .expect("grant fixture privileges");
    (app, role)
}

async fn begin_as_role(session: &Client, role: &str) {
    session
        .batch_execute(&format!("BEGIN; SET LOCAL ROLE {}", quote_ident(role)))
        .await
        .expect("enter fixture role");
}

async fn execute(
    session: &Client,
    query: crate::sql::compiler::CompiledQuery,
) -> Vec<compio_postgres::Row> {
    crate::backend::postgres::params::query(session, query.sql(), query.params())
        .await
        .unwrap_or_else(|error| panic!("registered statement failed: {error}\n{}", query.sql()))
}

async fn teardown(admin: &Client, app: &str, role: &str) {
    admin
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(app)
        ))
        .await
        .expect("drop fixture schema");
    admin
        .batch_execute(&format!("DROP OWNED BY {}", quote_ident(role)))
        .await
        .expect("drop fixture grants");
    admin
        .batch_execute(&format!("DROP ROLE {}", quote_ident(role)))
        .await
        .expect("drop fixture role");
}

#[test]
fn registered_writes_obey_column_scoped_read_grants() {
    Host::test(|host| {
        host.run(async {
            let postgres = crate::tests::fixtures::postgres::Postgres::start();
            let admin = connect(&postgres.url()).await;
            let (app, role) = fixture(&admin, &unique_suffix()).await;
            let schema = people_schema();
            let namespace = SchemaName::new(&app).expect("namespace");
            let registration = SqlRegistration::postgres();
            let session = connect(&postgres.url()).await;
            let raw = raw_column_name("ssn");
            let table = format!("{}.{}", quote_ident(&app), quote_ident(COLLECTION));

            begin_as_role(&session, &role).await;
            let denied = session
                .query_text_params(&format!("SELECT {} FROM {table}", quote_ident(&raw)), &[])
                .await
                .expect_err("raw storage must not be readable");
            assert_eq!(
                denied.code(),
                Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
            );
            session.batch_execute("ROLLBACK").await.unwrap();

            let insert = || {
                crate::crud::insert::build_one(
                    &namespace,
                    COLLECTION,
                    &schema,
                    value!({
                        "id":"person_seed",
                        "nickname":"seed",
                        "ssn":"***",
                        (raw.clone()):"sensitive"
                    }),
                    &registration,
                )
                .expect("compile insert")
            };

            begin_as_role(&session, &role).await;
            let compiled = insert();
            let returning = compiled
                .sql()
                .find(" RETURNING ")
                .expect("insert returns its declared projection");
            let starred = format!("{} RETURNING *", &compiled.sql()[..returning]);
            let parameters = compiled
                .params()
                .iter()
                .map(crate::backend::postgres::params::Parameter)
                .collect::<Vec<_>>();
            let refs = parameters
                .iter()
                .map(|parameter| parameter as &(dyn compio_postgres::types::ToSql + Sync))
                .collect::<Vec<_>>();
            let denied = session
                .query(&starred, &refs)
                .await
                .expect_err("starred returning must require raw-column read access");
            assert_eq!(
                denied.code(),
                Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
            );
            session.batch_execute("ROLLBACK").await.unwrap();

            begin_as_role(&session, &role).await;
            assert_eq!(execute(&session, insert()).await.len(), 1);
            let update = crate::crud::update::build_one(
                &namespace,
                COLLECTION,
                &schema,
                value!({"id":"person_seed"}).into(),
                value!({"nickname":"updated"}),
                &WriteAssignments::default(),
                &registration,
            )
            .expect("compile update");
            assert_eq!(execute(&session, update).await.len(), 1);
            let delete = crate::crud::delete::build_hard(
                &namespace,
                COLLECTION,
                &schema,
                value!({"id":"person_seed"}).into(),
                true,
                &registration,
            )
            .expect("compile delete");
            assert_eq!(execute(&session, delete).await.len(), 1);
            session.batch_execute("COMMIT").await.unwrap();

            let readable = readable_columns(&schema)
                .into_iter()
                .collect::<BTreeSet<_>>();
            assert!(!readable.contains(&raw));
            teardown(&admin, &app, &role).await;
        })
    })
}
