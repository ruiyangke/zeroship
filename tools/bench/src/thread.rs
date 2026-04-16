use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use compio::BufResult;

use crate::config::Config;
use crate::http;
use crate::lua::LuaScript;
use crate::stats::{LiveStats, ThreadStats};
use crate::tls::MaybeStream;

/// Run one worker thread: pin to CPU, spin up a compio runtime, open N
/// connections, and drive each one concurrently in its own task until the
/// deadline expires.
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
    let conns_per_thread = config.connections_per_thread();

    // Build the default request once per thread. If a Lua script is loaded,
    // honor its mutations to the `wrk` global (method/body/headers/path).
    let default_request_bytes: Rc<Vec<u8>> = Rc::new(match script.as_ref() {
        Some(s) => s.build_request_from_wrk(config),
        None => http::build_request(config),
    });

    let deadline = Instant::now() + config.duration;

    // Shared per-thread state — tasks run cooperatively on the same compio
    // runtime, so Rc<RefCell<...>> is the right tool.
    let stats = Rc::new(RefCell::new(ThreadStats::new()));
    let script = script.map(Rc::new);

    // Spawn one task per connection so they make progress concurrently.
    // The previous implementation served connections one-at-a-time in
    // round-robin, which turned a 150µs p50 into a 3.7ms (conns-per-thread
    // × p50) round-trip and capped throughput at ~6K req/s/thread.
    let mut tasks = Vec::with_capacity(conns_per_thread);
    for _ in 0..conns_per_thread {
        let host = config.host.clone();
        let port = config.port;
        let tls = config.tls;
        let req_bytes = default_request_bytes.clone();
        let stats = stats.clone();
        let script = script.clone();
        let live = live.clone();

        tasks.push(compio::runtime::spawn(async move {
            connection_task(host, port, tls, req_bytes, deadline, stats, script, live).await;
        }));
    }

    for t in tasks {
        let _ = t.await;
    }

    Rc::try_unwrap(stats)
        .unwrap_or_else(|_| unreachable!("all spawned tasks have completed"))
        .into_inner()
}

/// Drive one keep-alive connection: connect, then issue request/response
/// cycles in a tight loop until the deadline expires.
async fn connection_task(
    host: String,
    port: u16,
    tls: bool,
    req_bytes: Rc<Vec<u8>>,
    deadline: Instant,
    stats: Rc<RefCell<ThreadStats>>,
    script: Option<Rc<LuaScript>>,
    live: Option<Arc<LiveStats>>,
) {
    let mut stream: MaybeStream = match crate::tls::connect(&host, port, tls).await {
        Ok(s) => s,
        Err(_) => {
            stats.borrow_mut().errors_connect += 1;
            return;
        }
    };

    // Reuse a single read buffer across requests to avoid a 64KB alloc per
    // response. Responses on the benchmark path are small and the buffer
    // grows on demand.
    let mut read_buf: Vec<u8> = Vec::with_capacity(8192);

    while Instant::now() < deadline {
        // Determine request bytes: Lua `request()` override or default.
        let request_bytes: Vec<u8> = script
            .as_deref()
            .and_then(|s| if s.has_request() { s.call_request() } else { None })
            .unwrap_or_else(|| (*req_bytes).clone());

        let start = Instant::now();

        // --- Send request ---
        let BufResult(write_res, _buf) = stream.write_all(request_bytes).await;
        if write_res.is_err() {
            stats.borrow_mut().errors_write += 1;
            return;
        }

        // --- Read response ---
        read_buf.clear();
        let mut parsed: Option<http::ParsedResponse> = None;

        loop {
            let BufResult(read_res, returned_buf) = stream.read(read_buf).await;
            read_buf = returned_buf;

            match read_res {
                Ok(0) => {
                    // Server closed the connection — end this task.
                    return;
                }
                Ok(_) => {
                    if parsed.is_none() {
                        parsed = http::parse_response(&read_buf);
                    }

                    if let Some(ref p) = parsed {
                        let body_available = read_buf.len().saturating_sub(p.header_len);
                        let complete = if p.chunked {
                            let body_slice = &read_buf[p.header_len..];
                            http::find_chunked_end(body_slice).is_some()
                        } else {
                            body_available >= p.content_length
                        };

                        if complete {
                            break;
                        }

                        if read_buf.len() == read_buf.capacity() {
                            read_buf.reserve(65536);
                        }
                    } else if read_buf.len() == read_buf.capacity() {
                        read_buf.reserve(4096);
                    }
                }
                Err(_) => {
                    stats.borrow_mut().errors_read += 1;
                    return;
                }
            }
        }

        let elapsed = start.elapsed();

        if let Some(ref p) = parsed {
            {
                let mut s = stats.borrow_mut();
                s.record_latency(elapsed.as_micros() as u64);
                s.record_request(read_buf.len() as u64);
                if p.status >= 400 {
                    s.errors_status += 1;
                }
            }

            if let Some(ref l) = live {
                l.add_request(read_buf.len() as u64);
                if p.status >= 400 {
                    l.add_error();
                }
            }

            // Lua response() callback.
            if let Some(ref s) = script {
                if s.has_response() {
                    let header_slice = &read_buf[..p.header_len.min(read_buf.len())];
                    let body_slice = &read_buf[p.header_len.min(read_buf.len())..];
                    let headers_str = std::str::from_utf8(header_slice).unwrap_or("");
                    let body_str = std::str::from_utf8(body_slice).unwrap_or("");
                    s.call_response(p.status, headers_str, body_str);
                }
            }

            if !p.keep_alive {
                // Server signaled Connection: close — this slot is done.
                return;
            }
        }
    }
}
