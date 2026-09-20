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
    let (_, policy) = app::lock_app(&mut tx, &app_id).await.unwrap();
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

#[compio::test]
async fn sqlite_a_checkpoint_carrying_a_signal_consumption_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    forged_consumption_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_a_checkpoint_carrying_a_signal_consumption_is_refused() {
    let fixture = PostgresFixture::start().await;
    forged_consumption_contract(Rc::new(fixture.store.clone())).await;
}

/// The worker runs creator code, so a checkpoint is creator-supplied input. The
/// service is the only writer of `consumed_signal_id`, and a checkpoint that
/// arrives carrying one is claiming a consumption the service never performed.
/// Accepting it would journal a satisfied wait while the message stays on the
/// queue for another wait to take, so the claim is refused and the identical
/// checkpoint without it is what the service fills in for itself.
async fn forged_consumption_contract(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let started = service
        .fixture_app(app_id.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let mut tx = service.begin().await.unwrap();
    let (_, policy) = app::lock_app(&mut tx, &app_id).await.unwrap();
    let run = app::lock_run(&mut tx, &app_id, &started.id).await.unwrap();
    let now = tx.now().await.unwrap();
    let delivered = signals::deliver(
        &mut tx,
        &app_id,
        &started.id,
        &SignalOptions {
            signal_type: "ready".into(),
            payload: json!("go"),
        },
        "app",
        now,
    )
    .await
    .unwrap();
    assert_eq!(
        unconsumed(&tx, &app_id, &started.id).await,
        std::collections::BTreeSet::from([delivered.id.clone()])
    );
    let wait = |consumed: Option<String>| {
        let mut checkpoint = StepCheckpoint::completed_run(0, "wait-ready", json!(null));
        checkpoint.kind = "wait_signal".into();
        checkpoint.state = "running".into();
        checkpoint.output = None;
        checkpoint.signal_type = Some("ready".into());
        checkpoint.wake_at = Some(chrono::DateTime::from_timestamp_millis(now).unwrap());
        checkpoint.consumed_signal_id = consumed;
        vec![checkpoint]
    };

    let claimed = journal::append(
        &mut tx,
        &app_id,
        &run,
        &policy,
        wait(Some(delivered.id.clone())),
        now,
    )
    .await;
    assert!(matches!(
        claimed,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    assert!(journal::load(&mut tx, &app_id, &started.id, 0)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        unconsumed(&tx, &app_id, &started.id).await,
        std::collections::BTreeSet::from([delivered.id.clone()])
    );

    // The control differs from the refused submission in that field alone, so
    // the refusal is attributable to the claim and not to the wait's shape.
    journal::append(&mut tx, &app_id, &run, &policy, wait(None), now)
        .await
        .unwrap();
    assert!(journal::resolve(&mut tx, &app_id, &run, now).await.unwrap());
    let history = journal::load(&mut tx, &app_id, &started.id, 0)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].state, "completed");
    assert_eq!(
        history[0].consumed_signal_id.as_deref(),
        Some(delivered.id.as_str())
    );
    assert!(unconsumed(&tx, &app_id, &started.id).await.is_empty());
    tx.commit().await.unwrap();
}

/// The messages delivered to `run` that no generation has consumed yet.
async fn unconsumed(
    tx: &Transaction,
    app: &AppId,
    run: &str,
) -> std::collections::BTreeSet<String> {
    tx.database()
        .entity::<models::signals::Entity>()
        .unwrap()
        .find::<models::SignalMessage>(
            models::signals::app_id
                .eq(app.as_str())
                .unwrap()
                .and(models::signals::run_id.eq(run).unwrap())
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
        .map(|row| row.id)
        .collect()
}
