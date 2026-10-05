//! A topic page's journal round trips against the attempt ceiling that bounds
//! them.

use super::*;
use crate::service::{
    delivery::ATTEMPT_IO_CEILING,
    tests::wire::{Recorded, Wire},
};

/// Recipients of the page that sets the baseline.
const FEW: u32 = 2;
/// Recipients of the page measured against it, within one membership list.
const MANY: u32 = 64;

/// A topic page executes the same statements however many recipients it
/// delivers, so it commits within its attempt when every round trip is slow.
///
/// The store reaches its server through a proxy that, while armed, holds every
/// round trip, read or write, before the server sees it and records each
/// statement executed by the journal table it names. A page of [`FEW`]
/// recipients sets the baseline with nothing held. A page of [`MANY`] then runs
/// with each round trip held long enough that four times the baseline's round
/// trips fill [`ATTEMPT_IO_CEILING`], and it must commit inside the ceiling. It
/// must also execute exactly the baseline's statements on every table: one
/// statement more per recipient, on any table, fails that comparison even
/// where the held time alone would still fit. Each page must write one signal,
/// one outbox event and one Advance intent per recipient.
///
/// The connection pool's empty validation query, sent when it hands out a
/// connection idle past its bypass window, executes no statement and is not
/// compared: how many a page meets depends on how long its connections sat.
///
/// Both pages sit within one membership list. A page past one adds a statement
/// per membership list to each membership read, which this does not measure.
#[compio::test]
async fn postgres_topic_page_statements_do_not_grow_with_its_recipients() {
    let fixture = PostgresFixture::start().await;
    let wire = Wire::start(&fixture.journal_url).await;
    let store = Rc::new(Box::pin(orm_store(wire.url())).await);
    let (service, many, few, _deployments) = Box::pin(registered_service(store)).await;
    let worker = WorkerIdentity::new("fanout-latency".into()).unwrap();
    let few = service.fixture_app(few);
    let many = service.fixture_app(many);
    for _ in 0..FEW {
        wait_on_topic(&service, &few, &worker).await;
    }
    for _ in 0..MANY {
        wait_on_topic(&service, &many, &worker).await;
    }
    let (baseline, _) = Box::pin(deliver(&wire, &few, FEW, Duration::ZERO)).await;
    assert!(
        baseline.round_trips > 0,
        "the proxy carried the baseline page"
    );
    let held = ATTEMPT_IO_CEILING / (4 * baseline.round_trips);
    let (measured, elapsed) = Box::pin(deliver(&wire, &many, MANY, held)).await;
    // The control: the proxy held every round trip of the measured page.
    assert!(
        elapsed >= held * measured.round_trips,
        "the page did not wait on its held round trips: {elapsed:?}"
    );
    assert_eq!(
        measured.statements, baseline.statements,
        "a page of {MANY} recipients executed other statements than a page of {FEW} \
         (pool validation queries met: {} and {})",
        measured.empty_queries, baseline.empty_queries
    );
}

/// Broadcast on `scope`'s topic and deliver its one page with every round trip
/// held for `held`, checking the rows the page wrote for its `recipients`.
async fn deliver(
    wire: &Wire,
    scope: &AppWorkflows,
    recipients: u32,
    held: Duration,
) -> (Recorded, Duration) {
    let accepted = broadcast(scope, "measured").await;
    let grant = Grant::new(&job(scope, &accepted.id, 1).await);
    let before = written(scope).await;
    wire.arm(held);
    let started = Instant::now();
    let receipt = scope.fanout_job(&grant, FanoutOptions::default()).await;
    let elapsed = started.elapsed();
    let recorded = wire.disarm();
    assert!(
        receipt
            .expect("the page commits within its first attempt")
            .is_some(),
        "the broadcast heads its topic"
    );
    let after = written(scope).await;
    let delta: [i64; 3] = std::array::from_fn(|table| after[table] - before[table]);
    assert_eq!(
        delta,
        [i64::from(recipients); 3],
        "signals, outbox events and Advance intents the page wrote"
    );
    (recorded, elapsed)
}

/// Rows of the tables a page writes once per recipient: signals, outbox
/// events and publication intents.
async fn written(scope: &AppWorkflows) -> [i64; 3] {
    let tx = scope.service.begin().await.unwrap();
    let filter = json!({"app_id":scope.app_id().as_str()});
    let mut rows = [0; 3];
    for (count, table) in rows
        .iter_mut()
        .zip(["signals", "outbox", "job_publications"])
    {
        *count = journal_count(&tx, table, filter.clone()).await;
    }
    tx.commit().await.unwrap();
    rows
}
