//! RFC 6455 §4.1 client-side WebSocket handshake.
//!
//! Drives the connect spawn from `WebSocketImpl::new` on a compio
//! task. SSRF discipline mirrors fetch's two-step guard:
//!
//!   1. String-level validation: `crate::fetch::validate_url` rejects
//!     literal private / loopback / link-local IPs in the URL itself.
//!   2. DNS resolution: `crate::fetch::resolve_and_check_ssrf`
//!     resolves the hostname and revalidates each candidate against
//!     the blocklist; the returned `SocketAddr` is what we hand
//!     `TcpStream::connect`. This closes the "DNS rebinding" hole
//!     where a public hostname resolves to `127.0.0.1` between the
//!     URL check and the actual connect.
//!
//! Header validation:
//!   - Sec-WebSocket-Accept: COMPUTE the expected digest ourselves and
//!     reject any mismatch (RFC 6455 §4.1 step 6.5).
//!   - Sec-WebSocket-Protocol: any echo MUST be in the offered set.
//!   - Sec-WebSocket-Extensions: any non-empty value HARD-FAILS — we
//!     offer none (RFC 6455 §9.1).
//!
//! Origin is opt-in only per RFC 6455 §10.2 — emitted
//! only when `WebSocketInit.origin` was explicitly set.
//!
//! Permessage-deflate: not enabled — we send no Sec-WebSocket-Extensions
//! header, and any RSV1=1 frame hard-fails at the in-tree framer
//! (`frame_reader::DecodeError::NonZeroReserved`).
//!
//! ## Why we hand-roll the HTTP handshake (instead of using compio_ws)
//!
//! compio_ws wraps the raw stream in `SyncStream<S>` and hands that to
//! tungstenite. After tungstenite reads the 101 response, any extra
//! bytes the server pipelined past the response (a server-pushed first
//! frame, in particular) live INSIDE tungstenite's `FrameCodec` —
//! tungstenite has no public API to extract them, and the SyncStream
//! buffer is also opaque. To use our own framer post-handshake, we
//! need full control over the leftover bytes; the only way is to do
//! the HTTP handshake ourselves. The handshake is small (RFC 6455 §4.1
//! is one request + one response with fixed header validation), and
//! the SSRF / extension / subprotocol / Sec-WebSocket-Accept guards we
//! used to layer ON TOP of tungstenite were already most of the work.

#![cfg(feature = "runtime_native_websocket")]

use std::io;
use std::time::Duration;

use base64::Engine;
use compio::buf::IoBuf;
use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use compio::net::TcpStream;
use compio_tls::TlsStream;
use sha1::{Digest, Sha1};

use super::constants::RFC6455_GUID;

// ---------------------------------------------------------------------------
// Established — what `run_handshake` returns on success
// ---------------------------------------------------------------------------

/// Either a plain TCP or TLS-wrapped established stream. Both branches
/// expose `AsyncRead + AsyncWrite`. The owner task multiplexes reads
/// and channel-driven writes over the chosen variant.
pub enum EstablishedStream {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

/// The result of a successful client handshake.
pub struct Established {
    pub stream: EstablishedStream,
    /// Bytes the server sent past the HTTP response. The framer must
    /// consume these BEFORE issuing its first network read so a server-
    /// pushed first frame isn't dropped.
    pub leftover: Vec<u8>,
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
    /// TCP connect failed.
    Connect(std::io::Error),
    /// TLS handshake / network I/O failure.
    Tls(std::io::Error),
    /// Read/write on the connect stream during the HTTP exchange.
    Io(std::io::Error),
    /// HTTP response parser couldn't decode the bytes.
    BadResponse(String),
    /// Server responded with a non-101 status.
    UnexpectedStatus(u16),
    /// 101 response is missing a required header (Upgrade, Connection,
    /// Sec-WebSocket-Accept).
    MissingResponseHeader(&'static str),
    /// `Connection` value didn't include `Upgrade` / `Upgrade` value
    /// wasn't `websocket`.
    BadUpgradeHeaders,
    /// Server's Sec-WebSocket-Accept doesn't equal `base64(SHA1(key + GUID))`.
    AcceptMismatch,
    /// Sec-WebSocket-Protocol value isn't valid UTF-8.
    InvalidSubprotocol,
    /// Server echoed a subprotocol that wasn't in our offered set.
    UnrequestedSubprotocol(String),
    /// Sec-WebSocket-Extensions has a non-UTF-8 / malformed value.
    InvalidExtensions,
    /// Server returned ANY Sec-WebSocket-Extensions value — we offered
    /// none.
    UnrequestedExtensions(String),
    /// Response header section exceeded MAX_RESPONSE_BYTES.
    ResponseTooLarge,
    /// AbortSignal aborted during CONNECTING.
    Aborted { signal_reason: Option<String> },
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Ssrf(s) => write!(f, "SSRF: {s}"),
            HandshakeError::MissingHost => write!(f, "URL has no host"),
            HandshakeError::RandomFailure => write!(f, "random bytes for handshake key failed"),
            HandshakeError::Connect(e) => write!(f, "connect failed: {e}"),
            HandshakeError::Tls(e) => write!(f, "TLS handshake failed: {e}"),
            HandshakeError::Io(e) => write!(f, "handshake I/O error: {e}"),
            HandshakeError::BadResponse(s) => write!(f, "malformed HTTP response: {s}"),
            HandshakeError::UnexpectedStatus(c) => {
                write!(f, "expected 101 Switching Protocols, got status {c}")
            }
            HandshakeError::MissingResponseHeader(h) => {
                write!(f, "missing required response header `{h}`")
            }
            HandshakeError::BadUpgradeHeaders => {
                write!(f, "Connection / Upgrade headers missing required values")
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
            HandshakeError::ResponseTooLarge => write!(f, "response header section too large"),
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

/// Options derived from `WebSocketInit` and the `protocols` argument.
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

// ---------------------------------------------------------------------------
// run_handshake — the entry point (called from `network::spawn_connect_task`)
// ---------------------------------------------------------------------------

/// Cap the response header section. 16 KiB is generous (typical responses
/// are <1 KiB). Larger responses are almost certainly malicious or
/// misbehaving servers.
const MAX_RESPONSE_BYTES: usize = 16 * 1024;

/// Run RFC 6455 §4.1 client handshake on `url`.
///
/// Steps:
///   1. String-level SSRF validation rejects literal blocked IPs.
///   2. DNS-level SSRF validation checks the resolved address and
///      SocketAddr and we connect to that address directly.
///   3. Generate Sec-WebSocket-Key (16 random bytes, base64).
///   4. Build the GET request with Upgrade/Connection/Sec-WebSocket-*
///      headers; emit Origin only if explicitly set.
///   5. TCP connect → optional TLS upgrade (wss).
///   6. Send the request, read until end-of-headers (`\r\n\r\n`).
///   7. Parse response, verify status=101, Connection: Upgrade,
///      Upgrade: websocket, Sec-WebSocket-Accept matches, no
///      extensions, subprotocol (if any) was offered.
///   8. Return the underlying stream PLUS any bytes already read past
///      the response (the framer must consume those first).
pub async fn run_handshake(
    url: url::Url,
    opts: HandshakeOptions,
) -> Result<Established, HandshakeError> {
    // String-level SSRF validation.
    crate::fetch::validate_url(url.as_str()).map_err(HandshakeError::Ssrf)?;

    // DNS revalidation.
    let host = url.host_str().ok_or(HandshakeError::MissingHost)?.to_string();
    let port = url
        .port_or_known_default()
        .unwrap_or_else(|| if url.scheme() == "wss" { 443 } else { 80 });
    let addr = crate::fetch::resolve_and_check_ssrf(&host, port).map_err(HandshakeError::Ssrf)?;

    // Step 3: 16 random bytes → base64.
    let mut key_bytes = [0u8; 16];
    aws_lc_rs::rand::fill(&mut key_bytes).map_err(|_| HandshakeError::RandomFailure)?;
    let sec_websocket_key = base64::engine::general_purpose::STANDARD.encode(key_bytes);

    // Step 4: build the request bytes.
    let request_bytes = build_request_bytes(&url, &host, port, &sec_websocket_key, &opts);

    // Step 5–8: connect → handshake → return stream + leftover.
    match url.scheme() {
        "ws" => {
            let tcp = compio::net::TcpStream::connect(addr)
                .await
                .map_err(HandshakeError::Connect)?;
            let _ = tcp.set_nodelay(true);
            let (tcp, outcome) =
                exchange_handshake_inner(tcp, request_bytes, &sec_websocket_key, &opts).await?;
            Ok(Established {
                stream: EstablishedStream::Plain(tcp),
                leftover: outcome.leftover,
                protocol: outcome.protocol,
                extensions: outcome.extensions,
            })
        }
        "wss" => {
            let tcp = compio::net::TcpStream::connect(addr)
                .await
                .map_err(HandshakeError::Connect)?;
            let _ = tcp.set_nodelay(true);
            let connector = crate::transport::tls::build_tls_connector(
                &crate::transport::tls::TlsConnectorOptions::default(),
            )
            .map_err(HandshakeError::Tls)?;
            let tls = connector
                .connect(&host, tcp)
                .await
                .map_err(HandshakeError::Tls)?;
            let (tls, outcome) =
                exchange_handshake_inner(tls, request_bytes, &sec_websocket_key, &opts).await?;
            Ok(Established {
                stream: EstablishedStream::Tls(tls),
                leftover: outcome.leftover,
                protocol: outcome.protocol,
                extensions: outcome.extensions,
            })
        }
        _ => unreachable!("WebSocket scheme must be ws/wss after construction"),
    }
}

// ---------------------------------------------------------------------------
// Build the request bytes
// ---------------------------------------------------------------------------

fn build_request_bytes(
    url: &url::Url,
    host: &str,
    port: u16,
    sec_websocket_key: &str,
    opts: &HandshakeOptions,
) -> Vec<u8> {
    let path = url.path();
    let path_query = if let Some(q) = url.query() {
        format!("{path}?{q}")
    } else {
        path.to_string()
    };
    let path_query = if path_query.is_empty() {
        "/".to_string()
    } else {
        path_query
    };

    let host_header =
        if (url.scheme() == "wss" && port != 443) || (url.scheme() == "ws" && port != 80) {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };

    let mut req = String::with_capacity(256);
    req.push_str("GET ");
    req.push_str(&path_query);
    req.push_str(" HTTP/1.1\r\n");
    req.push_str("Host: ");
    req.push_str(&host_header);
    req.push_str("\r\n");
    req.push_str("Upgrade: websocket\r\n");
    req.push_str("Connection: Upgrade\r\n");
    req.push_str("Sec-WebSocket-Key: ");
    req.push_str(sec_websocket_key);
    req.push_str("\r\n");
    req.push_str("Sec-WebSocket-Version: 13\r\n");
    if !opts.protocols.is_empty() {
        req.push_str("Sec-WebSocket-Protocol: ");
        req.push_str(&opts.protocols.join(", "));
        req.push_str("\r\n");
    }
    if let Some(ref origin) = opts.origin {
        req.push_str("Origin: ");
        req.push_str(origin);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    req.into_bytes()
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

// ---------------------------------------------------------------------------
// Drive the HTTP exchange on any AsyncRead+AsyncWrite stream.
// ---------------------------------------------------------------------------

struct ExchangeOutcome {
    leftover: Vec<u8>,
    protocol: String,
    extensions: String,
}

async fn exchange_handshake_inner<S>(
    mut stream: S,
    request_bytes: Vec<u8>,
    sec_websocket_key: &str,
    opts: &HandshakeOptions,
) -> Result<(S, ExchangeOutcome), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Send the request.
    let res = stream.write_all(request_bytes).await;
    res.0.map_err(HandshakeError::Io)?;
    stream.flush().await.map_err(HandshakeError::Io)?;

    // Read until we see the end-of-headers `\r\n\r\n` marker, OR the
    // cap is exceeded.
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let header_end = loop {
        // Check size cap BEFORE the next read so a runaway server can't
        // make us allocate forever.
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(HandshakeError::ResponseTooLarge);
        }
        // Search the whole accumulated buffer for `\r\n\r\n`. (We could
        // skip the bytes before the previous boundary, but the scan is
        // cheap and the buffer is bounded by MAX_RESPONSE_BYTES.)
        if let Some(pos) = find_double_crlf(&buf) {
            break pos + 4;
        }

        // Read more. Use a 4 KiB chunk so we don't overshoot wildly
        // past the headers.
        let chunk = vec![0u8; 4096];
        let res = stream.read(chunk).await;
        let n = res.0.map_err(HandshakeError::Io)?;
        let chunk = res.1;
        if n == 0 {
            return Err(HandshakeError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before end of HTTP response headers",
            )));
        }
        buf.extend_from_slice(&chunk.as_slice()[..n]);
    };

    // Parse the response section.
    let (status, headers_owned) = parse_response_headers(&buf[..header_end])?;
    if status != 101 {
        return Err(HandshakeError::UnexpectedStatus(status));
    }

    // Header validation (per RFC 6455 §4.1 step 6.5–6.7):
    //   - Upgrade: websocket
    //   - Connection: contains "Upgrade" (case-insensitive token list)
    //   - Sec-WebSocket-Accept: matches base64(SHA1(key + GUID))
    let upgrade = lookup_header(&headers_owned, "upgrade")
        .ok_or(HandshakeError::MissingResponseHeader("Upgrade"))?;
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(HandshakeError::BadUpgradeHeaders);
    }
    let connection = lookup_header(&headers_owned, "connection")
        .ok_or(HandshakeError::MissingResponseHeader("Connection"))?;
    let mut found_upgrade = false;
    for tok in connection.split(',') {
        if tok.trim().eq_ignore_ascii_case("upgrade") {
            found_upgrade = true;
            break;
        }
    }
    if !found_upgrade {
        return Err(HandshakeError::BadUpgradeHeaders);
    }
    let accept = lookup_header(&headers_owned, "sec-websocket-accept")
        .ok_or(HandshakeError::MissingResponseHeader("Sec-WebSocket-Accept"))?;
    let expected = compute_sec_websocket_accept(sec_websocket_key);
    if accept.trim() != expected {
        return Err(HandshakeError::AcceptMismatch);
    }

    // Subprotocol echo MUST be in our offered set.
    let protocol = match lookup_header(&headers_owned, "sec-websocket-protocol") {
        None => String::new(),
        Some(server_pick) => {
            let server_pick = server_pick.trim().to_string();
            if !opts.protocols.iter().any(|p| p == &server_pick) {
                return Err(HandshakeError::UnrequestedSubprotocol(server_pick));
            }
            server_pick
        }
    };

    // Any non-empty Sec-WebSocket-Extensions is a violation — we offer
    // none.
    let extensions = match lookup_header(&headers_owned, "sec-websocket-extensions") {
        None => String::new(),
        Some(raw) => {
            let raw = raw.trim();
            if !raw.is_empty() {
                return Err(HandshakeError::UnrequestedExtensions(raw.to_string()));
            }
            String::new()
        }
    };

    // Bytes after the header section are the leftover — the server
    // pipelined frames past the response. Hand them to the framer.
    let leftover = buf[header_end..].to_vec();

    Ok((
        stream,
        ExchangeOutcome {
            leftover,
            protocol,
            extensions,
        },
    ))
}

/// Find the first occurrence of `\r\n\r\n` in `buf`. Returns the offset
/// of the leading `\r`.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    for i in 0..=buf.len() - 4 {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

/// Parse the response status line + headers using `httparse`.
/// `bytes` MUST end at the `\r\n\r\n` marker (i.e. only the header
/// section, no body).
fn parse_response_headers(bytes: &[u8]) -> Result<(u16, Vec<(String, String)>), HandshakeError> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    let parsed = resp
        .parse(bytes)
        .map_err(|e| HandshakeError::BadResponse(e.to_string()))?;
    if !parsed.is_complete() {
        return Err(HandshakeError::BadResponse(
            "response section is not complete".into(),
        ));
    }
    let status = resp.code.unwrap_or(0);
    let owned: Vec<(String, String)> = resp
        .headers
        .iter()
        .filter(|h| !h.name.is_empty())
        .map(|h| {
            let name = h.name.to_string();
            let value = std::str::from_utf8(h.value)
                .map(|s| s.to_string())
                .unwrap_or_default();
            (name, value)
        })
        .collect();
    Ok((status, owned))
}

fn lookup_header<'a>(headers: &'a [(String, String)], target: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(target))
        .map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    #[test]
    fn build_request_omits_origin_when_unset() {
        let url: url::Url = "ws://example.com/socket".parse().unwrap();
        let opts = HandshakeOptions::default();
        let bytes = build_request_bytes(&url, "example.com", 80, "abc", &opts);
        let s = String::from_utf8(bytes).unwrap();
        assert!(
            !s.contains("Origin:"),
            "should not include Origin header when not set"
        );
        assert!(s.contains("Host: example.com\r\n"));
        assert!(s.contains("Upgrade: websocket\r\n"));
        assert!(s.contains("Connection: Upgrade\r\n"));
        assert!(s.contains("Sec-WebSocket-Version: 13\r\n"));
        assert!(s.contains("Sec-WebSocket-Key: abc\r\n"));
    }

    #[test]
    fn build_request_includes_origin_when_set() {
        let url: url::Url = "ws://example.com/socket".parse().unwrap();
        let opts = HandshakeOptions {
            origin: Some("https://example.app".into()),
            ..HandshakeOptions::default()
        };
        let bytes = build_request_bytes(&url, "example.com", 80, "abc", &opts);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("Origin: https://example.app\r\n"));
    }

    #[test]
    fn build_request_protocol_list_joined_by_comma_space() {
        let url: url::Url = "ws://example.com/socket".parse().unwrap();
        let opts = HandshakeOptions {
            protocols: vec!["chat.v1".into(), "chat.v2".into()],
            ..HandshakeOptions::default()
        };
        let bytes = build_request_bytes(&url, "example.com", 80, "abc", &opts);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("Sec-WebSocket-Protocol: chat.v1, chat.v2\r\n"));
    }

    #[test]
    fn build_request_omits_default_port_in_host_header() {
        let url: url::Url = "ws://example.com/".parse().unwrap();
        let opts = HandshakeOptions::default();
        let bytes = build_request_bytes(&url, "example.com", 80, "k", &opts);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("Host: example.com\r\n"));
        let bytes2 = build_request_bytes(&url, "example.com", 8080, "k", &opts);
        let s2 = String::from_utf8(bytes2).unwrap();
        assert!(s2.contains("Host: example.com:8080\r\n"));
    }

    #[test]
    fn build_request_path_query_preserved() {
        let url: url::Url = "ws://example.com/path?x=1&y=2".parse().unwrap();
        let opts = HandshakeOptions::default();
        let bytes = build_request_bytes(&url, "example.com", 80, "k", &opts);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("GET /path?x=1&y=2 HTTP/1.1\r\n"));
    }

    #[test]
    fn parse_minimal_101_response() {
        let resp = b"HTTP/1.1 101 Switching Protocols\r\n\
                     Upgrade: websocket\r\n\
                     Connection: Upgrade\r\n\
                     Sec-WebSocket-Accept: hello\r\n\
                     \r\n";
        let (status, headers) = parse_response_headers(resp).unwrap();
        assert_eq!(status, 101);
        assert_eq!(lookup_header(&headers, "Upgrade"), Some("websocket"));
        assert_eq!(lookup_header(&headers, "connection"), Some("Upgrade"));
        assert_eq!(lookup_header(&headers, "sec-websocket-accept"), Some("hello"));
    }

    #[test]
    fn find_double_crlf_locates_marker() {
        assert_eq!(find_double_crlf(b"a\r\n\r\n"), Some(1));
        assert_eq!(find_double_crlf(b"hdr1: x\r\nhdr2: y\r\n\r\nbody"), Some(16));
        assert_eq!(find_double_crlf(b""), None);
        assert_eq!(find_double_crlf(b"\r\n"), None);
    }
}
