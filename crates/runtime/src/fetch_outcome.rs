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

/// Frozen env snapshot — carries the per-app environment as a three-part
/// split:
///
/// ```json
/// {
///   "vars":    { "NODE_ENV": "production" },
///   "secrets": { "OPENAI_API_KEY": "sk-..." },
///   "expose":  ["OPENAI_API_KEY"]
/// }
/// ```
///
/// Why split: any third-party npm package in the bundle can read
/// `process.env` (`Object.keys(process.env)`, `JSON.stringify(process.env)`,
/// `dotenv` debug printing, …). If secrets land there by default they
/// can be exfiltrated by a single malicious dependency. With this shape
/// the runtime keeps secrets out of `process.env` unless the creator
/// explicitly opts a name into the `expose` list (for libraries like
/// LangChain that defensively read `process.env.OPENAI_API_KEY`).
///
/// The merged map (`vars + secrets`) is what user code sees as
/// `import { env } from "zeroship"` and `fetch(req, env, ctx)`'s second
/// arg — the explicit, audited surface.
///
/// Wire format is JSON for simple cross-boundary handoff; the worker
/// receives it as bytes from the control plane, the runtime parses it
/// once and caches both the merged V8 object and the per-key
/// classification needed for `process.env`.
#[derive(Clone)]
pub struct EnvSnapshot {
    json: String,
}

impl EnvSnapshot {
    /// Build a snapshot from typed maps. Serializes to the JSON wire
    /// shape `{ vars, secrets, expose }`.
    ///
    /// `BTreeMap` is used so the JSON output is deterministic — the
    /// snapshot is hashed/compared against cached versions on the
    /// worker hot path, and a HashMap iterator order would inflate
    /// false-positive cache misses.
    pub fn new(
        vars: std::collections::BTreeMap<String, String>,
        secrets: std::collections::BTreeMap<String, String>,
        expose: Vec<String>,
    ) -> Self {
        let mut expose_sorted = expose;
        expose_sorted.sort();
        let value = serde_json::json!({
            "vars": vars,
            "secrets": secrets,
            "expose": expose_sorted,
        });
        Self { json: value.to_string() }
    }

    /// Convenience: build a snapshot from a `vars` map only — no secrets,
    /// empty expose. Accepts a `serde_json::Value::Object` for ergonomic
    /// use with the `serde_json::json!({...})` macro.
    ///
    /// Non-object values silently degrade to an empty vars map.
    pub fn vars_only(value: serde_json::Value) -> Self {
        let vars = match value {
            serde_json::Value::Object(map) => map
                .into_iter()
                .filter_map(|(k, v)| {
                    // Coerce scalars to string the same way the JS side
                    // would when reading env.X — preserves test ergonomics
                    // (`json!({"N": 42})` → "42").
                    match v {
                        serde_json::Value::String(s) => Some((k, s)),
                        serde_json::Value::Bool(b) => Some((k, b.to_string())),
                        serde_json::Value::Number(n) => Some((k, n.to_string())),
                        _ => None,
                    }
                })
                .collect(),
            _ => std::collections::BTreeMap::new(),
        };
        Self::new(vars, std::collections::BTreeMap::new(), Vec::new())
    }

    /// Trust pre-validated wire JSON. Used on the worker hot path —
    /// the bytes arrive from the control plane (which is the only
    /// producer of this JSON) and skip the parse-then-reserialize
    /// round-trip a second `from_str` would impose.
    ///
    /// The caller MUST guarantee `json` is a valid JSON object with
    /// the expected `{ vars, secrets, expose }` keys. Validation
    /// happens once at the producer; this constructor does no checks.
    pub fn from_validated_json(json: String) -> Self {
        Self { json }
    }

    pub fn empty() -> Self {
        Self {
            json: r#"{"vars":{},"secrets":{},"expose":[]}"#.into(),
        }
    }

    pub fn as_json(&self) -> &str {
        &self.json
    }
}
