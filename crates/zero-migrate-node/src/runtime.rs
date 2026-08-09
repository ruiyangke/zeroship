//! The tokio-free engine executor.
//!
//! The engine is a real state machine whose I/O leaves are `oneshot`/reply channels
//! woken **out-of-thread** by the host `done` callback. So it needs an
//! executor that drives ONE future with NO `io_uring` reactor and NO tokio: a single
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
//!
//! That worker thread is also where the engine's diagnostics are collected. The
//! engine emits `tracing` events for the secondary failures its reply cannot carry
//! (a release that failed, a `RESET ROLE` that failed), and a `tracing` event with
//! no subscriber installed reaches nobody. [`with_diagnostics`] installs one for
//! the length of the verb, on the thread that runs it, when the operator opts in
//! through `ZERO_MIGRATE_LOG`.

use std::io::IsTerminal;
use std::thread;

use tracing::{Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;

/// The opt-in switch for engine diagnostics, named for the `ZERO_MIGRATE_*` family
/// every other knob in this tree belongs to. `RUST_LOG` is deliberately not read:
/// it is a Rust-ecosystem name reaching an operator who runs a Node CLI, and it is
/// set incidentally in environments that have nothing to do with migrations.
const LOG_ENV: &str = "ZERO_MIGRATE_LOG";

/// The one target the switch turns on. Every event the engine emits is a `warn`
/// (each one is a secondary failure the reply cannot carry), so the switch itself
/// is the real control and this only keeps a future dependency's events out.
const LOG_TARGET: &str = "zero_migrate";

/// Whether this run asked for engine diagnostics.
///
/// Same asymmetry the live-database gate uses: anything but unset, empty, `0`,
/// `false` or `no` is a yes, so `ZERO_MIGRATE_LOG=0` reads as off rather than as a
/// non-empty value that happens to spell a falsehood.
fn diagnostics_requested() -> bool {
    diagnostics_requested_from(std::env::var(LOG_ENV).ok().as_deref())
}

/// The decision alone, split from the read so it is testable without mutating
/// process-wide environment state from a parallel test binary.
fn diagnostics_requested_from(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| {
        let flag = value.trim().to_ascii_lowercase();
        !matches!(flag.as_str(), "" | "0" | "false" | "no")
    })
}

/// The diagnostics subscriber, or `None` when this run did not ask for one.
///
/// STDERR, pinned explicitly: `tracing_subscriber::fmt()` defaults to STDOUT, and
/// `lint`/`plan`/`status`/`history` each write ONE JSON document to stdout that
/// callers parse. A diagnostic on that stream is a corrupted reply, not a noisy one.
///
/// ANSI only when stderr is a terminal: the crate is built with the `ansi` feature,
/// and escape codes in a piped CI log are their own corruption.
///
/// [`Targets`] rather than an `EnvFilter`: the filter is fixed, so there is no
/// directive string to parse and no reason to link a regex engine into a `.node`
/// that ships to every install.
fn diagnostics_subscriber() -> Option<impl Subscriber + Send + Sync + 'static> {
    diagnostics_requested().then(|| {
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr as fn() -> std::io::Stderr)
            .with_ansi(std::io::stderr().is_terminal());
        tracing_subscriber::registry()
            .with(layer)
            .with(Targets::new().with_target(LOG_TARGET, Level::WARN))
    })
}

/// Run `body` with the diagnostics subscriber installed as this THREAD's default,
/// or run it unchanged when diagnostics are off.
///
/// Thread-scoped, not global. `tracing_core` says a library should not call
/// `set_global_default`, and this crate is a library first: the published package
/// carries both `exports` and `bin`, so a global install would mutate process-wide
/// logging state for anyone who merely imports it. `with_default` does not
/// propagate into threads spawned inside it, and upstream's own answer to that is
/// to call it FROM the new thread -- which is what this does, on the worker that
/// runs the verb and emits every one of these events.
fn with_diagnostics<T>(body: impl FnOnce() -> T) -> T {
    match diagnostics_subscriber() {
        Some(subscriber) => tracing::subscriber::with_default(subscriber, body),
        None => body(),
    }
}

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
        .name("zero-migrate-cli".into())
        .spawn(move || {
            // The ONE future, driven with NO reactor. Every suspension inside it is
            // a channel receiver woken out-of-thread by the host `done` callback
            // — audited strictly-sequential (no join!/select!/spawn) in the
            // core apply path.
            let out = with_diagnostics(|| futures::executor::block_on(make_future()));
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
        assert_eq!(
            got, 42,
            "cross-thread oneshot woke the reactor-less block_on"
        );
    }

    #[test]
    fn diagnostics_are_off_until_the_operator_asks_for_them() {
        // Default off is the guard rail: four host arms assert a byte-empty stderr,
        // and a dozen of the engine's messages name the project lock that another
        // arm asserts is absent from an uncontended run.
        assert!(!diagnostics_requested_from(None), "unset is off");
        for off in ["", "   ", "0", "false", "FALSE", "no", " No "] {
            assert!(!diagnostics_requested_from(Some(off)), "{off:?} is off");
        }
        for on in ["1", "true", "yes", "warn", "on"] {
            assert!(diagnostics_requested_from(Some(on)), "{on:?} is on");
        }
    }
}
