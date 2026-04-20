//! Panic instrumentation for detached compio tasks.
//!
//! `compio::runtime::spawn(…).detach()` silently swallows panics. That's a
//! production hazard — a latent bug in an async path drops work with zero
//! operator visibility. These helpers wrap a detached task's body in
//! `catch_unwind` and log the panic so at least stderr gets a signal.
//!
//! Usage:
//! ```ignore
//! compio::runtime::spawn(async move {
//!     panic_util::guard("spawn_body_reader", async move {
//!         // ... real body ...
//!     }).await;
//! }).detach();
//! ```

use futures::FutureExt;
use std::panic::AssertUnwindSafe;

pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_string())
}

/// Run `fut` with `catch_unwind`. On panic, log with `site` as a tag and
/// return `None`. Use `AssertUnwindSafe` because zeroship futures routinely
/// close over `Rc<RefCell<_>>` which is not `UnwindSafe`; the catch is
/// strictly for observability, not for recovery of shared state.
pub(crate) async fn guard<F, T>(site: &'static str, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(v) => Some(v),
        Err(p) => {
            eprintln!(
                "[zeroship] panic in spawned task ({site}): {}",
                panic_message(&*p)
            );
            None
        }
    }
}
