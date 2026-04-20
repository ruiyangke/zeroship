//! Outcome of `Runtime::call_fetch_handler` — the kernel's sole dispatch
//! primitive. Replaces `DispatchOutcome`'s 7-variant split between RPC
//! and HTTP flavors.

use crate::channel::{CancelFlag, ResultReceiver, StreamReader};
use crate::runtime::DispatchError;

/// What the fetch handler produced — shape depends on whether the handler
/// was sync, async, streaming, or upgraded to WebSocket.
pub enum FetchOutcome {
    /// Sync / settled synchronously. Body is fully buffered.
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        logs: Vec<String>,
    },
    /// Headers known, body arrives chunk-by-chunk via `body_reader`.
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        body_reader: StreamReader,
        logs: Vec<String>,
    },
    /// Handler returned a Promise that hasn't settled. Poll `rx` for the
    /// final `FetchOutcome::Response` or `FetchOutcome::Stream`.
    Pending {
        rx: ResultReceiver<Result<SettledFetch, DispatchError>>,
        cancel: CancelFlag,
    },
    /// Handler returned a Response with status 101 + `webSocket` property.
    WebSocketUpgrade {
        ws_id: u32,
        headers: Vec<(String, String)>,
    },
}

/// Mirror of `FetchOutcome`'s three non-Pending variants — delivered
/// via the pending-resolver channel after a handler's promise settles.
/// Variant names match `FetchOutcome` so the kernel → receiver
/// translation is a 1:1 pattern match.
pub enum SettledFetch {
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        logs: Vec<String>,
    },
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        body_reader: StreamReader,
        logs: Vec<String>,
    },
    WebSocketUpgrade {
        ws_id: u32,
        headers: Vec<(String, String)>,
        logs: Vec<String>,
    },
}

/// Per-request execution context — what user code sees as `ctx`.
///
/// Built fresh by the gateway-facing layer (worker handler.rs or serve.rs)
/// for every request; carried across V8 reentries via the kernel's
/// `executing_request_id` tracking. Cancellation is wired to `cancel`;
/// `ctx.waitUntil(promise)` promises are stored on `RuntimeState`
/// (keyed by request_id) rather than on this struct, because the
/// native op that registers them only has the request_id in scope.
#[derive(Clone)]
pub struct RequestCtx {
    pub cancel: CancelFlag,
}

impl RequestCtx {
    pub fn new(cancel: CancelFlag) -> Self {
        Self { cancel }
    }
}

/// Frozen env snapshot — the `env` object user code imports from `zeroship`
/// and receives as the second `fetch(req, env, ctx)` parameter. Same
/// reference every request; populated once at worker boot from the app's
/// zeroship.toml + control-plane secrets.
///
/// Encoded as JSON for simple cross-boundary handoff; the JS side
/// JSON.parses once at module init and freezes the result. Richer
/// per-binding shapes (`env.DB.query(...)`) come in PR 3 when SDKs
/// land.
#[derive(Clone)]
pub struct EnvSnapshot {
    json: String,
}

impl EnvSnapshot {
    pub fn new(value: serde_json::Value) -> Self {
        Self { json: value.to_string() }
    }

    pub fn empty() -> Self {
        Self { json: "{}".into() }
    }

    pub fn as_json(&self) -> &str {
        &self.json
    }
}
