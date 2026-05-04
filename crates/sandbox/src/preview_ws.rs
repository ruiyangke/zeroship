//! Controller-side WebSocket-Upgrade forwarder for the preview proxy.
//!
//! See `docs/proposals/sandbox-preview-urls.md` § II.2 (controller
//! endpoint must handle Upgrade) and D-14 (V1_1_Ws canonical with
//! `ED25519-V1.1-WS` domain-separator).
//!
//! ## Implementation choice — separate compio TCP listener
//!
//! ntex's connection-hijack surface (`HttpRequest::head().take_io()`)
//! doesn't compose cleanly with a compio TcpStream upstream splice.
//! Per the proposal's Phase 2 plan: "If ntex's surface is too awkward,
//! fall back to a compio raw-socket path (the controller listens on a
//! separate port for Upgrade forwarding, …). Document the chosen
//! approach in code comments." We take that path; the controller
//! exposes a dedicated WS port (default 9092 — `SANDBOX_PORT + 1` by
//! convention; configurable via `SANDBOX_PREVIEW_WS_PORT`).
//!
//! Phase 4 lands public DNS at `*.preview.zeroship.dev`; until then
//! the WS port is reachable directly for integration testing.
//!
//! ## Wire flow
//!
//! ```text
//! browser/test → TCP :<ws-port>
//!   GET /sandboxes/{id}/preview/{port}/{path*}?user_id=alice HTTP/1.1
//!   Authorization: Bearer <token>
//!   Upgrade: websocket
//!   Connection: Upgrade
//!   Sec-WebSocket-Key: <base64(16 random)>
//!   Sec-WebSocket-Version: 13
//!   <CRLF>
//!
//!   ← controller: authn (bearer) + authz (owner check + port allow)
//!   ← controller: derive agent WS port from agent_url + sign V1_1_Ws
//!   ← controller forwards to <agent_ws_url>/proxy/{port}/{path*}?{query}
//!   ← agent verifies V1_1_Ws, dials 127.0.0.1:{port}, returns 101
//!   ← controller splices its two TCP sockets bidirectionally
//! ```
//!
//! Post-Upgrade frames are TCP-spliced through the controller and
//! agent (D-8). Header rewriting on a 101 is irrelevant — the
//! response is `101 Switching Protocols` with minimal headers, no
//! Set-Cookie/Location/Refresh to rewrite.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use compio::runtime::spawn;
use uuid::Uuid;

use crate::AppState;
use zeroship_core::preview_ports::{is_proxyable_port, DEFAULT_DENY};
use zeroship_sandbox_agent::sig::{self, CanonicalKind};

/// Default WS-Upgrade listener port for the controller.
/// `SANDBOX_PORT=9090 + 2 = 9092`. Configurable via
/// `SANDBOX_PREVIEW_WS_PORT`.
pub const DEFAULT_WS_PORT: u16 = 9092;

/// Maximum bytes to read for the request head before deciding it's
/// malformed. Mirrors ntex's default (32 KiB).
const MAX_REQUEST_BYTES: usize = 32 * 1024;

/// Per-direction read chunk for the splice loop.
const SPLICE_CHUNK: usize = 64 * 1024;

/// Maximum lifetime of a single forwarded WebSocket. Same 8 h cap as
/// the agent uses (matches the sandbox max-lifetime).
const WS_MAX_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);

/// Resolve the WS-Upgrade port. Reads `SANDBOX_PREVIEW_WS_PORT`,
/// falls back to [`DEFAULT_WS_PORT`].
pub fn ws_port_from_env() -> u16 {
    std::env::var("SANDBOX_PREVIEW_WS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_WS_PORT)
}

/// Bind the controller's WS listener and serve forever.
pub async fn serve(state: Arc<AppState>, port: u16) -> io::Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse().expect("valid bind");
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr = %addr, "sandbox/preview_ws listening");

    let active = Arc::new(AtomicU64::new(0));

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let st = state.clone();
                let active_c = active.clone();
                spawn(async move {
                    active_c.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = handle_connection(st, stream, peer).await {
                        tracing::warn!(error = %e, "sandbox/preview_ws connection error");
                    }
                    active_c.fetch_sub(1, Ordering::Relaxed);
                })
                .detach();
            }
            Err(e) => {
                tracing::warn!(error = %e, "sandbox/preview_ws accept error");
                continue;
            }
        }
    }
}

/// Handle a single inbound WS-Upgrade connection.
async fn handle_connection(
    state: Arc<AppState>,
    mut down: TcpStream,
    _peer: SocketAddr,
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let head_end = match read_request_head(&mut down, &mut buf).await {
        Ok(n) => n,
        Err(e) => {
            let _ = write_status(&mut down, 400, "Bad Request", b"bad request").await;
            return Err(e);
        }
    };
    let head = &buf[..head_end];
    let body_start = head_end;

    let mut header_storage = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut header_storage);
    let parsed = match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => req,
        _ => {
            let _ = write_status(&mut down, 400, "Bad Request", b"parse").await;
            return Ok(());
        }
    };

    let method = parsed.method.unwrap_or("").to_string();
    let raw_path = parsed.path.unwrap_or("").to_string();

    if !is_websocket_upgrade(&parsed) {
        // The WS port serves Upgrade only.
        write_status(&mut down, 426, "Upgrade Required", b"this listener only serves WebSocket Upgrade").await?;
        return Ok(());
    }
    if buf.len() > body_start {
        write_status(&mut down, 400, "Bad Request", b"upgrade body must be empty").await?;
        return Ok(());
    }

    // Parse the controller's preview path:
    // /sandboxes/{id}/preview/{port}/{path*}
    let (sandbox_id, port, sub_path) = match parse_preview_path(&raw_path) {
        Some(t) => t,
        None => {
            write_status(&mut down, 404, "Not Found", b"unknown path").await?;
            return Ok(());
        }
    };

    // Authenticate (bearer token from Authorization header).
    let token_ok = check_bearer(&parsed, &state);
    let user_id = extract_user_id_from_query(&raw_path);

    // Coalesced authorize: principal-OK + sandbox-exists + ownership +
    // port-allowed. Any failure → uniform 401/404 (round-6 H4).
    let info_opt = state.sandboxes.get(&sandbox_id);
    let port_allowed = is_proxyable_port(port, DEFAULT_DENY);

    let authorized = token_ok
        && user_id.is_some()
        && info_opt.is_some()
        && port_allowed
        && info_opt
            .as_ref()
            .map(|i| Some(&i.user_id) == user_id.as_ref())
            .unwrap_or(false);

    if !authorized {
        if !token_ok || user_id.is_none() {
            write_status(&mut down, 401, "Unauthorized", b"unauthorized").await?;
            return Ok(());
        }
        write_status(&mut down, 404, "Not Found", b"not found").await?;
        return Ok(());
    }

    // Resolve agent_url + signing_key.
    let auth_bundle = match state.backend.session_auth(sandbox_id).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "sandbox/preview_ws session_auth failed");
            write_status(&mut down, 502, "Bad Gateway", b"agent unreachable").await?;
            return Ok(());
        }
    };

    // Derive the agent WS host:port. agent_url is
    // `http://<host>:<port>` (loopback in tests, 10.99.<idx>.2:7777
    // in nomad-ch). The WS port convention is HTTP port + 1; we
    // extract host + http_port and substitute http_port + 1.
    let agent_ws_addr = match derive_agent_ws_addr(&auth_bundle.agent_url) {
        Some(a) => a,
        None => {
            tracing::warn!(
                agent_url = %auth_bundle.agent_url,
                "sandbox/preview_ws: couldn't derive WS addr from agent_url"
            );
            write_status(&mut down, 502, "Bad Gateway", b"agent address unparsable").await?;
            return Ok(());
        }
    };

    // Build the agent-side path-query: `/proxy/{port}/{sub_path}?{query}`.
    let agent_path_query = build_agent_path_query(port, &sub_path, &raw_path);

    // Sign the V1_1_Ws canonical.
    let ts = unix_now();
    let nonce = mint_nonce();
    let signature = sig::sign_kind(
        CanonicalKind::V1_1_Ws,
        &auth_bundle.signing_key,
        &method,
        &agent_path_query,
        b"",
        ts,
        &nonce,
    );

    // Dial the agent's WS port.
    let mut up = match TcpStream::connect(agent_ws_addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(addr = %agent_ws_addr, error = %e, "sandbox/preview_ws dial agent failed");
            write_status(&mut down, 502, "Bad Gateway", b"agent unreachable").await?;
            return Ok(());
        }
    };

    // Build the outbound request bytes: rewrite request line to use
    // the agent path, drop hop-by-hop + X-Sbx-* + Authorization,
    // inject our X-Sbx-* signature headers + X-Forwarded-* metadata.
    let outbound = build_outbound_request(
        &method,
        &agent_path_query,
        &agent_ws_addr,
        &parsed,
        ts,
        &nonce,
        &signature,
        &sandbox_id,
        port,
    );

    let compio::BufResult(res, _) = up.write_all(outbound).await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "sandbox/preview_ws write to agent failed");
        let _ = write_status(&mut down, 502, "Bad Gateway", b"upstream write failed").await;
        return Ok(());
    }

    // Read the agent's response head.
    let mut up_head_buf = Vec::with_capacity(4096);
    let up_head_end = match read_response_head(&mut up, &mut up_head_buf).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "sandbox/preview_ws read agent response failed");
            let _ = write_status(&mut down, 502, "Bad Gateway", b"agent malformed response").await;
            return Ok(());
        }
    };
    let (status_code, _) = match parse_response_status(&up_head_buf[..up_head_end]) {
        Some(s) => s,
        None => {
            let _ = write_status(&mut down, 502, "Bad Gateway", b"agent parse failed").await;
            return Ok(());
        }
    };

    // Forward the agent response head to the client. On 101 the
    // header is minimal (no Set-Cookie / Location to rewrite); on
    // non-101 we treat the response as a normal HTTP error and pass
    // it through.
    let resp_head = up_head_buf[..up_head_end].to_vec();
    let compio::BufResult(res, _) = down.write_all(resp_head).await;
    if res.is_err() {
        return Ok(());
    }

    if status_code != 101 {
        if up_head_buf.len() > up_head_end {
            let extra = up_head_buf[up_head_end..].to_vec();
            let compio::BufResult(_, _) = down.write_all(extra).await;
        }
        let _ = copy_until_eof(&mut up, &mut down).await;
        return Ok(());
    }

    // 101 — splice. Forward any post-101 bytes the agent may have
    // already buffered alongside its response head.
    if up_head_buf.len() > up_head_end {
        let extra = up_head_buf[up_head_end..].to_vec();
        let compio::BufResult(res, _) = down.write_all(extra).await;
        if res.is_err() {
            return Ok(());
        }
    }

    splice_bidi(down, up).await
}

/// Bidirectional TCP splice with a lifetime cap.
async fn splice_bidi(down: TcpStream, up: TcpStream) -> io::Result<()> {
    let (down_r, mut down_w) = down.into_split();
    let (up_r, mut up_w) = up.into_split();

    let to_up = async move {
        let mut r = down_r;
        loop {
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

    let lifetime = compio::time::sleep(WS_MAX_LIFETIME);
    let pair = futures::future::join(to_up, to_down);
    futures::future::select(std::pin::pin!(pair), std::pin::pin!(lifetime)).await;
    Ok(())
}

// ─── helpers ──────────────────────────────────────────────────────

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mint_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("ws-{pid}-{n}-{nanos}")
}

/// Read an HTTP request head ending in `\r\n\r\n`. Returns the offset
/// of the byte right after the second CRLF.
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

/// Same shape as `read_request_head` but for an HTTP/1.1 response.
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
                "agent closed before response head completed",
            ));
        }
        buf.extend_from_slice(&scratch[..n]);
    }
}

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

fn parse_response_status(head: &[u8]) -> Option<(u16, &str)> {
    let line = head.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).ok()?;
    let mut parts = line.splitn(3, ' ');
    let _v = parts.next()?;
    let code = parts.next()?.parse::<u16>().ok()?;
    Some((code, parts.next().unwrap_or("")))
}

/// Parse `/sandboxes/{uuid}/preview/{port}/{sub_path}[?query]`.
/// Returns `(sandbox_id, port, sub_path)`. Query and fragment are
/// stripped — we re-attach the query when building the agent path.
fn parse_preview_path(path: &str) -> Option<(Uuid, u16, String)> {
    let no_frag = path.split('#').next().unwrap_or(path);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let stripped = no_query.strip_prefix("/sandboxes/")?;
    // sandbox-id /preview/ port / sub_path
    let (id_str, rest) = stripped.split_once('/')?;
    let sandbox_id: Uuid = id_str.parse().ok()?;
    let rest = rest.strip_prefix("preview/")?;
    let (port_str, sub_path) = rest.split_once('/').unwrap_or((rest, ""));
    let port: u16 = port_str.parse().ok()?;
    Some((sandbox_id, port, sub_path.to_string()))
}

/// Build the agent-side path-query from the parsed preview path
/// components and the original request URL (to preserve the query).
fn build_agent_path_query(port: u16, sub_path: &str, original_path: &str) -> String {
    // Take the query from the original URL, if any. Strip user_id
    // (controller-internal — agent doesn't need it).
    let query = original_path
        .split('?')
        .nth(1)
        .map(|q| q.split('#').next().unwrap_or(q))
        .unwrap_or("");
    let stripped_query: String = query
        .split('&')
        .filter(|kv| !kv.starts_with("user_id="))
        .collect::<Vec<_>>()
        .join("&");

    let base = format!("/proxy/{port}/{sub_path}");
    if stripped_query.is_empty() {
        base
    } else {
        format!("{base}?{stripped_query}")
    }
}

/// Detect a WebSocket-Upgrade request shape.
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

fn header_value<'a>(req: &'a httparse::Request, name: &str) -> Option<&'a str> {
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case(name) {
            return std::str::from_utf8(h.value).ok();
        }
    }
    None
}

/// Check the bearer token. Mirrors `auth::check` (constant-time
/// compare to defeat token-prefix oracles). Reused inline rather than
/// pulled in via `auth::check` because `auth::check` takes an
/// `ntex::HttpRequest`, and we have a `httparse::Request` here.
fn check_bearer(req: &httparse::Request, state: &AppState) -> bool {
    use subtle::ConstantTimeEq;
    if state.config.token.is_empty() {
        // Same opt-in shape as `auth::check`: dev-mode allows
        // unauthenticated when the operator explicitly turned auth off.
        return true;
    }
    let auth = match header_value(req, "authorization") {
        Some(a) => a,
        None => return false,
    };
    // Match auth::check's prefix exactly — it accepts only "Bearer "
    // (capital B). Don't loosen here.
    let presented = auth.strip_prefix("Bearer ").unwrap_or("").as_bytes();
    let expected = state.config.token.as_bytes();
    if presented.len() != expected.len() {
        return false;
    }
    presented.ct_eq(expected).into()
}

/// Pull `?user_id=…` out of the request URL.
fn extract_user_id_from_query(path: &str) -> Option<String> {
    let q = path.split('?').nth(1)?.split('#').next().unwrap_or("");
    for kv in q.split('&') {
        if let Some(v) = kv.strip_prefix("user_id=") {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Derive the agent's WS host:port from a `http://host:port[/...]`
/// agent_url. We use the convention `ws_port = http_port + 1`
/// (matches `proxy_ws::DEFAULT_WS_PORT` in the agent).
fn derive_agent_ws_addr(agent_url: &str) -> Option<SocketAddr> {
    let stripped = agent_url
        .strip_prefix("http://")
        .or_else(|| agent_url.strip_prefix("https://"))?;
    let host_port = stripped.split('/').next().unwrap_or(stripped);
    let (host, http_port) = host_port.rsplit_once(':')?;
    let http_port: u16 = http_port.parse().ok()?;
    let ws_port = http_port.checked_add(1)?;
    let resolved: SocketAddr = format!("{host}:{ws_port}").parse().ok()?;
    Some(resolved)
}

/// Build the outbound HTTP/1.1 request bytes for the agent. Drops
/// hop-by-hop, X-Sbx-*, X-ZSPreview-Host, Authorization, and any
/// inbound X-Forwarded-*; injects our own.
#[allow(clippy::too_many_arguments)]
fn build_outbound_request(
    method: &str,
    agent_path_query: &str,
    agent_addr: &SocketAddr,
    req: &httparse::Request,
    ts: u64,
    nonce: &str,
    signature: &str,
    sandbox_id: &Uuid,
    port: u16,
) -> Vec<u8> {
    let preview_host = compute_preview_host(sandbox_id, port);

    let mut out = String::new();
    out.push_str(&format!("{method} {agent_path_query} HTTP/1.1\r\n"));
    // Host: always the agent target (for the agent's own routing /
    // ntex sees this on plain HTTP; the WS port doesn't strictly
    // need it but keeps the request shape sane).
    out.push_str(&format!("Host: {agent_addr}\r\n"));

    let drop_lower = ["host", "authorization", "x-zspreview-host"];
    let drop_prefix = ["x-sbx-", "x-forwarded-"];

    for h in req.headers.iter() {
        let nl = h.name.to_ascii_lowercase();
        // Hop-by-hop except Upgrade/Connection — those we MUST forward
        // (they're the WS handshake).
        if matches!(
            nl.as_str(),
            "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
        ) {
            continue;
        }
        if drop_lower.contains(&nl.as_str()) {
            continue;
        }
        if drop_prefix.iter().any(|p| nl.starts_with(p)) {
            continue;
        }
        if let Ok(v) = std::str::from_utf8(h.value) {
            out.push_str(&format!("{name}: {v}\r\n", name = h.name));
        }
    }

    // X-Forwarded-* for the agent's response-rewriter (HTTP path only;
    // on a 101 the body is empty anyway, but agents that reuse the
    // header for audit/log will see consistent values).
    out.push_str(&format!("X-Forwarded-Host: {preview_host}\r\n"));
    out.push_str("X-Forwarded-Proto: https\r\n");

    // X-Sbx-* signature headers.
    out.push_str(&format!("X-Sbx-Timestamp: {ts}\r\n"));
    out.push_str(&format!("X-Sbx-Nonce: {nonce}\r\n"));
    out.push_str(&format!("X-Sbx-Signature: {signature}\r\n"));

    // End of head.
    out.push_str("\r\n");
    out.into_bytes()
}

/// Compute the preview hostname pattern. Mirrors the function in
/// `preview.rs`; duplicated here to avoid pulling the whole HTTP
/// handler module in. Phase 4 will expose this from a shared helper.
fn compute_preview_host(sandbox_id: &Uuid, port: u16) -> String {
    let s = sandbox_id.to_string();
    let slug: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("preview-{slug}-{port}.preview.zeroship.dev")
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_preview_path() {
        let id = Uuid::now_v7();
        let path = format!("/sandboxes/{id}/preview/5173/foo/bar?user_id=alice");
        let (sb, p, sp) = parse_preview_path(&path).unwrap();
        assert_eq!(sb, id);
        assert_eq!(p, 5173);
        assert_eq!(sp, "foo/bar");
    }

    #[test]
    fn parses_preview_path_root() {
        let id = Uuid::now_v7();
        let path = format!("/sandboxes/{id}/preview/3000/");
        let (_sb, p, sp) = parse_preview_path(&path).unwrap();
        assert_eq!(p, 3000);
        assert_eq!(sp, "");
    }

    #[test]
    fn rejects_bad_preview_path() {
        assert!(parse_preview_path("/exec").is_none());
        assert!(parse_preview_path("/sandboxes/not-a-uuid/preview/5173/").is_none());
        assert!(parse_preview_path("/sandboxes/01234567-89ab-cdef-0123-456789abcdef/notpreview/5173/").is_none());
    }

    #[test]
    fn derives_agent_ws_addr() {
        let a = derive_agent_ws_addr("http://127.0.0.1:7777").unwrap();
        assert_eq!(a.port(), 7778);
        assert_eq!(a.ip().to_string(), "127.0.0.1");
        // With a trailing path the agent_url still parses.
        let b = derive_agent_ws_addr("http://10.99.101.2:7777/").unwrap();
        assert_eq!(b.port(), 7778);
    }

    #[test]
    fn rejects_unparsable_agent_url() {
        assert!(derive_agent_ws_addr("not-a-url").is_none());
        assert!(derive_agent_ws_addr("http://example.com").is_none()); // no port
    }

    #[test]
    fn builds_agent_path_query_strips_user_id() {
        let p = build_agent_path_query(5173, "foo", "/sandboxes/x/preview/5173/foo?user_id=alice&t=1");
        assert_eq!(p, "/proxy/5173/foo?t=1");
    }

    #[test]
    fn builds_agent_path_query_no_query() {
        let p = build_agent_path_query(5173, "foo", "/sandboxes/x/preview/5173/foo");
        assert_eq!(p, "/proxy/5173/foo");
    }

    #[test]
    fn builds_agent_path_query_only_user_id() {
        // user_id alone → empty query → no `?` appended.
        let p = build_agent_path_query(5173, "foo", "/sandboxes/x/preview/5173/foo?user_id=alice");
        assert_eq!(p, "/proxy/5173/foo");
    }

    #[test]
    fn computes_preview_host_consistently() {
        let id: Uuid = "01234567-89ab-cdef-0123-456789abcdef".parse().unwrap();
        let h = compute_preview_host(&id, 5173);
        assert_eq!(
            h,
            "preview-0123456789abcdef0123456789abcdef-5173.preview.zeroship.dev"
        );
    }

    #[test]
    fn ws_upgrade_detected() {
        let raw = b"GET /sandboxes/x/preview/5173/ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
        let mut hdrs = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut hdrs);
        req.parse(raw).unwrap();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn extracts_user_id() {
        assert_eq!(
            extract_user_id_from_query("/x?user_id=alice"),
            Some("alice".to_string())
        );
        assert_eq!(
            extract_user_id_from_query("/x?foo=1&user_id=bob&bar=2"),
            Some("bob".to_string())
        );
        assert_eq!(extract_user_id_from_query("/x?other=1"), None);
        assert_eq!(extract_user_id_from_query("/x?user_id="), None);
        assert_eq!(extract_user_id_from_query("/x"), None);
    }
}
