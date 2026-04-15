use std::time::Instant;

use compio::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;

use crate::config::Config;
use crate::stats::SseThreadStats;

/// Run one SSE worker thread: pin to CPU, spin up a compio runtime, open N long-lived
/// SSE connections and read streaming chunks until the duration expires.
pub fn run_sse_worker(config: &Config, _thread_id: usize, cpu: usize) -> SseThreadStats {
    crate::numa::pin_to_cpu(cpu);

    compio::runtime::Runtime::new()
        .expect("failed to create compio runtime")
        .block_on(sse_worker_loop(config))
}

async fn sse_worker_loop(config: &Config) -> SseThreadStats {
    let mut stats = SseThreadStats::new();
    let conns_per_thread = config.connections_per_thread();
    let request_bytes = build_sse_request(config);
    let deadline = Instant::now() + config.duration;
    let addr = format!("{}:{}", config.host, config.port);

    // Manage a pool of long-lived SSE connections.  Each connection is a
    // persistent streaming response so we keep all of them open concurrently by
    // driving them round-robin.  When one closes we reconnect immediately.
    //
    // Strategy: open all connections, then loop round-robin reading a small
    // buffer from each connection per iteration.  This is simpler than full
    // async fan-out and still achieves high throughput for benchmarking.

    let mut connections: Vec<SseConn> = Vec::with_capacity(conns_per_thread);
    for _ in 0..conns_per_thread {
        let conn = open_sse_connection(&addr, &request_bytes, &mut stats).await;
        connections.push(conn);
    }

    let mut conn_idx = 0;

    while Instant::now() < deadline {
        let conn = &mut connections[conn_idx];

        if conn.stream.is_none() {
            // Reconnect dead slot
            *conn = open_sse_connection(&addr, &request_bytes, &mut stats).await;
        }

        if conn.stream.is_some() {
            read_sse_chunks(conn, &mut stats, deadline).await;
        }

        conn_idx = (conn_idx + 1) % conns_per_thread;
    }

    stats
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// State for a single long-lived SSE connection.
struct SseConn {
    stream: Option<TcpStream>,
    /// Partial line buffer — bytes accumulated since the last `\n`.
    line_buf: Vec<u8>,
    /// Raw read buffer reused across calls.
    read_buf: Vec<u8>,
    /// Time of the last `data:` chunk (for inter-chunk latency).
    last_chunk_at: Option<Instant>,
    /// True once headers have been fully received.
    headers_done: bool,
    /// Time at which the TCP connect completed (for TTFB).
    connect_at: Instant,
    /// True once TTFB has been recorded for this connection.
    ttfb_recorded: bool,
}

impl SseConn {
    fn dead() -> Self {
        SseConn {
            stream: None,
            line_buf: Vec::new(),
            read_buf: Vec::with_capacity(65536),
            last_chunk_at: None,
            headers_done: false,
            connect_at: Instant::now(),
            ttfb_recorded: false,
        }
    }
}

/// Open a fresh SSE connection: TCP connect + send GET request.
async fn open_sse_connection(
    addr: &str,
    request_bytes: &[u8],
    stats: &mut SseThreadStats,
) -> SseConn {
    let connect_at = Instant::now();
    let mut stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => {
            stats.errors_connect += 1;
            return SseConn::dead();
        }
    };

    let BufResult(write_res, _) = stream.write_all(request_bytes.to_vec()).await;
    if write_res.is_err() {
        stats.errors_connect += 1;
        return SseConn::dead();
    }

    SseConn {
        stream: Some(stream),
        line_buf: Vec::new(),
        read_buf: Vec::with_capacity(65536),
        last_chunk_at: None,
        headers_done: false,
        connect_at,
        ttfb_recorded: false,
    }
}

/// Read one batch of bytes from the connection and process complete SSE lines.
/// Marks the connection dead on error or `data: [DONE]`.
async fn read_sse_chunks(
    conn: &mut SseConn,
    stats: &mut SseThreadStats,
    deadline: Instant,
) {
    let stream = match conn.stream.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Ensure the read buffer has spare capacity.
    if conn.read_buf.len() == conn.read_buf.capacity() {
        conn.read_buf.reserve(65536);
    }

    // Swap out read_buf so we can pass ownership to compio.
    let buf = std::mem::take(&mut conn.read_buf);
    let BufResult(read_res, returned_buf) = stream.read(buf).await;
    conn.read_buf = returned_buf;

    let n = match read_res {
        Ok(0) => {
            // Server closed the connection.
            conn.stream = None;
            stats.completed_streams += 1;
            return;
        }
        Ok(n) => n,
        Err(_) => {
            stats.errors_read += 1;
            conn.stream = None;
            return;
        }
    };

    let received_at = Instant::now();

    // If we're past the deadline there's nothing useful to record.
    if received_at > deadline {
        conn.stream = None;
        return;
    }

    // Record TTFB on the first byte ever received for this connection.
    if !conn.ttfb_recorded {
        let ttfb = received_at.duration_since(conn.connect_at).as_micros() as u64;
        stats.record_ttfb(ttfb);
        conn.ttfb_recorded = true;
    }

    stats.total_bytes += n as u64;

    // Feed bytes into the line buffer and process complete lines.
    // Copy to a local Vec so we release the borrow on conn.read_buf before
    // passing `conn` mutably to process_bytes.
    let new_bytes: Vec<u8> = conn.read_buf[conn.read_buf.len() - n..].to_vec();
    process_bytes(&new_bytes, conn, stats, received_at);
}

/// Scan `new_bytes` for `\n` boundaries, process each complete line, and leave
/// any trailing partial line in `conn.line_buf`.
fn process_bytes(
    new_bytes: &[u8],
    conn: &mut SseConn,
    stats: &mut SseThreadStats,
    received_at: Instant,
) {
    for &byte in new_bytes {
        if byte == b'\n' {
            // Trim trailing CR for CRLF line endings and clone into a local
            // buffer so we release the borrow on conn.line_buf before calling
            // process_line (which needs &mut conn).
            let line: Vec<u8> = if conn.line_buf.last() == Some(&b'\r') {
                conn.line_buf[..conn.line_buf.len() - 1].to_vec()
            } else {
                conn.line_buf.clone()
            };

            process_line(&line, conn, stats, received_at);
            conn.line_buf.clear();
        } else {
            conn.line_buf.push(byte);
        }
    }
}

/// Process a single complete SSE line (without the terminating `\n`).
fn process_line(
    line: &[u8],
    conn: &mut SseConn,
    stats: &mut SseThreadStats,
    received_at: Instant,
) {
    if !conn.headers_done {
        // We're still consuming the HTTP response headers.  The blank line
        // that separates headers from the body is the marker we wait for.
        if line.is_empty() {
            conn.headers_done = true;
        }
        return;
    }

    // SSE spec: lines that start with "data:" carry payload.
    if let Some(data) = strip_prefix_ci(line, b"data:") {
        let data = trim_leading_space(data);

        // `data: [DONE]` is the conventional end-of-stream sentinel.
        if data == b"[DONE]" {
            conn.stream = None;
            stats.completed_streams += 1;
            return;
        }

        // Record inter-chunk latency.
        if let Some(prev) = conn.last_chunk_at {
            let lat = received_at.duration_since(prev).as_micros() as u64;
            stats.record_chunk_latency(lat);
        }
        conn.last_chunk_at = Some(received_at);
        stats.total_chunks += 1;
    }
    // Other SSE field types (event:, id:, retry:, comment lines starting with
    // ':') are silently ignored — we only care about throughput metrics.
}

// ---------------------------------------------------------------------------
// Request builder
// ---------------------------------------------------------------------------

fn build_sse_request(config: &Config) -> Vec<u8> {
    let mut req = format!("GET {} HTTP/1.1\r\n", config.path);
    req.push_str(&format!("Host: {}:{}\r\n", config.host, config.port));
    req.push_str("Accept: text/event-stream\r\n");
    req.push_str("Cache-Control: no-cache\r\n");
    // Keep-alive is deliberately omitted: SSE connections are long-lived and
    // the server typically streams until the client disconnects.
    req.push_str("Connection: keep-alive\r\n");

    for (name, value) in &config.headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }

    req.push_str("\r\n");
    req.into_bytes()
}

// ---------------------------------------------------------------------------
// Byte-slice helpers
// ---------------------------------------------------------------------------

/// Case-insensitive prefix strip for short ASCII prefixes.
fn strip_prefix_ci<'a>(haystack: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if haystack.len() < prefix.len() {
        return None;
    }
    let matches = haystack[..prefix.len()]
        .iter()
        .zip(prefix.iter())
        .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase());
    if matches {
        Some(&haystack[prefix.len()..])
    } else {
        None
    }
}

/// Strip a single leading ASCII space, if present.
fn trim_leading_space(s: &[u8]) -> &[u8] {
    if s.first() == Some(&b' ') { &s[1..] } else { s }
}
