//! Timer callback storage.
//!
//! Timer scheduling is handled by tokio::time::sleep in the Runtime select! loop.
//! This module only stores the JS callback functions.

use std::time::Duration;

/// Backing storage for a timer's callback.
#[allow(missing_debug_implementations)]
pub struct TimerCallback {
    pub callback: v8::Global<v8::Function>,
    /// None = setTimeout (one-shot), Some(dur) = setInterval (repeating).
    pub interval: Option<Duration>,
}
