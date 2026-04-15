/// Minimal RFC 6455 WebSocket implementation for benchmarking.
///
/// Supports: upgrade handshake, text/binary frames, ping/pong, close.
/// Does NOT support: fragmentation, extensions, permessage-deflate.
use std::time::Instant;

use base64::Engine as _;
use compio::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;

use crate::config::Config;
use crate::stats::WsThreadStats;

// ─── Frame types ─────────────────────────────────────────────────────────────

pub enum WsFrame {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

// ─── Handshake helpers ───────────────────────────────────────────────────────

/// Build a WebSocket HTTP/1.1 upgrade request.
/// Returns `(request_bytes, key_b64)` — the caller must keep `key_b64` to validate
/// the server's `Sec-WebSocket-Accept` header.
pub fn build_upgrade_request(host: &str, port: u16, path: &str) -> (Vec<u8>, String) {
    let key_bytes = generate_ws_key();
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);

    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key_b64}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );

    (req.into_bytes(), key_b64)
}

/// Validate a 101 Switching Protocols upgrade response per RFC 6455 §4.1.
pub fn validate_upgrade_response(buf: &[u8], key_b64: &str) -> bool {
    let header_end = match find_header_end(buf) {
        Some(pos) => pos,
        None => return false,
    };
    let header_str = match std::str::from_utf8(&buf[..header_end]) {
        Ok(s) => s,
        Err(_) => return false,
    };

    if !header_str.starts_with("HTTP/1.1 101") {
        return false;
    }

    let expected = compute_accept_key(key_b64);

    for line in header_str.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("sec-websocket-accept:") {
            let colon = lower.find(':').unwrap_or(0);
            let value = line[colon + 1..].trim();
            return value == expected;
        }
    }
    false
}

/// Returns the byte offset just past `"\r\n\r\n"`, or `None` if not found.
pub fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// Generate 16 pseudo-random bytes using system time + an atomic counter.
/// Sufficient for an unpredictable WebSocket key; not cryptographically strong.
fn generate_ws_key() -> [u8; 16] {
    use std::time::SystemTime;

    static COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;

    // Splitmix64 to spread the bits.
    let mut v = nanos.wrapping_add(count).wrapping_add(0x9e37_79b9_7f4a_7c15);
    v = (v ^ (v >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    v = (v ^ (v >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    v ^= v >> 31;

    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&v.to_le_bytes());
    let v2 = v
        .wrapping_add(0x6c62_272e_07bb_0142)
        .wrapping_mul(0x517c_c1b7_2722_0a95);
    key[8..].copy_from_slice(&v2.to_le_bytes());
    key
}

/// SHA-1(`key` + GUID) base64-encoded — the expected `Sec-WebSocket-Accept` value.
fn compute_accept_key(key_b64: &str) -> String {
    use sha1::{Digest, Sha1};

    let mut hasher = Sha1::new();
    hasher.update(key_b64.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let hash = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(hash)
}

// ─── Frame codec ─────────────────────────────────────────────────────────────

/// Encode a text frame with a random 4-byte mask.  Client frames MUST be masked
/// (RFC 6455 §5.3).
pub fn encode_text_frame(data: &[u8]) -> Vec<u8> {
    encode_masked_frame(1, data) // opcode 1 = text
}

/// Encode a close frame (opcode 8, empty payload, masked).
pub fn encode_close_frame() -> Vec<u8> {
    encode_masked_frame(8, b"")
}

/// Build a masked client frame: `FIN=1 | opcode` + MASK bit + length + mask + payload.
fn encode_masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let ext_bytes = extended_len_bytes(len);
    let mut frame = Vec::with_capacity(2 + ext_bytes + 4 + len);

    // Byte 0: FIN=1, RSV=0, opcode.
    frame.push(0x80 | (opcode & 0x0f));

    // Byte 1+: MASK=1, length.
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len < 65536 {
        frame.push(0x80 | 126u8);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127u8);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }

    let mask = generate_mask();
    frame.extend_from_slice(&mask);

    for (i, &b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }

    frame
}

fn extended_len_bytes(len: usize) -> usize {
    if len < 126 { 0 } else if len < 65536 { 2 } else { 8 }
}

/// Fast 4-byte mask from an atomic Weyl sequence — not crypto, just varied.
fn generate_mask() -> [u8; 4] {
    static MASK_CTR: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0x1234_5678);
    let v = MASK_CTR.fetch_add(0x9e37_79b9, std::sync::atomic::Ordering::Relaxed);
    v.to_le_bytes()
}

/// Attempt to decode one frame from `buf`.
/// Returns `Some((frame, bytes_consumed))` or `None` if more data is needed.
pub fn decode_frame(buf: &[u8]) -> Option<(WsFrame, usize)> {
    if buf.len() < 2 {
        return None;
    }

    let first = buf[0];
    let second = buf[1];
    let opcode = first & 0x0f;
    let masked = (second & 0x80) != 0;

    let raw_len = (second & 0x7f) as usize;
    let (payload_len, header_len) = if raw_len < 126 {
        (raw_len, 2usize)
    } else if raw_len == 126 {
        if buf.len() < 4 {
            return None;
        }
        (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
    } else {
        if buf.len() < 10 {
            return None;
        }
        (
            u64::from_be_bytes(buf[2..10].try_into().ok()?) as usize,
            10,
        )
    };

    let mask_len = if masked { 4 } else { 0 };
    let total = header_len + mask_len + payload_len;
    if buf.len() < total {
        return None;
    }

    let payload_start = header_len + mask_len;
    let raw_payload = &buf[payload_start..payload_start + payload_len];

    let payload: Vec<u8> = if masked {
        let mask = &buf[header_len..header_len + 4];
        raw_payload
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ mask[i % 4])
            .collect()
    } else {
        raw_payload.to_vec()
    };

    let frame = match opcode {
        1 => WsFrame::Text(payload),
        2 => WsFrame::Binary(payload),
        8 => WsFrame::Close,
        9 => WsFrame::Ping(payload),
        10 => WsFrame::Pong,
        _ => WsFrame::Binary(payload),
    };

    Some((frame, total))
}

// ─── Connection bundle ────────────────────────────────────────────────────────

struct WsConn {
    stream: TcpStream,
    /// Leftover bytes from previous reads (partial frames).
    recv_buf: Vec<u8>,
}

// ─── WebSocket worker ─────────────────────────────────────────────────────────

/// Entry point: pin CPU, create compio runtime, run the WS benchmark loop.
pub fn run_ws_worker(config: &Config, _thread_id: usize, cpu: usize) -> WsThreadStats {
    crate::numa::pin_to_cpu(cpu);

    compio::runtime::Runtime::new()
        .expect("failed to create compio runtime")
        .block_on(ws_worker_loop(config))
}

async fn ws_worker_loop(config: &Config) -> WsThreadStats {
    let mut stats = WsThreadStats::new();
    let conns_per_thread = config.connections_per_thread();
    let deadline = Instant::now() + config.duration;
    let addr = format!("{}:{}", config.host, config.port);

    let msg_payload: Vec<u8> = config
        .body
        .as_deref()
        .unwrap_or("ping")
        .as_bytes()
        .to_vec();

    // Open connections.
    let mut connections: Vec<Option<WsConn>> = Vec::with_capacity(conns_per_thread);
    for _ in 0..conns_per_thread {
        connections.push(open_ws_conn(&addr, config, &mut stats).await);
    }

    let mut conn_idx = 0;

    while Instant::now() < deadline {
        // Reconnect dead slot.
        if connections[conn_idx].is_none() {
            connections[conn_idx] = open_ws_conn(&addr, config, &mut stats).await;
            if connections[conn_idx].is_none() {
                conn_idx = (conn_idx + 1) % conns_per_thread;
                continue;
            }
        }

        let conn = connections[conn_idx].as_mut().unwrap();
        let start = Instant::now();

        // Send a masked text frame.
        let frame_bytes = encode_text_frame(&msg_payload);
        let BufResult(write_res, _) = conn.stream.write_all(frame_bytes).await;
        if write_res.is_err() {
            stats.errors_write += 1;
            connections[conn_idx] = None;
            conn_idx = (conn_idx + 1) % conns_per_thread;
            continue;
        }

        // Read until we get one data frame back.
        let mut got_response = false;
        let mut dead = false;

        'read: loop {
            // compio read() takes ownership of the buffer.
            let buf = std::mem::take(&mut conn.recv_buf);
            let BufResult(read_res, returned_buf) = conn.stream.read(buf).await;
            conn.recv_buf = returned_buf;

            match read_res {
                Ok(0) => {
                    dead = true;
                    break 'read;
                }
                Ok(_) => {
                    let mut offset = 0usize;

                    loop {
                        match decode_frame(&conn.recv_buf[offset..]) {
                            None => break,
                            Some((frame, consumed)) => {
                                offset += consumed;
                                match frame {
                                    WsFrame::Text(payload) | WsFrame::Binary(payload) => {
                                        let elapsed = start.elapsed();
                                        stats.record_rtt(elapsed.as_micros() as u64);
                                        stats.record_message(payload.len() as u64);
                                        got_response = true;
                                    }
                                    WsFrame::Ping(payload) => {
                                        // RFC 6455 §5.5.2: must respond with pong.
                                        let pong = encode_masked_frame(10, &payload);
                                        let BufResult(pong_res, _) =
                                            conn.stream.write_all(pong).await;
                                        if pong_res.is_err() {
                                            dead = true;
                                        }
                                    }
                                    WsFrame::Pong => { /* unsolicited pong — ignore */ }
                                    WsFrame::Close => {
                                        dead = true;
                                    }
                                }
                            }
                        }

                        if got_response || dead {
                            break;
                        }
                    }

                    // Discard consumed bytes, keep leftovers.
                    if offset > 0 {
                        conn.recv_buf.drain(..offset);
                    }

                    if got_response || dead {
                        break 'read;
                    }

                    if conn.recv_buf.len() == conn.recv_buf.capacity() {
                        conn.recv_buf.reserve(4096);
                    }
                }
                Err(_) => {
                    stats.errors_read += 1;
                    dead = true;
                    break 'read;
                }
            }
        }

        if dead {
            connections[conn_idx] = None;
        }

        conn_idx = (conn_idx + 1) % conns_per_thread;
    }

    // Send close frames to all live connections.
    for slot in &mut connections {
        if let Some(conn) = slot {
            let close = encode_close_frame();
            let _ = conn.stream.write_all(close).await;
        }
    }

    stats
}

/// Open a TCP connection and perform the WebSocket upgrade handshake.
/// Returns `None` and increments the appropriate error counter on any failure.
async fn open_ws_conn(
    addr: &str,
    config: &Config,
    stats: &mut WsThreadStats,
) -> Option<WsConn> {
    let mut stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => {
            stats.errors_connect += 1;
            return None;
        }
    };

    let (req_bytes, key_b64) =
        build_upgrade_request(&config.host, config.port, &config.path);

    let BufResult(write_res, _) = stream.write_all(req_bytes).await;
    if write_res.is_err() {
        stats.errors_upgrade += 1;
        return None;
    }

    // Read the 101 response.
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        let BufResult(read_res, returned_buf) = stream.read(buf).await;
        buf = returned_buf;

        match read_res {
            Ok(0) => {
                stats.errors_upgrade += 1;
                return None;
            }
            Ok(_) => {
                if find_header_end(&buf).is_some() {
                    break;
                }
                if buf.len() == buf.capacity() {
                    buf.reserve(4096);
                }
            }
            Err(_) => {
                stats.errors_upgrade += 1;
                return None;
            }
        }
    }

    if !validate_upgrade_response(&buf, &key_b64) {
        stats.errors_upgrade += 1;
        return None;
    }

    // Carry over any bytes that arrived after the HTTP headers.
    let header_end = find_header_end(&buf).unwrap_or(buf.len());
    let mut recv_buf = Vec::with_capacity(65536);
    recv_buf.extend_from_slice(&buf[header_end..]);

    Some(WsConn { stream, recv_buf })
}
