//! Constants for the native WebSocket impl.

/// RFC 6455 §1.3 magic GUID for `Sec-WebSocket-Accept` derivation.
/// Verified against https://datatracker.ietf.org/doc/html/rfc6455#section-1.3.
pub const RFC6455_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Default `WebSocketInit.maxMessageSize`: 4 MiB. Tungstenite's 64 MiB
/// default would let a malicious server OOM the worker.
pub const DEFAULT_MAX_MESSAGE_SIZE: u32 = 4 * 1024 * 1024;

/// Default `WebSocketInit.maxFrameSize`: 1 MiB.
pub const DEFAULT_MAX_FRAME_SIZE: u32 = 1024 * 1024;

/// Default `WebSocketInit.pingIntervalMs`: 30s. Matches undici and
/// workerd defaults.
pub const DEFAULT_PING_INTERVAL_MS: u32 = 30_000;

/// Reason length cap per WHATWG §3.1 close algorithm + RFC 6455 §5.5.1:
/// the Close frame payload is ≤ 125 bytes and 2 bytes are reserved for
/// the status code, so the reason is ≤ 123 bytes UTF-8 encoded.
pub const MAX_CLOSE_REASON_BYTES: usize = 123;

/// Per-isolate `[[full]]` cap: 16 MiB. When `bufferedAmount` would
/// exceed this, the `[[full]]` flag is set and subsequent sends are
/// dropped on the floor.
pub const MAX_BUFFERED_AMOUNT: u64 = 16 * 1024 * 1024;
