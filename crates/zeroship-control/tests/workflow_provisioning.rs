use crate::workflow_postgres::Database;
use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_workflow::store::pg::PgStore;

#[compio::test]
async fn concurrent_control_and_worker_provisioning_preserves_the_journal() {
    let database = Database::new();
    let app = Uuid::new_v4();
    let (admin, connection) = connect(&database.url(), NoTls).await.unwrap();
    let driver = compio::runtime::spawn(connection.run());
    zeroship_migrate_server::provisioning::provision_workflow_journal_schema(&admin, &app)
        .await
        .unwrap();
    let tables = PgStore::provision(&admin, &app).await.unwrap();
    admin
        .batch_execute(&format!(
            "COMMENT ON TABLE {} IS 'journal must survive provisioning'",
            tables.runs
        ))
        .await
        .unwrap();

    let attempts = futures::future::join_all((0..8).map(|index| {
        let mut dsn = url::Url::parse(&database.url()).unwrap();
        let role = if index % 2 == 0 {
            "zeroship_control"
        } else {
            "zeroship_worker"
        };
        dsn.set_username(role).unwrap();
        dsn.set_password(Some(role)).unwrap();
        async move {
            let (client, connection) = connect(dsn.as_str(), NoTls).await.unwrap();
            let driver = compio::runtime::spawn(connection.run());
            let mut results = Vec::new();
            for _ in 0..8 {
                results.push(PgStore::provision(&client, &app).await);
            }
            let current: String = client
                .query_one("SELECT current_user", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(current, role, "provisioning must restore the caller's role");
            drop(client);
            driver.await.unwrap().unwrap();
            results
        }
    }))
    .await;
    for result in attempts.into_iter().flatten() {
        result.expect("concurrent journal provisioning must not collide on catalog writes");
    }
    let row = admin.query_one(
        "SELECT obj_description($1::text::regclass, 'pg_class'), pg_get_userbyid(relowner) FROM pg_class WHERE oid = $1::text::regclass",
        &[&tables.runs],
    ).await.unwrap();
    assert_eq!(row.get::<_, String>(0), "journal must survive provisioning");
    assert_eq!(row.get::<_, String>(1), "zeroship_workflow_owner");
    drop(admin);
    driver.await.unwrap().unwrap();
}
