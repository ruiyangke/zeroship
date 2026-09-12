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

#[compio::test]
async fn provisioning_waits_without_blocking_an_active_journal_transaction() {
    let database = Database::new();
    let app = Uuid::new_v4();
    let (admin, connection) = connect(&database.url(), NoTls).await.unwrap();
    let admin_driver = compio::runtime::spawn(connection.run());
    zeroship_migrate_server::provisioning::provision_workflow_journal_schema(&admin, &app)
        .await
        .unwrap();
    let tables = PgStore::provision(&admin, &app).await.unwrap();
    let (mut active, connection) = connect(&database.url(), NoTls).await.unwrap();
    let active_driver = compio::runtime::spawn(connection.run());
    let tx = active.transaction().await.unwrap();
    tx.query(&format!("SELECT id FROM {} FOR UPDATE", tables.runs), &[])
        .await
        .unwrap();

    let (provisioner, connection) = connect(&database.url(), NoTls).await.unwrap();
    let provision_driver = compio::runtime::spawn(connection.run());
    let pid: i32 = provisioner
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let provision = compio::runtime::spawn(async move {
        let result = PgStore::provision(&provisioner, &app).await;
        drop(provisioner);
        provision_driver.await.unwrap().unwrap();
        result
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: bool = admin.query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid = $1 AND relation = $2::text::regclass AND NOT granted)",
            &[&pid, &tables.runs],
        ).await.unwrap().get(0);
        if waiting {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "provisioning must wait for the active journal transaction"
        );
        compio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tx.execute(&format!("UPDATE {} SET state = state", tables.runs), &[])
        .await
        .expect("an active transaction must finish its update while provisioning waits");
    tx.commit().await.unwrap();
    provision
        .await
        .unwrap()
        .expect("provision after the active transaction releases its locks");
    drop(active);
    drop(admin);
    active_driver.await.unwrap().unwrap();
    admin_driver.await.unwrap().unwrap();
}
