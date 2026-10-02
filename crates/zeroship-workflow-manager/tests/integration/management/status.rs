use super::*;
use std::time::Duration;

case!(
    sqlite_management_status_keeps_unknown_scope_absent,
    postgres_management_status_keeps_unknown_scope_absent,
    unknown_scope
);

async fn unknown_scope(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let request = command(&app, &RunId::mint(), RunOperation::Pause);
    let before = snapshot(&host).await;
    assert_eq!(
        host.coordinator
            .management_receipt(&app, &request.request_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(snapshot(&host).await, before);
    assert!(
        rows(&host.database, "queue_scopes", value!({"id":app.as_str()}))
            .await
            .is_empty()
    );
    let accepted = host.manage(&request).await.unwrap();
    assert_eq!(
        host.coordinator
            .management_receipt(&app, &request.request_id)
            .await
            .unwrap(),
        Some(accepted)
    );
}

#[compio::test]
async fn postgres_management_status_serializes_with_atomic_acceptance() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let host = Host::new(&fixture).await;
    let status_queue = Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options::default(),
        host.holds.clone(),
    )
    .await
    .unwrap();
    let status_coordinator = support::coordinator(&status_queue, CoordinatorOptions::default());
    let app = AppId::mint();
    host.queue.register_scope(&app).await.unwrap();
    let request = command(&app, &RunId::mint(), RunOperation::Pause);
    let support::Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    admin
        .batch_execute("BEGIN; LOCK TABLE workflow_manager.management IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let accepting = host.manage(&request);
    let status = async {
        let accepting_pid = waiting_acceptance(admin).await;
        let release = async {
            wait_for_status(admin).await;
            let serialized: bool = admin.query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid \
                 WHERE a.usename='workflow_manager_test' AND NOT l.granted AND l.locktype='transactionid' \
                 AND $1=ANY(pg_blocking_pids(a.pid)))", &[&accepting_pid],
            ).await.unwrap().get(0);
            admin.batch_execute("COMMIT").await.unwrap();
            serialized
        };
        let (receipt, serialized) = futures::join!(
            status_coordinator.management_receipt(&app, &request.request_id),
            release,
        );
        (receipt, serialized)
    };
    let (accepted, (receipt, serialized)) = futures::join!(accepting, status);
    assert!(
        serialized,
        "status must wait for the accepting app lock before reading command/job anchors"
    );
    assert_eq!(receipt.unwrap(), Some(accepted.unwrap()));
    assert_eq!(
        rows(&host.database, "management", value!({})).await.len(),
        1
    );
    assert_eq!(rows(&host.database, "jobs", value!({})).await.len(), 1);
    assert_eq!(host.holds.acquired.get(), 0);
}

async fn waiting_acceptance(admin: &compio_postgres::Client) -> i32 {
    compio::time::timeout(Duration::from_secs(3), async {
        loop {
            admin.query_one("SELECT pg_stat_clear_snapshot()", &[]).await.unwrap();
            let waiting = admin.query(
                "SELECT a.pid FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid \
                 WHERE a.usename='workflow_manager_test' AND NOT l.granted AND l.locktype='relation' \
                 AND l.relation='workflow_manager.management'::regclass \
                 AND pg_backend_pid()=ANY(pg_blocking_pids(a.pid))", &[],
            ).await.unwrap();
            if let [row] = waiting.as_slice() { return row.get(0); }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("acceptance must reach the administrator's management table lock")
}

async fn wait_for_status(admin: &compio_postgres::Client) {
    compio::time::timeout(Duration::from_secs(3), async {
        loop {
            admin.query_one("SELECT pg_stat_clear_snapshot()", &[]).await.unwrap();
            let count: i64 = admin.query_one(
                "SELECT count(DISTINCT a.pid) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid \
                 WHERE a.usename='workflow_manager_test' AND NOT l.granted", &[],
            ).await.unwrap().get(0);
            if count >= 2 { break; }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("status must reach a database wait while acceptance owns the app lock");
}
