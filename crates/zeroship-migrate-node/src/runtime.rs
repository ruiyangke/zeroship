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
//! `block_on` completes (fire-and-resolve - the JS thread is NEVER blocked on a
//! `join()`, which would deadlock libuv/Bun).
//!
//! That worker thread is also where the engine's diagnostics are collected. The
//! engine emits `tracing` events for the secondary failures its reply cannot carry
//! (a release that failed, a `RESET ROLE` that failed), and a `tracing` event with
//! no subscriber installed reaches nobody. `with_diagnostics` installs one for
//! the length of the verb, on the thread that runs it, when the HOST has opted in
//! through [`set_diagnostics`].
//!
//! # Why the host states this instead of the addon reading it
//!
//! `clippy.toml` bans an ambient `std::env::var` read, and the ban is right on its own
//! terms - a read that takes a `&str` names nothing declared, so the population of the
//! process's configuration is not enumerable.
//!
//! The fix is not a suppression, because the same argument this module already makes
//! about the SUBSCRIBER applies one level up to the SWITCH. `with_diagnostics` refuses
//! `set_global_default` on the grounds that "this crate is a library first ... a global
//! install would mutate process-wide logging state for anyone who merely imports it".
//! An ambient environment read is that same defect at the decision: a process that
//! happens to carry the variable would turn on stderr diagnostics inside a caller - the
//! Vite plugin, say - that never asked for them and cannot see why.
//!
//! So the value is GIVEN to the addon rather than taken from the process. The host is a
//! Node CLI, `process.env` is ordinary and visible there, and the raw value crosses the
//! N-API boundary unparsed so that the truthiness contract below stays here, in Rust,
//! under the test that already covers it.
//!
//! The `zeroship-core` typed-key macro `clippy.toml` points at is not reachable from
//! this crate for the reason `build.rs` records: it would be a NORMAL dependency here,
//! and `zeroship-core` reaches tokio through `cyper -> hyper`, which is permitted
//! only for `kind == "dev"`. That constraint is why the value is passed in rather than
//! re-read through a typed key.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use tracing::{Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;

/// The migration-crate target prefix the switch turns on. Every engine event is a
/// `warn`, so this keeps dependency events out while covering the backend crates
/// that emit cleanup diagnostics.
const LOG_TARGET: &str = "zeroship_migrate";

/// Whether the host has asked for engine diagnostics. OFF until it says otherwise.
///
/// `Relaxed` is the right ordering and not a shortcut: this is a lone `bool` that
/// publishes no other memory, and it is set by the host thread before any verb is
/// dispatched. Nothing reads it expecting to see writes that were ordered against it.
static DIAGNOSTICS: AtomicBool = AtomicBool::new(false);

/// Turn engine diagnostics on or off for this process, from the raw value the host
/// holds - typically its own `process.env.ZERO_MIGRATE_LOG`.
///
/// The RAW value crosses, not a pre-parsed boolean, so the truthiness contract stays in
/// [`diagnostics_requested_from`] where a Rust test covers it rather than being restated
/// in every host that ever calls this.
///
/// Returns what it decided, so a caller can surface the setting it just made instead of
/// having to reimplement the rule to find out.
pub fn set_diagnostics(raw: Option<&str>) -> bool {
    let enabled = diagnostics_requested_from(raw);
    DIAGNOSTICS.store(enabled, Ordering::Relaxed);
    enabled
}

/// Whether this run asked for engine diagnostics.
fn diagnostics_requested() -> bool {
    DIAGNOSTICS.load(Ordering::Relaxed)
}

/// The decision alone, split from the switch so it is testable without touching the
/// process-wide flag from a parallel test binary.
///
/// Same asymmetry the live-database gate uses: anything but unset, empty, `0`,
/// `false` or `no` is a yes, so a host that forwards `ZERO_MIGRATE_LOG=0` gets "off"
/// rather than "a non-empty value that happens to spell a falsehood".
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

/// What an engine future left behind when it panicked instead of returning.
///
/// A panic on the worker thread used to destroy the completion callback along with
/// the rest of the frame. In the napi wrapper that callback owns the `JsDeferred`,
/// which has no `Drop` impl and settles a promise only through `resolve` or
/// `reject` - so dropping it left the JS promise pending forever. The caller's
/// `finally` never ran, its connection was never closed, and on PostgreSQL the
/// session-scoped advisory project lock was held until the process died. Carrying
/// the panic to the callback is what turns that hang into an error the caller can
/// see and clean up after.
#[derive(Debug)]
pub struct EnginePanic {
    message: String,
}

impl EnginePanic {
    /// Read the panic message out of the payload `catch_unwind` returns.
    ///
    /// `panic!` with a literal yields `&'static str` and with formatting yields
    /// `String`; anything else came from a `panic_any` this crate does not use, and
    /// says so rather than pretending to a message it cannot read.
    fn from_payload(payload: &Box<dyn std::any::Any + Send>) -> Self {
        let message = payload
            .downcast_ref::<&'static str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a non-string panic payload".to_string());
        Self { message }
    }

    /// The operator-facing reason, written for someone holding a hung deploy.
    ///
    /// It names the panic, says the operation did not finish, and points at the
    /// journal rather than at a retry: a panic can land after a committed step, so
    /// what already ran is a question only the journal answers.
    #[must_use]
    pub fn reason(&self) -> String {
        format!(
            "the migration engine panicked and the operation did not complete: {}. \
             The connection is being closed, so no lock is left held. Run `status` \
             before retrying - a panic can land after a step has already committed, \
             and the journal is what says which ones did.",
            self.message
        )
    }
}

/// Run `make_future()`'s future to completion on a dedicated worker thread via a
/// reactor-less `futures::executor::block_on`, then hand the result to `on_done`.
///
/// - `make_future` is invoked **on the worker thread** so the future never crosses
///   a thread boundary: the engine's driver and apply futures are `!Send` by
///   construction, which is why the workspace allows `future_not_send`, and `Fut`
///   carries no `Send` bound. Only `on_done`'s `T` result crosses, so the caller
///   chooses a `Send` result type (`bridge.rs` uses a serializable outcome).
/// - `on_done` also runs on the worker thread; the napi wrapper makes it a
///   `deferred.resolve(...)` or `deferred.reject(...)`, both thread-safe and
///   cross-thread in napi-rs, so the JS thread is never joined.
///
/// This is fire-and-forget from the JS thread's perspective: the worker owns the
/// engine future's whole lifetime.
///
/// `on_done` runs whether the future returned or panicked, so a caller holding a
/// one-shot completion handle always gets to use it. `AssertUnwindSafe` is sound
/// here because nothing that was in flight is read again: the whole operation - the
/// session, the backend, the partial outcome - is owned by this frame and dropped
/// during the unwind, and the only thing crossing to `on_done` afterwards is the
/// panic message. It asserts nothing about the DATABASE, which a panic can leave
/// mid-migration; [`EnginePanic::reason`] says so to the caller.
pub fn run_engine_blocking<F, Fut, T, C>(make_future: F, on_done: C)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
    C: FnOnce(std::result::Result<T, EnginePanic>) + Send + 'static,
{
    thread::Builder::new()
        .name("zero-migrate-cli".into())
        .spawn(move || {
            // The ONE future, driven with NO reactor. Every suspension inside it is
            // a channel receiver woken out-of-thread by the host `done` callback
            // - audited strictly-sequential (no join!/select!/spawn) in the
            // core apply path.
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_diagnostics(|| futures::executor::block_on(make_future()))
            }));
            // The default panic hook has already printed the payload and location to
            // stderr by now, so the message carried here is a summary, not the only
            // record of what happened.
            on_done(out.map_err(|payload| EnginePanic::from_payload(&payload)));
        })
        .expect("spawn zero-migrate engine worker thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A panicking engine future still reports, carrying the panic message.
    ///
    /// Before this, the panic destroyed the completion callback with the rest of
    /// the frame and the receiver saw `Err(Disconnected)` - which in the napi
    /// wrapper is a `JsDeferred` dropped without `resolve` or `reject`, so the JS
    /// promise never settled, the caller's `finally` never ran, its connection was
    /// never closed and the PostgreSQL project lock was never released.
    #[test]
    fn a_panicking_engine_future_reports_the_panic_instead_of_reporting_nothing() {
        let (tx, rx) = mpsc::channel::<std::result::Result<u64, String>>();

        run_engine_blocking(
            || async { panic!("engine invariant broken while lowering") },
            move |outcome: std::result::Result<u64, EnginePanic>| {
                tx.send(outcome.map_err(|panicked| panicked.reason()))
                    .expect("the callback ran, so the receiver is still alive");
            },
        );

        let reason = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the callback runs on the panic path, not only on the return path")
            .expect_err("a panicking future does not produce a value");
        assert!(
            reason.contains("engine invariant broken while lowering"),
            "the panic's own message reaches the caller: {reason}"
        );
        assert!(
            reason.contains("status"),
            "the reason points at the journal, since a panic can land after a \
             committed step: {reason}"
        );
    }

    #[test]
    fn block_on_runs_a_future_on_a_worker_thread_and_reports_out_of_thread() {
        // Prove the exact out-of-thread wakeup shape without napi: a future that awaits a oneshot
        // fired from ANOTHER thread wakes the parked block_on and the result is
        // delivered to on_done.
        let (result_tx, result_rx) = mpsc::channel::<u64>();
        let (fire_tx, fire_rx) = futures::channel::oneshot::channel::<u64>();

        run_engine_blocking(
            move || async move { fire_rx.await.unwrap_or(0) },
            move |v: std::result::Result<u64, EnginePanic>| {
                result_tx
                    .send(v.expect("the future returned rather than panicking"))
                    .unwrap();
            },
        );

        // Fire the oneshot from THIS (a different) thread after the worker has
        // parked - the cross-thread unpark is the whole feasibility hinge.
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

    /// The switch the host sets is the switch `with_diagnostics` reads.
    ///
    /// Without this, `set_diagnostics` could store into a flag nothing consults and every
    /// other test here would still pass: `diagnostics_requested_from` is pure, and the two
    /// `run_engine_blocking` cases assert on their RESULT, not on whether a subscriber was
    /// installed. That is the shape the env-var version had - a read on one side, a
    /// decision on the other, and no test joining them.
    ///
    /// It restores the flag before returning, because this is the one piece of
    /// process-global state in the crate and leaving it on would put engine diagnostics on
    /// stderr for whichever test in this binary runs next.
    #[test]
    fn the_host_switch_is_the_one_the_engine_reads() {
        let restore = diagnostics_requested();

        assert!(set_diagnostics(Some("1")), "an on value decides on");
        assert!(
            diagnostics_requested(),
            "set_diagnostics must store into the flag with_diagnostics consults"
        );

        assert!(!set_diagnostics(Some("0")), "an off value decides off");
        assert!(
            !diagnostics_requested(),
            "the switch turns back off; it is not one-way"
        );

        assert!(!set_diagnostics(None), "absent decides off");
        assert!(!diagnostics_requested(), "and absent stores off");

        DIAGNOSTICS.store(restore, Ordering::Relaxed);
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
