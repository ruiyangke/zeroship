//! Deterministic public-path coverage for pool waiter fairness.

use compio_postgres::{Pool, PoolConfig};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const CALLERS: usize = 8;
const LONG_WINDOW: Duration = Duration::from_secs(6 * 60 * 60);

fn test_url() -> String {
    common::test_url()
}

fn config(max_size: usize, min_idle: usize) -> PoolConfig {
    let mut config = PoolConfig::new();
    config
        .max_size(max_size)
        .min_idle(min_idle)
        .acquire_timeout(Duration::from_secs(30))
        .max_lifetime(LONG_WINDOW)
        .validation_bypass(LONG_WINDOW);
    config
}

async fn connect_pool(url: &str, config: PoolConfig) -> Pool {
    Pool::connect_with_pool_config(url, config)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error))
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

#[compio::test]
async fn queued_callers_acquire_in_fifo_parking_order() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;
    let held = pool.get().await.expect("hold the pool's only connection");
    let mut waiters = Vec::with_capacity(CALLERS);

    for caller in 0..CALLERS {
        let mut acquire = Box::pin(pool.get());
        assert!(
            poll_once(acquire.as_mut()).is_pending(),
            "caller {caller} acquired before the sole connection was released"
        );
        waiters.push(acquire);
        assert_eq!(
            pool.pending_count(),
            caller + 1,
            "caller {caller} did not park behind every earlier caller"
        );
    }

    drop(held);
    let mut observed = Vec::with_capacity(CALLERS);
    for expected in 0..CALLERS {
        // Poll later callers first to prove poll order cannot overtake queue order.
        for caller in ((expected + 1)..CALLERS).rev() {
            assert!(
                poll_once(waiters[caller].as_mut()).is_pending(),
                "caller {caller} acquired before FIFO head {expected}"
            );
        }

        let client = match poll_once(waiters[expected].as_mut()) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("caller {expected} failed to acquire: {error}"),
            Poll::Pending => panic!("FIFO head {expected} was not given the released connection"),
        };
        observed.push(expected);
        drop(client);
    }

    assert_eq!(observed, (0..CALLERS).collect::<Vec<_>>());
    assert_eq!(pool.pending_count(), 0);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 1);
    assert_eq!(pool.total_count(), 1);

    drop(waiters);
    pool.close().await;
}

#[compio::test]
async fn fresh_caller_cannot_barge_a_parked_waiter_after_release() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;
    let held = pool.get().await.expect("hold the pool's only connection");
    let mut earlier = Box::pin(pool.get());
    assert!(poll_once(earlier.as_mut()).is_pending());
    assert_eq!(pool.pending_count(), 1, "earlier caller did not park");

    drop(held);

    // This caller is first polled only after the release, before the woken
    // earlier caller is re-polled. A shared-idle handoff lets it barge here.
    let mut later = Box::pin(pool.get());
    let mut observed = Vec::with_capacity(2);
    let later_parked = match poll_once(later.as_mut()) {
        Poll::Pending => true,
        Poll::Ready(Ok(client)) => {
            observed.push("later");
            drop(client);
            false
        }
        Poll::Ready(Err(error)) => panic!("later caller failed to acquire: {error}"),
    };

    let client = match poll_once(earlier.as_mut()) {
        Poll::Ready(Ok(client)) => client,
        Poll::Ready(Err(error)) => panic!("earlier caller failed to acquire: {error}"),
        Poll::Pending => panic!("earlier caller did not receive the released connection"),
    };
    observed.push("earlier");
    drop(client);

    if later_parked {
        let client = match poll_once(later.as_mut()) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("later caller failed to acquire: {error}"),
            Poll::Pending => panic!("later caller remained parked after the earlier release"),
        };
        observed.push("later");
        drop(client);
    }

    assert_eq!(
        observed,
        ["earlier", "later"],
        "observed acquisition order {observed:?}; the fresh caller barged"
    );
    assert_eq!(pool.pending_count(), 0);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 1);
    assert_eq!(pool.total_count(), 1);

    drop(earlier);
    drop(later);
    pool.close().await;
}

#[compio::test]
async fn cancelling_parked_waiter_preserves_handoff_and_capacity() {
    let url = test_url();
    let pool = connect_pool(&url, config(1, 1)).await;
    let initial_total = pool.total_count();
    let held = pool.get().await.expect("hold the pool's only connection");

    let mut cancelled = Box::pin(pool.get());
    assert!(poll_once(cancelled.as_mut()).is_pending());
    assert_eq!(pool.pending_count(), 1);

    let mut successor = Box::pin(pool.get());
    assert!(poll_once(successor.as_mut()).is_pending());
    assert_eq!(pool.pending_count(), 2);

    drop(cancelled);
    assert_eq!(
        pool.pending_count(),
        1,
        "cancelled waiter remained in the FIFO queue"
    );

    drop(held);
    let client = match poll_once(successor.as_mut()) {
        Poll::Ready(Ok(client)) => client,
        Poll::Ready(Err(error)) => panic!("successor failed to acquire: {error}"),
        Poll::Pending => panic!("cancelled waiter consumed the successor's handoff"),
    };
    let observed = vec![1];
    drop(client);

    assert_eq!(observed, [1]);
    assert_eq!(pool.pending_count(), 0);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 1);
    assert_eq!(
        pool.total_count(),
        initial_total,
        "waiter cancellation leaked pool capacity"
    );

    drop(successor);
    pool.close().await;
}

#[compio::test]
async fn uncontended_callers_never_enter_the_wait_queue() {
    let url = test_url();
    let pool = connect_pool(&url, config(CALLERS, CALLERS)).await;
    let mut clients = Vec::with_capacity(CALLERS);
    let mut observed = Vec::with_capacity(CALLERS);

    for caller in 0..CALLERS {
        let mut acquire = Box::pin(pool.get());
        let client = match poll_once(acquire.as_mut()) {
            Poll::Ready(Ok(client)) => client,
            Poll::Ready(Err(error)) => panic!("caller {caller} failed to acquire: {error}"),
            Poll::Pending => panic!("uncontended caller {caller} entered the wait queue"),
        };
        observed.push(caller);
        clients.push(client);
        assert_eq!(pool.pending_count(), 0);
        assert_eq!(pool.active_count(), caller + 1);
    }

    assert_eq!(observed, (0..CALLERS).collect::<Vec<_>>());
    assert_eq!(pool.total_count(), CALLERS);
    assert_eq!(pool.idle_count(), 0);

    drop(clients);
    assert_eq!(pool.pending_count(), 0);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), CALLERS);
    assert_eq!(pool.total_count(), CALLERS);
    pool.close().await;
}

/// The acquire timeout an error REPORTS has to be the one that was configured.
///
/// It was rendered with `Duration::as_secs`, which truncates: every sub-second
/// setting described itself as `0s`. A caller who set 300 ms and is told the
/// pool "timed out after 0s" reads that as a misconfigured zero timeout and
/// goes looking for the wrong thing - the number is the one piece of the
/// message they would act on.
#[compio::test]
async fn a_sub_second_acquire_timeout_is_reported_accurately() {
    let mut pool_config = config(1, 0);
    pool_config.acquire_timeout(Duration::from_millis(300));
    let pool = Pool::connect_with_config(
        test_url().parse().expect("the suite DSN parses"),
        pool_config,
    )
    .await
    .expect("build a single-connection pool");

    let _held = pool.get().await.expect("hold the only connection");

    let error = pool
        .get()
        .await
        .expect_err("a second checkout cannot succeed while the only one is held");

    let cause = std::error::Error::source(&error)
        .map(ToString::to_string)
        .expect("the timeout error carries a cause describing itself");
    assert!(
        !cause.contains("after 0s"),
        "a 300ms acquire timeout reported itself as zero seconds: {cause}"
    );
    assert!(
        cause.contains("300ms"),
        "the reported timeout is not the configured one: {cause}"
    );
}
