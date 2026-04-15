use std::time::Instant;

use compio::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;

use crate::config::Config;
use crate::http;
use crate::stats::ThreadStats;

/// Run one worker thread: pin to CPU, spin up a compio runtime, open N connections,
/// send requests in a tight loop until the duration expires.
pub fn run_worker(config: &Config, _thread_id: usize, cpu: usize) -> ThreadStats {
    crate::numa::pin_to_cpu(cpu);

    compio::runtime::Runtime::new()
        .expect("failed to create compio runtime")
        .block_on(worker_loop(config))
}

async fn worker_loop(config: &Config) -> ThreadStats {
    let mut stats = ThreadStats::new();
    let conns_per_thread = config.connections_per_thread();
    let request_bytes: Vec<u8> = http::build_request(config);
    let deadline = Instant::now() + config.duration;
    let addr = format!("{}:{}", config.host, config.port);

    // Open initial connections (best-effort; failures are recorded)
    let mut connections: Vec<Option<TcpStream>> = Vec::with_capacity(conns_per_thread);
    for _ in 0..conns_per_thread {
        match TcpStream::connect(addr.as_str()).await {
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
            match TcpStream::connect(addr.as_str()).await {
                Ok(stream) => connections[conn_idx] = Some(stream),
                Err(_) => {
                    stats.errors_connect += 1;
                    conn_idx = (conn_idx + 1) % conns_per_thread;
                    continue;
                }
            }
        }

        let stream = connections[conn_idx].as_mut().unwrap();
        let start = Instant::now();

        // --- Send request ---
        // write_all takes ownership of the buffer and returns it; we clone so
        // request_bytes is reusable across iterations.
        let BufResult(write_res, _buf) = stream.write_all(request_bytes.clone()).await;
        if write_res.is_err() {
            stats.errors_write += 1;
            connections[conn_idx] = None;
            conn_idx = (conn_idx + 1) % conns_per_thread;
            continue;
        }

        // --- Read response ---
        // compio's read() takes ownership of the buffer and returns (Result<usize>, buf).
        // We accumulate bytes by repeatedly calling read() and extending a scratch Vec.
        let mut buf: Vec<u8> = Vec::with_capacity(65536);
        let mut parsed: Option<http::ParsedResponse> = None;
        let mut read_error = false;

        loop {
            // read() appends into the Vec's spare capacity.
            let BufResult(read_res, returned_buf) = stream.read(buf).await;
            buf = returned_buf;

            match read_res {
                Ok(0) => {
                    // Server closed connection
                    connections[conn_idx] = None;
                    break;
                }
                Ok(_) => {
                    // Parse headers once we have enough data
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

                        // If buffer is full but response not complete, grow it
                        if buf.len() == buf.capacity() {
                            buf.reserve(65536);
                        }
                    } else {
                        // Haven't parsed headers yet — need more data
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

            if p.status >= 400 {
                stats.errors_status += 1;
            }

            if !p.keep_alive {
                connections[conn_idx] = None;
            }
        }

        conn_idx = (conn_idx + 1) % conns_per_thread;
    }

    stats
}
