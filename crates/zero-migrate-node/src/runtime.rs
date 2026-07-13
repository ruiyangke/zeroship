//! The tokio-free engine executor.
//!
//! The engine is a real state machine whose I/O leaves are `oneshot`/reply channels
//! woken **out-of-thread** by the host `done` callback. So it needs an
//! executor that drives ONE future with NO io_uring reactor and NO tokio: a single
//! `futures::executor::block_on` on a dedicated `std::thread`. `block_on` parks the
//! worker thread on a `std::thread`-parking `Waker`; a `Sender::send` from the JS
//! thread `unpark`s it (cross-thread-safe per `std::thread::Thread::unpark`) and the
//! awaited receiver resolves. No reactor is needed because the only wakeups the
//! engine future ever sees come from those out-of-thread sends.
//!
//! [`run_engine_blocking`] is the reusable primitive: give it a future factory and
//! it runs `block_on` on a fresh worker thread, delivering the result to a
//! completion callback. The napi entrypoints (`bridge.rs`) wrap it with a
//! `JsDeferred` so the JS side gets a `Promise` resolved cross-thread when
//! `block_on` completes (fire-and-resolve — the JS thread is NEVER blocked on a
//! `join()`, which would deadlock libuv/Bun).

use std::thread;

/// Run `make_future()`'s future to completion on a dedicated worker thread via a
/// reactor-less `futures::executor::block_on`, then hand the result to `on_done`.
///
/// - `make_future` is invoked **on the worker thread** so the future (which may
///   capture `!Send` engine state, e.g. a `NapiHostSession` holding a `RefCell`)
///   never crosses a thread boundary — only `on_done`'s `T` result does, so the
///   caller chooses a `Send` result type (`bridge.rs` uses a serializable outcome).
/// - `on_done` also runs on the worker thread; the napi wrapper makes it a
///   `deferred.resolve(...)` (itself thread-safe / cross-thread in napi-rs), so the
///   JS thread is never joined.
///
/// This is fire-and-forget from the JS thread's perspective: the worker owns the
/// engine future's whole lifetime.
pub fn run_engine_blocking<F, Fut, T, C>(make_future: F, on_done: C)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
    C: FnOnce(T) + Send + 'static,
{
    thread::Builder::new()
        .name("zero-migrate-engine".into())
        .spawn(move || {
            // The ONE future, driven with NO reactor. Every suspension inside it is
            // a channel receiver woken out-of-thread by the host `done` callback
            // — audited strictly-sequential (no join!/select!/spawn) in the
            // core apply path.
            let out = futures::executor::block_on(make_future());
            on_done(out);
        })
        .expect("spawn zero-migrate engine worker thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn block_on_runs_a_future_on_a_worker_thread_and_reports_out_of_thread() {
        // Prove the exact out-of-thread wakeup shape without napi: a future that awaits a oneshot
        // fired from ANOTHER thread wakes the parked block_on and the result is
        // delivered to on_done.
        let (result_tx, result_rx) = mpsc::channel::<u64>();
        let (fire_tx, fire_rx) = futures::channel::oneshot::channel::<u64>();

        run_engine_blocking(
            move || async move { fire_rx.await.unwrap_or(0) },
            move |v| {
                result_tx.send(v).unwrap();
            },
        );

        // Fire the oneshot from THIS (a different) thread after the worker has
        // parked — the cross-thread unpark is the whole feasibility hinge.
        thread::sleep(std::time::Duration::from_millis(20));
        fire_tx.send(42).unwrap();

        let got = result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("worker delivered result");
        assert_eq!(got, 42, "cross-thread oneshot woke the reactor-less block_on");
    }
}
