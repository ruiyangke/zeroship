//! Test-only `tracing` capture layer for warn/error-shape contract
//! pinning.
//!
//! # Why this exists
//!
//! The plugin-db crate emits 9+ structured `tracing::warn!` /
//! `tracing::error!` events whose **field names** form an
//! operator-grep contract — production runbooks search log streams
//! for `app_id=…`, `audit_id=…`, `transition="Failed/…"`, and
//! `audit_err=…`. A future commit that renames one of those fields
//! silently breaks the runbook with no compile error.
//!
//! Cycle 13:17 (`7c6bd2ec`) drifted one such site: the F1 warn-half
//! in `register_model::apply` had its `audit_err`
//! field renamed to `error` in passing; commit `18aee490` reverted
//! the drift after a code review caught it. A snapshot test running
//! under this capture layer would have caught the drift pre-commit.
//!
//! References:
//! - test-coverage r11 NEW-R11-1 — establish a `tracing-subscriber`
//!   capture harness so warn-shape can be unit-tested.
//! - test-coverage r12 NEW-R12-1 — the F1 warn half (`5d9acab8` /
//!   `fcf7ce3c` / `7c6bd2ec` / `18aee490`) needs the harness.
//! - test-coverage r13 NEW-R13-* — extend to the [I6]
//!   `release_advisory_lock` typed-error caller paths and the [I23]
//!   `mig_lock` state-machine drift logs.
//!
//! # Scope
//!
//! Captures every event emitted under the
//! [`capture`] helper's `with_default` subscriber. Each event is
//! recorded as a [`TestEvent`] carrying:
//!
//! - `level` — `Level::ERROR`, `Level::WARN`, etc.
//! - `target` — module path (e.g. `"zeroship_plugin_db::context"`).
//! - `fields` — a `HashMap<String, String>` whose keys are the
//!   field names from the `event!(name = value, …, "message")`
//!   call. Values are rendered via `Display` (for `record_str`)
//!   or `Debug` (for `record_debug`). Numeric fields are
//!   stringified.
//! - `message` — the format-string body (the unnamed trailing
//!   argument), captured via the special `"message"` field key
//!   the `tracing` macros assign.
//!
//! `MAX_EVENTS` caps the buffer at 1024 entries so a buggy test
//! that spins under the layer cannot OOM the runner.
//!
//! # Usage
//!
//! ```ignore
//! use crate::test_support::capture;
//!
//! let ((), events) = capture(|| {
//!     tracing::warn!(app_id = "app_x", audit_id = 7, "boom");
//! });
//!
//! assert_eq!(events.len(), 1);
//! let ev = &events[0];
//! assert_eq!(ev.level, tracing::Level::WARN);
//! assert_eq!(ev.fields.get("app_id").map(String::as_str), Some("app_x"));
//! assert_eq!(ev.fields.get("audit_id").map(String::as_str), Some("7"));
//! assert!(ev.message.contains("boom"));
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Cap on the per-`capture()` event buffer. A buggy test that emits
/// in a tight loop under the layer therefore fails loudly (events
/// past the cap are dropped) rather than OOM-ing the runner.
const MAX_EVENTS: usize = 1024;

/// One captured tracing event. The shape is intentionally narrow:
/// only the bits warn-shape contract tests inspect.
#[derive(Debug, Clone)]
pub(crate) struct TestEvent {
    /// Verbosity level — `WARN` / `ERROR` for the contract sites.
    pub(crate) level: Level,
    /// Module path of the emitting site
    /// (e.g. `"zeroship_plugin_db::context"`). Useful when a test
    /// runs code paths that span multiple modules and the assertion
    /// needs to disambiguate which site fired.
    pub(crate) target: String,
    /// Structured fields keyed by name. Values rendered via
    /// `Display` (for `record_str`) or `Debug` (for everything else)
    /// so tests can string-compare regardless of the original type.
    pub(crate) fields: HashMap<String, String>,
    /// The format-string body (the unnamed trailing argument to the
    /// macro). `tracing` records this under a synthetic
    /// `"message"` field; we hoist it for ergonomics.
    pub(crate) message: String,
}

/// Layer that appends every `Event` it sees to a shared buffer.
///
/// `Send + Sync + 'static` because [`tracing::subscriber::with_default`]
/// requires the subscriber to satisfy those bounds — the buffer
/// therefore lives behind `Arc<Mutex<…>>`. Cloning the layer (which
/// [`Layer::and_then`] internally does) shares the same buffer
/// across clones.
#[derive(Clone)]
pub(crate) struct CaptureLayer {
    events: Arc<Mutex<Vec<TestEvent>>>,
}

impl CaptureLayer {
    /// Construct a fresh layer with an empty buffer. The buffer
    /// handle is exposed via [`Self::buffer`] so [`capture`] can
    /// read it back after the subscriber scope ends.
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Clone the buffer handle. The buffer is shared, not copied —
    /// reads after the subscriber scope ends still see every event
    /// the layer recorded.
    fn buffer(&self) -> Arc<Mutex<Vec<TestEvent>>> {
        Arc::clone(&self.events)
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        // Hoist `"message"` out of the field map for ergonomics.
        let message = visitor.fields.remove("message").unwrap_or_default();

        let test_event = TestEvent {
            level: *metadata.level(),
            target: metadata.target().to_string(),
            fields: visitor.fields,
            message,
        };

        // Lock poisoning here is harmless — a panic inside the layer
        // would only happen on OOM during `Vec::push`; either way the
        // test is already failing. Use `lock().ok()` to keep the layer
        // panic-free.
        if let Ok(mut buf) = self.events.lock() {
            if buf.len() < MAX_EVENTS {
                buf.push(test_event);
            }
        }
    }

    /// Spans don't carry warn-shape contract — implement a no-op so
    /// the bound is satisfied without spending allocator cycles
    /// during tests.
    fn on_new_span(&self, _attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {}
}

/// `tracing::field::Visit` impl that funnels every field value into
/// a flat `HashMap<String, String>`. Values are rendered via:
///
/// - `record_str` — verbatim (no `Debug`-quoting).
/// - `record_debug` — `{:?}` (captures structs, errors, anything
///   that lands via `error = ?e` syntax).
/// - `record_i64` / `record_u64` / `record_bool` / `record_f64` —
///   `to_string`.
///
/// The conversion preserves the field NAMES (the operator-grep
/// contract); the rendered VALUE form is best-effort and only
/// needs to be stable enough for `contains()` / equality checks
/// in tests.
#[derive(Default)]
struct FieldVisitor {
    fields: HashMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }

    fn record_error(
        &mut self,
        field: &Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}

/// Run `f` under a fresh capture layer; return its result plus
/// every event it emitted.
///
/// Uses [`tracing::subscriber::with_default`] so the capture is
/// scoped to the closure's execution on the current thread —
/// concurrent tests on other threads are unaffected. The
/// [`tracing_subscriber::Registry`] underneath is required because
/// `Layer<S>` needs an `S: Subscriber + LookupSpan` host.
///
/// # Panics
///
/// Panics if the layer's internal mutex is poisoned. In practice
/// this can only happen if `f` itself panics while inside an
/// event-record call (vanishingly rare); the rethrow surfaces the
/// underlying test failure rather than the mutex symptom.
pub(crate) fn capture<F, R>(f: F) -> (R, Vec<TestEvent>)
where
    F: FnOnce() -> R,
{
    use tracing_subscriber::layer::SubscriberExt;

    let layer = CaptureLayer::new();
    let buffer = layer.buffer();
    let subscriber = tracing_subscriber::registry().with(layer);

    let result = tracing::subscriber::with_default(subscriber, f);

    let events = buffer
        .lock()
        .expect("CaptureLayer buffer mutex poisoned — a record path panicked")
        .clone();
    (result, events)
}

#[cfg(test)]
mod self_tests {
    //! Sanity tests for the capture layer itself. These pin the
    //! contract that downstream tests rely on:
    //!
    //! - Field names are preserved verbatim.
    //! - Numeric / debug / str values all round-trip through
    //!   `Display`-able strings.
    //! - The `"message"` field is hoisted out of the field map.
    //! - `target` is the emitting module's path so cross-module
    //!   tests can disambiguate.

    use super::*;

    #[test]
    fn captures_warn_event_with_named_fields() {
        let ((), events) = capture(|| {
            tracing::warn!(app_id = "app_x", audit_id = 7_i64, "boom");
        });
        assert_eq!(events.len(), 1, "expected one event");
        let ev = &events[0];
        assert_eq!(ev.level, Level::WARN);
        assert_eq!(ev.fields.get("app_id").map(String::as_str), Some("app_x"));
        assert_eq!(ev.fields.get("audit_id").map(String::as_str), Some("7"));
        assert_eq!(ev.message, "boom");
    }

    #[test]
    fn captures_error_event_with_debug_field() {
        #[derive(Debug)]
        struct Boom;
        let ((), events) = capture(|| {
            tracing::error!(cause = ?Boom, "explosion");
        });
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.level, Level::ERROR);
        assert_eq!(ev.fields.get("cause").map(String::as_str), Some("Boom"));
        assert_eq!(ev.message, "explosion");
    }

    #[test]
    fn captures_multiple_events_in_order() {
        let ((), events) = capture(|| {
            tracing::warn!(seq = 1_i64, "first");
            tracing::warn!(seq = 2_i64, "second");
            tracing::warn!(seq = 3_i64, "third");
        });
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].message, "first");
        assert_eq!(events[1].message, "second");
        assert_eq!(events[2].message, "third");
    }

    #[test]
    fn capture_returns_closure_result() {
        let (value, _events) = capture(|| 42_u32);
        assert_eq!(value, 42);
    }

    #[test]
    fn target_carries_emitting_module_path() {
        let ((), events) = capture(|| {
            tracing::warn!("ping");
        });
        assert_eq!(events.len(), 1);
        // `tracing::warn!` defaults `target` to `module_path!()` —
        // here that is this test module.
        assert!(
            events[0].target.starts_with("zeroship_plugin_db::test_support"),
            "unexpected target: {}",
            events[0].target,
        );
    }

    #[test]
    fn captures_bool_and_display_via_percent_sigil() {
        let ((), events) = capture(|| {
            tracing::warn!(retried = true, name = %"users", "shape");
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].fields.get("retried").map(String::as_str), Some("true"));
        assert_eq!(events[0].fields.get("name").map(String::as_str), Some("users"));
    }
}
