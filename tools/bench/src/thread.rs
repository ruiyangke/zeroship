use std::sync::Arc;
use std::time::Instant;

use compio::BufResult;

use crate::config::Config;
use crate::http;
use crate::lua::LuaScript;
use crate::stats::{LiveStats, ThreadStats};
use crate::tls::MaybeStream;

/// Run one worker thread: pin to CPU, spin up a compio runtime, open N connections,
/// send requests in a tight loop until the duration expires.
pub fn run_worker(
    config: &Config,
    _thread_id: usize,
    cpu: usize,
    live: Option<Arc<LiveStats>>,
) -> ThreadStats {
    crate::numa::pin_to_cpu(cpu);

    // Load the Lua script on this thread (mlua Lua state is not Send).
    let script = config.script.as_deref().and_then(|path| {
        match LuaScript::load(path, config) {
            Ok(s) => Some(s),
            Err(e) => { eprintln!("[lua] failed to load script: {e}"); None }
        }
    });

    compio::runtime::Runtime::new()
        .expect("failed to create compio runtime")
        .block_on(worker_loop(config, script, live))
}

async fn worker_loop(
    config: &Config,
    script: Option<LuaScript>,
    live: Option<Arc<LiveStats>>,
) -> ThreadStats {
    let mut stats = ThreadStats::new();
    let conns_per_thread = config.connections_per_thread();
    let default_request_bytes: Vec<u8> = http::build_request(config);
    let deadline = Instant::now() + config.duration;

    // Open initial connections (best-effort; failures are recorded)
    let mut connections: Vec<Option<MaybeStream>> = Vec::with_capacity(conns_per_thread);
    for _ in 0..conns_per_thread {
        match crate::tls::connect(&config.host, config.port, config.tls).await {
            Ok(stream) => connections.push(Some(stream)),
            Err(_) => {
                stats.errors_connect += 1;
                connections.push(None);
            }
        }
    }

    let mut conn_idx = 0;

    while Instant::now() < deadline {
        // Reconnect if this slot is dead
        if connections[conn_idx].is_none() {
            match crate::tls::connect(&config.host, config.port, config.tls).await {
                Ok(stream) => connections[conn_idx] = Some(stream),
                Err(_) => {
                    stats.errors_connect += 1;
                    conn_idx = (conn_idx + 1) % conns_per_thread;
                    continue;
                }
            }
        }

        // Determine request bytes: Lua `request()` override or default.
        let request_bytes = script
            .as_ref()
            .and_then(|s| if s.has_request() { s.call_request() } else { None })
            .unwrap_or_else(|| default_request_bytes.clone());

        let stream = connections[conn_idx].as_mut().unwrap();
        let start = Instant::now();

        // --- Send request ---
        let BufResult(write_res, _buf) = stream.write_all(request_bytes).await;
        if write_res.is_err() {
            stats.errors_write += 1;
            connections[conn_idx] = None;
            conn_idx = (conn_idx + 1) % conns_per_thread;
            continue;
        }

        // --- Read response ---
        let mut buf: Vec<u8> = Vec::with_capacity(65536);
        let mut parsed: Option<http::ParsedResponse> = None;
        let mut read_error = false;

        loop {
            let BufResult(read_res, returned_buf) = stream.read(buf).await;
            buf = returned_buf;

            match read_res {
                Ok(0) => {
                    connections[conn_idx] = None;
                    break;
                }
                Ok(_) => {
                    if parsed.is_none() {
                        parsed = http::parse_response(&buf);
                    }

                    if let Some(ref p) = parsed {
                        let body_available = buf.len().saturating_sub(p.header_len);

                        let complete = if p.chunked {
                            let body_slice = &buf[p.header_len..];
                            http::find_chunked_end(body_slice).is_some()
                        } else {
                            body_available >= p.content_length
                        };

                        if complete {
                            break;
                        }

                        if buf.len() == buf.capacity() {
                            buf.reserve(65536);
                        }
                    } else {
                        if buf.len() == buf.capacity() {
                            buf.reserve(4096);
                        }
                    }
                }
                Err(_) => {
                    stats.errors_read += 1;
                    connections[conn_idx] = None;
                    read_error = true;
                    break;
                }
            }
        }

        if read_error {
            conn_idx = (conn_idx + 1) % conns_per_thread;
            continue;
        }

        let elapsed = start.elapsed();

        if let Some(ref p) = parsed {
            stats.record_latency(elapsed.as_micros() as u64);
            stats.record_request(buf.len() as u64);

            // Update live stats for TUI.
            if let Some(ref l) = live {
                l.add_request(buf.len() as u64);
            }

            // Lua response() callback.
            if let Some(ref s) = script {
                if s.has_response() {
                    let header_slice = &buf[..p.header_len.min(buf.len())];
                    let body_slice = &buf[p.header_len.min(buf.len())..];
                    let headers_str = std::str::from_utf8(header_slice).unwrap_or("");
                    let body_str = std::str::from_utf8(body_slice).unwrap_or("");
                    s.call_response(p.status, headers_str, body_str);
                }
            }

            if p.status >= 400 {
                stats.errors_status += 1;
                if let Some(ref l) = live {
                    l.add_error();
                }
            }

            if !p.keep_alive {
                connections[conn_idx] = None;
            }
        }

        conn_idx = (conn_idx + 1) % conns_per_thread;
    }

    stats
}
