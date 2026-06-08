//! Single-connection Redis client. Owns a TcpStream + a reusable read
//! buffer. One command in flight at a time.

use std::net::IpAddr;
use std::time::Duration;

use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use compio::time::timeout;
use redis_protocol::resp2::types::OwnedFrame;
use url::Url;

use crate::error::{Error, Result};
use crate::protocol::{
    build_cmd, encode_frame, expect_array, expect_bulk_or_null, expect_integer, expect_ok,
    try_decode,
};

const READ_CHUNK: usize = 4096;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum single-reply size the client will accept, in bytes. Mirrors the
/// sibling `compio-postgres` driver's `MAX_MESSAGE_SIZE` (64 MB): a
/// malicious or MITM'd server that declares a giant bulk/array length
/// (`$2000000000\r\n…`) would otherwise grow the read buffer until the
/// worker OOMs. We reject the declared length up front (before buffering
/// the body) and also cap the accumulated buffer as a backstop.
const MAX_REPLY_SIZE: usize = 64 * 1024 * 1024;

/// Peek the declared length of a RESP reply from the front of `buf`,
/// returning `Some(declared)` when `buf` begins with a length-prefixed
/// header (`$` bulk, or one of the `*`/`%`/`~`/`>` aggregates) whose
/// `<len>\r\n` is fully present. Returns `None` if the leading byte is not
/// a length-prefixed kind or the header is still incomplete.
///
/// For `$` the declared length is the body byte count; for the aggregate
/// kinds it is the element count. Both are checked against `MAX_REPLY_SIZE`
/// by the caller: a reply cannot contain more elements than its byte cap
/// (each wire element is at least one byte), so the element-count ceiling
/// is a safe over-approximation that stops a `*2000000000\r\n` array bomb
/// from coercing a 2-billion-capacity `Vec` allocation in the decoder.
///
/// A negative length (`$-1`, RESP null) yields `None` — it is not an
/// oversized declaration.
fn peek_declared_len(buf: &[u8]) -> Option<u64> {
    let (&kind, rest) = buf.split_first()?;
    if !matches!(kind, b'$' | b'*' | b'%' | b'~' | b'>') {
        return None;
    }
    // Find the CRLF that terminates the length field.
    let crlf = rest.windows(2).position(|w| w == b"\r\n")?;
    let digits = &rest[..crlf];
    // RESP integers are ASCII. A leading '-' is a null/streaming marker,
    // not an oversized length — ignore it here.
    std::str::from_utf8(digits).ok()?.parse::<u64>().ok()
}

/// A Redis client owning one TCP connection.
pub struct Client {
    stream: TcpStream,
    /// Accumulating read buffer — holds partial frames between reads so a
    /// reply split across syscalls still decodes cleanly.
    rx: BytesMut,
    /// Scratch buffer for `read_exact`-style reads. Compio's ownership-
    /// transfer API returns the Vec back; we reuse it.
    read_scratch: Vec<u8>,
    cmd_timeout: Duration,
    /// "Dirty" barrier for safe pool reuse. Set SYNCHRONOUSLY at the start
    /// of `send_recv` (before the write/await) so it survives even if the
    /// future is cancelled or times out mid-reply, and cleared ONLY after a
    /// full frame is decoded and returned. A decode error or the OOM-cap
    /// rejection leaves it set. The pool (R2) refuses to recycle a dirty
    /// connection — a cancelled/errored conn may have a pending or partial
    /// reply on the wire, so reusing it would desync the reply stream.
    dirty: bool,
}

impl Client {
    /// Parse a `redis://[:password@]host[:port][/db]` URL and connect.
    pub async fn connect(url_str: &str) -> Result<Self> {
        let url = Url::parse(url_str)
            .map_err(|e| Error::Config(format!("bad URL '{url_str}': {e}")))?;
        if url.scheme() != "redis" {
            return Err(Error::Config(format!(
                "unsupported scheme '{}' (expected redis://)", url.scheme()
            )));
        }

        let host = url.host_str()
            .ok_or_else(|| Error::Config("missing host".into()))?;
        let port = url.port().unwrap_or(6379);
        let password = url.password().map(str::to_string);
        let db: Option<i64> = url
            .path()
            .trim_start_matches('/')
            .parse()
            .ok();

        let ip: IpAddr = host
            .parse()
            .map_err(|_| Error::Config(format!("expected ip, got hostname '{host}' — DNS not implemented")))?;

        let stream = timeout(DEFAULT_CONNECT_TIMEOUT, TcpStream::connect((ip, port)))
            .await
            .map_err(|_| Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut, "connect timed out",
            )))?
            .map_err(Error::Io)?;
        stream.set_nodelay(true).map_err(Error::Io)?;

        let mut client = Client {
            stream,
            rx: BytesMut::with_capacity(READ_CHUNK),
            read_scratch: vec![0u8; READ_CHUNK],
            cmd_timeout: DEFAULT_CMD_TIMEOUT,
            dirty: false,
        };

        if let Some(pw) = password {
            client.auth(&pw).await?;
        }
        if let Some(db) = db {
            client.select(db).await?;
        }
        Ok(client)
    }

    /// Dev-only constructor for in-process tests — connects without
    /// running AUTH/SELECT via the URL parser.
    pub async fn connect_tcp(addr: (IpAddr, u16)) -> Result<Self> {
        let stream = TcpStream::connect(addr).await.map_err(Error::Io)?;
        stream.set_nodelay(true).map_err(Error::Io)?;
        Ok(Client {
            stream,
            rx: BytesMut::with_capacity(READ_CHUNK),
            read_scratch: vec![0u8; READ_CHUNK],
            cmd_timeout: DEFAULT_CMD_TIMEOUT,
            dirty: false,
        })
    }

    /// True if this connection is in a "dirty" state — a command was
    /// started but no full reply has been cleanly decoded since (timed out,
    /// cancelled, decode-errored, or rejected by the size cap). The pool
    /// (R2) must NOT return a dirty connection to the idle stack.
    ///
    /// `allow(dead_code)`: consumed by the pool fail-safe in R2; for now only
    /// the in-crate red-team tests call it.
    #[allow(dead_code)]
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// True if the accumulating read buffer is empty. A non-empty `rx` at
    /// checkout time means leftover/partial reply bytes from a prior
    /// command — the pool (R2) treats that as "do not reuse".
    ///
    /// `allow(dead_code)`: consumed by the pool fail-safe in R2; for now only
    /// the in-crate red-team tests call it.
    #[allow(dead_code)]
    pub(crate) fn is_rx_empty(&self) -> bool {
        self.rx.is_empty()
    }

    /// Override the per-command timeout. Used by tests to drive the
    /// timeout/dirty-barrier paths deterministically without waiting the
    /// 5 s default; also lets the pool/cluster layers tune liveness.
    ///
    /// `allow(dead_code)`: test/R2-facing knob; no production caller yet.
    #[allow(dead_code)]
    pub(crate) fn set_cmd_timeout(&mut self, d: Duration) {
        self.cmd_timeout = d;
    }

    // -----------------------------------------------------------------
    // Command wrappers
    // -----------------------------------------------------------------

    pub async fn ping(&mut self) -> Result<()> {
        let frame = self.send_recv(build_cmd(&[b"PING"])).await?;
        match frame {
            OwnedFrame::SimpleString(s) if s == b"PONG" => Ok(()),
            OwnedFrame::Error(msg) => Err(Error::Server(msg)),
            other => Err(Error::Unexpected(format!("ping: {other:?}"))),
        }
    }

    /// Send the ASKING marker — used once, after an -ASK redirect, before
    /// replaying the redirected command. The target node answers with +OK.
    pub async fn asking(&mut self) -> Result<()> {
        let frame = self.send_recv(build_cmd(&[b"ASKING"])).await?;
        expect_ok(frame)
    }

    pub async fn auth(&mut self, password: &str) -> Result<()> {
        let frame = self.send_recv(build_cmd(&[b"AUTH", password.as_bytes()])).await?;
        expect_ok(frame).map_err(|e| match e {
            Error::Server(m) => Error::Auth(m),
            other => other,
        })
    }

    pub async fn select(&mut self, db: i64) -> Result<()> {
        let db_s = db.to_string();
        let frame = self.send_recv(build_cmd(&[b"SELECT", db_s.as_bytes()])).await?;
        expect_ok(frame)
    }

    /// GET a key. Returns None when the key doesn't exist.
    pub async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        let frame = self.send_recv(build_cmd(&[b"GET", key.as_bytes()])).await?;
        expect_bulk_or_null(frame)
    }

    /// SET key value [PX millis]. Value is any bytes. Always overwrites.
    pub async fn set(
        &mut self,
        key: &str,
        value: &[u8],
        ttl_ms: Option<u64>,
    ) -> Result<()> {
        let frame = if let Some(ms) = ttl_ms {
            let ms_s = ms.to_string();
            self.send_recv(build_cmd(&[b"SET", key.as_bytes(), value, b"PX", ms_s.as_bytes()])).await?
        } else {
            self.send_recv(build_cmd(&[b"SET", key.as_bytes(), value])).await?
        };
        expect_ok(frame)
    }

    /// `SET key value NX [PX ms]` — the canonical lock primitive.
    /// Returns `true` if the key was set (it didn't exist), `false` if the
    /// key already existed (NX condition failed). Redis returns null on the
    /// NX miss and "+OK" on success.
    pub async fn set_nx(
        &mut self,
        key: &str,
        value: &[u8],
        ttl_ms: Option<u64>,
    ) -> Result<bool> {
        let frame = if let Some(ms) = ttl_ms {
            let ms_s = ms.to_string();
            self.send_recv(build_cmd(&[
                b"SET", key.as_bytes(), value, b"NX", b"PX", ms_s.as_bytes(),
            ])).await?
        } else {
            self.send_recv(build_cmd(&[b"SET", key.as_bytes(), value, b"NX"])).await?
        };
        match frame {
            OwnedFrame::SimpleString(s) if s == b"OK" => Ok(true),
            OwnedFrame::Null => Ok(false),
            OwnedFrame::Error(msg) => Err(Error::Server(msg)),
            other => Err(Error::Unexpected(format!("SET NX: {other:?}"))),
        }
    }

    /// DEL key. Returns true if the key existed.
    pub async fn del(&mut self, key: &str) -> Result<bool> {
        let frame = self.send_recv(build_cmd(&[b"DEL", key.as_bytes()])).await?;
        Ok(expect_integer(frame)? > 0)
    }

    /// EXISTS key. Returns true if the key exists.
    pub async fn exists(&mut self, key: &str) -> Result<bool> {
        let frame = self.send_recv(build_cmd(&[b"EXISTS", key.as_bytes()])).await?;
        Ok(expect_integer(frame)? > 0)
    }

    /// PEXPIRE key ms — set/refresh the TTL in milliseconds.
    /// Returns true if the TTL was set (key exists), false if the key is
    /// missing and no TTL could be set.
    pub async fn pexpire(&mut self, key: &str, ttl_ms: u64) -> Result<bool> {
        let ms = ttl_ms.to_string();
        let frame = self.send_recv(build_cmd(&[
            b"PEXPIRE", key.as_bytes(), ms.as_bytes(),
        ])).await?;
        Ok(expect_integer(frame)? > 0)
    }

    /// PTTL key — remaining TTL in milliseconds. Wire semantics:
    ///   >= 0 → milliseconds remaining
    ///   -1   → key exists but has no TTL
    ///   -2   → key does not exist
    pub async fn pttl(&mut self, key: &str) -> Result<i64> {
        let frame = self.send_recv(build_cmd(&[b"PTTL", key.as_bytes()])).await?;
        expect_integer(frame)
    }

    pub async fn incr_by(&mut self, key: &str, delta: i64) -> Result<i64> {
        let d = delta.to_string();
        let frame = self.send_recv(build_cmd(&[b"INCRBY", key.as_bytes(), d.as_bytes()])).await?;
        expect_integer(frame)
    }

    /// PERSIST key — remove the TTL so the key never expires. Returns
    /// true when a TTL was removed, false when the key is missing or
    /// already had no TTL.
    pub async fn persist(&mut self, key: &str) -> Result<bool> {
        let frame = self.send_recv(build_cmd(&[b"PERSIST", key.as_bytes()])).await?;
        Ok(expect_integer(frame)? > 0)
    }

    /// EVAL a Lua script, returning the integer reply. `keys` are the
    /// `KEYS[1..]` arguments (slot-routed by Redis) and `args` are the
    /// `ARGV[1..]` arguments. Only the integer reply is decoded —
    /// sufficient for the atomic incr-with-TTL script (see plugin-kv).
    pub async fn eval(&mut self, script: &str, keys: &[&str], args: &[&str]) -> Result<i64> {
        let nkeys = keys.len().to_string();
        let mut parts: Vec<&[u8]> = Vec::with_capacity(3 + keys.len() + args.len());
        parts.push(b"EVAL");
        parts.push(script.as_bytes());
        parts.push(nkeys.as_bytes());
        for k in keys { parts.push(k.as_bytes()); }
        for a in args { parts.push(a.as_bytes()); }
        let frame = self.send_recv(build_cmd(&parts)).await?;
        expect_integer(frame)
    }

    /// DECRBY — negative counterpart. `decr_by(k, n)` == `incr_by(k, -n)`
    /// but ships the idiomatic command the Redis tools expect in MONITOR
    /// output etc.
    pub async fn decr_by(&mut self, key: &str, delta: i64) -> Result<i64> {
        let d = delta.to_string();
        let frame = self.send_recv(build_cmd(&[b"DECRBY", key.as_bytes(), d.as_bytes()])).await?;
        expect_integer(frame)
    }

    /// STRLEN key — byte length of the value (0 if key missing).
    pub async fn strlen(&mut self, key: &str) -> Result<u64> {
        let frame = self.send_recv(build_cmd(&[b"STRLEN", key.as_bytes()])).await?;
        Ok(expect_integer(frame)?.max(0) as u64)
    }

    /// MGET — batch fetch. Returns one `Option<Vec<u8>>` per key (None for
    /// missing keys). Preserves input order.
    pub async fn mget(&mut self, keys: &[&str]) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() { return Ok(Vec::new()); }
        let mut parts: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
        parts.push(b"MGET");
        for k in keys { parts.push(k.as_bytes()); }
        let frame = self.send_recv(build_cmd(&parts)).await?;
        let items = expect_array(frame)?;
        items.into_iter().map(expect_bulk_or_null).collect()
    }

    /// MSET — atomic batch write. All keys set or none.
    pub async fn mset(&mut self, kvs: &[(&str, &[u8])]) -> Result<()> {
        if kvs.is_empty() { return Ok(()); }
        let mut parts: Vec<&[u8]> = Vec::with_capacity(kvs.len() * 2 + 1);
        parts.push(b"MSET");
        for (k, v) in kvs {
            parts.push(k.as_bytes());
            parts.push(v);
        }
        let frame = self.send_recv(build_cmd(&parts)).await?;
        expect_ok(frame)
    }

    /// SCAN with a cursor + MATCH pattern. Returns (next_cursor, keys).
    /// Cursor of "0" starts iteration; when SCAN returns cursor "0" again,
    /// the scan is complete.
    pub async fn scan(
        &mut self,
        cursor: &str,
        pattern: &str,
        count: u32,
    ) -> Result<(String, Vec<String>)> {
        let c = count.to_string();
        let frame = self.send_recv(build_cmd(&[
            b"SCAN", cursor.as_bytes(), b"MATCH", pattern.as_bytes(), b"COUNT", c.as_bytes(),
        ])).await?;
        let items = expect_array(frame)?;
        if items.len() != 2 {
            return Err(Error::Unexpected(format!("SCAN: expected 2-elem array, got {}", items.len())));
        }
        let mut iter = items.into_iter();
        let cursor = expect_bulk_or_null(iter.next().unwrap())?
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        let keys_frame = iter.next().unwrap();
        let keys_arr = expect_array(keys_frame)?;
        let keys: Vec<String> = keys_arr
            .into_iter()
            .filter_map(|f| match expect_bulk_or_null(f).ok().flatten() {
                Some(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                None => None,
            })
            .collect();
        Ok((cursor, keys))
    }

    // -----------------------------------------------------------------
    // Wire I/O
    // -----------------------------------------------------------------

    /// Write a command frame, read until a full reply decodes, return it.
    pub(crate) async fn send_recv(&mut self, cmd: OwnedFrame) -> Result<OwnedFrame> {
        // Mark the connection dirty SYNCHRONOUSLY, before the write/await.
        // This is the only state observable at `PooledConn::drop` time after
        // the future is cancelled or times out mid-reply — so the pool (R2)
        // can refuse to recycle a connection that may have a pending or
        // partial reply still on the wire. We clear it ONLY after a full
        // frame is cleanly decoded and returned below; any error path
        // (timeout, IO, decode, or the OOM-cap rejection) leaves it set.
        self.dirty = true;
        let out = timeout(self.cmd_timeout, self.send_recv_inner(cmd))
            .await
            .map_err(|_| Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut, "redis command timed out",
            )))?;
        if out.is_ok() {
            self.dirty = false;
        }
        out
    }

    async fn send_recv_inner(&mut self, cmd: OwnedFrame) -> Result<OwnedFrame> {
        let wire = encode_frame(&cmd)?;
        let compio::BufResult(res, _buf) = self.stream.write_all(wire).await;
        res.map_err(Error::Io)?;
        // No flush — compio's TcpStream writes directly to the kernel; no
        // userspace buffering to drain. `nodelay` already prevents Nagle.

        loop {
            // OOM guard (early rejection): if the buffered reply begins with
            // a length-prefixed header whose declared length already exceeds
            // the cap, bail BEFORE we ever buffer the (multi-GB) body. This
            // is O(1) and mirrors compio-postgres's `validate_length`.
            if let Some(declared) = peek_declared_len(&self.rx)
                && declared > MAX_REPLY_SIZE as u64
            {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "redis reply too large: declared {declared} bytes \
                         (max {MAX_REPLY_SIZE}) — refusing to buffer",
                    ),
                )));
            }

            // Try to decode from what we already buffered.
            if !self.rx.is_empty()
                && let Some((frame, consumed)) = try_decode(&self.rx)?
            {
                let _ = self.rx.split_to(consumed);
                return Ok(frame);
            }

            // OOM backstop: even absent a parseable oversized header (e.g. a
            // huge aggregate of small elements, or a header not yet fully
            // arrived), never let the accumulating buffer exceed the cap
            // without yielding a complete frame.
            if self.rx.len() > MAX_REPLY_SIZE {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "redis reply too large: buffered {} bytes without a \
                         complete frame (max {MAX_REPLY_SIZE})",
                        self.rx.len(),
                    ),
                )));
            }

            // Need more bytes. Read a chunk.
            let scratch = std::mem::take(&mut self.read_scratch);
            let compio::BufResult(res, buf) = self.stream.read(scratch).await;
            let n = res.map_err(Error::Io)?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "redis connection closed",
                )));
            }
            self.rx.extend_from_slice(&buf[..n]);
            // Return the scratch buffer for reuse next iteration.
            self.read_scratch = buf;
        }
    }
}

// =====================================================================
// Red-team regression tests (R1 — REDIS-OOM-1 + dirty-flag infra).
//
// These drive the REAL client against a small in-process mock Redis
// (a `compio::net::TcpListener` speaking raw RESP bytes) so we can
// reproduce adversarial wire conditions a real server won't emit.
// =====================================================================
#[cfg(test)]
mod red_team_tests {
    use super::*;
    use compio::net::TcpListener;
    use std::time::Duration;

    /// Spawn a mock Redis that accepts ONE connection, reads (and discards)
    /// the client's command bytes, then writes `reply` and holds the socket
    /// open (sleeping) so the client can't rely on EOF. Returns the bound
    /// `host:port` for `Client::connect_tcp`.
    async fn mock_server_reply(reply: &'static [u8]) -> (std::net::IpAddr, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            // Read whatever command the client sends (one chunk is enough —
            // we don't parse it, we just need the client to have written).
            let buf = vec![0u8; 1024];
            let compio::BufResult(_n, _buf) = stream.read(buf).await;
            // Send the crafted reply.
            let _ = stream.write_all(reply).await;
            // Hold the connection open so the client never sees EOF; it must
            // bail on the size cap, not on a closed socket.
            compio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        })
        .detach();
        (addr.ip(), addr.port())
    }

    /// A mock that reads the command but NEVER replies, then holds the
    /// socket open — used to drive the command-timeout path.
    async fn mock_server_silent() -> (std::net::IpAddr, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let buf = vec![0u8; 1024];
            let compio::BufResult(_n, _buf) = stream.read(buf).await;
            // Never reply; just hold the conn open.
            compio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        })
        .detach();
        (addr.ip(), addr.port())
    }

    // -----------------------------------------------------------------
    // FIX 1 — REDIS-OOM-1: oversized declared reply length is rejected
    // promptly, WITHOUT buffering the (multi-GB) body.
    // -----------------------------------------------------------------
    #[compio::test]
    async fn oversized_bulk_reply_is_rejected_promptly() {
        // Bulk header declaring ~2 GB, plus a few filler body bytes. The
        // body is NEVER fully sent, so a pre-fix client buffers forever
        // (bounded only by the 5 s cmd_timeout) waiting for 2 GB.
        let (ip, port) = mock_server_reply(b"$2000000000\r\nABCDEFGH").await;
        let mut c = Client::connect_tcp((ip, port)).await.expect("connect mock");

        // Wrap in a SHORT timeout so a pre-fix hang fails cleanly (RED)
        // rather than blocking the test for 5 s. A correct client rejects
        // the oversized header essentially immediately (one read).
        let res = compio::time::timeout(Duration::from_secs(2), c.get("k")).await;

        // RED (pre-fix): this `timeout` itself elapses (the inner future is
        // stuck buffering), so `res` is Err(Elapsed) -> .expect panics.
        let inner = res.expect("client did not reject oversized reply promptly (it hung buffering)");

        // The inner result must be an error identifying an oversized reply.
        let err = inner.expect_err("oversized bulk reply must be an error, not Ok");
        let msg = err.to_string();
        let is_oversize = matches!(&err, Error::Protocol(_))
            || matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData);
        assert!(
            is_oversize,
            "expected an oversized-reply Protocol/InvalidData error, got: {err:?} ({msg})"
        );
        assert!(
            msg.contains("too large") || msg.to_lowercase().contains("oversiz") || msg.contains("exceed"),
            "error should name the size violation, got: {msg}"
        );
    }

    /// An oversized *aggregate* (array) header — a `*2000000000\r\n` "array
    /// bomb" — must also be rejected up front, not handed to the decoder
    /// (which would try to reserve a 2-billion-element Vec).
    #[compio::test]
    async fn oversized_array_reply_is_rejected_promptly() {
        let (ip, port) = mock_server_reply(b"*2000000000\r\n").await;
        let mut c = Client::connect_tcp((ip, port)).await.expect("connect mock");
        let inner = compio::time::timeout(Duration::from_secs(2), c.mget(&["k"]))
            .await
            .expect("client did not reject oversized array promptly (it hung)");
        let err = inner.expect_err("oversized array reply must be an error");
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData),
            "expected InvalidData, got {err:?}"
        );
        assert!(err.to_string().contains("too large"), "got: {err}");
    }

    /// Positive control: a normal-sized bulk reply still decodes fine.
    #[compio::test]
    async fn normal_bulk_reply_still_works() {
        let (ip, port) = mock_server_reply(b"$5\r\nhello\r\n").await;
        let mut c = Client::connect_tcp((ip, port)).await.expect("connect mock");
        let v = compio::time::timeout(Duration::from_secs(2), c.get("k"))
            .await
            .expect("normal reply timed out")
            .expect("normal reply errored");
        assert_eq!(v.as_deref(), Some(b"hello".as_ref()));
    }

    /// Positive control: a large-but-under-cap bulk (1 MB) round-trips —
    /// the 64 MB cap must not reject legitimate big values. The mock streams
    /// the body in chunks across multiple writes to exercise the
    /// accumulating read path under the cap.
    #[compio::test]
    async fn large_under_cap_bulk_reply_still_works() {
        // 1 MiB body. Header + body + CRLF, all valid.
        const N: usize = 1024 * 1024;
        // Build the full reply once; leak it to get the &'static the mock
        // helper wants (test-only, freed at process exit).
        let mut reply = Vec::with_capacity(N + 32);
        reply.extend_from_slice(format!("${N}\r\n").as_bytes());
        reply.extend(std::iter::repeat_n(b'x', N));
        reply.extend_from_slice(b"\r\n");
        let reply: &'static [u8] = Box::leak(reply.into_boxed_slice());

        let (ip, port) = mock_server_reply(reply).await;
        let mut c = Client::connect_tcp((ip, port)).await.expect("connect mock");
        let v = compio::time::timeout(Duration::from_secs(5), c.get("k"))
            .await
            .expect("1MB reply timed out")
            .expect("1MB reply errored")
            .expect("1MB reply was null");
        assert_eq!(v.len(), N);
        assert!(v.iter().all(|&b| b == b'x'));
        assert!(!c.is_dirty(), "successful large reply must leave conn clean");
    }

    // -----------------------------------------------------------------
    // FIX 2 — dirty-flag infra: a timed-out command leaves the Client
    // dirty (so the pool — R2 — can refuse to reuse it); a successful
    // command leaves it clean.
    // -----------------------------------------------------------------
    #[compio::test]
    async fn timeout_marks_connection_dirty() {
        let (ip, port) = mock_server_silent().await;
        let mut c = Client::connect_tcp((ip, port)).await.expect("connect mock");
        c.set_cmd_timeout(Duration::from_millis(150));

        let res = c.get("k").await;
        // The command must time out (server never replies).
        let err = res.expect_err("silent server should produce a timeout error");
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut),
            "expected TimedOut, got {err:?}"
        );
        // And the connection must be marked dirty so the pool won't reuse it.
        assert!(c.is_dirty(), "a timed-out connection must be left dirty");
    }

    #[compio::test]
    async fn successful_command_leaves_connection_clean() {
        let (ip, port) = mock_server_reply(b"$5\r\nhello\r\n").await;
        let mut c = Client::connect_tcp((ip, port)).await.expect("connect mock");
        let v = c.get("k").await.expect("get");
        assert_eq!(v.as_deref(), Some(b"hello".as_ref()));
        assert!(!c.is_dirty(), "a successful command must leave the conn clean");
        assert!(c.is_rx_empty(), "rx must be fully drained after a single reply");
    }
}
