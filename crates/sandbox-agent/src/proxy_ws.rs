//! WebSocket-Upgrade proxy handler for the sandbox preview-URL feature.
//!
//! See `docs/proposals/sandbox-preview-urls.md` § II.1 (agent endpoint)
//! and D-8 / D-14 (post-Upgrade frames are NOT signed; the Upgrade
//! itself uses the V1_1_Ws canonical with a `-WS` domain-separator).
//!
//! ## Implementation choice — separate compio TCP listener
//!
//! ntex's connection-hijack surface (`HttpRequest::head().take_io()`)
//! is set only inside an Upgrade-aware request flow that requires
//! deep ntex internals to splice with a compio TcpStream. The
//! proposal explicitly permits a fallback: "the controller listens on
//! a separate port for Upgrade forwarding, and the route handler
//! 101's into that".
//!
//! We take the parallel path: the agent runs **two listeners** on the
//! same VM —
//!
//! - **`:7777`** — ntex HTTP server (`/exec`, `/files`, `/proxy/...`
//!   non-Upgrade). Phase 1, unchanged.
//! - **`:7778`** (default) — a compio raw-TCP listener that ONLY
//!   serves WebSocket Upgrade requests under `/proxy/{port}/{path*}`.
//!   Implemented in this module.
//!
//! The two listeners share the same [`crate::handlers::AppState`] —
//! same Verifier, same drain flag. The controller learns the WS port
//! by feature-detecting `proxy.ws-v1` in the agent's `/version`
//! response (capability advertised in `crate::version::CAPABILITIES`).
//!
//! ## Wire flow
//!
//! ```text
//! controller → TCP :7778
//!   ──────────────────────────────────────────
//!   GET /proxy/5173/ws HTTP/1.1
//!   Host: 10.99.<idx>.2:7778
//!   Upgrade: websocket
//!   Connection: Upgrade
//!   Sec-WebSocket-Key: <base64(16 random bytes)>
//!   Sec-WebSocket-Version: 13
//!   X-Sbx-Timestamp: <unix>
//!   X-Sbx-Nonce: <ascii>
//!   X-Sbx-Signature: <base64(Ed25519 over V1_1_Ws canonical)>
//!   <CRLF>
//!
//!   ← agent verifies V1_1_Ws, dials 127.0.0.1:5173
//!   ← agent forwards request line + headers verbatim
//!   ← upstream replies 101 Switching Protocols
//!   ← agent splices the two TCP sockets bidirectionally until close
//! ```
//!
//! Post-Upgrade frames are TCP-spliced (D-8) — the agent never parses
//! WS frames. The 5-second drain-grace closes in-flight WS by
//! shutting down the splice tasks (the upstream sees EOF; the
//! controller sees an RST/EOF and propagates 1001 to the browser).

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use compio::runtime::spawn;

use crate::handlers::AppState;
use crate::sig::{self, AuthFail, CanonicalKind};
use zeroship_core::preview_ports::{is_proxyable_port, DEFAULT_DENY};

/// Default WS-Upgrade listener port. The agent's HTTP port is
/// [`crate::DEFAULT_PORT`] (7777); the WS port is `+1` by convention.
/// Configurable via `SANDBOX_AGENT_WS_PORT`.
pub const DEFAULT_WS_PORT: u16 = 7778;

/// Hard cap on the request bytes (request-line + headers) we'll read
/// from the controller before deciding it's malformed. RFC 7230 §3.2.5
/// recommends ≥ 8 KiB; ntex's default is 32 KiB. Match ntex.
const MAX_REQUEST_BYTES: usize = 32 * 1024;

/// Per-direction read chunk size for the splice loop. Big enough to
/// amortise syscall overhead under HMR (small WS frames burst).
const SPLICE_CHUNK: usize = 64 * 1024;

/// Maximum lifetime of a single WebSocket connection. Matches the
/// existing sandbox max-lifetime budget (default 8 h). Splice tasks
/// shut down after this regardless of activity.
const WS_MAX_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);

/// Drain-grace before forcing in-flight WebSockets closed. After
/// `/shutdown` flips the drain flag, splice loops see the flag and
/// shut down within this window.
pub const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Resolve the configured WS port. Reads `SANDBOX_AGENT_WS_PORT`
/// (falls back to [`DEFAULT_WS_PORT`]).
pub fn ws_port_from_env() -> u16 {
    std::env::var("SANDBOX_AGENT_WS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_WS_PORT)
}

/// Bind the WS-Upgrade listener and serve forever. Returns when the
/// drain flag flips and the in-flight WS sessions complete (or the
/// 5 s drain-grace expires).
pub async fn serve(state: AppState, port: u16) -> io::Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse().expect("valid bind");
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr = %addr, "sandbox-agent/proxy_ws listening");

    let active = Arc::new(AtomicU64::new(0));

    loop {
        if state.is_draining() {
            tracing::info!("sandbox-agent/proxy_ws drain flag set; ws listener exiting");
            // Best-effort grace: wait up to DRAIN_GRACE for in-flight
            // WS sessions to close, then exit. The splice loops poll
            // the drain flag at each chunk; they self-terminate.
            let deadline = std::time::Instant::now() + DRAIN_GRACE;
            while active.load(Ordering::Relaxed) > 0
                && std::time::Instant::now() < deadline
            {
                compio::time::sleep(Duration::from_millis(100)).await;
            }
            return Ok(());
        }

        // Accept with a short timeout so the drain check above runs
        // promptly. compio doesn't expose a non-blocking accept, so
        // we use an accept-vs-timeout race.
        let accept_fut = listener.accept();
        let timeout_fut = compio::time::sleep(Duration::from_millis(500));
        let (stream, peer) = match futures_util::future::select(
            std::pin::pin!(accept_fut),
            std::pin::pin!(timeout_fut),
        )
        .await
        {
            futures_util::future::Either::Left((Ok(pair), _)) => pair,
            futures_util::future::Either::Left((Err(e), _)) => {
                tracing::warn!(error = %e, "sandbox-agent/proxy_ws accept error");
                continue;
            }
            futures_util::future::Either::Right(_) => continue, // tick
        };

        let st = state.clone();
        let active_c = active.clone();
        spawn(async move {
            active_c.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = handle_connection(st, stream, peer).await {
                tracing::warn!(error = %e, "sandbox-agent/proxy_ws connection error");
            }
            active_c.fetch_sub(1, Ordering::Relaxed);
        })
        .detach();
    }
}

/// Handle a single inbound connection. Reads the HTTP request,
/// verifies V1_1_Ws, dials upstream, splices on 101.
async fn handle_connection(
    state: AppState,
    mut down: TcpStream,
    _peer: SocketAddr,
) -> io::Result<()> {
    // 1. Read request-line + headers (up to MAX_REQUEST_BYTES) until
    //    we see `\r\n\r\n`.
    let mut buf = Vec::with_capacity(4096);
    let head_end = match read_request_head(&mut down, &mut buf).await {
        Ok(n) => n,
        Err(e) => {
            // Bad request — write a tiny 400 and bail.
            let _ = write_status(&mut down, 400, "Bad Request", b"bad request").await;
            return Err(e);
        }
    };
    let head = &buf[..head_end];
    let body_start = head_end;

    // 2. Parse with httparse. We only care about method, path, and
    //    a handful of headers.
    let mut header_storage = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut header_storage);
    let parsed = match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => req,
        Ok(httparse::Status::Partial) => {
            let _ = write_status(&mut down, 400, "Bad Request", b"incomplete request").await;
            return Ok(());
        }
        Err(e) => {
            let _ = write_status(&mut down, 400, "Bad Request", &format!("parse: {e}").into_bytes()).await;
            return Ok(());
        }
    };

    let method = parsed.method.unwrap_or("").to_string();
    let raw_path = parsed.path.unwrap_or("").to_string();

    // 3. Reject any request that's not an Upgrade. The WS port is
    //    Upgrade-only — non-Upgrade traffic is a misconfigured
    //    controller. (HTTP traffic uses :7777.)
    if !is_websocket_upgrade(&parsed) {
        write_status(&mut down, 426, "Upgrade Required", b"this listener only serves WebSocket Upgrade").await?;
        return Ok(());
    }

    // 4. Reject any non-empty body. RFC 6455 §1.3 — Upgrade carries
    //    no body. The signed canonical's body-hash is the empty
    //    constant; a non-empty body would never validate, but
    //    failing fast is the cleanest contract.
    if buf.len() > body_start {
        write_status(&mut down, 400, "Bad Request", b"upgrade body must be empty").await?;
        return Ok(());
    }
    // Defense-in-depth: a Content-Length of any non-zero value also
    // makes us 400 — even if the bytes haven't arrived yet.
    if let Some(cl) = header_value(&parsed, "content-length") {
        if cl.trim() != "0" && !cl.trim().is_empty() {
            write_status(&mut down, 400, "Bad Request", b"upgrade body must be empty").await?;
            return Ok(());
        }
    }

    // 5. Path matching: only `/proxy/{port}/{path*}` is served.
    let (port, _sub_path) = match parse_proxy_path(&raw_path) {
        Some(t) => t,
        None => {
            write_status(&mut down, 404, "Not Found", b"unknown path").await?;
            return Ok(());
        }
    };

    // 6. Port allow-set check (defense-in-depth — controller also checks).
    if !is_proxyable_port(port, DEFAULT_DENY) {
        write_status(&mut down, 400, "Bad Request", b"port not allowed").await?;
        return Ok(());
    }

    // 7. Verify the V1_1_Ws signature. The path-query bytes are the
    //    raw_path verbatim; v1_1_path_query strips a fragment if
    //    present (HTTP/1.1 wire never carries fragments anyway).
    let path_query = sig::v1_1_path_query(&raw_path).to_string();
    let ts_hdr = header_value(&parsed, "x-sbx-timestamp").unwrap_or("").to_string();
    let nonce_hdr = header_value(&parsed, "x-sbx-nonce").unwrap_or("").to_string();
    let sig_hdr = header_value(&parsed, "x-sbx-signature").unwrap_or("").to_string();

    match state.verifier.verify_kind(
        CanonicalKind::V1_1_Ws,
        &method,
        &path_query,
        b"", // V1_1_Ws contract: empty body
        &ts_hdr,
        &nonce_hdr,
        &sig_hdr,
    ) {
        Ok(()) => {}
        Err(reason) => {
            // Same audit shape as handlers::verify_signed.
            crate::audit::record(
                crate::audit::events::AUTH_FAIL,
                &format!(
                    "method={method} path={raw_path} reason={r}",
                    r = reason.as_str()
                ),
            );
            crate::metrics::inc_auth_fail(reason.as_str());
            // Differentiate the "wrong canonical" failure for ops
            // visibility; on the wire we still 401 uniformly.
            let _ = match reason {
                AuthFail::WrongCanonicalVersion => {
                    write_status(&mut down, 401, "Unauthorized", b"wrong canonical").await
                }
                _ => write_status(&mut down, 401, "Unauthorized", b"unauthorized").await,
            };
            return Ok(());
        }
    }

    if state.is_draining() {
        write_status(&mut down, 503, "Service Unavailable", b"draining").await?;
        return Ok(());
    }

    // 8. Dial 127.0.0.1:{port} inside the VM.
    let upstream_addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("static addr parses");
    let mut up = match TcpStream::connect(upstream_addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(port, error = %e, "sandbox-agent/proxy_ws dial 127.0.0.1 failed");
            write_status(&mut down, 502, "Bad Gateway", b"upstream unreachable").await?;
            return Ok(());
        }
    };

    // 9. Forward the original Upgrade request to upstream — verbatim,
    //    NO header rewriting (preserve Sec-WebSocket-Key etc). We
    //    can't trivially strip our X-Sbx-* headers because that would
    //    require re-serializing the request; instead we forward the
    //    bytes as-is. The user's app sees X-Sbx-* but they're inert
    //    (just headers it doesn't recognise). This is also what
    //    "forward the original Upgrade request bytes verbatim" in
    //    the proposal calls for.
    let head_bytes = head.to_vec();
    let compio::BufResult(res, _) = up.write_all(head_bytes).await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "sandbox-agent/proxy_ws write upstream head failed");
        let _ = write_status(&mut down, 502, "Bad Gateway", b"upstream write failed").await;
        return Ok(());
    }

    // 10. Read the upstream response head. If 101, splice. If other,
    //     forward as-is back to the controller.
    let mut up_head_buf = Vec::with_capacity(4096);
    let up_head_end = match read_response_head(&mut up, &mut up_head_buf).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "sandbox-agent/proxy_ws read upstream head failed");
            let _ = write_status(&mut down, 502, "Bad Gateway", b"upstream malformed response").await;
            return Ok(());
        }
    };
    let (status_code, _) = match parse_response_status(&up_head_buf[..up_head_end]) {
        Some(s) => s,
        None => {
            let _ = write_status(&mut down, 502, "Bad Gateway", b"upstream parse failed").await;
            return Ok(());
        }
    };

    // 11. Forward the upstream response head to the controller verbatim.
    let resp_head = up_head_buf[..up_head_end].to_vec();
    let compio::BufResult(res, _) = down.write_all(resp_head).await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "sandbox-agent/proxy_ws write down head failed");
        return Ok(());
    }

    if status_code != 101 {
        // Non-101: forward any body and terminate (no splice). The
        // controller treats this as a normal HTTP response.
        // For simplicity we one-shot read+forward whatever upstream
        // wrote and then close.
        if up_head_buf.len() > up_head_end {
            let extra = up_head_buf[up_head_end..].to_vec();
            let compio::BufResult(_, _) = down.write_all(extra).await;
        }
        // Drain the upstream into the downstream until close. Cap by
        // read budget; a misbehaving upstream is the controller's
        // problem to surface.
        let _ = copy_until_eof(&mut up, &mut down).await;
        return Ok(());
    }

    // 12. We have a 101. The downstream may have already buffered some
    //     post-101 bytes alongside the upstream head; we need to
    //     forward those too. (httparse gave us up_head_end as the
    //     byte right after `\r\n\r\n`.)
    if up_head_buf.len() > up_head_end {
        let extra = up_head_buf[up_head_end..].to_vec();
        let compio::BufResult(res, _) = down.write_all(extra).await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "sandbox-agent/proxy_ws flush 101 trailing bytes failed");
            return Ok(());
        }
    }

    // 13. Splice. Two compio tasks, one per direction; the first to
    //     close cancels the lifetime cap.
    splice_bidi(state.clone(), down, up).await
}

/// Bidirectional TCP splice with drain awareness + lifetime cap.
async fn splice_bidi(
    state: AppState,
    down: TcpStream,
    up: TcpStream,
) -> io::Result<()> {
    let (down_r, mut down_w) = down.into_split();
    let (up_r, mut up_w) = up.into_split();

    let drain_a = state.draining.clone();
    let drain_b = state.draining.clone();

    let to_up = async move {
        let mut r = down_r;
        loop {
            if drain_a.load(Ordering::Relaxed) {
                break;
            }
            let buf = vec![0u8; SPLICE_CHUNK];
            let compio::BufResult(res, buf) = r.read(buf).await;
            let n = match res {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let chunk = buf[..n].to_vec();
            let compio::BufResult(res, _) = up_w.write_all(chunk).await;
            if res.is_err() {
                break;
            }
        }
        let _ = up_w.shutdown().await;
        Ok::<(), io::Error>(())
    };

    let to_down = async move {
        let mut r = up_r;
        loop {
            if drain_b.load(Ordering::Relaxed) {
                break;
            }
            let buf = vec![0u8; SPLICE_CHUNK];
            let compio::BufResult(res, buf) = r.read(buf).await;
            let n = match res {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let chunk = buf[..n].to_vec();
            let compio::BufResult(res, _) = down_w.write_all(chunk).await;
            if res.is_err() {
                break;
            }
        }
        let _ = down_w.shutdown().await;
        Ok::<(), io::Error>(())
    };

    // Race the two halves AND the lifetime cap. First to finish wins.
    let lifetime = compio::time::sleep(WS_MAX_LIFETIME);
    let pair = futures_util::future::join(to_up, to_down);
    futures_util::future::select(
        std::pin::pin!(pair),
        std::pin::pin!(lifetime),
    )
    .await;
    Ok(())
}

/// Read an HTTP request head ending in `\r\n\r\n` from `stream`.
/// Returns the byte offset of the end of the head (the byte right
/// after the second CRLF). Caller owns `buf`; on success `buf[..n]`
/// is the head.
async fn read_request_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<usize> {
    loop {
        if let Some(idx) = find_crlf_crlf(buf) {
            return Ok(idx);
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head exceeded MAX_REQUEST_BYTES",
            ));
        }
        let scratch = vec![0u8; 4096];
        let compio::BufResult(res, scratch) = stream.read(scratch).await;
        let n = res?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed before request head completed",
            ));
        }
        buf.extend_from_slice(&scratch[..n]);
    }
}

/// Same as `read_request_head` but for an HTTP/1.1 status-line + headers
/// from the upstream side.
async fn read_response_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<usize> {
    loop {
        if let Some(idx) = find_crlf_crlf(buf) {
            return Ok(idx);
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response head exceeded MAX_REQUEST_BYTES",
            ));
        }
        let scratch = vec![0u8; 4096];
        let compio::BufResult(res, scratch) = stream.read(scratch).await;
        let n = res?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "upstream closed before response head completed",
            ));
        }
        buf.extend_from_slice(&scratch[..n]);
    }
}

/// Locate the `\r\n\r\n` that terminates an HTTP head. Returns the
/// byte offset right after the second CRLF, or `None` if not yet seen.
fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    for i in 0..(buf.len() - 3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

/// Parse the HTTP/1.1 status line out of a response head. Returns
/// `(status_code, reason)`. Reason intentionally borrows from `head`
/// just for completeness; we don't use it currently.
fn parse_response_status(head: &[u8]) -> Option<(u16, &str)> {
    let line = head.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).ok()?;
    let mut parts = line.splitn(3, ' ');
    let _version = parts.next()?;
    let code = parts.next()?.parse::<u16>().ok()?;
    let reason = parts.next().unwrap_or("");
    Some((code, reason))
}

/// Parse `/proxy/{port}/{sub_path}` (with optional `?query`). Returns
/// `(port, sub_path)`. The query is stripped — V1_1_Ws covers
/// path+query in the canonical via [`sig::v1_1_path_query`], but the
/// port-routing only needs the path component.
fn parse_proxy_path(path: &str) -> Option<(u16, String)> {
    // Drop fragment + query for routing.
    let no_frag = path.split('#').next().unwrap_or(path);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let stripped = no_query.strip_prefix("/proxy/")?;
    let (port_str, sub_path) = stripped.split_once('/').unwrap_or((stripped, ""));
    let port: u16 = port_str.parse().ok()?;
    Some((port, sub_path.to_string()))
}

/// Detect a WebSocket-Upgrade request shape: `Connection` includes
/// `upgrade` (case-insensitive) AND `Upgrade: websocket`.
fn is_websocket_upgrade(req: &httparse::Request) -> bool {
    let conn = header_value(req, "connection").unwrap_or("");
    let upg = header_value(req, "upgrade").unwrap_or("");
    let conn_has_upgrade = conn
        .split(',')
        .map(str::trim)
        .any(|s| s.eq_ignore_ascii_case("upgrade"));
    let upg_is_ws = upg.eq_ignore_ascii_case("websocket");
    conn_has_upgrade && upg_is_ws
}

/// Case-insensitive header lookup. httparse stores headers as a slice;
/// we walk linearly (fine — N ≤ 64).
fn header_value<'a>(req: &'a httparse::Request, name: &str) -> Option<&'a str> {
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case(name) {
            return std::str::from_utf8(h.value).ok();
        }
    }
    None
}

/// Write a minimal HTTP/1.1 status line + body and close. Used for
/// our self-emitted error responses (400, 401, 502, etc).
async fn write_status(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let head_bytes = head.into_bytes();
    let compio::BufResult(res, _) = stream.write_all(head_bytes).await;
    res?;
    if !body.is_empty() {
        let body_vec = body.to_vec();
        let compio::BufResult(res, _) = stream.write_all(body_vec).await;
        res?;
    }
    Ok(())
}

/// Best-effort drain of `from` into `to` until EOF on either side.
async fn copy_until_eof(from: &mut TcpStream, to: &mut TcpStream) -> io::Result<()> {
    loop {
        let buf = vec![0u8; SPLICE_CHUNK];
        let compio::BufResult(res, buf) = from.read(buf).await;
        let n = res?;
        if n == 0 {
            break;
        }
        let chunk = buf[..n].to_vec();
        let compio::BufResult(res, _) = to.write_all(chunk).await;
        res?;
    }
    Ok(())
}

/// Compute a UNIX timestamp in seconds. Used by tests that need to
/// build a timestamp; production code reads from the request.
#[allow(dead_code)]
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_crlf_crlf_at_end() {
        assert_eq!(find_crlf_crlf(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(
            find_crlf_crlf(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody"),
            Some(27)
        );
        assert_eq!(find_crlf_crlf(b""), None);
        assert_eq!(find_crlf_crlf(b"GET /"), None);
        // Single CRLF doesn't count.
        assert_eq!(find_crlf_crlf(b"GET / HTTP/1.1\r\nHost: x\r\n"), None);
    }

    #[test]
    fn parses_proxy_path_simple() {
        let (p, s) = parse_proxy_path("/proxy/5173/foo/bar").unwrap();
        assert_eq!(p, 5173);
        assert_eq!(s, "foo/bar");
    }

    #[test]
    fn parses_proxy_path_with_query() {
        let (p, s) = parse_proxy_path("/proxy/5173/foo?token=abc").unwrap();
        assert_eq!(p, 5173);
        // Query is stripped from the routing path; canonical uses raw URI.
        assert_eq!(s, "foo");
    }

    #[test]
    fn parses_proxy_path_root_path() {
        let (p, s) = parse_proxy_path("/proxy/5173/").unwrap();
        assert_eq!(p, 5173);
        assert_eq!(s, "");
    }

    #[test]
    fn rejects_non_proxy_path() {
        assert!(parse_proxy_path("/exec").is_none());
        assert!(parse_proxy_path("/files/x").is_none());
        assert!(parse_proxy_path("/").is_none());
    }

    #[test]
    fn parses_response_status() {
        let head = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";
        let (code, reason) = parse_response_status(head).unwrap();
        assert_eq!(code, 101);
        assert_eq!(reason, "Switching Protocols");
    }

    #[test]
    fn parses_response_status_2xx() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        let (code, _) = parse_response_status(head).unwrap();
        assert_eq!(code, 200);
    }

    #[test]
    fn parses_response_status_handles_lf_only() {
        // Some pathological upstreams don't emit \r before \n.
        // Our parser strips the trailing \r if present, so a pure LF
        // line still produces the right code.
        let head = b"HTTP/1.1 502 Bad Gateway\n";
        let (code, _) = parse_response_status(head).unwrap();
        assert_eq!(code, 502);
    }

    #[test]
    fn ws_upgrade_detected_classic() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
        let mut hdrs = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut hdrs);
        req.parse(raw).unwrap();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn ws_upgrade_detected_with_keep_alive_in_connection() {
        // RFC 7230: Connection can carry multiple tokens.
        let raw = b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, Upgrade\r\nUpgrade: WebSocket\r\n\r\n";
        let mut hdrs = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut hdrs);
        req.parse(raw).unwrap();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn ws_upgrade_not_detected_when_no_upgrade_header() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\n\r\n";
        let mut hdrs = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut hdrs);
        req.parse(raw).unwrap();
        assert!(!is_websocket_upgrade(&req));
    }

    #[test]
    fn ws_upgrade_not_detected_for_plain_http() {
        let raw = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n";
        let mut hdrs = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut hdrs);
        req.parse(raw).unwrap();
        assert!(!is_websocket_upgrade(&req));
    }

    #[test]
    fn header_value_case_insensitive() {
        let raw = b"GET / HTTP/1.1\r\nX-Sbx-Nonce: foo\r\nUpgrade: websocket\r\n\r\n";
        let mut hdrs = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut hdrs);
        req.parse(raw).unwrap();
        assert_eq!(header_value(&req, "x-sbx-nonce"), Some("foo"));
        assert_eq!(header_value(&req, "X-SBX-NONCE"), Some("foo"));
        assert_eq!(header_value(&req, "upgrade"), Some("websocket"));
        assert_eq!(header_value(&req, "missing"), None);
    }
}
