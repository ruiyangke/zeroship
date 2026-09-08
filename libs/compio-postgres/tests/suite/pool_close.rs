//! Live coverage for graceful pool shutdown.

use compio_postgres::{Client, Error, Pool, PoolConfig};
use futures_channel::oneshot;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

fn test_url() -> String {
    common::test_url()
}

fn config(max_size: usize, min_idle: usize) -> PoolConfig {
    let mut config = PoolConfig::new();
    config
        .max_size(max_size)
        .min_idle(min_idle)
        .acquire_timeout(Duration::from_secs(30))
        .validation_bypass(Duration::from_secs(60));
    config
}

async fn connect_pool(url: &str, config: PoolConfig) -> Pool {
    Pool::connect_with_pool_config(url, config)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error))
}

fn assert_pool_closed(error: &Error) {
    assert!(
        error.is_pool_closed(),
        "expected the pool-closed error, got {error:?}"
    );
}

struct WakeCounter(Arc<AtomicUsize>);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn counting_waker(counter: &Arc<AtomicUsize>) -> Waker {
    Waker::from(Arc::new(WakeCounter(Arc::clone(counter))))
}

fn poll_with_waker<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
    let mut context = Context::from_waker(waker);
    future.poll(&mut context)
}

fn assert_drained(pool: &Pool) {
    assert_eq!(pool.active_count(), 0, "closed pool retained a borrower");
    assert_eq!(pool.idle_count(), 0, "closed pool retained an idle entry");
    assert_eq!(
        pool.total_count(),
        0,
        "closed pool retained a capacity slot"
    );
    assert_eq!(
        pool.pending_count(),
        0,
        "closed pool retained a FIFO waiter"
    );
}

async fn abandon_running_query(client: &Client, observer: &Pool) {
    // PostgreSQL must check for a disconnected client while executing the
    // query; otherwise pg_sleep can finish before it observes the closed socket.
    client
        .batch_execute("SET client_connection_check_interval = '10ms'")
        .await
        .unwrap();
    let pid = client.process_id();
    let query = std::pin::pin!(client.batch_execute("SELECT pg_sleep(30)"));
    let observed =
        std::pin::pin!(async {
            loop {
                let rows = observer.query(
                "SELECT pid FROM pg_stat_activity WHERE pid = $1 AND wait_event = 'PgSleep'",
                &[&pid],
            ).await.unwrap();
                if !rows.is_empty() {
                    break;
                }
                compio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    let outcome = compio::time::timeout(
        Duration::from_secs(3),
        futures_util::future::select(observed, query),
    )
    .await
    .expect("query never reached PostgreSQL");
    assert!(
        matches!(outcome, futures_util::future::Either::Left(_)),
        "query ended before disposal could be exercised"
    );
}

async fn discarded_lease_closes_session_and_wakes_waiter(owned: bool) {
    let release_calls = Rc::new(Cell::new(0));
    let calls = Rc::clone(&release_calls);
    let mut config = config(1, 1);
    config.after_release(move |_| {
        calls.set(calls.get() + 1);
        true
    });
    let pool = Rc::new(connect_pool(&test_url(), config).await);
    let observer = connect_pool(&test_url(), PoolConfig::new()).await;
    let (pid, discard): (i32, Box<dyn FnOnce() + '_>) = if owned {
        let client = pool.get_owned().await.unwrap();
        abandon_running_query(&client, &observer).await;
        (client.process_id(), Box::new(move || client.discard()))
    } else {
        let client = pool.get().await.unwrap();
        abandon_running_query(&client, &observer).await;
        (client.process_id(), Box::new(move || client.discard()))
    };
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(&wakes);
    let mut waiter = Box::pin(pool.get());
    assert!(poll_with_waker(waiter.as_mut(), &waker).is_pending());
    assert_eq!(pool.pending_count(), 1);
    discard();
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.total_count(), 0);
    assert_eq!(release_calls.get(), 0, "disposal ran a reuse hook");
    assert!(
        wakes.load(Ordering::Relaxed) > 0,
        "disposal stranded a waiter"
    );
    let replacement = waiter.await.unwrap();
    assert_ne!(
        replacement.process_id(),
        pid,
        "discarded session was reused"
    );
    compio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = observer
                .query("SELECT pid FROM pg_stat_activity WHERE pid = $1", &[&pid])
                .await
                .unwrap();
            if rows.is_empty() {
                break;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("discard left the abandoned query running on the server");
    assert_eq!(
        replacement
            .query_one("SELECT 42", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        42
    );
    drop(replacement);
    assert_eq!(
        release_calls.get(),
        1,
        "ordinary return skipped its reuse hook"
    );
    pool.close().await;
    assert_drained(&pool);
}

#[compio::test]
async fn discarding_a_borrowed_lease_stops_its_query_and_releases_capacity() {
    discarded_lease_closes_session_and_wakes_waiter(false).await;
}

#[compio::test]
async fn discarding_an_owned_lease_stops_its_query_and_releases_capacity() {
    discarded_lease_closes_session_and_wakes_waiter(true).await;
}

#[compio::test]
async fn acquire_after_close_fails_immediately_with_pool_closed_error() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;

    pool.close().await;
    assert!(pool.is_closed());
    assert_drained(&pool);

    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(&wake_count);
    let mut acquire = Box::pin(pool.get());
    match poll_with_waker(acquire.as_mut(), &waker) {
        Poll::Ready(Err(error)) => assert_pool_closed(&error),
        Poll::Ready(Ok(_)) => panic!("closed pool handed out a connection"),
        Poll::Pending => panic!("acquire after close parked instead of failing immediately"),
    }
    assert_eq!(
        pool.metrics.timeouts.get(),
        0,
        "close was reported as a timeout"
    );
}

#[compio::test]
async fn close_wakes_every_parked_fifo_waiter_with_pool_closed_error() {
    let url = test_url();
    let pool = Rc::new(connect_pool(&url, config(1, 1)).await);
    let held = pool.get().await.unwrap();

    let first_wakes = Arc::new(AtomicUsize::new(0));
    let second_wakes = Arc::new(AtomicUsize::new(0));
    let first_waker = counting_waker(&first_wakes);
    let second_waker = counting_waker(&second_wakes);
    let mut first = Box::pin(pool.get());
    let mut second = Box::pin(pool.get());
    assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
    assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
    assert_eq!(pool.pending_count(), 2);
    assert_eq!(first_wakes.load(Ordering::Relaxed), 0);
    assert_eq!(second_wakes.load(Ordering::Relaxed), 0);

    let close_wakes = Arc::new(AtomicUsize::new(0));
    let close_waker = counting_waker(&close_wakes);
    let mut close = Box::pin(pool.close());
    assert!(poll_with_waker(close.as_mut(), &close_waker).is_pending());
    assert_eq!(close_wakes.load(Ordering::Relaxed), 0);

    assert!(pool.is_closed());
    assert_eq!(pool.pending_count(), 0, "close left FIFO waiters queued");
    assert!(
        first_wakes.load(Ordering::Relaxed) > 0,
        "first waiter was not woken"
    );
    assert!(
        second_wakes.load(Ordering::Relaxed) > 0,
        "second waiter was not woken"
    );

    for (name, mut acquire, waker) in [
        ("first", first, first_waker),
        ("second", second, second_waker),
    ] {
        match poll_with_waker(acquire.as_mut(), &waker) {
            Poll::Ready(Err(error)) => assert_pool_closed(&error),
            Poll::Ready(Ok(_)) => panic!("{name} waiter acquired after close"),
            Poll::Pending => panic!("{name} waiter stayed parked after close"),
        }
    }
    assert_eq!(
        pool.metrics.timeouts.get(),
        0,
        "waiters were reported as timeouts"
    );
    assert_eq!(pool.active_count(), 1);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.total_count(), 1);

    drop(held);
    assert!(
        close_wakes.load(Ordering::Relaxed) > 0,
        "last return did not wake close"
    );
    assert!(poll_with_waker(close.as_mut(), &close_waker).is_ready());
    assert_drained(&pool);
}

#[compio::test]
async fn close_waits_for_a_borrower_and_finishes_when_it_is_dropped() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;
    let held = pool.get().await.unwrap();

    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(&wakes);
    let mut close = Box::pin(pool.close());
    assert!(
        poll_with_waker(close.as_mut(), &waker).is_pending(),
        "close returned while its borrower was still held"
    );
    assert_eq!(wakes.load(Ordering::Relaxed), 0);
    assert_eq!(pool.active_count(), 1);
    assert_eq!(pool.total_count(), 1);

    drop(held);
    assert!(
        wakes.load(Ordering::Relaxed) > 0,
        "borrower return did not wake close"
    );
    assert!(poll_with_waker(close.as_mut(), &waker).is_ready());
    assert_drained(&pool);
}

#[compio::test]
async fn close_discards_every_idle_entry_and_its_capacity_slot() {
    let url = test_url();
    let pool = connect_pool(&url, config(3, 3)).await;
    assert_eq!(pool.idle_count(), 3);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.total_count(), 3);

    pool.close().await;

    assert_drained(&pool);
    assert_eq!(
        pool.metrics.evictions.get(),
        0,
        "intentional shutdown was counted as unhealthy eviction"
    );
}

#[compio::test]
async fn concurrent_and_repeated_close_calls_are_idempotent() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;
    let held = pool.get().await.unwrap();

    let first_wakes = Arc::new(AtomicUsize::new(0));
    let second_wakes = Arc::new(AtomicUsize::new(0));
    let first_waker = counting_waker(&first_wakes);
    let second_waker = counting_waker(&second_wakes);
    let mut first = Box::pin(pool.close());
    let mut second = Box::pin(pool.close());
    assert!(poll_with_waker(first.as_mut(), &first_waker).is_pending());
    assert!(poll_with_waker(second.as_mut(), &second_waker).is_pending());
    assert_eq!(first_wakes.load(Ordering::Relaxed), 0);
    assert_eq!(second_wakes.load(Ordering::Relaxed), 0);

    drop(held);
    assert!(first_wakes.load(Ordering::Relaxed) > 0);
    assert!(second_wakes.load(Ordering::Relaxed) > 0);
    assert!(poll_with_waker(first.as_mut(), &first_waker).is_ready());
    assert!(poll_with_waker(second.as_mut(), &second_waker).is_ready());
    drop(first);
    drop(second);

    pool.close().await;
    assert!(pool.is_closed());
    assert_drained(&pool);
}

#[compio::test]
async fn close_discards_an_entry_already_assigned_to_a_waiter() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;
    let held = pool.get().await.unwrap();

    let waiter_wakes = Arc::new(AtomicUsize::new(0));
    let waiter_waker = counting_waker(&waiter_wakes);
    let mut waiter = Box::pin(pool.get());
    assert!(poll_with_waker(waiter.as_mut(), &waiter_waker).is_pending());
    assert_eq!(pool.pending_count(), 1);
    assert_eq!(waiter_wakes.load(Ordering::Relaxed), 0);

    drop(held);
    assert_eq!(
        pool.pending_count(),
        0,
        "returned entry was not assigned directly"
    );
    assert_eq!(pool.idle_count(), 0, "assigned entry leaked into idle");
    assert_eq!(pool.active_count(), 0);
    assert_eq!(
        pool.total_count(),
        1,
        "assigned entry lost its accounting slot"
    );
    assert!(waiter_wakes.load(Ordering::Relaxed) > 0);

    pool.close().await;
    assert_drained(&pool);

    match poll_with_waker(waiter.as_mut(), &waiter_waker) {
        Poll::Ready(Err(error)) => assert_pool_closed(&error),
        Poll::Ready(Ok(_)) => panic!("waiter received an entry shutdown had discarded"),
        Poll::Pending => panic!("assigned waiter stayed parked after close"),
    }
}

#[compio::test]
async fn after_release_is_skipped_when_a_borrower_returns_during_close() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let hook_calls = Rc::clone(&calls);
    let mut config = config(1, 1);
    config.after_release(move |_client| {
        hook_calls.set(hook_calls.get() + 1);
        true
    });
    let pool = connect_pool(&url, config).await;
    let held = pool.get().await.unwrap();

    let waker = Waker::noop();
    let mut close = Box::pin(pool.close());
    assert!(poll_with_waker(close.as_mut(), waker).is_pending());
    drop(held);

    assert_eq!(
        calls.get(),
        0,
        "after_release ran even though shutdown had already chosen discard"
    );
    assert!(poll_with_waker(close.as_mut(), waker).is_ready());
    assert_drained(&pool);
}

#[compio::test]
async fn close_interrupts_acquisition_without_waiting_for_its_hook() {
    let url = test_url();
    let entered = Rc::new(Cell::new(false));
    let hook_entered = Rc::clone(&entered);
    let (release_tx, release_rx) = oneshot::channel();
    let release_rx = Rc::new(RefCell::new(Some(release_rx)));
    let hook_release_rx = Rc::clone(&release_rx);
    let mut config = config(1, 1);
    config.before_acquire(move |_client| {
        let hook_entered = Rc::clone(&hook_entered);
        let release_rx = hook_release_rx
            .borrow_mut()
            .take()
            .expect("before_acquire ran more than once");
        Box::pin(async move {
            hook_entered.set(true);
            let _ = release_rx.await;
            Ok(true)
        })
    });
    let pool = connect_pool(&url, config).await;

    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(&wakes);
    let mut acquire = Box::pin(pool.get());
    assert!(poll_with_waker(acquire.as_mut(), &waker).is_pending());
    assert!(entered.get(), "checkout never entered before_acquire");
    assert_eq!(wakes.load(Ordering::Relaxed), 0);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(
        pool.total_count(),
        1,
        "hook candidate lost its capacity slot"
    );

    pool.close().await;
    assert!(pool.is_closed());
    assert_eq!(
        pool.total_count(),
        1,
        "close waited for or stole a non-borrowed hook candidate"
    );

    assert!(
        wakes.load(Ordering::Relaxed) > 0,
        "pool close did not wake the acquisition parked in its hook"
    );
    match poll_with_waker(acquire.as_mut(), &waker) {
        Poll::Ready(Err(error)) => assert_pool_closed(&error),
        Poll::Ready(Ok(_)) => panic!("hook candidate became active after close"),
        Poll::Pending => panic!("closed pool kept waiting for its hook"),
    }
    assert!(
        release_tx.send(()).is_err(),
        "closed acquisition retained its hook"
    );
    assert_eq!(pool.metrics.timeouts.get(), 0);
    assert_drained(&pool);
}

#[compio::test]
async fn close_accounts_for_active_idle_and_assigned_entries_together() {
    let url = test_url();
    let pool = connect_pool(&url, config(3, 3)).await;
    let first = pool.get().await.unwrap();
    let second = pool.get().await.unwrap();
    let last = pool.get().await.unwrap();
    assert_eq!(pool.active_count(), 3);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.total_count(), 3);

    let waiter_wakes = Arc::new(AtomicUsize::new(0));
    let waiter_waker = counting_waker(&waiter_wakes);
    let mut waiter = Box::pin(pool.get());
    assert!(poll_with_waker(waiter.as_mut(), &waiter_waker).is_pending());
    assert_eq!(waiter_wakes.load(Ordering::Relaxed), 0);
    assert_eq!(pool.pending_count(), 1);

    drop(first);
    assert!(waiter_wakes.load(Ordering::Relaxed) > 0);
    assert_eq!(pool.pending_count(), 0);
    assert_eq!(pool.active_count(), 2);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.total_count(), 3);

    drop(second);
    assert_eq!(pool.active_count(), 1);
    assert_eq!(pool.idle_count(), 1);
    assert_eq!(pool.total_count(), 3);

    let close_wakes = Arc::new(AtomicUsize::new(0));
    let close_waker = counting_waker(&close_wakes);
    let mut close = Box::pin(pool.close());
    assert!(poll_with_waker(close.as_mut(), &close_waker).is_pending());
    assert_eq!(close_wakes.load(Ordering::Relaxed), 0);
    assert_eq!(pool.pending_count(), 0);
    assert_eq!(pool.active_count(), 1);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(
        pool.total_count(),
        1,
        "close did not subtract idle and assigned slots exactly once"
    );

    match poll_with_waker(waiter.as_mut(), &waiter_waker) {
        Poll::Ready(Err(error)) => assert_pool_closed(&error),
        Poll::Ready(Ok(_)) => panic!("assigned waiter acquired during mixed close"),
        Poll::Pending => panic!("assigned waiter stayed pending during mixed close"),
    }

    drop(last);
    assert!(close_wakes.load(Ordering::Relaxed) > 0);
    assert!(poll_with_waker(close.as_mut(), &close_waker).is_ready());
    assert_drained(&pool);
}

#[compio::test]
async fn cancelling_close_leaves_the_pool_closed_and_a_later_close_resumes() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;
    let held = pool.get().await.unwrap();

    let cancelled_wakes = Arc::new(AtomicUsize::new(0));
    let cancelled_waker = counting_waker(&cancelled_wakes);
    let mut cancelled = Box::pin(pool.close());
    assert!(poll_with_waker(cancelled.as_mut(), &cancelled_waker).is_pending());
    assert_eq!(cancelled_wakes.load(Ordering::Relaxed), 0);
    drop(cancelled);
    assert!(pool.is_closed());

    let error = pool
        .get()
        .await
        .expect_err("cancelled close reopened the pool");
    assert_pool_closed(&error);

    let resumed_wakes = Arc::new(AtomicUsize::new(0));
    let resumed_waker = counting_waker(&resumed_wakes);
    let mut resumed = Box::pin(pool.close());
    assert!(poll_with_waker(resumed.as_mut(), &resumed_waker).is_pending());
    assert_eq!(resumed_wakes.load(Ordering::Relaxed), 0);

    drop(held);
    assert_eq!(
        cancelled_wakes.load(Ordering::Relaxed),
        0,
        "cancelled close left a stale wake registration"
    );
    assert!(resumed_wakes.load(Ordering::Relaxed) > 0);
    assert!(poll_with_waker(resumed.as_mut(), &resumed_waker).is_ready());
    assert_drained(&pool);
}

#[compio::test]
async fn closing_one_pool_does_not_wait_for_another_pools_borrower() {
    let url = test_url();
    let first_pool = connect_pool(&url, config(1, 1)).await;
    let second_pool = connect_pool(&url, config(1, 1)).await;
    let unrelated_borrower = second_pool.get().await.unwrap();

    let waker = Waker::noop();
    let mut close = Box::pin(first_pool.close());
    assert!(
        poll_with_waker(close.as_mut(), waker).is_ready(),
        "close waited for a borrower owned by another pool"
    );
    assert_drained(&first_pool);
    assert_eq!(second_pool.active_count(), 1);
    assert_eq!(second_pool.total_count(), 1);

    drop(unrelated_borrower);
    assert_eq!(second_pool.active_count(), 0);
    assert_eq!(second_pool.idle_count(), 1);
    assert_eq!(second_pool.total_count(), 1);
}
