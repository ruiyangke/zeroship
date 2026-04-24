//! Single-connection Redis client. Owns a TcpStream + a reusable read
//! buffer. One command in flight at a time.

use std::net::IpAddr;
use std::time::Duration;

use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
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
        })
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
        timeout(self.cmd_timeout, self.send_recv_inner(cmd))
            .await
            .map_err(|_| Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut, "redis command timed out",
            )))?
    }

    async fn send_recv_inner(&mut self, cmd: OwnedFrame) -> Result<OwnedFrame> {
        let wire = encode_frame(&cmd)?;
        let compio::BufResult(res, _buf) = self.stream.write_all(wire).await;
        res.map_err(Error::Io)?;
        // No flush — compio's TcpStream writes directly to the kernel; no
        // userspace buffering to drain. `nodelay` already prevents Nagle.

        loop {
            // Try to decode from what we already buffered.
            if !self.rx.is_empty()
                && let Some((frame, consumed)) = try_decode(&self.rx)?
            {
                let _ = self.rx.split_to(consumed);
                return Ok(frame);
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
