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
use crate::config::{Config, LoadBalanceHosts, ReplicationMode};
use crate::connect_socket::connect_socket;
use crate::connect_tls::{Encryption, negotiate_tls};
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::MakeTlsConnect;
use crate::{Error, Socket};
use crate::client::Addr;
use crate::config::Host;
use bytes::{BufMut, BytesMut};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use rand::seq::SliceRandom;
use postgres_protocol::message::backend::{DataRowBody, Message};
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
    // Host selection is `connect.rs`'s, and deliberately so: a `Config` here
    // is the same `Config` the query path takes, so a list of endpoints has to
    // mean the same thing on both. This walked `get_hosts().first()` and
    // stopped, which gave a multi-endpoint configuration no failover at all
    // and - because the same `first()` was applied to DNS - made a name that
    // resolves to an AAAA and an A record fail on the first address against a
    // server bound only to IPv4.
    if config.get_hosts().is_empty() && config.get_hostaddrs().is_empty() {
        return Err(Error::config(
            "replication: host or hostaddr is required".into(),
        ));
    }

    if !config.get_hosts().is_empty()
        && !config.get_hostaddrs().is_empty()
        && config.get_hosts().len() != config.get_hostaddrs().len()
    {
        let msg = format!(
            "number of hosts ({}) is different from number of hostaddrs ({})",
            config.get_hosts().len(),
            config.get_hostaddrs().len(),
        );
        return Err(Error::config(msg.into()));
    }

    let num_hosts = std::cmp::max(config.get_hosts().len(), config.get_hostaddrs().len());
    if config.get_ports().len() > 1 && config.get_ports().len() != num_hosts {
        return Err(Error::config("invalid number of ports".into()));
    }

    // We need a Config with replication=database set. Most callers
    // already set it; tolerate both shapes and force-set as a defensive
    // measure if they didn't.
    let mut cfg = config.clone();
    if cfg.get_replication().is_none() {
        cfg.replication(ReplicationMode::Logical);
    }

    let mut indices = (0..num_hosts).collect::<Vec<_>>();
    if cfg.get_load_balance_hosts() == LoadBalanceHosts::Random {
        indices.shuffle(&mut rand::rng());
    }

    let mut error = None;
    for i in indices {
        let host = config.get_hosts().get(i);
        let hostaddr = config.get_hostaddrs().get(i).copied();
        // libpq broadcasts a single port across every host and only demands
        // one per host when more than one is given.
        let port = config
            .get_ports()
            .get(i)
            .or_else(|| config.get_ports().first())
            .copied()
            .unwrap_or(5432);
        // `host` is the TLS validation hostname even when `hostaddr` supplies
        // the address to dial.
        let hostname = match host {
            Some(Host::Tcp(host)) => Some(host.clone()),
            #[cfg(unix)]
            Some(Host::Unix(_)) => None,
            None => None,
        };

        match connect_replication_host(host, hostaddr, hostname, port, &mut tls, &cfg).await {
            Ok(connection) => return Ok(connection),
            Err(e) => error = Some(e),
        }
    }

    Err(error.expect("num_hosts > 0, so at least one endpoint was attempted"))
}

/// Every address one configured endpoint denotes, in order, until one
/// connects.
async fn connect_replication_host<T>(
    host: Option<&Host>,
    hostaddr: Option<std::net::IpAddr>,
    hostname: Option<String>,
    port: u16,
    tls: &mut T,
    cfg: &Config,
) -> Result<ReplicationConnection<Socket, T::Stream>, Error>
where
    T: MakeTlsConnect<Socket>,
{
    // A numeric `hostaddr` is the address; `host` stays the name TLS
    // validates against. There is nothing to resolve.
    if let Some(ip) = hostaddr {
        return connect_replication_addr(Addr::Tcp(ip), hostname.as_deref(), port, tls, cfg).await;
    }

    match host.expect("one of host / hostaddr is present at this index") {
        Host::Tcp(host) => {
            use compio::net::ToSocketAddrsAsync;
            let mut addrs = (&**host, port)
                .to_socket_addrs_async()
                .await
                .map_err(Error::connect)?
                .collect::<Vec<_>>();

            if cfg.get_load_balance_hosts() == LoadBalanceHosts::Random {
                addrs.shuffle(&mut rand::rng());
            }

            let mut error = None;
            for addr in addrs {
                match connect_replication_addr(
                    Addr::Tcp(addr.ip()),
                    hostname.as_deref(),
                    port,
                    tls,
                    cfg,
                )
                .await
                {
                    Ok(connection) => return Ok(connection),
                    Err(e) => error = Some(e),
                }
            }

            Err(error.unwrap_or_else(|| {
                Error::config("replication: DNS yielded no addresses".into())
            }))
        }
        #[cfg(unix)]
        Host::Unix(path) => {
            connect_replication_addr(Addr::Unix(path.clone()), None, port, tls, cfg).await
        }
    }
}

/// One address: a socket, one transport, one startup exchange.
async fn connect_replication_addr<T>(
    addr: Addr,
    hostname: Option<&str>,
    port: u16,
    tls: &mut T,
    cfg: &Config,
) -> Result<ReplicationConnection<Socket, T::Stream>, Error>
where
    T: MakeTlsConnect<Socket>,
{
    let socket = connect_socket(
        &addr,
        port,
        cfg.get_connect_timeout().copied(),
        cfg.get_tcp_user_timeout().copied(),
        if cfg.get_keepalives() {
            Some(&cfg.keepalive_config)
        } else {
            None
        },
    )
    .await?;

    let tls_inst = tls
        .make_tls_connect(hostname.unwrap_or(""))
        .map_err(|e| Error::tls(e.into()))?;
    let has_hostname = hostname.is_some();

    // One transport per address, no reconnect: this path opens its own socket
    // rather than going through `connect::connect`, so it does not inherit the
    // `allow` / `prefer` fallback that lives there. Those two modes therefore
    // get the transport they try first and stop. A replication connection is a
    // deliberate, operator-configured thing - it is not the surface where
    // "whatever the server happens to accept" is worth the plumbing.
    let stream = negotiate_tls(
        socket,
        Encryption::first_for(cfg.get_ssl_mode()),
        cfg.get_ssl_mode(),
        cfg.get_ssl_negotiation(),
        tls_inst,
        has_hostname,
    )
    .await?;

    // Run the normal startup + auth handshake — connect_raw_into
    // exposes the post-handshake BufStream that the replication
    // connection then owns.
    let (stream, parameters) = handshake_replication(stream, cfg).await?;

    Ok(ReplicationConnection {
        stream: BufStream::new(stream),
        parameters,
        in_flight: InFlight::default(),
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
    /// See [`InFlight`].
    in_flight: InFlight,
}

/// Records that an I/O call owns the stream, so that a call which never
/// returned can be told from one that did.
///
/// NONE OF THESE METHODS IS CANCEL-SAFE, and the flag is how that is enforced
/// rather than merely documented. Dropping the future of an awaited call
/// leaves the connection out of step with the server in a way no later call
/// can repair:
///
/// * A read hands its buffer to the kernel with the submitted operation.
///   Cancelling it discards whatever was delivered into that buffer, so the
///   bytes are off the socket and nowhere - the next read resumes in the
///   middle of a frame and reads a payload byte as a tag.
/// * A write is a loop over partial writes ([`BufStream::flush`] takes the
///   encoded frame out of the write buffer BEFORE it awaits), so a cancelled
///   one can leave a fraction of a frame on the wire with the rest discarded.
///   The peer is then parsing our frame's middle as a frame's start.
///
/// The flag is set on entry and cleared on return. A future dropped in
/// between never reaches the clear, so it stays set and every later call on
/// the same connection fails. This does not recover the connection - nothing
/// can - it stops the driver from pretending the connection is still in step.
///
/// This is reachable from ordinary code, not just from a timeout: a consumer
/// driving `next()` inside a `futures::select!` drops the losing branch's
/// future on every iteration.
#[derive(Debug, Default)]
struct InFlight(bool);

impl InFlight {
    /// Claim the stream for one I/O call, or refuse because a previous call
    /// never gave it back.
    fn enter(&mut self) -> Result<(), Error> {
        if self.0 {
            return Err(Error::cancelled());
        }
        self.0 = true;
        Ok(())
    }

    /// Give the stream back after a call returned under its own power.
    fn leave(&mut self) {
        self.0 = false;
    }
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
        self.in_flight.enter()?;
        let result = self.identify_system_inner().await;
        self.in_flight.leave();
        result
    }

    async fn identify_system_inner(&mut self) -> Result<IdentifySystem, Error> {
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
                    let parsed = parse_identify_system_row(&row)?;
                    systemid = parsed.systemid;
                    timeline = parsed.timeline;
                    xlogpos = parsed.xlogpos;
                    dbname = parsed.dbname;
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
        // Taken and never given back: this call consumes the connection, so a
        // future dropped part-way through takes the stream with it and there
        // is nothing left to refuse. The claim still has to happen, because a
        // connection whose `identify_system` was dropped must not go on to
        // start streaming on a stream that is mid-frame.
        self.in_flight.enter()?;

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
                        lsn: LsnTracker::new(parse_lsn(opts.start_lsn).unwrap_or(0)),
                        in_flight: InFlight::default(),
                    });
                }
                ERROR_RESPONSE_TAG => {
                    let bytes = self.stream.buf().split_to(header.body_len()).freeze();
                    return Err(error_from_error_response_frame(&header, &bytes));
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
    /// The two StandbyStatusUpdate positions (received vs flushed).
    lsn: LsnTracker,
    /// See [`InFlight`]. Neither [`ReplicationStream::next`] nor
    /// [`ReplicationStream::send_standby_status_update`] is cancel-safe.
    in_flight: InFlight,
}

/// Tracks the two distinct LSN positions a logical-replication client
/// reports back to the walsender in a `StandbyStatusUpdate`:
///
/// - `received` — the highest LSN we've *seen on the wire* (the
///   `wal_end` of an XLogData / PrimaryKeepalive). Reported as
///   `write_lsn`.
/// - `processed` — the highest LSN the caller has *durably handled*
///   (advanced via [`ReplicationStream::advance_lsn`]). Reported as both
///   `flush_lsn` and `apply_lsn`.
///
/// Keeping them separate matters: `flush_lsn` is a *durability promise*
/// — Postgres recycles WAL and advances the slot's `confirmed_flush`
/// up to it. Conflating "received" (merely buffered) with "flushed"
/// (durably processed) would let the server discard WAL the consumer
/// hasn't actually persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LsnTracker {
    received: u64,
    processed: u64,
}

impl LsnTracker {
    /// Seed both positions from the resume LSN passed to
    /// `START_REPLICATION`.
    const fn new(start_lsn: u64) -> Self {
        Self {
            received: start_lsn,
            processed: start_lsn,
        }
    }

    /// Record that `lsn` has been seen on the wire (`XLogData` /
    /// `PrimaryKeepalive` `wal_end`). Advances ONLY the received
    /// position; merely seeing bytes is not a durability promise.
    /// Monotonic: never regresses.
    const fn observe_received(&mut self, lsn: u64) {
        if lsn > self.received {
            self.received = lsn;
        }
    }

    /// Record that the caller has durably processed up to `lsn`.
    /// Advances ONLY the flush position. Monotonic: never regresses.
    const fn advance_processed(&mut self, lsn: u64) {
        if lsn > self.processed {
            self.processed = lsn;
        }
    }

    /// The `(write, flush, apply)` triple for a `StandbyStatusUpdate`.
    ///
    /// `write` reports the received position (highest seen on the wire);
    /// `flush` and `apply` report the durably-processed position. They
    /// are deliberately NOT collapsed into one value: `flush` is the
    /// WAL-recycling promise, and reporting the received position there
    /// would let the server discard WAL the consumer hasn't persisted.
    const fn standby_lsns(&self) -> (u64, u64, u64) {
        (self.received, self.processed, self.processed)
    }
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
    ///
    /// NOT CANCEL-SAFE. Dropping this future before it resolves loses the
    /// bytes its read had in flight and leaves the stream mid-frame; every
    /// later call on the stream then fails. See [`InFlight`]. To read with a
    /// timeout, hold ONE future across the wait rather than starting a new
    /// one each time round.
    pub async fn next(&mut self) -> Result<Option<ReplicationMessage>, Error> {
        self.in_flight.enter()?;
        let result = self.next_inner().await;
        self.in_flight.leave();
        result
    }

    async fn next_inner(&mut self) -> Result<Option<ReplicationMessage>, Error> {
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
                            // as the "received" position for
                            // StandbyStatusUpdate reporting.
                            self.lsn.observe_received(wal_end);
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
                            self.lsn.observe_received(wal_end);
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
                    // The walsender's own failures arrive here: the slot
                    // dropped underneath us, the requested WAL segment
                    // recycled, the publication gone. A consumer has to tell
                    // those apart - one is fatal, one means re-create and
                    // re-snapshot - so the SQLSTATE and the server's message
                    // travel with the error rather than being dropped for a
                    // fixed string.
                    let bytes = self.stream.buf().split_to(header.body_len()).freeze();
                    return Err(error_from_error_response_frame(&header, &bytes));
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
        self.lsn.received
    }

    /// Highest WAL LSN the caller has confirmed it processed —
    /// reported as `flush_lsn` in StandbyStatusUpdate frames.
    pub fn last_processed_lsn(&self) -> u64 {
        self.lsn.processed
    }

    /// Mark `lsn` as durably processed. The next StandbyStatusUpdate
    /// surfaces this as the `flush_lsn` (and `apply_lsn`), letting
    /// Postgres recycle WAL and advance the slot's `confirmed_flush`
    /// up to it.
    ///
    /// This advances ONLY the flush position; the received position
    /// (`write_lsn`) tracks `wal_end` independently in
    /// [`next`](Self::next). Fully realizing the durability guarantee
    /// therefore requires the CONSUMER to call `advance_lsn` only
    /// *after* a durable hand-off (persisted / acknowledged), never on
    /// mere receipt — a concern that lives in the consumer
    /// (`wal_consumer.rs`), intentionally out of scope of this driver.
    pub fn advance_lsn(&mut self, lsn: u64) {
        self.lsn.advance_processed(lsn);
    }

    /// Send a `StandbyStatusUpdate` frame upstream.
    ///
    /// The `write_lsn` slot reports the highest LSN seen on the wire
    /// (the received position, advanced in [`next`](Self::next)); the
    /// `flush_lsn` and `apply_lsn` slots report the durably-processed
    /// position (advanced via [`advance_lsn`](Self::advance_lsn)).
    /// These are kept distinct on purpose: `flush_lsn` is a durability
    /// promise that lets the server recycle WAL, so reporting the
    /// received position there would let Postgres discard WAL the
    /// consumer hasn't actually persisted.
    ///
    /// `reply_requested = true` makes the server reply with an
    /// immediate PrimaryKeepalive — typically left `false`.
    ///
    /// NOT CANCEL-SAFE. Dropping this future before it resolves can leave a
    /// fraction of the frame on the wire, which no later frame can repair;
    /// every later call on the stream then fails. See [`InFlight`].
    pub async fn send_standby_status_update(
        &mut self,
        reply_requested: bool,
    ) -> Result<(), Error> {
        self.in_flight.enter()?;
        let result = self.send_standby_status_update_inner(reply_requested).await;
        self.in_flight.leave();
        result
    }

    async fn send_standby_status_update_inner(
        &mut self,
        reply_requested: bool,
    ) -> Result<(), Error> {
        let now = postgres_microseconds_since_epoch();
        let (write, flush, apply) = self.lsn.standby_lsns();
        encode_standby_status_update(&mut self.stream, write, flush, apply, now, reply_requested)?;
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
    ///
    /// The subtraction is total because [`read_header`] refuses to build a
    /// `WireHeader` whose `length` is below 4, which is the only way it could
    /// underflow. Do not relax that check without giving this a return type:
    /// an underflow here is a panic in a debug build and a `usize::MAX`-ish
    /// size handed to `split_to` in a release one, and both kill the
    /// replication task on a frame that should merely have ended the stream.
    fn body_len(&self) -> usize {
        self.length as usize - 4
    }
}

/// The error a frame whose declared length cannot describe a body raises.
fn impossible_frame_length(length: u32) -> Error {
    Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "replication frame declares length {length}, below the 4 bytes of the length field"
        ),
    ))
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
    // `validate_length` is a CEILING - it stops a crafted 4 GB frame from
    // being buffered. The floor is checked here: the length field counts
    // itself, so 4 is the smallest value the protocol can express and
    // anything below it describes a body of negative size.
    if length < 4 {
        return Err(impossible_frame_length(length));
    }
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

/// Turn an `ErrorResponse` whose header [`read_header`] already peeled back
/// into an [`Error`] carrying the server's `DbError`.
///
/// The framer in `postgres_protocol` wants a whole wire message, so the tag
/// and length go back on in front of the payload before it runs.
fn error_from_error_response_frame(header: &WireHeader, payload: &[u8]) -> Error {
    let mut body = BytesMut::with_capacity(payload.len() + 5);
    body.put_u8(ERROR_RESPONSE_TAG);
    body.put_u32(header.length);
    body.extend_from_slice(payload);
    error_from_error_response_body(body)
}

/// Turn a reconstructed `ErrorResponse` wire message (`E` tag + 4-byte
/// big-endian length + field payload) into an [`Error`].
///
/// Runs `postgres_protocol`'s framer over `body` so the SQLSTATE,
/// severity, and message survive as a [`crate::error::DbError`] — the
/// same shape the `IDENTIFY_SYSTEM` loop produces via
/// `Message::ErrorResponse(body) => Error::db(body)`. A byte count alone
/// (the former behaviour) made `START_REPLICATION` failures
/// undebuggable. If the framer can't parse the bytes, fall back to a
/// parse error rather than silently dropping the failure.
fn error_from_error_response_body(mut body: BytesMut) -> Error {
    match Message::parse(&mut body) {
        Ok(Some(Message::ErrorResponse(b))) => Error::db(b),
        _ => Error::parse(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "START_REPLICATION: malformed ErrorResponse",
        )),
    }
}

/// Parse an `IDENTIFY_SYSTEM` `DataRow` into an [`IdentifySystem`].
///
/// The four fields, in order, are `systemid`, `timeline`, `xlogpos` and
/// `dbname`; `dbname` is SQL NULL when the connection is not bound to a
/// database, and maps to `None`.
///
/// THE FIELD COUNT IS NOT IN THE BUFFER, which is the whole reason this takes
/// a [`DataRowBody`] rather than a byte slice. `Message::parse` consumes the
/// `DataRow`'s `u16` count into `DataRowBody::len` and keeps only the
/// length-prefixed fields in `storage` - and `storage` is exactly what
/// `buffer()` returns. This function used to take `row.buffer()` and read a
/// `u16` count off the front of it, which landed on the top two bytes of the
/// FIRST FIELD'S `i32` length. Any length below 65536 encodes as
/// `00 00 hi lo`, so the count read as zero for every real server: the field
/// loop never ran and `identify_system` returned an empty systemid, a zero
/// timeline and an empty xlogpos while reporting success.
///
/// [`DataRowBody::ranges`] is the accessor for this, and it carries the count
/// `parse` already took off the wire, so the framing here cannot drift from
/// the framing that produced the row. It is also bounds-checked, so a
/// truncated or malformed row yields an `Err` rather than panicking the
/// replication task.
fn parse_identify_system_row(row: &DataRowBody) -> Result<IdentifySystem, Error> {
    let mut fields: Vec<Option<&str>> = Vec::new();
    let mut ranges = row.ranges();
    let buf = row.buffer();
    while let Some(range) = ranges.next().map_err(Error::parse)? {
        match range {
            None => fields.push(None),
            Some(range) => {
                let slice = buf.get(range).ok_or_else(eof_identify_row)?;
                fields.push(Some(
                    std::str::from_utf8(slice)
                        .map_err(|e| Error::parse(std::io::Error::other(e)))?,
                ));
            }
        }
    }

    Ok(IdentifySystem {
        systemid: fields.first().and_then(|f| *f).unwrap_or("").to_string(),
        timeline: fields
            .get(1)
            .and_then(|f| *f)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        xlogpos: fields.get(2).and_then(|f| *f).unwrap_or("").to_string(),
        dbname: fields.get(3).and_then(|f| *f).map(|s| s.to_string()),
    })
}

/// The `UnexpectedEof` error returned when the `IDENTIFY_SYSTEM`
/// `DataRow` body is truncated mid-field.
fn eof_identify_row() -> Error {
    Error::parse(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "IDENTIFY_SYSTEM DataRow truncated",
    ))
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
/// to [`pgoutput::decode`] to obtain a [`pgoutput::PgOutputMessage`].
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
                // Reserve for what the frame can actually still contain, not
                // for what the count claims. `nrelations` is attacker- or
                // corruption-controlled and the frame length does not bound it:
                // a 6-byte body carrying 0xFFFFFFFF would otherwise reserve
                // ~16 GiB before the first read fails. Each id is 4 bytes, so
                // `cur.len() / 4` is an EXACT upper bound on how many remain -
                // it rejects nothing and truncates no valid message.
                //
                // Deliberately not a constant cap. `TRUNCATE ... CASCADE` on a
                // heavily partitioned table emits one id per partition and
                // PostgreSQL enforces no ceiling there, so a fixed limit would
                // refuse valid input - and because the WAL consumer propagates
                // a decode error out of its run loop, refusing valid input
                // kills replication permanently rather than degrading.
                let mut relation_ids = Vec::with_capacity(nrelations.min(cur.len() / 4));
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
    // to assert the decoder reads what we expect. Encoders are
    // `#[cfg(test)]` so they don't bloat the release binary.
    //
    // Plain `cfg(test)`, NOT `cfg(any(test, feature = "..."))`: there is
    // no encoder feature on this crate, and nothing outside it needs to
    // build frames. If a downstream crate ever does, that is the moment
    // to add the feature - not to describe one that does not exist.

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

    /// Build a whole `DataRow` WIRE MESSAGE and hand it to
    /// `Message::parse`, so the fixture is the shape production actually
    /// receives.
    ///
    /// The previous helper returned a bare body with the `u16` field count
    /// still on the front and passed that straight to the parser. No
    /// `DataRowBody` ever looks like that: `Message::parse` consumes the count
    /// into `DataRowBody::len` and leaves only the fields in the buffer. So
    /// the fixture and the parser agreed on a layout the server never sends,
    /// every case below passed, and `identify_system` returned empty values
    /// against a real server. Going through `Message::parse` is what makes
    /// these tests able to fail.
    fn identify_row(fields: &[Option<&str>]) -> DataRowBody {
        let mut body = Vec::new();
        body.extend_from_slice(&u16::try_from(fields.len()).unwrap().to_be_bytes());
        for f in fields {
            match f {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(s) => {
                    body.extend_from_slice(&i32::try_from(s.len()).unwrap().to_be_bytes());
                    body.extend_from_slice(s.as_bytes());
                }
            }
        }

        let mut wire = vec![b'D'];
        wire.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);

        let mut buf = BytesMut::from(&wire[..]);
        match Message::parse(&mut buf) {
            Ok(Some(Message::DataRow(row))) => row,
            other => panic!("fixture did not parse as a DataRow: {:?}", other.is_ok()),
        }
    }

    #[test]
    fn identify_row_parses_well_formed() {
        // 4 fields: systemid, timeline, xlogpos, dbname.
        let row = identify_row(&[
            Some("7012345678901234567"),
            Some("3"),
            Some("0/16B3750"),
            Some("zeroship"),
        ]);
        let got = parse_identify_system_row(&row).expect("well-formed row must parse");
        assert_eq!(got.systemid, "7012345678901234567");
        assert_eq!(got.timeline, 3);
        assert_eq!(got.xlogpos, "0/16B3750");
        assert_eq!(got.dbname.as_deref(), Some("zeroship"));

        // dbname NULL (physical replication connection) must parse to None.
        let row = identify_row(&[Some("701"), Some("1"), Some("0/0"), None]);
        let got = parse_identify_system_row(&row).expect("row with NULL dbname must parse");
        assert_eq!(got.dbname, None);
        assert_eq!(got.timeline, 1);
    }

    /// A systemid whose length happens to start with zero bytes is the exact
    /// shape that made the old parser return nothing.
    ///
    /// Any length below 65536 encodes as `00 00 hi lo`, so reading a `u16`
    /// off the front of the first field's `i32` length always yielded 0. That
    /// is every real system identifier, which is why this failed against every
    /// server rather than some unusual one.
    #[test]
    fn identify_row_field_count_is_not_re_read_from_the_body() {
        let row = identify_row(&[
            Some("7676199916444573733"),
            Some("1"),
            Some("0/1A2B3C8"),
            Some("zeroship"),
        ]);
        let got = parse_identify_system_row(&row).expect("well-formed row must parse");
        assert_eq!(
            got.systemid, "7676199916444573733",
            "the field count was re-read from the body and swallowed every field"
        );
        assert_eq!(got.timeline, 1);
    }

    #[test]
    fn identify_row_truncated_is_error_not_panic() {
        // A DataRow whose header claims more fields than its body carries.
        // `Message::parse` accepts it (it only splits tag and length), so the
        // short read surfaces from `ranges()` and must be an Err, not a panic
        // and not a silently short result.
        let mut wire = vec![b'D'];
        let body = [0x00u8, 0x04]; // count 4, no fields
        wire.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);
        let mut buf = BytesMut::from(&wire[..]);
        let Ok(Some(Message::DataRow(row))) = Message::parse(&mut buf) else {
            panic!("fixture did not parse as a DataRow");
        };
        assert!(
            parse_identify_system_row(&row).is_err(),
            "a row claiming 4 fields with no body must be Err, not panic"
        );

        // Count 1, then a length prefix that runs off the end.
        let mut wire = vec![b'D'];
        let body = [0x00u8, 0x01, 0x00, 0x00];
        wire.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);
        let mut buf = BytesMut::from(&wire[..]);
        let Ok(Some(Message::DataRow(row))) = Message::parse(&mut buf) else {
            panic!("fixture did not parse as a DataRow");
        };
        assert!(
            parse_identify_system_row(&row).is_err(),
            "a truncated length prefix must be Err, not panic"
        );

        // Count 1, declared length 10, only 3 payload bytes present.
        let mut wire = vec![b'D'];
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&10i32.to_be_bytes());
        body.extend_from_slice(b"abc");
        wire.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);
        let mut buf = BytesMut::from(&wire[..]);
        let Ok(Some(Message::DataRow(row))) = Message::parse(&mut buf) else {
            panic!("fixture did not parse as a DataRow");
        };
        assert!(
            parse_identify_system_row(&row).is_err(),
            "a field length exceeding the remaining bytes must be Err, not panic"
        );

        // A row with no fields at all yields the empty identity rather than an
        // error: there is nothing malformed about it, and `identify_system`
        // reports the absence through its own values.
        let row = identify_row(&[]);
        let got = parse_identify_system_row(&row).expect("an empty row is not malformed");
        assert_eq!(got.systemid, "");
    }

    /// Build a full `ErrorResponse` wire message: `E` tag + 4-byte
    /// big-endian length + field payload, where each field is
    /// `type-byte + NUL-terminated string`, terminated by a single
    /// `0x00`. The length field counts itself (4 bytes) + the payload,
    /// but NOT the leading tag.
    fn error_response_message(fields: &[(u8, &str)]) -> BytesMut {
        let mut payload = Vec::new();
        for (ty, val) in fields {
            payload.push(*ty);
            payload.extend_from_slice(val.as_bytes());
            payload.push(0);
        }
        payload.push(0); // field-list terminator

        let mut msg = BytesMut::new();
        msg.put_u8(ERROR_RESPONSE_TAG);
        msg.put_u32(u32::try_from(payload.len() + 4).unwrap());
        msg.extend_from_slice(&payload);
        msg
    }

    #[test]
    fn start_replication_error_response_surfaces_dberror() {
        // A walsender refusing START_REPLICATION (e.g. the role lacks
        // REPLICATION) sends an ErrorResponse carrying SQLSTATE + message.
        // The driver must surface that as a DbError, not a byte count.
        let body = error_response_message(&[
            (b'S', "ERROR"),
            (b'V', "ERROR"),
            (b'C', "42501"),
            (b'M', "permission denied to start WAL sender"),
        ]);
        let err = error_from_error_response_body(body);

        let db = err
            .as_db_error()
            .expect("START_REPLICATION ErrorResponse must surface as a DbError");
        assert_eq!(db.code().code(), "42501");
        assert_eq!(db.message(), "permission denied to start WAL sender");

        // The convenience accessor on Error must also expose the SQLSTATE.
        assert_eq!(
            err.code().map(crate::error::SqlState::code),
            Some("42501")
        );
    }

    #[test]
    fn lsn_tracker_reports_flush_below_received() {
        // The walsender protocol distinguishes write (received) from
        // flush (durably processed). A careful caller that has *seen*
        // up to LSN 100 on the wire but only durably *flushed* 50 must
        // be able to report write=100, flush=50, apply=50 — otherwise
        // Postgres would recycle WAL the consumer hasn't persisted.
        let mut t = LsnTracker::new(0);
        t.observe_received(100);
        t.advance_processed(50);

        assert_eq!(
            t.standby_lsns(),
            (100, 50, 50),
            "write must reflect received (100); flush/apply must reflect processed (50)"
        );
        assert_eq!(t.received, 100);
        assert_eq!(t.processed, 50);
    }

    #[test]
    fn lsn_tracker_is_monotonic() {
        let mut t = LsnTracker::new(0);
        t.observe_received(100);
        t.advance_processed(50);

        // Advancing processed backwards must not regress.
        t.advance_processed(40);
        assert_eq!(t.processed, 50, "advance_processed must not regress");

        // Observing an older received must not regress, and must NOT
        // drag the (already-higher) processed position back.
        t.advance_processed(80);
        t.observe_received(70);
        assert_eq!(t.received, 100, "observe_received must not regress");
        assert_eq!(t.processed, 80, "observe_received must not touch processed");
        assert_eq!(t.standby_lsns(), (100, 80, 80));

        // A fresh tracker seeded at a non-zero resume LSN reports it in
        // all three slots until something advances.
        let seeded = LsnTracker::new(0x016B_3750);
        assert_eq!(
            seeded.standby_lsns(),
            (0x016B_3750, 0x016B_3750, 0x016B_3750)
        );
    }

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

    /// A TRUNCATE naming more relations than any fixed cap would allow must
    /// still decode.
    ///
    /// This fails if someone bounds the count with a constant that REJECTS -
    /// verified by mutation: adding `if nrelations > 65535 { return Err(..) }`
    /// turns it red with "a large TRUNCATE is valid input: UnexpectedEof".
    /// A constant that only caps the RESERVATION (`nrelations.min(65535)`)
    /// leaves it green, because the vector still grows; that mutation was tried
    /// first and passed, so this test does not cover it.
    ///
    /// That uncovered case is a HOLE, not a handoff - nothing else in this
    /// crate asserts the reservation size, and no assertion here could: the
    /// malformed frame below errors identically whether the capacity came from
    /// the frame or from the wire count. Stated rather than left implicit,
    /// because an exclusion that does not say where the class IS covered leaves
    /// a reader unable to tell a gap from a delegation.
    ///
    /// `TRUNCATE ... CASCADE` on a heavily partitioned table emits one id per
    /// partition and PostgreSQL enforces no ceiling, so a rejecting limit
    /// refuses valid input - and the WAL consumer propagates a decode error out
    /// of its run loop, so refusing valid input stops replication permanently
    /// instead of degrading.
    ///
    /// It does NOT test the reservation itself. Decoding the malformed frame
    /// below errors identically whether the capacity is bounded by the frame
    /// or taken from the wire count, so no assertion here can tell those apart.
    #[test]
    fn pgoutput_decode_accepts_a_truncate_larger_than_any_fixed_cap() {
        const N: u32 = 70_000; // above the u16 ceiling a sibling arm uses
        let mut bytes = vec![b'T'];
        bytes.extend_from_slice(&N.to_be_bytes());
        bytes.push(0); // options
        for id in 0..N {
            bytes.extend_from_slice(&id.to_be_bytes());
        }
        let msg = pgoutput::decode(&bytes).expect("a large TRUNCATE is valid input");
        match msg {
            pgoutput::PgOutputMessage::Truncate { relation_ids, .. } => {
                assert_eq!(relation_ids.len(), N as usize);
                assert_eq!(relation_ids[0], 0);
                assert_eq!(relation_ids[N as usize - 1], N - 1);
            }
            other => panic!("expected Truncate, got {other:?}"),
        }
    }

    /// A TRUNCATE whose relation count exceeds what the frame can hold is
    /// rejected, and the reservation it triggers is bounded by the frame rather
    /// than by the claimed count.
    #[test]
    fn pgoutput_decode_rejects_a_truncate_count_larger_than_the_frame() {
        // count = u32::MAX, options = 0, and no ids at all.
        let bytes = vec![b'T', 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        let err = pgoutput::decode(&bytes).unwrap_err();
        assert!(matches!(err, pgoutput::DecodeError::UnexpectedEof));
    }

    #[test]
    fn pgoutput_decode_rejects_truncated_insert() {
        // Insert message with column count = 1 but no body.
        let bytes = vec![b'I', 0, 0, 0x40, 0, b'N', 0, 1];
        let err = pgoutput::decode(&bytes).unwrap_err();
        assert!(matches!(err, pgoutput::DecodeError::UnexpectedEof));
    }

    /// A byte source that hands the framer exactly the bytes a test wrote and
    /// reports EOF once they run out.
    ///
    /// Only the READ direction carries meaning here: every assertion below is
    /// about what the decoder makes of a frame, so writes are accepted and
    /// discarded. For anything whose subject is what went upstream, or how the
    /// stream behaves under a cancelled I/O call, the tests use a real socket
    /// instead - a hand-rolled peer's answer to "how many bytes reached the
    /// server" is whatever the peer was written to say.
    struct ScriptedPeer {
        unread: Vec<u8>,
    }

    impl compio::io::AsyncRead for ScriptedPeer {
        async fn read<B: compio::buf::IoBufMut>(
            &mut self,
            buf: B,
        ) -> compio::buf::BufResult<usize, B> {
            let mut src: &[u8] = &self.unread;
            let before = src.len();
            let result = src.read(buf).await;
            let consumed = before - src.len();
            self.unread.drain(..consumed);
            result
        }
    }

    impl compio::io::AsyncWrite for ScriptedPeer {
        async fn write<B: compio::buf::IoBuf>(
            &mut self,
            buf: B,
        ) -> compio::buf::BufResult<usize, B> {
            compio::buf::BufResult(Ok(compio::buf::IoBuf::buf_len(&buf)), buf)
        }
        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A `ReplicationStream` reading the given bytes as if the walsender had
    /// sent them inside the CopyBoth channel.
    fn stream_over(bytes: Vec<u8>) -> ReplicationStream<ScriptedPeer, ScriptedPeer> {
        ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(ScriptedPeer { unread: bytes })),
            lsn: LsnTracker::new(0),
            in_flight: InFlight::default(),
        }
    }

    /// A message header whose declared length is below the 4 bytes the length
    /// field itself occupies must be rejected, not turned into a body size.
    ///
    /// The length field counts itself, so 4 is the smallest value the protocol
    /// can express and `length - 4` is the payload size. Nothing checked the
    /// floor, so a declared 0..=3 underflowed that subtraction: a panic in a
    /// debug build, and in a release build a `usize::MAX`-ish size handed
    /// straight to `split_to`, which panics too. Either way a malformed frame
    /// took down the replication task instead of ending the stream.
    #[compio::test]
    async fn a_frame_length_below_the_length_field_is_an_error() {
        for declared in 0u32..4 {
            let mut wire = vec![COPY_DATA_TAG];
            wire.extend_from_slice(&declared.to_be_bytes());
            // Trailing bytes so the 5-byte header read itself is satisfied and
            // the failure is the arithmetic, not a short read.
            wire.extend_from_slice(&[0u8; 8]);

            let mut stream = stream_over(wire);
            assert!(
                stream.next().await.is_err(),
                "a frame declaring length {declared} must be an error, not a panic"
            );
        }
    }

    /// The smallest length the protocol can express is 4 - an empty body - and
    /// it must still decode. `CopyDone` is exactly that frame.
    ///
    /// This is the control for the floor check above: a fix that rejects
    /// `length <= 4`, or that demands a non-empty body, turns this red.
    #[compio::test]
    async fn a_frame_declaring_an_empty_body_still_decodes() {
        let mut wire = vec![COPY_DONE_TAG];
        wire.extend_from_slice(&4u32.to_be_bytes());

        let mut stream = stream_over(wire);
        assert!(
            stream
                .next()
                .await
                .expect("CopyDone is a valid empty-bodied frame")
                .is_none(),
            "CopyDone ends the stream"
        );
    }

    /// An `ErrorResponse` arriving mid-stream must reach the caller with the
    /// server's SQLSTATE and message.
    ///
    /// This is how a walsender reports that the slot was dropped underneath
    /// us, or that the requested WAL segment has been recycled - the two
    /// failures a consumer has to tell apart, because one is fatal and the
    /// other means "re-create the slot and re-snapshot". The arm dropped the
    /// payload on the floor and returned a bare io error reading
    /// "replication stream: ErrorResponse", so both looked identical.
    #[compio::test]
    async fn a_mid_stream_error_response_surfaces_the_sqlstate() {
        let wire = error_response_message(&[
            (b'S', "ERROR"),
            (b'V', "ERROR"),
            (b'C', "58P01"),
            (b'M', "requested WAL segment has already been removed"),
        ]);

        let mut stream = stream_over(wire.to_vec());
        let err = stream
            .next()
            .await
            .expect_err("an ErrorResponse must end the stream with an error");

        let db = err
            .as_db_error()
            .expect("a mid-stream ErrorResponse must surface as a DbError");
        assert_eq!(db.code().code(), "58P01");
        assert_eq!(
            db.message(),
            "requested WAL segment has already been removed"
        );
    }

    /// The control for the arm above: a well-formed `XLogData` frame still
    /// decodes into its payload, and a `NoticeResponse` is still skipped
    /// rather than raised.
    ///
    /// A fix that routes every non-CopyData tag through the error path, or
    /// that treats any frame it cannot turn into a `DbError` as a failure,
    /// turns this red.
    #[compio::test]
    async fn a_notice_is_skipped_and_the_next_xlog_frame_decodes() {
        let notice = {
            let mut msg = BytesMut::new();
            msg.put_u8(NOTICE_RESPONSE_TAG);
            let payload = b"Mterminating walsender\0\0";
            msg.put_u32(u32::try_from(payload.len() + 4).unwrap());
            msg.extend_from_slice(payload);
            msg
        };

        let mut body = vec![XLOG_DATA_TAG];
        body.extend_from_slice(&0x100u64.to_be_bytes());
        body.extend_from_slice(&0x200u64.to_be_bytes());
        body.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        body.extend_from_slice(b"pgoutput-payload");

        let mut wire = notice.to_vec();
        wire.push(COPY_DATA_TAG);
        wire.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);

        let mut stream = stream_over(wire);
        match stream.next().await.expect("a valid XLogData frame decodes") {
            Some(ReplicationMessage::XLogData {
                wal_start,
                wal_end,
                body,
                ..
            }) => {
                assert_eq!(wal_start, 0x100);
                assert_eq!(wal_end, 0x200);
                assert_eq!(&body[..], b"pgoutput-payload");
            }
            other => panic!("expected XLogData, got {other:?}"),
        }
        assert_eq!(stream.last_received_lsn(), 0x200);
    }

    /// Open a real, connected TCP socket pair. The returned peer is silent for
    /// as long as the caller holds it.
    ///
    /// A real socket, not a scripted one: what these tests turn on is what
    /// happens when an in-flight `io_uring` operation is dropped, and a
    /// hand-rolled stream that returns `Pending` reproduces the scheduling
    /// shape without reproducing the submitted operation underneath it.
    async fn silent_peer() -> (
        ReplicationStream<compio::net::TcpStream, compio::net::TcpStream>,
        compio::net::TcpStream,
    ) {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let accepting = compio::runtime::spawn(async move {
            listener.accept().await.expect("accept").0
        });
        let client = compio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the listener");
        let server = accepting.await.expect("accept task");

        (
            ReplicationStream {
                stream: BufStream::new(MaybeTlsStream::Raw(client)),
                lsn: LsnTracker::new(0),
                in_flight: InFlight::default(),
            },
            server,
        )
    }

    /// A read dropped while its operation is in flight must leave the stream
    /// refusing every later call.
    ///
    /// The bytes are gone: the buffer went to the kernel with the submitted
    /// read and whatever was delivered into it is discarded when the operation
    /// is cancelled. Nothing recorded that, so the stream stayed usable and
    /// the next `next()` resumed mid-frame - the framer would read a payload
    /// byte as a tag and either raise a nonsense "unexpected tag" or, worse,
    /// accept it. This is reachable from ordinary code: the WAL consumer in
    /// `zeroship-plugin-db` drives `next()` inside a `futures::select!`, which
    /// drops the losing branch's future every iteration.
    ///
    /// The fix cannot un-lose the bytes. What it can do - and what this pins -
    /// is refuse to pretend the stream is still in step.
    #[compio::test]
    async fn a_dropped_read_poisons_the_stream() {
        let (mut stream, _peer) = silent_peer().await;

        {
            let mut reading = std::pin::pin!(stream.next());
            assert!(
                futures_util::poll!(reading.as_mut()).is_pending(),
                "the peer sent nothing, so the read must still be in flight"
            );
        }

        // Bounded, because the failure being pinned is a stream that carries
        // on waiting: without the timeout an unfixed driver hangs here rather
        // than reporting.
        let again = compio::time::timeout(std::time::Duration::from_secs(2), stream.next()).await;
        let Ok(result) = again else {
            panic!("the stream went back to waiting for a frame after a dropped read");
        };
        let err = result.expect_err("a stream with a dropped read in flight must refuse to read");
        assert!(
            err.is_cancelled(),
            "the refusal must be reported as a cancellation, got: {err}"
        );
    }

    /// A feedback write dropped while its operation is in flight must leave
    /// the stream refusing every later call.
    ///
    /// `BufStream::flush` takes the encoded frame OUT of the write buffer
    /// before it awaits, and `write_all` is a loop over partial writes, so a
    /// drop can leave a fraction of a `StandbyStatusUpdate` on the wire with
    /// the rest discarded. The walsender is then reading the middle of our
    /// frame as the start of the next one. Sending a fresh, well-formed frame
    /// on top of that - which is what the driver did - cannot recover it.
    #[compio::test]
    async fn a_dropped_feedback_write_poisons_the_stream() {
        let (mut stream, _peer) = silent_peer().await;

        {
            let mut sending = std::pin::pin!(stream.send_standby_status_update(false));
            assert!(
                futures_util::poll!(sending.as_mut()).is_pending(),
                "the write is submitted, not completed, on its first poll"
            );
        }

        let err = stream
            .send_standby_status_update(false)
            .await
            .expect_err("a stream with a dropped write in flight must refuse to send");
        assert!(
            err.is_cancelled(),
            "the refusal must be reported as a cancellation, got: {err}"
        );
    }

    /// The control for both poison arms: a stream nobody cancelled keeps
    /// working across repeated reads and writes.
    ///
    /// A guard that arms on entry and never disarms, or one that treats an
    /// ordinary completed call as a cancellation, turns this red.
    #[compio::test]
    async fn an_uncancelled_stream_keeps_reading_and_writing() {
        let (mut stream, mut peer) = silent_peer().await;

        stream
            .send_standby_status_update(false)
            .await
            .expect("first feedback frame");
        stream
            .send_standby_status_update(true)
            .await
            .expect("a second feedback frame on the same stream");

        let mut keepalive = vec![COPY_DATA_TAG];
        let mut body = vec![PRIMARY_KEEPALIVE_TAG];
        body.extend_from_slice(&0x2A0u64.to_be_bytes());
        body.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        body.push(0);
        keepalive.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        keepalive.extend_from_slice(&body);
        // Two frames, so the read after a completed read is exercised too.
        let mut wire = keepalive.clone();
        wire.extend_from_slice(&keepalive);
        let compio::buf::BufResult(sent, _) =
            compio::io::AsyncWriteExt::write_all(&mut peer, wire).await;
        sent.expect("peer wrote two keepalives");

        for _ in 0..2 {
            match stream.next().await.expect("keepalive decodes") {
                Some(ReplicationMessage::PrimaryKeepalive { wal_end, .. }) => {
                    assert_eq!(wal_end, 0x2A0);
                }
                other => panic!("expected PrimaryKeepalive, got {other:?}"),
            }
        }

        stream
            .send_standby_status_update(false)
            .await
            .expect("feedback still works after reads");
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
