use super::*;

case!(
    sqlite_worker_draining_can_be_the_first_registration,
    postgres_worker_draining_can_be_the_first_registration,
    initially_draining
);
case!(
    sqlite_worker_draining_is_terminal_after_expiry_and_preserves_assignments,
    postgres_worker_draining_is_terminal_after_expiry_and_preserves_assignments,
    terminal_draining
);
case!(
    sqlite_worker_draining_cannot_be_resurrected_by_concurrent_registration,
    postgres_worker_draining_cannot_be_resurrected_by_concurrent_registration,
    concurrent_draining
);

fn registration(state: WorkerState, capacity: u32) -> RegisterWorker {
    RegisterWorker {
        capacity: NonZeroU32::new(capacity).unwrap(),
        state,
    }
}

async fn assert_ready_refused(coordinator: &Coordinator, database: &Database, worker: &WorkerId) {
    let filter = value!({"id":worker.as_str()});
    let before = row(database, "workers", filter.clone()).await;
    let assigned = rows(
        database,
        "assignments",
        value!({"worker_id":worker.as_str()}),
    )
    .await;
    assert_eq!(
        coordinator
            .register(worker, &registration(WorkerState::Ready, 9))
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(row(database, "workers", filter).await, before);
    assert_eq!(
        rows(
            database,
            "assignments",
            value!({"worker_id":worker.as_str()}),
        )
        .await,
        assigned
    );
}

async fn initially_draining(fixture: &Fixture) {
    let (left, _) = host(fixture, Options::default()).await;
    let (right, _) = host(fixture, Options::default()).await;
    let database = fixture.database().await;
    let worker = WorkerId::mint();
    let request = registration(WorkerState::Draining, 2);
    let drained = left.register(&worker, &request).await.unwrap();
    assert_eq!(drained.worker_id, worker);
    assert_eq!(drained.state, WorkerState::Draining);
    assert_eq!(drained.capacity, request.capacity);
    assert!(drained.expires_at.get() > 0);
    assert_ready_refused(&right, &database, &worker).await;
    assert!(left.ready_workers(None).await.unwrap().is_empty());

    let replacement = WorkerId::mint();
    register(&right, &replacement, 1).await;
    let ready = left.ready_workers(None).await.unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].worker_id, replacement);
    assert_ready_refused(&left, &database, &worker).await;
}

async fn terminal_draining(fixture: &Fixture) {
    let (left, _) = host(fixture, Options::default()).await;
    let (right, _) = host(fixture, Options::default()).await;
    let database = fixture.database().await;
    let worker = WorkerId::mint();
    register(&left, &worker, 2).await;
    let assignment = place(&left, &AppId::mint()).await;
    let assignment_filter = value!({"worker_id":worker.as_str()});
    let assigned = row(&database, "assignments", assignment_filter.clone()).await;
    let filter = value!({"id":worker.as_str()});
    update(
        &database,
        "workers",
        filter.clone(),
        value!({"lock_version":7}),
    )
    .await;
    let request = registration(WorkerState::Draining, 3);
    let drained = right.register(&worker, &request).await.unwrap();
    assert_eq!(drained.state, WorkerState::Draining);
    assert_eq!(drained.capacity, request.capacity);
    assert_ready_refused(&left, &database, &worker).await;
    assert_eq!(
        left.assignments(&worker, None).await.unwrap(),
        vec![assignment]
    );
    // A draining registration is no longer a placement candidate.
    assert!(matches!(
        left.place(&AppId::mint()).await.unwrap(),
        Placed::Unplaced(_)
    ));

    let repeated = left.register(&worker, &request).await.unwrap();
    assert_eq!(repeated.worker_id, worker);
    assert_eq!(repeated.state, WorkerState::Draining);
    assert_eq!(repeated.capacity, request.capacity);
    assert!(repeated.expires_at >= drained.expires_at);
    assert_ready_refused(&right, &database, &worker).await;

    update(
        &database,
        "workers",
        filter.clone(),
        value!({"expires_at":0}),
    )
    .await;
    assert_ready_refused(&left, &database, &worker).await;
    let renewed = right.register(&worker, &request).await.unwrap();
    assert_eq!(renewed.state, WorkerState::Draining);
    assert_eq!(renewed.capacity, request.capacity);
    assert!(renewed.expires_at.get() > 0);
    assert_ready_refused(&left, &database, &worker).await;
    let stored = row(&database, "workers", filter).await;
    assert_eq!(stored["id"], value!(worker.as_str()));
    assert_eq!(stored["lock_version"], value!(7));
    assert_eq!(
        row(&database, "assignments", assignment_filter).await,
        assigned
    );
    assert!(left.ready_workers(None).await.unwrap().is_empty());
}

async fn concurrent_draining(fixture: &Fixture) {
    let hosts = futures::future::join_all((0..4).map(|_| host(fixture, Options::default())))
        .await
        .into_iter()
        .map(|(coordinator, _)| coordinator)
        .collect::<Vec<_>>();
    let database = fixture.database().await;
    let requests = [
        registration(WorkerState::Ready, 1),
        registration(WorkerState::Draining, 3),
        registration(WorkerState::Ready, 2),
        registration(WorkerState::Draining, 4),
    ];
    for initial in [None, Some(WorkerState::Ready), Some(WorkerState::Draining)] {
        let worker = WorkerId::mint();
        let filter = value!({"id":worker.as_str()});
        if let Some(state) = initial {
            hosts[0]
                .register(&worker, &registration(state, 2))
                .await
                .unwrap();
            if state == WorkerState::Ready {
                place(&hosts[0], &AppId::mint()).await;
            }
            update(
                &database,
                "workers",
                filter.clone(),
                value!({"lock_version":7}),
            )
            .await;
        }
        let assigned = rows(
            &database,
            "assignments",
            value!({"worker_id":worker.as_str()}),
        )
        .await;
        let results = registration_race(fixture, &hosts, &worker, &requests).await;
        for (request, result) in requests.iter().zip(results) {
            match result {
                Ok(registered) => {
                    assert!(
                        initial != Some(WorkerState::Draining)
                            || request.state == WorkerState::Draining,
                        "a committed drain must refuse every later Ready request"
                    );
                    assert_eq!(registered.worker_id, worker);
                    assert_eq!(registered.state, request.state);
                    assert_eq!(registered.capacity, request.capacity);
                }
                Err(error) => {
                    assert_eq!(request.state, WorkerState::Ready);
                    assert_eq!(error, Error::Conflict);
                }
            }
        }
        let stored = row(&database, "workers", filter).await;
        assert_eq!(stored["id"], value!(worker.as_str()));
        assert_eq!(stored["state"], value!("draining"));
        assert!(stored["capacity"] == value!(3) || stored["capacity"] == value!(4));
        assert_eq!(
            stored["lock_version"],
            value!(if initial.is_some() { 7 } else { 0 })
        );
        assert_eq!(
            rows(
                &database,
                "assignments",
                value!({"worker_id":worker.as_str()}),
            )
            .await,
            assigned
        );
        for coordinator in &hosts {
            assert_ready_refused(coordinator, &database, &worker).await;
        }
    }
    assert!(hosts[0].ready_workers(None).await.unwrap().is_empty());
}
