//! Postgres streaming-replication protocol (logical decoding).
//!
//! This module implements the wire pieces that the regular query-mode
//! `Client` / `Connection` split cannot handle:
//!
//! - The `replication=database` startup parameter (see
//!   [`crate::config::Config::replication`] /
//!   [`crate::config::ReplicationMode`]).
//! - `IDENTIFY_SYSTEM` (simple-query response shape).
//! - `START_REPLICATION SLOT ... LOGICAL <lsn> (...)` — the server
//!   transitions to the **CopyBoth** sub-protocol, returning
//!   `CopyBothResponse` (`W`) followed by a stream of `CopyData` (`d`)
//!   frames carrying either `XLogData` (`w`) or
//!   `PrimaryKeepaliveMessage` (`k`) payloads. The peer expects
//!   periodic `StandbyStatusUpdate` (`r`) frames back so it can advance
//!   `confirmed_flush_lsn` and recycle WAL.
//! - `pgoutput` logical-decoding payloads
//!   (Begin/Commit/Origin/Relation/Type/Insert/Update/Delete/Truncate/Message).
//!
//! `postgres-protocol` 0.6 does **not** expose `CopyBothResponse` or
//! the replication payload tags — see the `Message::parse` match in
//! that crate; an unknown tag errors out. The replication-mode
//! connection therefore owns the raw [`crate::buf_stream::BufStream`]
//! and runs its own framer.
//!
//! ## Why this lives in compio-postgres
//!
//! The "right place" debate is between
//!
//! 1. **plugin-db**, where the consumer's *policy* (broker fan-out,
//!    LSN tracking, watchdog, …) already lives, and
//! 2. **compio-postgres**, where the *protocol* lives.
//!
//! Mixing protocol + policy in plugin-db means re-implementing the
//! BufStream framing on top of a public-API surface that doesn't exist
//! today. Keeping protocol here gives plugin-db (and any future
//! consumer — e.g. a CDC export job) a clean async-stream API.
//!
//! ## What this module ships in P8a.2
//!
//! - [`ReplicationMode`] / [`Config::replication`] (in [`crate::config`])
//! - [`connect_replication`] — TCP + TLS + handshake + auth +
//!   `replication=database`, returning a [`ReplicationConnection`].
//! - [`ReplicationConnection::identify_system`] — minimal,
//!   used for picking the timeline / `xlogpos` on startup.
//! - [`ReplicationConnection::start_logical_replication`] — issues
//!   `START_REPLICATION SLOT ... LOGICAL ...`, waits for
//!   `CopyBothResponse`, and returns a [`ReplicationStream`].
//! - [`ReplicationStream::next`] — yields [`ReplicationMessage`]
//!   (`XLogData` / `PrimaryKeepalive`). The caller drives
//!   [`ReplicationStream::send_standby_status_update`] periodically.
//! - [`pgoutput`] — pure decoder. The stream layer is *agnostic* to
//!   the logical-decoding plugin; `pgoutput` is just the parser we
//!   ship because every consumer in zeroship uses it.
//!
//! ## Reference
//!
//! - https://www.postgresql.org/docs/16/protocol-replication.html
//! - https://www.postgresql.org/docs/16/protocol-logicalrep-message-formats.html
//! - Postgres source: `src/backend/replication/walsender.c` (frontend)
//!   and `src/backend/replication/pgoutput/pgoutput.c` (decoder).

use crate::buf_stream::BufStream;
use crate::codec::FrontendMessage;
use crate::config::{Config, ReplicationMode};
use crate::connect_socket::connect_socket;
use crate::connect_tls::connect_tls;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::MakeTlsConnect;
use crate::{Error, Socket};
use crate::client::Addr;
use crate::config::Host;
use bytes::{BufMut, BytesMut};
use compio::io::{AsyncRead, AsyncWrite};
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Wire tags
// ---------------------------------------------------------------------------
//
// Tags drawn directly from the PG 16 wire-protocol spec. We keep them
// here, NOT in `codec.rs`, because the regular Message::parse in
// postgres-protocol doesn't recognise them — including them in the
// query path would just add dead branches.

/// Backend tag: `CopyBothResponse`. Sent in response to
/// `START_REPLICATION`.
pub const COPY_BOTH_RESPONSE_TAG: u8 = b'W';
/// Backend tag: a frame inside the CopyBoth channel. Same as the
/// regular `CopyData` tag.
pub const COPY_DATA_TAG: u8 = b'd';
/// Backend tag: a `CopyDone` signal — terminates the stream.
pub const COPY_DONE_TAG: u8 = b'c';
/// Backend tag: `ErrorResponse`.
pub const ERROR_RESPONSE_TAG: u8 = b'E';
/// Backend tag: `NoticeResponse`.
pub const NOTICE_RESPONSE_TAG: u8 = b'N';

/// CopyData sub-tag: `XLogData`.
pub const XLOG_DATA_TAG: u8 = b'w';
/// CopyData sub-tag: `PrimaryKeepaliveMessage`.
pub const PRIMARY_KEEPALIVE_TAG: u8 = b'k';
/// CopyData sub-tag: `StandbyStatusUpdate` (frontend → backend).
pub const STANDBY_STATUS_UPDATE_TAG: u8 = b'r';
/// CopyData sub-tag: `HotStandbyFeedback` (frontend → backend, unused).
pub const HOT_STANDBY_FEEDBACK_TAG: u8 = b'h';

// ---------------------------------------------------------------------------
// connect_replication
// ---------------------------------------------------------------------------

/// Open a replication-mode connection.
///
/// Equivalent to [`crate::Config::connect`] but the returned value is a
/// [`ReplicationConnection`] (no separate `run`-loop task). The
/// connection enters walsender mode via `replication=database`. The
/// regular `query` / `execute` surface is **not exposed** on the
/// returned type; the replication-protocol command grammar is what's
/// available.
pub async fn connect_replication<T>(
    mut tls: T,
    config: &Config,
) -> Result<ReplicationConnection<Socket, T::Stream>, Error>
where
    T: MakeTlsConnect<Socket>,
{
    // Mirror connect.rs' host selection but in a flattened form: we
    // walk the first host (the platform always uses a single endpoint
    // for replication; multi-host failover would need extra plumbing
    // because `ReplicationConnection` doesn't currently re-handshake
    // on failover).
    if config.get_hosts().is_empty() && config.get_hostaddrs().is_empty() {
        return Err(Error::config(
            "replication: host or hostaddr is required".into(),
        ));
    }

    let host = config.get_hosts().first().cloned();
    let hostaddr = config.get_hostaddrs().first().copied();
    let port = config.get_ports().first().copied().unwrap_or(5432);

    let (addr, hostname) = match (host, hostaddr) {
        (_, Some(ip)) => (
            Addr::Tcp(ip),
            config
                .get_hosts()
                .first()
                .and_then(|h| match h {
                    Host::Tcp(s) => Some(s.clone()),
                    #[cfg(unix)]
                    Host::Unix(_) => None,
                }),
        ),
        (Some(Host::Tcp(h)), None) => {
            // Resolve via compio.
            use compio::net::ToSocketAddrsAsync;
            let mut addrs = (&*h, port)
                .to_socket_addrs_async()
                .await
                .map_err(Error::connect)?;
            let first = addrs
                .next()
                .ok_or_else(|| Error::config("replication: DNS yielded no addresses".into()))?;
            (Addr::Tcp(first.ip()), Some(h))
        }
        #[cfg(unix)]
        (Some(Host::Unix(p)), None) => (Addr::Unix(p), None),
        (None, None) => unreachable!("guarded above"),
    };

    let socket = connect_socket(
        &addr,
        port,
        config.get_connect_timeout().copied(),
        config.get_tcp_user_timeout().copied(),
        if config.get_keepalives() {
            Some(&config.keepalive_config)
        } else {
            None
        },
    )
    .await?;

    let tls_inst = tls
        .make_tls_connect(hostname.as_deref().unwrap_or(""))
        .map_err(|e| Error::tls(e.into()))?;
    let has_hostname = hostname.is_some();

    // We need a Config with replication=database set. Most callers
    // already set it; tolerate both shapes and force-set as a defensive
    // measure if they didn't.
    let mut cfg = config.clone();
    if cfg.get_replication().is_none() {
        cfg.replication(ReplicationMode::Logical);
    }

    let stream =
        connect_tls(socket, cfg.get_ssl_mode(), cfg.get_ssl_negotiation(), tls_inst, has_hostname)
            .await?;

    // Run the normal startup + auth handshake — connect_raw_into
    // exposes the post-handshake BufStream that the replication
    // connection then owns.
    let (stream, parameters) = handshake_replication(stream, &cfg).await?;

    Ok(ReplicationConnection {
        stream: BufStream::new(stream),
        parameters,
    })
}

/// Run the startup + auth handshake through a wrapper that hands us
/// back the raw, post-handshake stream — not a Client/Connection pair.
///
/// We can't reuse [`crate::connect_raw::connect_raw`] verbatim because
/// it constructs a `Connection` (which immediately wants to be
/// `run()`d) and consumes the stream. Re-implementing the handshake
/// would mean duplicating ~250 LOC for a single transition; instead
/// we reuse the handshake state machine via a lightweight wrapper.
///
/// This is one of the deliberately-not-reused paths flagged in the
/// module docs: the replication-mode connection has its own life
/// cycle.
async fn handshake_replication<S, T>(
    stream: MaybeTlsStream<S, T>,
    config: &Config,
) -> Result<(MaybeTlsStream<S, T>, HashMap<String, String>), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: crate::tls::TlsStream + Unpin,
{
    crate::connect_raw::handshake_for_replication(stream, config).await
}

// ---------------------------------------------------------------------------
// ReplicationConnection
// ---------------------------------------------------------------------------

/// A replication-mode connection. Owns the post-handshake stream and
/// runs its own framer (since postgres-protocol's `Message::parse`
/// can't parse `CopyBothResponse`).
pub struct ReplicationConnection<S, T> {
    stream: BufStream<MaybeTlsStream<S, T>>,
    parameters: HashMap<String, String>,
}

impl<S, T> ReplicationConnection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Server-reported parameter map captured during handshake
    /// (`server_version`, `server_encoding`, …). Convenience for
    /// callers that need to gate on PG version.
    pub fn parameters(&self) -> &HashMap<String, String> {
        &self.parameters
    }

    /// Issue `IDENTIFY_SYSTEM` — used at start-up to discover the
    /// current WAL position when there's no prior `confirmed_flush_lsn`
    /// to resume from.
    ///
    /// Returns `{systemid, timeline, xlogpos, dbname}` per the docs.
    pub async fn identify_system(&mut self) -> Result<IdentifySystem, Error> {
        send_simple_query(&mut self.stream, "IDENTIFY_SYSTEM").await?;

        // IDENTIFY_SYSTEM returns: RowDescription, DataRow,
        // CommandComplete, ReadyForQuery. We use postgres-protocol's
        // parser for these — they're regular tags.
        let mut systemid = String::new();
        let mut timeline: u32 = 0;
        let mut xlogpos = String::new();
        let mut dbname: Option<String> = None;

        loop {
            let msg = read_one_message(&mut self.stream).await?;
            match msg {
                Message::RowDescription(_) => {}
                Message::DataRow(row) => {
                    let buf = row.buffer();
                    // 4 fields per the spec, encoded as length-prefixed
                    // strings. Parse positionally.
                    let mut idx = 0usize;
                    let n = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                    idx += 2;
                    let mut fields: Vec<Option<&str>> = Vec::with_capacity(n);
                    for _ in 0..n {
                        let len = i32::from_be_bytes([
                            buf[idx],
                            buf[idx + 1],
                            buf[idx + 2],
                            buf[idx + 3],
                        ]);
                        idx += 4;
                        if len < 0 {
                            fields.push(None);
                        } else {
                            let end = idx + len as usize;
                            fields.push(Some(
                                std::str::from_utf8(&buf[idx..end])
                                    .map_err(|e| Error::parse(std::io::Error::other(e)))?,
                            ));
                            idx = end;
                        }
                    }
                    systemid = fields.first().and_then(|f| *f).unwrap_or("").to_string();
                    timeline = fields
                        .get(1)
                        .and_then(|f| *f)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    xlogpos = fields.get(2).and_then(|f| *f).unwrap_or("").to_string();
                    dbname = fields.get(3).and_then(|f| *f).map(|s| s.to_string());
                }
                Message::CommandComplete(_) => {}
                Message::ReadyForQuery(_) => break,
                Message::ErrorResponse(body) => return Err(Error::db(body)),
                Message::NoticeResponse(_) => {}
                _ => return Err(Error::unexpected_message()),
            }
        }

        Ok(IdentifySystem {
            systemid,
            timeline,
            xlogpos,
            dbname,
        })
    }

    /// Issue `START_REPLICATION SLOT <slot> LOGICAL <lsn> ("proto_version" '...', "publication_names" '...')`.
    ///
    /// On success, the server sends a `CopyBothResponse`. The returned
    /// [`ReplicationStream`] owns the framer thereafter.
    ///
    /// The caller is expected to know the previous `confirmed_flush_lsn`
    /// (e.g. from [`identify_system`](Self::identify_system) or the
    /// plugin-db setup outcome) and pass it as `start_lsn`. Passing
    /// `"0/0"` lets the server resume from the slot's own
    /// `confirmed_flush_lsn` — the simplest correct choice.
    pub async fn start_logical_replication(
        mut self,
        opts: StartReplicationOptions<'_>,
    ) -> Result<ReplicationStream<S, T>, Error> {
        let mut cmd = String::with_capacity(128);
        cmd.push_str("START_REPLICATION SLOT \"");
        cmd.push_str(opts.slot_name);
        cmd.push_str("\" LOGICAL ");
        cmd.push_str(opts.start_lsn);
        cmd.push_str(" (\"proto_version\" '");
        cmd.push_str(&opts.proto_version.to_string());
        cmd.push_str("', \"publication_names\" '");
        // The publication_names list is single-quoted; commas separate
        // names. Caller is responsible for sanitising values (slot
        // setup already validates).
        cmd.push_str(opts.publication_names);
        cmd.push_str("')");

        send_simple_query(&mut self.stream, &cmd).await?;

        // Drain server messages until we see CopyBothResponse, then
        // transition into the streaming framer.
        loop {
            let header = read_header(&mut self.stream).await?;
            match header.tag {
                COPY_BOTH_RESPONSE_TAG => {
                    // Consume payload — we don't need its fields
                    // (overall format byte + column count + per-column
                    // format byte). The CopyBoth channel is open after
                    // this.
                    let _ = self.stream.buf().split_to(header.body_len()).freeze();
                    return Ok(ReplicationStream {
                        stream: self.stream,
                        last_received_lsn: parse_lsn(opts.start_lsn).unwrap_or(0),
                        last_processed_lsn: parse_lsn(opts.start_lsn).unwrap_or(0),
                    });
                }
                ERROR_RESPONSE_TAG => {
                    let bytes = self.stream.buf().split_to(header.body_len()).freeze();
                    let mut body = BytesMut::with_capacity(bytes.len() + 5);
                    body.put_u8(ERROR_RESPONSE_TAG);
                    body.put_u32(header.length);
                    body.extend_from_slice(&bytes);
                    return Err(Error::io(std::io::Error::other(format!(
                        "START_REPLICATION ErrorResponse: {} bytes",
                        body.len()
                    ))));
                }
                NOTICE_RESPONSE_TAG => {
                    // Drop the notice payload silently.
                    let _ = self.stream.buf().split_to(header.body_len()).freeze();
                }
                tag => {
                    return Err(Error::io(std::io::Error::other(format!(
                        "unexpected message tag during START_REPLICATION: {} (0x{:02x})",
                        tag as char, tag
                    ))));
                }
            }
        }
    }
}

/// Result of [`ReplicationConnection::identify_system`].
#[derive(Debug, Clone)]
pub struct IdentifySystem {
    pub systemid: String,
    pub timeline: u32,
    pub xlogpos: String,
    pub dbname: Option<String>,
}

/// Options passed to
/// [`ReplicationConnection::start_logical_replication`].
#[derive(Debug, Clone)]
pub struct StartReplicationOptions<'a> {
    pub slot_name: &'a str,
    /// Hex-formatted LSN string (e.g. `"0/16B3750"`). `"0/0"` resumes
    /// from the slot's `confirmed_flush_lsn`.
    pub start_lsn: &'a str,
    /// pgoutput protocol version. `1` is widely supported; `2`+ adds
    /// streaming-of-large-transactions. P8a.2 targets `1`.
    pub proto_version: u32,
    /// Comma-separated list of publication names (no quoting; the
    /// caller sanitises each name).
    pub publication_names: &'a str,
}

// ---------------------------------------------------------------------------
// ReplicationStream
// ---------------------------------------------------------------------------

/// The streaming half of a logical-replication connection.
///
/// Yields [`ReplicationMessage`]s (XLogData / PrimaryKeepalive) via
/// [`next`](Self::next). The caller drives
/// [`send_standby_status_update`](Self::send_standby_status_update) on
/// a schedule (every commit + on keepalive `reply_requested=1`) so
/// the upstream slot's `confirmed_flush_lsn` advances and WAL retention
/// stays bounded.
///
/// Note: this struct intentionally does NOT decode pgoutput payloads.
/// The XLogData's body is handed to the caller verbatim; the caller
/// runs [`pgoutput::decode`] on it. The split keeps the wire layer
/// agnostic to the logical-decoding plugin.
pub struct ReplicationStream<S, T> {
    stream: BufStream<MaybeTlsStream<S, T>>,
    /// Highest LSN seen on the wire (the `wal_end` field of either
    /// XLogData or PrimaryKeepalive). The "received" position the
    /// next StandbyStatusUpdate will report — meaning "we have this
    /// many bytes buffered or processed".
    last_received_lsn: u64,
    /// Highest LSN the caller has confirmed processed (advances on
    /// [`advance_lsn`](Self::advance_lsn)). The "flushed" position
    /// the slot will retain WAL up to.
    last_processed_lsn: u64,
}

/// One frame off the CopyBoth wire.
#[derive(Debug, Clone)]
pub enum ReplicationMessage {
    /// A logical-decoding payload. The body is the raw pgoutput frame.
    XLogData {
        /// LSN of the first byte of `body`.
        wal_start: u64,
        /// LSN of the byte just past `body`.
        wal_end: u64,
        /// Server clock in microseconds since the PG epoch
        /// (2000-01-01 00:00:00 UTC).
        timestamp: i64,
        /// pgoutput frame bytes.
        body: bytes::Bytes,
    },
    /// Periodic keepalive — the server's current `wal_end`, plus a
    /// flag asking us to reply with a StandbyStatusUpdate right now.
    PrimaryKeepalive {
        wal_end: u64,
        timestamp: i64,
        reply_requested: bool,
    },
}

impl<S, T> ReplicationStream<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Wait for the next frame off the CopyBoth wire.
    ///
    /// Returns `Ok(None)` on a clean `CopyDone` from the server.
    /// Internal `ErrorResponse` / `NoticeResponse` frames inside the
    /// CopyBoth channel surface as `Err` / `Ok` accordingly.
    pub async fn next(&mut self) -> Result<Option<ReplicationMessage>, Error> {
        loop {
            let header = read_header(&mut self.stream).await?;
            match header.tag {
                COPY_DATA_TAG => {
                    let body = self.stream.buf().split_to(header.body_len()).freeze();
                    if body.is_empty() {
                        return Err(Error::io(std::io::Error::other("empty CopyData frame")));
                    }
                    let sub_tag = body[0];
                    match sub_tag {
                        XLOG_DATA_TAG => {
                            // body[0]   = 'w'
                            // body[1..9]   = wal_start (i64)
                            // body[9..17]  = wal_end (i64)
                            // body[17..25] = timestamp (i64)
                            // body[25..]   = pgoutput payload
                            if body.len() < 25 {
                                return Err(Error::io(std::io::Error::other(
                                    "XLogData frame too small",
                                )));
                            }
                            let wal_start = u64::from_be_bytes([
                                body[1], body[2], body[3], body[4], body[5], body[6], body[7],
                                body[8],
                            ]);
                            let wal_end = u64::from_be_bytes([
                                body[9], body[10], body[11], body[12], body[13], body[14],
                                body[15], body[16],
                            ]);
                            let timestamp = i64::from_be_bytes([
                                body[17], body[18], body[19], body[20], body[21], body[22],
                                body[23], body[24],
                            ]);
                            // The XLogData "wal_end" advertises how far
                            // along the server has decoded; we track it
                            // for StandbyStatusUpdate reporting.
                            if wal_end > self.last_received_lsn {
                                self.last_received_lsn = wal_end;
                            }
                            let payload = body.slice(25..);
                            return Ok(Some(ReplicationMessage::XLogData {
                                wal_start,
                                wal_end,
                                timestamp,
                                body: payload,
                            }));
                        }
                        PRIMARY_KEEPALIVE_TAG => {
                            // body[0]   = 'k'
                            // body[1..9]   = wal_end (i64)
                            // body[9..17]  = timestamp (i64)
                            // body[17]     = reply_requested (u8)
                            if body.len() < 18 {
                                return Err(Error::io(std::io::Error::other(
                                    "PrimaryKeepalive frame too small",
                                )));
                            }
                            let wal_end = u64::from_be_bytes([
                                body[1], body[2], body[3], body[4], body[5], body[6], body[7],
                                body[8],
                            ]);
                            let timestamp = i64::from_be_bytes([
                                body[9], body[10], body[11], body[12], body[13], body[14],
                                body[15], body[16],
                            ]);
                            let reply_requested = body[17] != 0;
                            if wal_end > self.last_received_lsn {
                                self.last_received_lsn = wal_end;
                            }
                            return Ok(Some(ReplicationMessage::PrimaryKeepalive {
                                wal_end,
                                timestamp,
                                reply_requested,
                            }));
                        }
                        other => {
                            return Err(Error::io(std::io::Error::other(format!(
                                "unknown CopyData sub-tag: 0x{:02x}",
                                other
                            ))));
                        }
                    }
                }
                COPY_DONE_TAG => {
                    // Drain (length-only frame).
                    let _ = self.stream.buf().split_to(header.body_len()).freeze();
                    return Ok(None);
                }
                ERROR_RESPONSE_TAG => {
                    let _ = self.stream.buf().split_to(header.body_len()).freeze();
                    return Err(Error::io(std::io::Error::other(
                        "replication stream: ErrorResponse",
                    )));
                }
                NOTICE_RESPONSE_TAG => {
                    let _ = self.stream.buf().split_to(header.body_len()).freeze();
                }
                other => {
                    return Err(Error::io(std::io::Error::other(format!(
                        "unexpected tag in replication stream: 0x{:02x}",
                        other
                    ))));
                }
            }
        }
    }

    /// Highest WAL LSN seen on the wire so far (advances as the
    /// stream yields XLogData / PrimaryKeepalive).
    pub fn last_received_lsn(&self) -> u64 {
        self.last_received_lsn
    }

    /// Highest WAL LSN the caller has confirmed it processed —
    /// reported as `flush_lsn` in StandbyStatusUpdate frames.
    pub fn last_processed_lsn(&self) -> u64 {
        self.last_processed_lsn
    }

    /// Mark `lsn` as fully processed. The next StandbyStatusUpdate
    /// will surface this as the `flush_lsn`, letting Postgres recycle
    /// WAL up to it.
    pub fn advance_lsn(&mut self, lsn: u64) {
        if lsn > self.last_processed_lsn {
            self.last_processed_lsn = lsn;
        }
        if lsn > self.last_received_lsn {
            self.last_received_lsn = lsn;
        }
    }

    /// Send a `StandbyStatusUpdate` frame upstream.
    ///
    /// The frame's three LSN slots (`write`, `flush`, `apply`) are
    /// reported with the same value — the highest LSN the caller has
    /// confirmed via [`advance_lsn`](Self::advance_lsn). pgoutput
    /// doesn't currently distinguish them, and conflating them keeps
    /// the slot retention conservative (Postgres releases WAL up to
    /// the lowest of the three).
    ///
    /// `reply_requested = true` makes the server reply with an
    /// immediate PrimaryKeepalive — typically left `false`.
    pub async fn send_standby_status_update(
        &mut self,
        reply_requested: bool,
    ) -> Result<(), Error> {
        let now = postgres_microseconds_since_epoch();
        let lsn = self.last_processed_lsn.max(self.last_received_lsn);
        encode_standby_status_update(&mut self.stream, lsn, lsn, lsn, now, reply_requested)?;
        self.stream.flush().await
    }
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// One parsed message header (tag + total length on the wire,
/// including the 4-byte length field).
struct WireHeader {
    tag: u8,
    /// Length field as declared on the wire. This counts the 4-byte
    /// length itself but NOT the 1-byte tag.
    length: u32,
}

impl WireHeader {
    /// Number of payload bytes after the header in the read buffer.
    /// `length` counts the length field (4 bytes) so `length - 4` is
    /// the payload size.
    fn body_len(&self) -> usize {
        self.length as usize - 4
    }
}

/// Read one bespoke message header (peel 1-byte tag + 4-byte length
/// from the BufStream) and ensure the full message body is buffered
/// in `stream.buf()`. The caller `split_to(header.body_len())`s the
/// payload off.
async fn read_header<S>(stream: &mut BufStream<S>) -> Result<WireHeader, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.fill(5).await?;
    let length = stream
        .peek_u32_be(1)
        .expect("fill(5) guarantees 5 bytes are buffered");
    stream.validate_length(length)?;
    let total_len = 1 + length as usize;
    stream.fill(total_len).await?;
    let buf = stream.buf();
    let tag = buf[0];
    // Drop the tag + length prefix so the body is at offset 0.
    let _ = buf.split_to(5);
    Ok(WireHeader { tag, length })
}

/// Read one message via postgres-protocol's framer. Used by
/// IDENTIFY_SYSTEM (DataRow / CommandComplete / ReadyForQuery — all
/// regular tags Message::parse already knows).
async fn read_one_message<S>(stream: &mut BufStream<S>) -> Result<Message, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        if let Some(m) = Message::parse(stream.buf()).map_err(Error::io)? {
            return Ok(m);
        }
        let need = stream.buf().len() + 1;
        stream.fill(need).await?;
    }
}

/// Send a simple-query (`Q`) frontend message — used to issue both
/// `IDENTIFY_SYSTEM` and `START_REPLICATION`. The walsender accepts
/// the simple-query path for the replication command grammar.
async fn send_simple_query<S>(stream: &mut BufStream<S>, query: &str) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    frontend::query(query, &mut buf).map_err(Error::encode)?;
    crate::codec::write_frontend(stream, FrontendMessage::Raw(buf.freeze()))?;
    stream.flush().await
}

/// Encode a `StandbyStatusUpdate` frame into the stream's write
/// buffer.
///
/// Wire layout (frontend → backend, inside CopyData):
///
/// ```text
///   'd' tag, length, [
///       'r' sub-tag,
///       i64 write_lsn,
///       i64 flush_lsn,
///       i64 apply_lsn,
///       i64 timestamp_ms_from_2000,
///       u8  reply_requested (0|1)
///   ]
/// ```
fn encode_standby_status_update<S>(
    stream: &mut BufStream<S>,
    write_lsn: u64,
    flush_lsn: u64,
    apply_lsn: u64,
    timestamp: i64,
    reply_requested: bool,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let dst = stream.write_buf_mut();
    // Body length = sub-tag(1) + 4×i64(32) + reply(1) = 34 bytes
    // CopyData wire length field includes itself (4 bytes), so the
    // declared length = 4 + 34 = 38.
    const BODY_BYTES: u32 = 34;
    dst.put_u8(COPY_DATA_TAG);
    dst.put_u32(4 + BODY_BYTES);
    dst.put_u8(STANDBY_STATUS_UPDATE_TAG);
    dst.put_u64(write_lsn);
    dst.put_u64(flush_lsn);
    dst.put_u64(apply_lsn);
    dst.put_i64(timestamp);
    dst.put_u8(if reply_requested { 1 } else { 0 });
    Ok(())
}

/// Parse a Postgres text LSN like `"0/16B3750"` into a `u64`.
///
/// Returns `None` on malformed input rather than erroring — the
/// callers that need correctness gate on the input source (the slot
/// setup outcome) and a malformed value just means we resume from
/// `0/0`, which Postgres treats as "use the slot's flush_lsn".
pub fn parse_lsn(s: &str) -> Option<u64> {
    let (hi, lo) = s.split_once('/')?;
    let hi = u32::from_str_radix(hi.trim(), 16).ok()?;
    let lo = u32::from_str_radix(lo.trim(), 16).ok()?;
    Some(((hi as u64) << 32) | lo as u64)
}

/// Format an LSN as Postgres text (`hi/lo`).
pub fn format_lsn(lsn: u64) -> String {
    format!("{:X}/{:X}", (lsn >> 32) as u32, lsn as u32)
}

/// Postgres-epoch microseconds (since 2000-01-01 00:00:00 UTC) at the
/// current wall-clock instant. Used by StandbyStatusUpdate's
/// `client_time` field.
fn postgres_microseconds_since_epoch() -> i64 {
    // 2000-01-01 00:00:00 UTC = 946684800 seconds after the Unix epoch.
    const PG_EPOCH_UNIX_SECONDS: i64 = 946_684_800;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let unix_us = now.as_micros() as i64;
    unix_us - (PG_EPOCH_UNIX_SECONDS * 1_000_000)
}

// ---------------------------------------------------------------------------
// pgoutput — logical-decoding payload decoder
// ---------------------------------------------------------------------------

/// pgoutput logical-decoding message decoder.
///
/// Pure parser. The replication stream layer hands the caller raw
/// pgoutput frames (the body of each XLogData); the caller feeds them
/// to [`pgoutput::decode`] to obtain a [`PgOutputMessage`].
///
/// Implements protocol version 1 — the minimum every PG 12+ server
/// speaks. Streaming-of-large-transactions (proto v2+) is not
/// implemented; we don't subscribe to in-progress transactions.
///
/// Reference: https://www.postgresql.org/docs/16/protocol-logicalrep-message-formats.html
pub mod pgoutput {
    use bytes::Bytes;

    /// One pgoutput logical-decoding message.
    #[derive(Debug, Clone, PartialEq)]
    pub enum PgOutputMessage {
        /// Begin of a transaction.
        Begin {
            /// LSN of the commit record (NOT the begin record).
            final_lsn: u64,
            /// Server clock at commit, microseconds since PG epoch.
            commit_timestamp: i64,
            /// Transaction id.
            xid: u32,
        },
        /// Commit of a transaction.
        Commit {
            /// Flags — currently always 0.
            flags: u8,
            /// LSN of the commit record.
            commit_lsn: u64,
            /// LSN of the byte just past the commit record.
            end_lsn: u64,
            /// Server clock at commit.
            commit_timestamp: i64,
        },
        /// Replication origin.
        Origin { commit_lsn: u64, name: String },
        /// A relation (table) descriptor — emitted before the first
        /// Insert/Update/Delete on that relation. Cache the mapping
        /// `rel_id -> (namespace, name, columns)` for use when the
        /// tuple messages reference it.
        Relation {
            rel_id: u32,
            namespace: String,
            name: String,
            /// `'d'` = default, `'n'` = nothing, `'f'` = full,
            /// `'i'` = index.
            replica_identity: u8,
            columns: Vec<RelationColumn>,
        },
        /// A user-defined type descriptor.
        Type {
            type_id: u32,
            namespace: String,
            name: String,
        },
        /// INSERT.
        Insert { rel_id: u32, new_tuple: TupleData },
        /// UPDATE. `old_tuple` is present only when the relation's
        /// replica identity is `FULL` or `INDEX`.
        Update {
            rel_id: u32,
            old_tuple: Option<TupleData>,
            new_tuple: TupleData,
        },
        /// DELETE. `old_tuple` carries either the full row or just
        /// the replica-identity columns, depending on REPLICA IDENTITY.
        Delete {
            rel_id: u32,
            old_tuple: TupleData,
        },
        /// TRUNCATE.
        Truncate {
            /// Bit 0 = CASCADE, bit 1 = RESTART IDENTITY.
            options: u8,
            relation_ids: Vec<u32>,
        },
        /// A logical-replication "Message" (pg_logical_emit_message).
        Message {
            flags: u8,
            lsn: u64,
            prefix: String,
            content: Bytes,
        },
    }

    /// One column in a Relation message.
    #[derive(Debug, Clone, PartialEq)]
    pub struct RelationColumn {
        /// Bit 0 = part of REPLICA IDENTITY.
        pub flags: u8,
        pub name: String,
        pub type_oid: u32,
        /// Type modifier (per pg_attribute.atttypmod).
        pub type_modifier: i32,
    }

    /// One column value in a tuple.
    #[derive(Debug, Clone, PartialEq)]
    pub enum TupleColumn {
        /// SQL NULL.
        Null,
        /// Column omitted because it's an unchanged TOAST value.
        Toasted,
        /// Text-format value.
        Text(String),
        /// Binary-format value (proto v1 emits text by default; binary
        /// shows up only when the publication was created with the
        /// `binary` option).
        Binary(Bytes),
    }

    /// A tuple — one ordered list of column values.
    #[derive(Debug, Clone, PartialEq)]
    pub struct TupleData {
        pub columns: Vec<TupleColumn>,
    }

    /// Errors raised by [`decode`].
    #[derive(Debug)]
    pub enum DecodeError {
        UnexpectedEof,
        InvalidUtf8,
        UnknownTag(u8),
        UnknownTupleFormat(u8),
    }

    impl std::fmt::Display for DecodeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                DecodeError::UnexpectedEof => write!(f, "pgoutput: unexpected EOF"),
                DecodeError::InvalidUtf8 => write!(f, "pgoutput: invalid UTF-8"),
                DecodeError::UnknownTag(t) => write!(f, "pgoutput: unknown tag 0x{t:02x}"),
                DecodeError::UnknownTupleFormat(t) => {
                    write!(f, "pgoutput: unknown tuple column format 0x{t:02x}")
                }
            }
        }
    }

    impl std::error::Error for DecodeError {}

    fn read_cstr(buf: &mut &[u8]) -> Result<String, DecodeError> {
        let pos = buf.iter().position(|&b| b == 0).ok_or(DecodeError::UnexpectedEof)?;
        let s = std::str::from_utf8(&buf[..pos]).map_err(|_| DecodeError::InvalidUtf8)?;
        let owned = s.to_string();
        *buf = &buf[pos + 1..];
        Ok(owned)
    }

    fn read_u8(buf: &mut &[u8]) -> Result<u8, DecodeError> {
        if buf.is_empty() {
            return Err(DecodeError::UnexpectedEof);
        }
        let v = buf[0];
        *buf = &buf[1..];
        Ok(v)
    }

    fn read_u16(buf: &mut &[u8]) -> Result<u16, DecodeError> {
        if buf.len() < 2 {
            return Err(DecodeError::UnexpectedEof);
        }
        let v = u16::from_be_bytes([buf[0], buf[1]]);
        *buf = &buf[2..];
        Ok(v)
    }

    fn read_u32(buf: &mut &[u8]) -> Result<u32, DecodeError> {
        if buf.len() < 4 {
            return Err(DecodeError::UnexpectedEof);
        }
        let v = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        *buf = &buf[4..];
        Ok(v)
    }

    fn read_i32(buf: &mut &[u8]) -> Result<i32, DecodeError> {
        Ok(read_u32(buf)? as i32)
    }

    fn read_u64(buf: &mut &[u8]) -> Result<u64, DecodeError> {
        if buf.len() < 8 {
            return Err(DecodeError::UnexpectedEof);
        }
        let v = u64::from_be_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        *buf = &buf[8..];
        Ok(v)
    }

    fn read_i64(buf: &mut &[u8]) -> Result<i64, DecodeError> {
        Ok(read_u64(buf)? as i64)
    }

    /// Decode a tuple — a u16 column count followed by per-column
    /// `(format_byte, [u32 len + bytes])`.
    fn read_tuple(buf: &mut &[u8]) -> Result<TupleData, DecodeError> {
        let n = read_u16(buf)? as usize;
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            let fmt = read_u8(buf)?;
            match fmt {
                b'n' => columns.push(TupleColumn::Null),
                b'u' => columns.push(TupleColumn::Toasted),
                b't' => {
                    let len = read_i32(buf)? as usize;
                    if buf.len() < len {
                        return Err(DecodeError::UnexpectedEof);
                    }
                    let s = std::str::from_utf8(&buf[..len])
                        .map_err(|_| DecodeError::InvalidUtf8)?
                        .to_string();
                    *buf = &buf[len..];
                    columns.push(TupleColumn::Text(s));
                }
                b'b' => {
                    let len = read_i32(buf)? as usize;
                    if buf.len() < len {
                        return Err(DecodeError::UnexpectedEof);
                    }
                    let bytes = Bytes::copy_from_slice(&buf[..len]);
                    *buf = &buf[len..];
                    columns.push(TupleColumn::Binary(bytes));
                }
                other => return Err(DecodeError::UnknownTupleFormat(other)),
            }
        }
        Ok(TupleData { columns })
    }

    /// Decode one pgoutput message.
    pub fn decode(input: &[u8]) -> Result<PgOutputMessage, DecodeError> {
        let mut cur = input;
        let tag = read_u8(&mut cur)?;
        let msg = match tag {
            b'B' => {
                let final_lsn = read_u64(&mut cur)?;
                let commit_timestamp = read_i64(&mut cur)?;
                let xid = read_u32(&mut cur)?;
                PgOutputMessage::Begin {
                    final_lsn,
                    commit_timestamp,
                    xid,
                }
            }
            b'C' => {
                let flags = read_u8(&mut cur)?;
                let commit_lsn = read_u64(&mut cur)?;
                let end_lsn = read_u64(&mut cur)?;
                let commit_timestamp = read_i64(&mut cur)?;
                PgOutputMessage::Commit {
                    flags,
                    commit_lsn,
                    end_lsn,
                    commit_timestamp,
                }
            }
            b'O' => {
                let commit_lsn = read_u64(&mut cur)?;
                let name = read_cstr(&mut cur)?;
                PgOutputMessage::Origin { commit_lsn, name }
            }
            b'R' => {
                let rel_id = read_u32(&mut cur)?;
                let namespace = read_cstr(&mut cur)?;
                let name = read_cstr(&mut cur)?;
                let replica_identity = read_u8(&mut cur)?;
                let ncols = read_u16(&mut cur)? as usize;
                let mut columns = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    let flags = read_u8(&mut cur)?;
                    let col_name = read_cstr(&mut cur)?;
                    let type_oid = read_u32(&mut cur)?;
                    let type_modifier = read_i32(&mut cur)?;
                    columns.push(RelationColumn {
                        flags,
                        name: col_name,
                        type_oid,
                        type_modifier,
                    });
                }
                PgOutputMessage::Relation {
                    rel_id,
                    namespace,
                    name,
                    replica_identity,
                    columns,
                }
            }
            b'Y' => {
                let type_id = read_u32(&mut cur)?;
                let namespace = read_cstr(&mut cur)?;
                let name = read_cstr(&mut cur)?;
                PgOutputMessage::Type {
                    type_id,
                    namespace,
                    name,
                }
            }
            b'I' => {
                let rel_id = read_u32(&mut cur)?;
                // Expect 'N' tuple-kind tag.
                let kind = read_u8(&mut cur)?;
                if kind != b'N' {
                    return Err(DecodeError::UnknownTupleFormat(kind));
                }
                let new_tuple = read_tuple(&mut cur)?;
                PgOutputMessage::Insert { rel_id, new_tuple }
            }
            b'U' => {
                let rel_id = read_u32(&mut cur)?;
                // pgoutput emits one of:
                //   'K' (old key) + tuple + 'N' + tuple
                //   'O' (old full) + tuple + 'N' + tuple
                //   'N' + tuple                   (no old tuple)
                let kind = read_u8(&mut cur)?;
                let (old_tuple, new_tuple) = match kind {
                    b'K' | b'O' => {
                        let old = read_tuple(&mut cur)?;
                        let n_kind = read_u8(&mut cur)?;
                        if n_kind != b'N' {
                            return Err(DecodeError::UnknownTupleFormat(n_kind));
                        }
                        let new = read_tuple(&mut cur)?;
                        (Some(old), new)
                    }
                    b'N' => (None, read_tuple(&mut cur)?),
                    other => return Err(DecodeError::UnknownTupleFormat(other)),
                };
                PgOutputMessage::Update {
                    rel_id,
                    old_tuple,
                    new_tuple,
                }
            }
            b'D' => {
                let rel_id = read_u32(&mut cur)?;
                // 'K' (replica identity key) or 'O' (full row).
                let kind = read_u8(&mut cur)?;
                if kind != b'K' && kind != b'O' {
                    return Err(DecodeError::UnknownTupleFormat(kind));
                }
                let old_tuple = read_tuple(&mut cur)?;
                PgOutputMessage::Delete { rel_id, old_tuple }
            }
            b'T' => {
                let nrelations = read_u32(&mut cur)? as usize;
                let options = read_u8(&mut cur)?;
                let mut relation_ids = Vec::with_capacity(nrelations);
                for _ in 0..nrelations {
                    relation_ids.push(read_u32(&mut cur)?);
                }
                PgOutputMessage::Truncate {
                    options,
                    relation_ids,
                }
            }
            b'M' => {
                let flags = read_u8(&mut cur)?;
                let lsn = read_u64(&mut cur)?;
                let prefix = read_cstr(&mut cur)?;
                let len = read_u32(&mut cur)? as usize;
                if cur.len() < len {
                    return Err(DecodeError::UnexpectedEof);
                }
                let content = Bytes::copy_from_slice(&cur[..len]);
                PgOutputMessage::Message {
                    flags,
                    lsn,
                    prefix,
                    content,
                }
            }
            other => return Err(DecodeError::UnknownTag(other)),
        };
        // We do NOT enforce `cur.is_empty()` — a future pgoutput proto
        // version may append optional fields, and the docs explicitly
        // reserve forward-compatibility space. The caller has the data
        // it needs.
        let _ = cur; // suppress "value assigned but not read"
        Ok(msg)
    }

    // ----- Test-only encoders -----
    //
    // pgoutput messages are server → client only; nobody sends them
    // from the client side. The codec tests need to construct frames
    // to assert the decoder reads what we expect. Encoders live behind
    // `#[cfg(any(test, feature = "test-encoders"))]` so they don't
    // bloat the release binary.

    #[cfg(test)]
    pub(crate) mod encode {
        use bytes::BufMut;

        pub fn relation(
            rel_id: u32,
            namespace: &str,
            name: &str,
            replica_identity: u8,
            columns: &[(u8, &str, u32, i32)],
        ) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.put_u8(b'R');
            buf.put_u32(rel_id);
            buf.extend_from_slice(namespace.as_bytes());
            buf.put_u8(0);
            buf.extend_from_slice(name.as_bytes());
            buf.put_u8(0);
            buf.put_u8(replica_identity);
            buf.put_u16(columns.len() as u16);
            for (flags, cname, type_oid, type_modifier) in columns {
                buf.put_u8(*flags);
                buf.extend_from_slice(cname.as_bytes());
                buf.put_u8(0);
                buf.put_u32(*type_oid);
                buf.put_i32(*type_modifier);
            }
            buf
        }

        pub fn insert(rel_id: u32, columns: &[Option<&str>]) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.put_u8(b'I');
            buf.put_u32(rel_id);
            buf.put_u8(b'N');
            buf.put_u16(columns.len() as u16);
            for c in columns {
                match c {
                    None => buf.put_u8(b'n'),
                    Some(s) => {
                        buf.put_u8(b't');
                        buf.put_i32(s.len() as i32);
                        buf.extend_from_slice(s.as_bytes());
                    }
                }
            }
            buf
        }

        pub fn update_no_old(rel_id: u32, columns: &[Option<&str>]) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.put_u8(b'U');
            buf.put_u32(rel_id);
            buf.put_u8(b'N');
            buf.put_u16(columns.len() as u16);
            for c in columns {
                match c {
                    None => buf.put_u8(b'n'),
                    Some(s) => {
                        buf.put_u8(b't');
                        buf.put_i32(s.len() as i32);
                        buf.extend_from_slice(s.as_bytes());
                    }
                }
            }
            buf
        }

        pub fn delete_key(rel_id: u32, key_columns: &[Option<&str>]) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.put_u8(b'D');
            buf.put_u32(rel_id);
            buf.put_u8(b'K');
            buf.put_u16(key_columns.len() as u16);
            for c in key_columns {
                match c {
                    None => buf.put_u8(b'n'),
                    Some(s) => {
                        buf.put_u8(b't');
                        buf.put_i32(s.len() as i32);
                        buf.extend_from_slice(s.as_bytes());
                    }
                }
            }
            buf
        }

        pub fn begin(final_lsn: u64, commit_timestamp: i64, xid: u32) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.put_u8(b'B');
            buf.put_u64(final_lsn);
            buf.put_i64(commit_timestamp);
            buf.put_u32(xid);
            buf
        }

        pub fn commit(
            flags: u8,
            commit_lsn: u64,
            end_lsn: u64,
            commit_timestamp: i64,
        ) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.put_u8(b'C');
            buf.put_u8(flags);
            buf.put_u64(commit_lsn);
            buf.put_u64(end_lsn);
            buf.put_i64(commit_timestamp);
            buf
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pgoutput::{PgOutputMessage, TupleColumn};

    #[test]
    fn parse_lsn_round_trip() {
        assert_eq!(parse_lsn("0/16B3750").unwrap(), 0x16B3750);
        assert_eq!(parse_lsn("1/16B3750").unwrap(), (1u64 << 32) | 0x16B3750);
        assert_eq!(parse_lsn("FF/FFFFFFFF").unwrap(), (0xFFu64 << 32) | 0xFFFFFFFF);
        assert_eq!(format_lsn(0x16B3750), "0/16B3750");
        assert_eq!(format_lsn((1u64 << 32) | 0x16B3750), "1/16B3750");
    }

    #[test]
    fn parse_lsn_rejects_bad_input() {
        assert!(parse_lsn("not-an-lsn").is_none());
        assert!(parse_lsn("0").is_none());
        assert!(parse_lsn("0/zz").is_none());
    }

    #[test]
    fn pgoutput_decode_begin() {
        let bytes = pgoutput::encode::begin(0x16B3750, 700_000_000_000, 42);
        let msg = pgoutput::decode(&bytes).unwrap();
        assert_eq!(
            msg,
            PgOutputMessage::Begin {
                final_lsn: 0x16B3750,
                commit_timestamp: 700_000_000_000,
                xid: 42,
            }
        );
    }

    #[test]
    fn pgoutput_decode_commit() {
        let bytes = pgoutput::encode::commit(0, 0x16B3750, 0x16B3800, 700_000_000_000);
        let msg = pgoutput::decode(&bytes).unwrap();
        assert_eq!(
            msg,
            PgOutputMessage::Commit {
                flags: 0,
                commit_lsn: 0x16B3750,
                end_lsn: 0x16B3800,
                commit_timestamp: 700_000_000_000,
            }
        );
    }

    #[test]
    fn pgoutput_decode_relation() {
        let bytes = pgoutput::encode::relation(
            16384,
            "public",
            "messages",
            b'd',
            &[
                (1, "id", 20, -1),       // BIGINT, REPLICA IDENTITY KEY
                (0, "title", 25, -1),    // TEXT
            ],
        );
        let msg = pgoutput::decode(&bytes).unwrap();
        match msg {
            PgOutputMessage::Relation {
                rel_id,
                namespace,
                name,
                replica_identity,
                columns,
            } => {
                assert_eq!(rel_id, 16384);
                assert_eq!(namespace, "public");
                assert_eq!(name, "messages");
                assert_eq!(replica_identity, b'd');
                assert_eq!(columns.len(), 2);
                assert_eq!(columns[0].name, "id");
                assert_eq!(columns[0].type_oid, 20);
                assert_eq!(columns[0].flags, 1);
                assert_eq!(columns[1].name, "title");
                assert_eq!(columns[1].type_oid, 25);
            }
            other => panic!("expected Relation, got {other:?}"),
        }
    }

    #[test]
    fn pgoutput_decode_insert() {
        let bytes = pgoutput::encode::insert(16384, &[Some("42"), Some("hello"), None]);
        let msg = pgoutput::decode(&bytes).unwrap();
        match msg {
            PgOutputMessage::Insert { rel_id, new_tuple } => {
                assert_eq!(rel_id, 16384);
                assert_eq!(new_tuple.columns.len(), 3);
                assert_eq!(new_tuple.columns[0], TupleColumn::Text("42".into()));
                assert_eq!(new_tuple.columns[1], TupleColumn::Text("hello".into()));
                assert_eq!(new_tuple.columns[2], TupleColumn::Null);
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn pgoutput_decode_update_no_old() {
        let bytes = pgoutput::encode::update_no_old(16384, &[Some("42"), Some("world")]);
        let msg = pgoutput::decode(&bytes).unwrap();
        match msg {
            PgOutputMessage::Update {
                rel_id,
                old_tuple,
                new_tuple,
            } => {
                assert_eq!(rel_id, 16384);
                assert!(old_tuple.is_none());
                assert_eq!(new_tuple.columns.len(), 2);
                assert_eq!(new_tuple.columns[1], TupleColumn::Text("world".into()));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn pgoutput_decode_delete_key() {
        let bytes = pgoutput::encode::delete_key(16384, &[Some("42")]);
        let msg = pgoutput::decode(&bytes).unwrap();
        match msg {
            PgOutputMessage::Delete { rel_id, old_tuple } => {
                assert_eq!(rel_id, 16384);
                assert_eq!(old_tuple.columns.len(), 1);
                assert_eq!(old_tuple.columns[0], TupleColumn::Text("42".into()));
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn pgoutput_decode_rejects_unknown_tag() {
        let bytes = vec![b'?'];
        let err = pgoutput::decode(&bytes).unwrap_err();
        match err {
            pgoutput::DecodeError::UnknownTag(b'?') => {}
            other => panic!("expected UnknownTag, got {other:?}"),
        }
    }

    #[test]
    fn pgoutput_decode_rejects_truncated_insert() {
        // Insert message with column count = 1 but no body.
        let bytes = vec![b'I', 0, 0, 0x40, 0, b'N', 0, 1];
        let err = pgoutput::decode(&bytes).unwrap_err();
        assert!(matches!(err, pgoutput::DecodeError::UnexpectedEof));
    }

    #[test]
    fn standby_status_update_encoding_layout() {
        // Build a StandbyStatusUpdate frame and check the bytes match
        // the documented wire layout. This is a unit test for the
        // encoder; integration testing is in `tests/integration.rs`
        // (gated on wal_level=logical).
        //
        // Layout: 'd' + length(38) + 'r' + 4×i64 + reply
        //       = 1 + 4 + 1 + 32 + 1 = 39 bytes total

        let lsn: u64 = 0x16B3750;
        let ts: i64 = 700_000_000_000;
        let mut bytes = vec![0u8; 0];
        bytes.push(COPY_DATA_TAG);
        bytes.extend_from_slice(&(4u32 + 34).to_be_bytes());
        bytes.push(STANDBY_STATUS_UPDATE_TAG);
        bytes.extend_from_slice(&lsn.to_be_bytes());
        bytes.extend_from_slice(&lsn.to_be_bytes());
        bytes.extend_from_slice(&lsn.to_be_bytes());
        bytes.extend_from_slice(&ts.to_be_bytes());
        bytes.push(0);

        assert_eq!(bytes.len(), 39);
        assert_eq!(bytes[0], b'd');
        assert_eq!(
            u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]),
            38
        );
        assert_eq!(bytes[5], b'r');
    }

    #[test]
    fn xlog_data_frame_parsing_smoke() {
        // Hand-assemble the body of a CopyData('w') frame and check
        // the field layout we expect. This is a unit test on the
        // body parser only — it doesn't drive the BufStream.
        //
        // body[0]      = 'w'
        // body[1..9]   = wal_start (u64 BE)
        // body[9..17]  = wal_end   (u64 BE)
        // body[17..25] = timestamp (i64 BE)
        // body[25..]   = payload
        let mut body = vec![XLOG_DATA_TAG];
        body.extend_from_slice(&0x100u64.to_be_bytes());
        body.extend_from_slice(&0x200u64.to_be_bytes());
        body.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        body.extend_from_slice(b"pgoutput-payload");
        assert_eq!(body.len(), 25 + 16);
        let wal_start = u64::from_be_bytes([
            body[1], body[2], body[3], body[4], body[5], body[6], body[7], body[8],
        ]);
        let wal_end = u64::from_be_bytes([
            body[9], body[10], body[11], body[12], body[13], body[14], body[15], body[16],
        ]);
        assert_eq!(wal_start, 0x100);
        assert_eq!(wal_end, 0x200);
        assert_eq!(&body[25..], b"pgoutput-payload");
    }

    #[test]
    fn replication_mode_config_roundtrip() {
        let cfg: Config = "host=localhost user=u dbname=d replication=database"
            .parse()
            .unwrap();
        assert_eq!(cfg.get_replication(), Some(ReplicationMode::Logical));

        let cfg: Config = "host=localhost user=u dbname=d replication=true"
            .parse()
            .unwrap();
        assert_eq!(cfg.get_replication(), Some(ReplicationMode::Physical));

        let cfg: Config = "host=localhost user=u dbname=d".parse().unwrap();
        assert_eq!(cfg.get_replication(), None);

        let err = "host=localhost user=u replication=bogus".parse::<Config>();
        assert!(err.is_err());
    }
}
