use super::*;
use crate::{
    engine::StepCheckpoint,
    service::{app, journal, models, signals},
};
use zeroship_data_orm::orm::FindOptions;

#[compio::test]
async fn sqlite_signal_window_includes_its_edges_and_consumes_each_message_once() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    window_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_signal_window_includes_its_edges_and_consumes_each_message_once() {
    let fixture = PostgresFixture::start().await;
    window_contract(Rc::new(fixture.store.clone())).await;
}

async fn window_contract(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let started = service
        .fixture_app(app_id.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    let policy = app::lock_app(&mut tx, &app_id).await.unwrap();
    let run = app::lock_run(&mut tx, &app_id, &started.id).await.unwrap();
    let now = tx.now().await.unwrap();
    let age = 100;
    let oldest = now - age;
    let mut ignored = std::collections::BTreeSet::new();
    for timestamp in [oldest - 1, oldest, now, now + 1] {
        let delivered = signals::deliver(
            &mut tx,
            &app_id,
            &started.id,
            &SignalOptions {
                signal_type: "ready".into(),
                payload: json!(timestamp),
            },
            "app",
            timestamp,
        )
        .await
        .unwrap();
        if timestamp < oldest || timestamp > now {
            ignored.insert((delivered.id, timestamp));
        }
    }
    let checkpoints = (0..3)
        .map(|ordinal| {
            let mut checkpoint =
                StepCheckpoint::completed_run(ordinal, format!("wait-{ordinal}"), json!(null));
            checkpoint.kind = "wait_signal".into();
            checkpoint.state = "running".into();
            checkpoint.output = None;
            checkpoint.signal_type = Some("ready".into());
            checkpoint.max_signal_age_ms = Some(age);
            checkpoint.wake_at = Some(chrono::DateTime::from_timestamp_millis(now).unwrap());
            checkpoint
        })
        .collect();
    journal::append(&mut tx, &app_id, &run, &policy, checkpoints, now)
        .await
        .unwrap();
    assert!(journal::resolve(&mut tx, &app_id, &run, now).await.unwrap());
    let history = journal::load(&mut tx, &app_id, &started.id, 0)
        .await
        .unwrap();
    assert_eq!(history.len(), 3);
    for (step, timestamp) in history.iter().zip([oldest, now]) {
        assert_eq!(step.state, "completed");
        assert_eq!(step.output.as_ref().unwrap()["payload"], json!(timestamp));
        assert!(step.consumed_signal_id.is_some());
    }
    assert_eq!(history[2].state, "completed");
    assert_eq!(
        serde_json::to_value(&history[2]).unwrap()["output"],
        json!(null)
    );
    assert!(history[2].consumed_signal_id.is_none());
    let pending = tx
        .database()
        .entity::<models::signals::Entity>()
        .unwrap()
        .find::<models::SignalMessage>(
            models::signals::app_id
                .eq(app_id.as_str())
                .unwrap()
                .and(models::signals::run_id.eq(started.id.as_str()).unwrap())
                .and(
                    models::signals::consumed_generation
                        .eq(None::<i64>)
                        .unwrap(),
                ),
            FindOptions::default(),
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.id, row.created_at))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(pending, ignored);
    assert!(!journal::resolve(&mut tx, &app_id, &run, now).await.unwrap());
    tx.commit().await.unwrap();
}
