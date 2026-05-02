//! RFC 6455 §4.1 client-side WebSocket handshake.
//!
//! Drives the connect spawn from `WebSocketImpl::new` (§V.3) on a
//! compio task. SSRF discipline mirrors fetch's two-phase guard:
//!
//!   Phase 1 — string-level: `crate::fetch::validate_url` rejects
//!     literal private / loopback / link-local IPs in the URL itself.
//!   Phase 2 — DNS resolution: `crate::fetch::resolve_and_check_ssrf`
//!     resolves the hostname and revalidates each candidate against
//!     the blocklist; the returned `SocketAddr` is what we hand
//!     `TcpStream::connect`. This closes the "DNS rebinding" hole
//!     where a public hostname resolves to `127.0.0.1` between the
//!     URL check and the actual connect (design CRITICAL #8).
//!
//! Defence-in-depth: we re-verify Sec-WebSocket-Accept ourselves
//! (tungstenite already checks; we double-check), enforce
//! Sec-WebSocket-Protocol echo against the offered set
//! (CRITICAL #9), and HARD-FAIL on any non-empty
//! Sec-WebSocket-Extensions response (we offer none — CRITICAL #7).
//!
//! Origin is opt-in only per RFC 6455 §10.2 (CRITICAL #3) — emitted
//! only when `WebSocketInit.origin` was explicitly set.
//!
//! Permessage-deflate: not enabled — `compio-ws` features are pinned
//! to `["rustls", "rustls-native-certs"]` (default-features = false),
//! and tungstenite 0.28 has no built-in deflate. Any RSV1=1 frame
//! hard-fails at the framer (defence in depth — design MAJOR #18).

#![cfg(feature = "runtime_native_websocket")]

use std::time::Duration;

use base64::Engine;
use compio_ws::WebSocketStream;
use compio_ws::tungstenite::{
    self,
    handshake::client::Request as TungsteniteRequest,
    protocol::{Role, WebSocketConfig},
};
use sha1::{Digest, Sha1};

use super::constants::RFC6455_GUID;

// ---------------------------------------------------------------------------
// Established — what `run_client_handshake` returns on success
// ---------------------------------------------------------------------------

/// Either a plain or TLS-wrapped established WebSocket. Both branches
/// expose the same `WebSocketStream` interface (read / send / close);
/// the receive_loop and send_pump are generic over the concrete type
/// via dynamic dispatch (Box<dyn ...>) so we keep one code path.
pub enum EstablishedStream {
    Plain(WebSocketStream<compio::net::TcpStream>),
    Tls(WebSocketStream<compio_tls::MaybeTlsStream<compio::net::TcpStream>>),
}

/// The result of a successful client handshake.
pub struct Established {
    pub stream: EstablishedStream,
    pub protocol: String,
    pub extensions: String,
}

// ---------------------------------------------------------------------------
// HandshakeError — exhaustive failure modes
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum HandshakeError {
    /// SSRF check (string-level OR DNS revalidation) rejected the URL.
    Ssrf(String),
    /// URL had no host component.
    MissingHost,
    /// Random bytes for Sec-WebSocket-Key could not be generated.
    RandomFailure,
    /// `http::Request::builder()` rejected one of the headers.
    BadRequest(String),
    /// TCP connect failed.
    Connect(std::io::Error),
    /// Lower-layer tungstenite handshake error (status mismatch,
    /// missing Upgrade, framer init, …).
    Protocol(tungstenite::Error),
    /// 101 response is missing the Sec-WebSocket-Accept header.
    MissingAccept,
    /// Server's Sec-WebSocket-Accept doesn't equal `base64(SHA1(key + GUID))`.
    AcceptMismatch,
    /// Sec-WebSocket-Protocol value isn't valid UTF-8.
    InvalidSubprotocol,
    /// Server echoed a subprotocol that wasn't in our offered set
    /// (RFC 6455 §4.1 — design CRITICAL #9).
    UnrequestedSubprotocol(String),
    /// Sec-WebSocket-Extensions has a non-UTF-8 / malformed value.
    InvalidExtensions,
    /// Server returned ANY Sec-WebSocket-Extensions value — we offered
    /// none (RFC 6455 §9.1 — design CRITICAL #7).
    UnrequestedExtensions(String),
    /// AbortSignal aborted during CONNECTING.
    Aborted { signal_reason: Option<String> },
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Ssrf(s) => write!(f, "SSRF: {s}"),
            HandshakeError::MissingHost => write!(f, "URL has no host"),
            HandshakeError::RandomFailure => write!(f, "random bytes for handshake key failed"),
            HandshakeError::BadRequest(s) => write!(f, "request build failed: {s}"),
            HandshakeError::Connect(e) => write!(f, "connect failed: {e}"),
            HandshakeError::Protocol(e) => write!(f, "WebSocket protocol error: {e}"),
            HandshakeError::MissingAccept => {
                write!(f, "missing Sec-WebSocket-Accept response header")
            }
            HandshakeError::AcceptMismatch => {
                write!(f, "Sec-WebSocket-Accept did not match expected digest")
            }
            HandshakeError::InvalidSubprotocol => {
                write!(f, "Sec-WebSocket-Protocol response is not valid UTF-8")
            }
            HandshakeError::UnrequestedSubprotocol(s) => {
                write!(f, "server selected unrequested subprotocol '{s}'")
            }
            HandshakeError::InvalidExtensions => {
                write!(f, "Sec-WebSocket-Extensions response is not valid UTF-8")
            }
            HandshakeError::UnrequestedExtensions(s) => {
                write!(f, "server returned unrequested extensions '{s}'")
            }
            HandshakeError::Aborted { signal_reason } => match signal_reason {
                Some(r) => write!(f, "aborted: {r}"),
                None => write!(f, "aborted"),
            },
        }
    }
}

impl std::error::Error for HandshakeError {}

// ---------------------------------------------------------------------------
// HandshakeOptions — minimal, threaded through from the constructor
// ---------------------------------------------------------------------------

/// Options derived from `WebSocketInit` (D-28) + `protocols` arg.
pub struct HandshakeOptions {
    pub protocols: Vec<String>,
    /// Opt-in Origin header per RFC 6455 §10.2 — emitted ONLY when set.
    pub origin: Option<String>,
    pub max_message_size: usize,
    pub max_frame_size: usize,
    /// Per-connect timeout for the TCP+TLS+WS handshake. Hard-coded to
    /// 30s — long enough for slow TLS over high-latency links, short
    /// enough that abandoned handshakes don't leak slots.
    pub connect_timeout: Duration,
}

impl Default for HandshakeOptions {
    fn default() -> Self {
        HandshakeOptions {
            protocols: Vec::new(),
            origin: None,
            max_message_size: super::constants::DEFAULT_MAX_MESSAGE_SIZE as usize,
            max_frame_size: super::constants::DEFAULT_MAX_FRAME_SIZE as usize,
            connect_timeout: Duration::from_secs(30),
        }
    }
}

impl HandshakeOptions {
    /// Build the tungstenite framer config (size caps; permessage-deflate
    /// stays off — design §VII.1 / MAJOR #18).
    pub fn websocket_config(&self) -> WebSocketConfig {
        WebSocketConfig::default()
            .max_message_size(Some(self.max_message_size))
            .max_frame_size(Some(self.max_frame_size))
    }
}

// ---------------------------------------------------------------------------
// run_client_handshake — the entry point
// ---------------------------------------------------------------------------

/// Run RFC 6455 §4.1 client handshake on `url`.
///
/// Steps (per RFC 6455 §4.1 + WHATWG WebSockets §4):
///   1. SSRF phase 1: `validate_url` rejects literal blocked IPs.
///   2. SSRF phase 2: `resolve_and_check_ssrf` validates the resolved
///      SocketAddr and we connect to that address directly.
///   3. Generate Sec-WebSocket-Key (16 random bytes, base64).
///   4. Build the GET request with Upgrade/Connection/Sec-WebSocket-*
///      headers; emit Origin only if explicitly set.
///   5. TCP connect → optional TLS upgrade (wss).
///   6. Hand the stream to compio_ws::client_async_with_config which
///      drives the tungstenite client handshake.
///   7. Verify Sec-WebSocket-Accept ourselves (defence in depth) +
///      the subprotocol echo + reject any extensions.
pub async fn run_client_handshake(
    url: url::Url,
    opts: HandshakeOptions,
) -> Result<Established, HandshakeError> {
    // Phase 1: string-level SSRF.
    crate::fetch::validate_url(url.as_str()).map_err(HandshakeError::Ssrf)?;

    // Phase 2: DNS revalidation. The returned SocketAddr is what we hand
    // `TcpStream::connect` so the kernel cannot re-resolve the hostname.
    let host = url.host_str().ok_or(HandshakeError::MissingHost)?.to_string();
    let port = url
        .port_or_known_default()
        .unwrap_or_else(|| if url.scheme() == "wss" { 443 } else { 80 });
    let addr = crate::fetch::resolve_and_check_ssrf(&host, port).map_err(HandshakeError::Ssrf)?;

    // Step 5: 16 random bytes → base64.
    let mut key_bytes = [0u8; 16];
    aws_lc_rs::rand::fill(&mut key_bytes).map_err(|_| HandshakeError::RandomFailure)?;
    let sec_websocket_key = base64::engine::general_purpose::STANDARD.encode(key_bytes);

    // Step 6: build the GET request. tungstenite reads the URI, headers,
    // and method from this `http::Request<()>`. Pass the full URL —
    // tungstenite's IntoClientRequest derives path/query/host from it.
    let host_header = if url.port().is_some() {
        format!("{host}:{port}")
    } else {
        host.clone()
    };

    // tungstenite computes the URI from the `http::Request`'s URI;
    // pass the FULL ws://host/path so its IntoClientRequest impl picks
    // the right scheme (the framer uses the scheme to decide
    // ws vs wss handshake validation, even when we pre-wrap the
    // stream with TLS ourselves).
    let mut req = http::Request::builder()
        .method(http::Method::GET)
        .uri(url.as_str())
        .header(http::header::HOST, host_header.clone())
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Key", &sec_websocket_key)
        .header("Sec-WebSocket-Version", "13");

    if !opts.protocols.is_empty() {
        req = req.header("Sec-WebSocket-Protocol", opts.protocols.join(", "));
    }
    if let Some(ref origin) = opts.origin {
        req = req.header(http::header::ORIGIN, origin);
    }

    let request: TungsteniteRequest = req
        .body(())
        .map_err(|e| HandshakeError::BadRequest(e.to_string()))?;

    // Step 7-8: TCP connect (validated SocketAddr) → optional TLS upgrade.
    // compio_ws's `*_with_config` signatures accept `impl Into<Config>`;
    // `WebSocketConfig` itself implements that conversion (per
    // compio-ws docs), preserving compio-ws's default buffer sizes.
    let ws_config = opts.websocket_config();

    // Pin the client role explicitly so unsolicited unmasked frames
    // from a misbehaving server are rejected (RFC 6455 §5.2). The
    // `client_async_with_config` helper already runs as Role::Client;
    // the explicit binding is for the audit trail.
    let _explicit_role: Role = Role::Client;

    let scheme = url.scheme();
    let (stream, response) = match scheme {
        "ws" => {
            let tcp = compio::net::TcpStream::connect(addr)
                .await
                .map_err(HandshakeError::Connect)?;
            let (ws, resp) =
                compio_ws::client_async_with_config(request, tcp, Some(ws_config))
                    .await
                    .map_err(HandshakeError::Protocol)?;
            (EstablishedStream::Plain(ws), resp)
        }
        "wss" => {
            let tcp = compio::net::TcpStream::connect(addr)
                .await
                .map_err(HandshakeError::Connect)?;
            // Use compio_ws's TLS-wrapping helper; passes None for
            // connector → it builds a rustls connector via
            // `rustls-native-certs` (the feature flag we enabled).
            // SNI uses the URL hostname (NOT the SocketAddr), so
            // virtual-hosted servers are reached correctly.
            let (ws, resp) = compio_ws::client_async_tls_with_config(
                request,
                tcp,
                None, /* default rustls connector */
                Some(ws_config),
            )
            .await
            .map_err(HandshakeError::Protocol)?;
            (EstablishedStream::Tls(ws), resp)
        }
        // Constructor already validated to ws/wss.
        _ => unreachable!("WebSocket scheme must be ws/wss after construction"),
    };

    // Step 9: verify Sec-WebSocket-Accept (RFC 6455 §4.1 step 6 of the
    // response checks). tungstenite already verifies internally, but
    // we double-check so the audit trail lives in our code.
    let accept_header = response
        .headers()
        .get("Sec-WebSocket-Accept")
        .ok_or(HandshakeError::MissingAccept)?;
    let expected = compute_sec_websocket_accept(&sec_websocket_key);
    if accept_header.as_bytes() != expected.as_bytes() {
        return Err(HandshakeError::AcceptMismatch);
    }

    // Step 10: subprotocol echo MUST be in our offered set
    // (RFC 6455 §4.1 — design CRITICAL #9).
    let protocol = match response.headers().get("Sec-WebSocket-Protocol") {
        None => String::new(),
        Some(v) => {
            let server_pick = v
                .to_str()
                .map_err(|_| HandshakeError::InvalidSubprotocol)?
                .trim()
                .to_string();
            if !opts.protocols.iter().any(|p| p == &server_pick) {
                return Err(HandshakeError::UnrequestedSubprotocol(server_pick));
            }
            server_pick
        }
    };

    // Step 11: any non-empty Sec-WebSocket-Extensions is a violation —
    // we offer none (RFC 6455 §9.1 — design CRITICAL #7).
    let extensions = match response.headers().get("Sec-WebSocket-Extensions") {
        None => String::new(),
        Some(v) => {
            let raw = v
                .to_str()
                .map_err(|_| HandshakeError::InvalidExtensions)?
                .trim();
            if !raw.is_empty() {
                return Err(HandshakeError::UnrequestedExtensions(raw.to_string()));
            }
            String::new()
        }
    };

    Ok(Established {
        stream,
        protocol,
        extensions,
    })
}

// ---------------------------------------------------------------------------
// compute_sec_websocket_accept — RFC 6455 §4.1
// ---------------------------------------------------------------------------

/// Per RFC 6455 §1.3 (https://datatracker.ietf.org/doc/html/rfc6455#section-1.3)
/// + §4.1: `base64(SHA1(client_key || RFC6455_GUID))`.
pub fn compute_sec_websocket_accept(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(RFC6455_GUID.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec example from RFC 6455 §1.3:
    ///   client key = "dGhlIHNhbXBsZSBub25jZQ=="
    ///   accept    = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    #[test]
    fn rfc6455_example_accept() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(
            compute_sec_websocket_accept(key),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
