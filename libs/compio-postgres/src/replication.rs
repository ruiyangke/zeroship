//! Postgres streaming-replication protocol (logical decoding).
//!
//! This module implements the wire pieces that the regular query-mode
//! `Client` / `Connection` split cannot handle:
//!
//! - The `replication=database` startup parameter (see
//!   [`crate::config::Config::replication`] /
//!   [`crate::config::ReplicationMode`]).
//! - `IDENTIFY_SYSTEM` (simple-query response shape).
//! - `START_REPLICATION SLOT ... LOGICAL <lsn> (...)` - the server
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
//! the replication payload tags - see the `Message::parse` match in
//! that crate; an unknown tag errors out. The replication-mode
//! connection therefore owns the raw [`crate::buf_stream::BufStream`]
//! and runs its own framer.
//!
//! ## Why this lives in compio-postgres
//!
//! The "right place" debate is between
//!
//! 1. **plugin-db**, where the consumer's *policy* (broker fan-out,
//!    LSN tracking, watchdog, ...) already lives, and
//! 2. **compio-postgres**, where the *protocol* lives.
//!
//! Mixing protocol + policy in plugin-db means re-implementing the
//! BufStream framing on top of a public-API surface that doesn't exist
//! today. Keeping protocol here gives plugin-db (and any future
//! consumer - e.g. a CDC export job) a clean async-stream API.
//!
//! ## What this module ships in P8a.2
//!
//! - [`ReplicationMode`] / [`Config::replication`] (in [`crate::config`])
//! - [`connect_replication`] - TCP + TLS + handshake + auth +
//!   `replication=database`, returning a [`ReplicationConnection`].
//! - [`ReplicationConnection::identify_system`] - minimal,
//!   used for picking the timeline / `xlogpos` on startup.
//! - [`ReplicationConnection::start_logical_replication`] - issues
//!   `START_REPLICATION SLOT ... LOGICAL ...`, waits for
//!   `CopyBothResponse`, and returns a [`ReplicationStream`].
//! - [`ReplicationConnection::cancel_token`] and
//!   [`ReplicationStream::cancel_token`] - retain BackendKeyData and expose the
//!   same out-of-band cancellation capability as an ordinary connection.
//! - [`ReplicationStream::next`] - yields [`ReplicationMessage`]
//!   (`XLogData` / `PrimaryKeepalive`) and answers a keepalive whose
//!   `reply_requested` flag is set. The caller drives
//!   [`ReplicationStream::send_standby_status_update`] periodically for
//!   durability progress.
//! - [`pgoutput`] - pure decoder. The stream layer is *agnostic* to
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
use crate::cancel_token::CancelKey;
use crate::client::{Addr, SocketConfig};
use crate::codec::FrontendMessage;
use crate::config::{Config, ReplicationMode};
use crate::connect::{
    Endpoint, Resolver, SystemResolver, endpoints, first_encryption_for_addr, tls_server_name,
    with_connect_timeout,
};
use crate::connect_socket::connect_socket;
use crate::connect_tls::negotiate_tls;
use crate::encryption::Encryption;
use crate::escape::{escape_literal_body, quote_identifier};
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::release::ConnectionRelease;
use crate::tls::{MakeTlsConnect, ServerVerification, TlsConnect};
use crate::{CancelToken, Error, Socket};
use bytes::{BufMut, BytesMut};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use postgres_protocol::message::backend::{DataRowBody, Message};
use postgres_protocol::message::frontend;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// ---------------------------------------------------------------------------
// Wire tags
// ---------------------------------------------------------------------------
//
// Tags drawn directly from the PG 16 wire-protocol spec. We keep them
// here, NOT in `codec.rs`, because the regular Message::parse in
// postgres-protocol doesn't recognise them - including them in the
// query path would just add dead branches.

/// Backend tag: `CopyBothResponse`. Sent in response to
/// `START_REPLICATION`.
pub const COPY_BOTH_RESPONSE_TAG: u8 = b'W';
/// Backend tag: a frame inside the CopyBoth channel. Same as the
/// regular `CopyData` tag.
pub const COPY_DATA_TAG: u8 = b'd';
/// Backend tag: a `CopyDone` signal - terminates the stream.
pub const COPY_DONE_TAG: u8 = b'c';
/// Backend tag: `ErrorResponse`.
pub const ERROR_RESPONSE_TAG: u8 = b'E';
/// Backend tag: asynchronous `NotificationResponse`.
pub const NOTIFICATION_RESPONSE_TAG: u8 = b'A';
/// Backend tag: `NoticeResponse`.
pub const NOTICE_RESPONSE_TAG: u8 = b'N';
/// Backend tag: asynchronous `ParameterStatus`.
pub const PARAMETER_STATUS_TAG: u8 = b'S';

/// CopyData sub-tag: `XLogData`.
pub const XLOG_DATA_TAG: u8 = b'w';
/// CopyData sub-tag: `PrimaryKeepaliveMessage`.
pub const PRIMARY_KEEPALIVE_TAG: u8 = b'k';
/// CopyData sub-tag: `StandbyStatusUpdate` (frontend -> backend).
pub const STANDBY_STATUS_UPDATE_TAG: u8 = b'r';
/// CopyData sub-tag: `HotStandbyFeedback` (frontend -> backend, unused).
pub const HOT_STANDBY_FEEDBACK_TAG: u8 = b'h';

// ---------------------------------------------------------------------------
// connect_replication
// ---------------------------------------------------------------------------

/// Open a replication-mode connection.
///
/// Takes the same [`Config`] as [`crate::Config::connect`] and walks the same
/// endpoint list, but is NOT equivalent to it. Known differences, none of them
/// accidental: this path opens one transport per address with no `allow` /
/// `prefer` fallback, it does not honour `target_session_attrs`, and it
/// currently applies `sslmode` to Unix-socket addresses where the query path
/// follows libpq and ignores it. The returned value is a
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
    let endpoints = endpoints(config)?;

    // We need a Config with replication=database set. Most callers
    // already set it; tolerate both shapes and force-set as a defensive
    // measure if they didn't.
    let mut cfg = config.clone();
    if cfg.get_replication().is_none() {
        cfg.replication(ReplicationMode::Logical);
    }

    let mut resolver = SystemResolver;
    let mut error = None;
    for endpoint in endpoints {
        match connect_replication_host(&endpoint, &mut resolver, &mut tls, &cfg).await {
            Ok(connection) => return Ok(connection),
            Err(e) => error = Some(e),
        }
    }

    Err(error.expect("endpoints rejects an empty host list"))
}

/// Every address one configured endpoint denotes, in order, until one
/// connects.
///
/// The deadline is applied exactly as `connect::connect_host` applies it:
/// once to resolution, then AFRESH per address. See that function for why
/// per-address matches libpq, and for what bounding resolution does and does
/// not buy.
///
/// Budgeting is the ONLY axis on which these two walks are claimed to agree.
/// They deliberately differ elsewhere and a reader should not generalise:
/// the query path retries a failed TLS leg in the clear under a permissive
/// `sslmode` and this path does not, `sslmode` is handled differently over
/// Unix sockets, and `target_session_attrs` is honoured only by the query
/// path.
///
/// Before this shared shape, the replication deadline covered only the
/// socket dial (`connect_socket` took the timeout directly); TLS, startup
/// and authentication ran unbounded. Extending it to the whole attempt is
/// the change, not the per-address restart, which the socket-level timeout
/// already had.
async fn connect_replication_host<T, R>(
    endpoint: &Endpoint,
    resolver: &mut R,
    tls: &mut T,
    cfg: &Config,
) -> Result<ReplicationConnection<Socket, T::Stream>, Error>
where
    T: MakeTlsConnect<Socket>,
    R: Resolver,
{
    let timeout = cfg.get_connect_timeout().copied();

    let addrs = with_connect_timeout(
        timeout,
        endpoint.addresses(resolver, cfg.get_load_balance_hosts()),
    )
    .await?;

    let mut error = None;
    for addr in addrs {
        match with_connect_timeout(
            timeout,
            connect_replication_addr(addr, endpoint.hostname(), endpoint.port(), tls, cfg),
        )
        .await
        {
            Ok(connection) => return Ok(connection),
            Err(e) => error = Some(e),
        }
    }

    Err(error.expect("Endpoint::addresses rejects an empty address list"))
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
        cfg.get_tcp_user_timeout().copied(),
        if cfg.get_keepalives() {
            Some(&cfg.keepalive_config)
        } else {
            None
        },
        cfg.get_require_peer(),
    )
    .await?;
    // Keep an owned dup before TLS wraps the descriptor. A read timeout may
    // cancel a partially completed frame, so logical poisoning alone is not
    // enough: the peer must observe this physical session end immediately.
    let mut release = socket.release_handle();

    let server_name = tls_server_name(&addr, hostname);
    let tls_inst = tls
        .make_tls_connect(&server_name)
        .map_err(|e| Error::tls(e.into()))?;
    let has_hostname = hostname.is_some();
    let encryption = first_encryption_for_addr(&addr, cfg.get_ssl_mode());
    crate::connect_raw::validate_tls_connector_parameters(&tls_inst, encryption, cfg)?;
    let cancel_tls_policy_identity = tls_inst.cancel_policy_identity().cloned();

    // One transport per address, no reconnect: this path opens its own socket
    // rather than going through `connect::connect`, so it does not inherit the
    // `allow` / `prefer` fallback that lives there. Those two modes therefore
    // get the transport they try first and stop. A replication connection is a
    // deliberate, operator-configured thing - it is not the surface where
    // "whatever the server happens to accept" is worth the plumbing.
    // Routed through the SAME helper the query path uses, not
    // `Encryption::first_for`, because the choice depends on the ADDRESS and
    // not only on the mode. libpq: "sslmode is ignored for Unix domain socket
    // communication." A local socket has no network to eavesdrop on and no
    // host name to put in a certificate. Selecting on the mode alone made
    // `host=/path sslmode=require` open an ordinary query connection and fail
    // a replication one.
    let stream = negotiate_tls(
        socket,
        encryption,
        cfg.get_ssl_mode(),
        cfg.get_ssl_negotiation(),
        tls_inst,
        has_hostname,
    )
    .await?;
    let negotiated = stream.negotiated_encryption();
    if let Some(release) = release.as_mut() {
        crate::tls::TlsStream::configure_release(
            &stream,
            crate::tls::private::ForcePrivateApi,
            crate::tls::private::ReleaseConfig::new(release),
        );
    }

    // Run the normal startup + auth handshake and retain its BufStream. A
    // backend can send an idle-session message immediately after startup's
    // ReadyForQuery, and the handshake read may already have buffered it.
    let (stream, process_id, secret_key, parameters) = handshake_replication(stream, cfg).await?;
    let server_verification = if negotiated == Encryption::Plaintext {
        ServerVerification::None
    } else {
        ServerVerification::demanded_by(cfg.get_ssl_mode(), cfg.get_ssl_root_cert())?
    };

    let cancel_token = CancelToken {
        socket_config: Some(SocketConfig {
            addr: addr.clone(),
            hostname: hostname.map(str::to_owned),
            port,
            connect_timeout: cfg.get_connect_timeout().copied(),
            tcp_user_timeout: cfg.get_tcp_user_timeout().copied(),
            keepalive: if cfg.get_keepalives() {
                Some(cfg.keepalive_config.clone())
            } else {
                None
            },
            require_peer: cfg.get_require_peer().map(str::to_owned),
            encryption: negotiated,
            ssl_sni: cfg.get_ssl_sni(),
            ssl_cert_mode: cfg.get_ssl_cert_mode(),
            server_verification,
        }),
        encryption: negotiated,
        ssl_sni: cfg.get_ssl_sni(),
        ssl_cert_mode: cfg.get_ssl_cert_mode(),
        server_verification,
        tls_policy_identity: if negotiated == Encryption::Tls {
            cancel_tls_policy_identity
        } else {
            None
        },
        ssl_mode: cfg.get_ssl_mode(),
        ssl_negotiation: cfg.get_ssl_negotiation(),
        process_id,
        secret_key,
        pool_lease: None,
        drop_target: Some(crate::cancel_token::CancelDropTarget::Replication(
            Arc::new(AtomicBool::new(false)),
        )),
    };

    let mut stream = stream;
    stream.set_read_timeout(cfg.get_read_timeout().copied());
    // The handshake intentionally uses `DEFAULT_MAX_MESSAGE_SIZE`; apply the
    // caller's ceiling now, exactly as `connect_raw` does, so it governs the
    // data phase without making authentication unreachable. Leaving it out
    // made `Config::max_message_size` accepted and ignored on replication
    // connections in BOTH directions: a lowered ceiling still admitted 64
    // MiB, and a raised one still tore the stream down at 64 MiB when a
    // legitimate large frame arrived.
    if let Some(max) = cfg.get_max_message_size() {
        stream.set_max_message_size(max);
    }
    Ok(ReplicationConnection {
        stream,
        parameters,
        in_flight: InFlight::default(),
        release,
        cancel_token,
    })
}

/// Run the startup + auth handshake through a wrapper that hands us back the
/// buffered post-handshake stream - not a Client/Connection pair.
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
) -> Result<
    (
        BufStream<MaybeTlsStream<S, T>>,
        i32,
        Option<CancelKey>,
        HashMap<String, String>,
    ),
    Error,
>
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
    /// Standard socket paths retain a synchronous shutdown handle so a
    /// cancelled partial read cannot leave a live walsender behind.
    release: Option<ConnectionRelease>,
    cancel_token: CancelToken,
}

impl<S, T> std::fmt::Debug for ReplicationConnection<S, T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplicationConnection")
            .field("parameter_count", &self.parameters.len())
            .field("in_flight", &self.in_flight)
            .field("has_release", &self.release.is_some())
            .field("cancel_token", &self.cancel_token)
            .finish_non_exhaustive()
    }
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
struct InFlight {
    /// A call has claimed the stream and not yet given it back.
    busy: bool,
    /// The stream can never be used again, whatever happens next.
    ///
    /// Separate from `busy` because the two recover differently: a call that
    /// returns clears `busy`, and nothing clears this. A framer that has lost
    /// sync does not know where the next real frame begins, so there is no
    /// state to recover TO.
    poisoned: bool,
}

impl InFlight {
    /// Claim the stream for one I/O call, or refuse because a previous call
    /// never gave it back, or because the stream is unusable.
    fn enter(&mut self, cancel_token: &CancelToken) -> Result<(), Error> {
        if cancel_token.target_was_abandoned() || self.busy || self.poisoned {
            return Err(Error::cancelled());
        }
        self.busy = true;
        Ok(())
    }

    /// Give the stream back after a call returned under its own power.
    fn leave(&mut self) {
        self.busy = false;
    }

    /// Retire the stream permanently.
    ///
    /// For failures that leave the wire in a state no later call can make
    /// sense of - a frame header the protocol cannot express, so the framer
    /// has no idea where the next one starts. Returning the same error
    /// forever would look transient to a caller that retries.
    fn poison(&mut self) {
        self.poisoned = true;
    }
}

impl<S, T> ReplicationConnection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Server-reported parameter map captured during handshake
    /// (`server_version`, `server_encoding`, ...). Convenience for
    /// callers that need to gate on PG version.
    pub fn parameters(&self) -> &HashMap<String, String> {
        &self.parameters
    }

    /// Returns a token which can cancel an in-progress replication command or
    /// CopyBoth stream over PostgreSQL's dedicated CancelRequest connection.
    ///
    /// The cancellation result is reported on this original connection. During
    /// CopyBoth, an effective cancel surfaces SQLSTATE `57014` from
    /// [`ReplicationStream::next`] and retires the ended replication session.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel_token.clone()
    }

    /// Issue `IDENTIFY_SYSTEM` - used at start-up to discover the
    /// current WAL position when there's no prior `confirmed_flush_lsn`
    /// to resume from.
    ///
    /// Returns `{systemid, timeline, xlogpos, dbname}` per the docs.
    pub async fn identify_system(&mut self) -> Result<IdentifySystem, Error> {
        self.in_flight.enter(&self.cancel_token)?;
        self.stream.begin_read_response();
        let result = self.identify_system_inner().await;
        self.stream.finish_read_response();
        if result.as_ref().is_err_and(Error::is_read_timeout) {
            // A timeout cancels a possibly partial frame read. Retrying on the
            // same replication session would parse from an unknown boundary.
            self.in_flight.poison();
            if let Some(release) = &self.release {
                release.shutdown();
            }
        }
        self.in_flight.leave();
        result
    }

    async fn identify_system_inner(&mut self) -> Result<IdentifySystem, Error> {
        if let Err(error) = send_simple_query(&mut self.stream, "IDENTIFY_SYSTEM").await {
            // `BufStream::flush` removes the frame from its write buffer before
            // awaiting the transport. A failure can therefore leave PostgreSQL
            // holding a frontend fragment whose missing tail cannot be replayed.
            self.in_flight.poison();
            if let Some(release) = &self.release {
                release.shutdown();
            }
            return Err(error);
        }

        // IDENTIFY_SYSTEM returns: RowDescription, DataRow,
        // CommandComplete, ReadyForQuery. We use postgres-protocol's
        // parser for these - they're regular tags.
        //
        // The outcome is decided at `ReadyForQuery`, NOT at the frame that
        // produced it. Two reasons, and each was its own defect:
        //
        // * `ReadyForQuery` closes a simple-query response. Returning the
        //   moment an `ErrorResponse` arrived left it in the read buffer, and
        //   the NEXT command on this connection read that stale frame as its
        //   own reply - a second `IDENTIFY_SYSTEM` broke out of this loop
        //   before the server had answered it at all.
        // * There is no seeded "empty identity" to fall out of the loop with.
        //   This used to start from `systemid = String::new()`, `timeline = 0`
        //   and `xlogpos = String::new()` and return them when no `DataRow`
        //   arrived - exactly the three sentinels `parse_identify_system_row`
        //   refuses inside a row, reported as `Ok`. Combined with the stale
        //   frame above, a retry after a refused `IDENTIFY_SYSTEM` returned a
        //   successful empty identity for a command nothing had answered.
        //
        // The trade this makes is explicit: a peer that sends an
        // `ErrorResponse` and then goes silent is now waited on rather than
        // reported immediately. That surface is not new - the same is already
        // true of a peer that sends only `NoticeResponse` and stops - and
        // `Config::read_timeout` is what bounds it, which is why the arm below
        // that CANNOT rely on a `ReadyForQuery` arriving does not wait at all.
        let mut identity: Option<IdentifySystem> = None;
        let mut failure: Option<Error> = None;

        loop {
            let msg = match read_one_message(&mut self.stream).await {
                Ok(msg) => msg,
                Err(error) => {
                    // Retiring the session is this arm's OWN job, not the
                    // caller wrapper's. `identify_system` poisons when the
                    // error it gets back `is_read_timeout`, and the error
                    // returned below may be the SERVER's instead - so keying
                    // the retirement to it would leave a session whose
                    // response ended at an unknown frame boundary reusable.
                    self.in_flight.poison();
                    if let Some(release) = &self.release {
                        release.shutdown();
                    }
                    // A stashed diagnostic outranks the transport error. This
                    // loop reports an `ErrorResponse` only after the
                    // `ReadyForQuery` that closes the response, and PostgreSQL
                    // sends no `ReadyForQuery` after a FATAL - it writes the
                    // `ErrorResponse` and hangs up. The read that fails IS how
                    // that response ends, so propagating the read error
                    // replaced `57P01 terminating connection due to
                    // administrator command` with `connection closed by
                    // server`, on every admin-terminated, shutting-down or
                    // idle-timed-out walsender.
                    return Err(failure.unwrap_or(error));
                }
            };
            match msg {
                Message::RowDescription(_) => {}
                Message::DataRow(row) => match parse_identify_system_row(&row) {
                    // `Message::parse` consumed the whole row, so a rejected
                    // one leaves the session in step: carry the reason to the
                    // end of the phase rather than abandoning it here.
                    Ok(parsed) => identity = Some(parsed),
                    Err(error) => failure = failure.or(Some(error)),
                },
                Message::CommandComplete(_) => {}
                Message::ReadyForQuery(_) => break,
                Message::ErrorResponse(body) => failure = failure.or(Some(Error::db(body))),
                // Asynchronous, and legal at any point in any response - the
                // backend interleaves them whenever a reported GUC changes or a
                // notification fires. `connect_raw.rs` and `connection.rs` both
                // fold them out of the query path for that reason; only
                // `NoticeResponse` was skipped here, so a walsender reporting a
                // changed GUC mid-response failed the command below.
                Message::NoticeResponse(_)
                | Message::ParameterStatus(_)
                | Message::NotificationResponse(_) => {}
                _ => {
                    // NOT carried to `ReadyForQuery` like the `ErrorResponse`
                    // and rejected-row arms above, and the difference is a
                    // guarantee: PostgreSQL always closes a simple-query
                    // response with `ReadyForQuery`, whatever else it sent, so
                    // draining to it terminates. A message the driver does not
                    // expect in this phase says the peer is not running that
                    // state machine, and waiting for a frame it may never send
                    // is a hang the caller cannot break. Retire the session
                    // instead, so a caller cannot reuse a connection whose
                    // response was abandoned part-way.
                    self.in_flight.poison();
                    return Err(failure.unwrap_or_else(Error::unexpected_message));
                }
            }
        }

        if let Some(error) = failure {
            return Err(error);
        }
        // A conforming server answers `IDENTIFY_SYSTEM` with exactly one row.
        // No row is not an empty identity - see the note above.
        identity.ok_or_else(identify_system_returned_no_row)
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
    /// `confirmed_flush_lsn` - the simplest correct choice.
    pub async fn start_logical_replication(
        mut self,
        opts: StartReplicationOptions<'_>,
    ) -> Result<ReplicationStream<S, T>, Error> {
        // Taken and never given back: this call consumes the connection, so a
        // future dropped part-way through takes the stream with it and there
        // is nothing left to refuse. The claim still has to happen, because a
        // connection whose `identify_system` was dropped must not go on to
        // start streaming on a stream that is mid-frame.
        // Refused HERE, before a command goes out, because the two parsers
        // involved disagree. The replication grammar accepts an LSN half wider
        // than 32 bits -- `0/100000000` reaches the slot lookup rather than
        // failing on the LSN -- while `'0/100000000'::pg_lsn` is refused as
        // invalid input. So the server can accept a position this driver's
        // u32-per-half representation cannot hold. This used to be
        // `parse_lsn(..).unwrap_or(0)` at the point the stream was built, and
        // 0 is not a neutral default for an LSN: it is the start of WAL. The
        // server would stream from wherever it read the oversized value while
        // the tracker reported 0, so every standby status update acknowledged
        // a position the stream had never reached, silently.
        let start_lsn = parse_lsn(opts.start_lsn).ok_or_else(|| {
            Error::config(
                format!(
                    "start_lsn {:?} is not an LSN this driver can represent; each half must fit \
                     in 32 bits, as `pg_lsn` requires",
                    opts.start_lsn
                )
                .into(),
            )
        })?;

        // An option the running `proto_version` cannot carry is refused here,
        // not sent. The server does reject it - `requested proto_version=1
        // does not support streaming, need 2 or higher` - but only after the
        // command goes out, and these floors are properties of the message
        // formats THIS decoder implements, so they are ours to state.
        //
        if opts.proto_version == 0 {
            return Err(Error::config(
                "proto_version 0 is not supported; use proto_version 1 or higher".into(),
            ));
        }
        if opts.proto_version > 4 {
            return Err(Error::config(
                format!(
                    "proto_version {} is not supported; use proto_version 4 or lower",
                    opts.proto_version
                )
                .into(),
            ));
        }
        for (needed, what) in [
            (opts.streaming.minimum_proto_version(), "streaming"),
            (opts.two_phase.then_some(3), "two_phase"),
        ] {
            if let Some(needed) = needed
                && opts.proto_version < needed
            {
                return Err(Error::config(
                    format!(
                        "{what} needs proto_version {needed} or higher, but {} was requested",
                        opts.proto_version
                    )
                    .into(),
                ));
            }
        }

        self.in_flight.enter(&self.cancel_token)?;
        self.stream.begin_read_response();

        // Both names below are IDENTIFIERS the server parses, not opaque
        // strings it hands to pgoutput verbatim, and neither can travel as a
        // Bind parameter: this is a simple query on a walsender, and
        // `START_REPLICATION SLOT $1` is not a thing. So they are quoted
        // here, through the same helper the savepoint statements use.
        //
        // `publication_names` needs BOTH escapes, applied in this order: each
        // name is quoted as an identifier, because the walsender re-parses
        // the option value with `SplitIdentifierString` (an unquoted element
        // is case-folded and a comma ends it); then the joined list is
        // escaped as the body of a single-quoted literal, because that is
        // what encloses it on the wire. Quoting alone would still let a name
        // containing `'` close the literal early.
        let publications = opts
            .publication_names
            .iter()
            .map(|name| quote_identifier(name))
            .collect::<Vec<_>>()
            .join(",");

        let mut cmd = String::with_capacity(128);
        cmd.push_str("START_REPLICATION SLOT ");
        cmd.push_str(&quote_identifier(opts.slot_name));
        cmd.push_str(" LOGICAL ");
        cmd.push_str(opts.start_lsn);
        cmd.push_str(" (\"proto_version\" '");
        cmd.push_str(&opts.proto_version.to_string());
        cmd.push_str("', \"publication_names\" '");
        cmd.push_str(&escape_literal_body(&publications));
        cmd.push('\'');

        // Only options the caller actually asked for are sent. pgoutput
        // REFUSES an option it does not know - `unrecognized pgoutput option:
        // nonsense` - so naming every option unconditionally would make this
        // command fail against any server whose pgoutput predates the newest
        // one here, for callers who never wanted it. Omitting an option asks
        // for the server's default, which is what these defaults are.
        if opts.binary {
            cmd.push_str(", \"binary\" 'true'");
        }
        if opts.messages {
            cmd.push_str(", \"messages\" 'true'");
        }
        if opts.origin == OriginFilter::None {
            cmd.push_str(", \"origin\" 'none'");
        }
        if opts.streaming != Streaming::Off {
            cmd.push_str(", \"streaming\" '");
            cmd.push_str(opts.streaming.option_value());
            cmd.push('\'');
        }
        if opts.two_phase {
            cmd.push_str(", \"two_phase\" 'true'");
        }
        cmd.push(')');

        send_simple_query(&mut self.stream, &cmd).await?;

        // Drain server messages until we see CopyBothResponse, then
        // transition into the streaming framer.
        loop {
            let header = read_header(&mut self.stream).await?;
            match header.tag {
                COPY_BOTH_RESPONSE_TAG => {
                    let body = self.stream.buf().split_to(header.body_len()).freeze();
                    let copy_response = crate::copy_format::CopyResponse::from_wire(&body)?;
                    // A started streaming frame arms its own completion budget
                    // in `next`; an idle stream must not inherit this command's
                    // old deadline.
                    self.stream.finish_read_response();
                    return Ok(ReplicationStream {
                        stream: self.stream,
                        lsn: LsnTracker::new(start_lsn),
                        copy_response,
                        in_flight: InFlight::default(),
                        release: self.release,
                        cancel_token: self.cancel_token,
                    });
                }
                ERROR_RESPONSE_TAG => {
                    let bytes = self.stream.buf().split_to(header.body_len()).freeze();
                    return Err(error_from_error_response_frame(&header, &bytes));
                }
                NOTICE_RESPONSE_TAG | NOTIFICATION_RESPONSE_TAG | PARAMETER_STATUS_TAG => {
                    // These messages are asynchronous and do not answer
                    // START_REPLICATION. Drop their payloads and keep waiting
                    // for CopyBothResponse or ErrorResponse.
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
    /// pgoutput protocol version. This decoder supports versions 1 through 4:
    /// `2` adds streaming, `3` adds two-phase transactions, and `4` adds
    /// parallel streaming.
    ///
    /// A protocol version is not the same thing as a SERVER version, and the
    /// server is the one that refuses. Version 4 needs PostgreSQL 16 or newer:
    /// measured 2026-08-25, `streaming 'parallel'` on 15.19 answers
    /// `streaming requires a Boolean value` - its pgoutput takes only a
    /// boolean there - while 16.14 understands the word and objects instead
    /// that the proto version is too low. Nothing here downgrades an option
    /// the server cannot take; the request goes as written and the server's
    /// refusal reaches the caller unchanged.
    pub proto_version: u32,
    /// The publications to stream, one name per element, unquoted and
    /// unescaped as the user wrote them. This driver quotes each one.
    ///
    /// A LIST, not a pre-joined string, because a comma is a legal
    /// character in a publication name and a joined string cannot say
    /// whether one separates two names or belongs to one. It used to be
    /// `&str`, and a publication named `eu,us` was streamed as the two
    /// publications `eu` and `us` - neither of which existed.
    pub publication_names: &'a [&'a str],
    /// Send column values in each type's BINARY format instead of text.
    ///
    /// Values then arrive as [`pgoutput::TupleColumn::Binary`]. Until this
    /// option existed that variant was unreachable: the decoder had an arm
    /// for the `b` tuple kind, and no request this driver could make would
    /// ever make a server send one.
    pub binary: bool,
    /// Deliver `pg_logical_emit_message` payloads as
    /// [`pgoutput::PgOutputMessage::Message`]. Off, the server omits them
    /// and the decoder's `M` arm never runs.
    pub messages: bool,
    /// Which changes to send, by replication origin.
    pub origin: OriginFilter,
    /// Deliver a large transaction in chunks as it happens, rather than
    /// buffering it on the server until commit.
    ///
    /// This REPLACES `Begin`/`Commit` with
    /// [`pgoutput::PgOutputMessage::StreamStart`] / `StreamStop` /
    /// `StreamCommit`, measured against 16.14: the same 4000-row transaction
    /// decodes as `B:1 C:1 I:4000 R:1` without it and `S:21 E:21 c:1 I:4000
    /// R:1` with it. A consumer that only handles `Commit` therefore sees a
    /// transaction that never ends.
    ///
    /// Inside a chunk, transactional messages such as `Insert` carry
    /// `xid: Some(...)`. That value can be a SAVEPOINT's subtransaction xid,
    /// not just the top-level xid in `StreamStart`.
    pub streaming: Streaming,
    /// Deliver `PREPARE TRANSACTION` as its own message rather than waiting
    /// for `COMMIT PREPARED`. This enables two-phase decoding for the slot;
    /// creating the slot with `TWO_PHASE` enables it ahead of time instead.
    pub two_phase: bool,
}

impl Default for StartReplicationOptions<'_> {
    fn default() -> Self {
        Self {
            slot_name: "",
            start_lsn: "0/0",
            proto_version: 1,
            publication_names: &[],
            binary: false,
            messages: false,
            origin: OriginFilter::Any,
            streaming: Streaming::Off,
            two_phase: false,
        }
    }
}

/// Whether the server may send a large transaction before it commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Streaming {
    /// `off` - buffer each transaction on the server until commit.
    #[default]
    Off,
    /// `on` - stream chunks as they spill past `logical_decoding_work_mem`.
    /// Needs `proto_version` 2 or higher.
    On,
    /// `parallel` - stream, and mark the chunks so a subscriber can apply
    /// them with parallel workers. Needs `proto_version` 4 or higher; the
    /// server refuses it below that with `does not support parallel
    /// streaming, need 4 or higher`.
    Parallel,
}

impl Streaming {
    /// The lowest `proto_version` that carries this setting, or `None` when
    /// it imposes no floor.
    const fn minimum_proto_version(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::On => Some(2),
            Self::Parallel => Some(4),
        }
    }

    const fn option_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Parallel => "parallel",
        }
    }
}

/// Whether to receive changes that arrived from another replication origin.
///
/// The server's default is [`OriginFilter::Any`]; a bidirectional setup sets
/// [`OriginFilter::None`] so a change this node received from a peer is not
/// echoed straight back to it.
///
/// REQUIRES PostgreSQL 16 OR NEWER. The `origin` pgoutput option does not
/// exist before it: measured 2026-08-25, 15.19 answers `unrecognized pgoutput
/// option: origin` and the stream never starts. Setting this against an older
/// server is refused by the server, not quietly ignored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OriginFilter {
    /// `any` - every change, whatever origin it came from.
    #[default]
    Any,
    /// `none` - only changes with no replication origin, i.e. written
    /// locally rather than replayed from a peer.
    None,
}

// ---------------------------------------------------------------------------
// ReplicationStream
// ---------------------------------------------------------------------------

/// The streaming half of a logical-replication connection.
///
/// Yields [`ReplicationMessage`]s (XLogData / PrimaryKeepalive) via
/// [`next`](Self::next). A PrimaryKeepalive with `reply_requested=1` is
/// answered before it is yielded. The caller also drives
/// [`send_standby_status_update`](Self::send_standby_status_update) on a
/// schedule, typically after durable commit processing, so the upstream
/// slot's `confirmed_flush_lsn` advances and WAL retention stays bounded.
///
/// Note: this struct intentionally does NOT decode pgoutput payloads.
/// The XLogData's body is handed to the caller verbatim; the caller
/// runs [`pgoutput::decode`] when streaming is off or keeps one
/// [`pgoutput::Decoder`] for an ordered streaming conversation. The split
/// keeps the wire layer agnostic to the logical-decoding plugin.
pub struct ReplicationStream<S, T> {
    stream: BufStream<MaybeTlsStream<S, T>>,
    /// The two StandbyStatusUpdate positions (received vs flushed).
    lsn: LsnTracker,
    copy_response: crate::copy_format::CopyResponse,
    /// See [`InFlight`]. Neither [`ReplicationStream::next`] nor
    /// [`ReplicationStream::send_standby_status_update`] is cancel-safe.
    in_flight: InFlight,
    release: Option<ConnectionRelease>,
    cancel_token: CancelToken,
}

impl<S, T> std::fmt::Debug for ReplicationStream<S, T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplicationStream")
            .field("lsn", &self.lsn)
            .field("copy_response", &self.copy_response)
            .field("in_flight", &self.in_flight)
            .field("has_release", &self.release.is_some())
            .field("cancel_token", &self.cancel_token)
            .finish_non_exhaustive()
    }
}

/// Tracks the two distinct LSN positions a logical-replication client
/// reports back to the walsender in a `StandbyStatusUpdate`:
///
/// - `received` - the highest LSN we've *seen on the wire* (the
///   `wal_end` of an XLogData / PrimaryKeepalive). Reported as
///   `write_lsn`.
/// - `processed` - the highest LSN the caller has *durably handled*
///   (advanced via [`ReplicationStream::advance_lsn`]). Reported as both
///   `flush_lsn` and `apply_lsn`.
///
/// Keeping them separate matters: `flush_lsn` is a *durability promise*
/// - Postgres recycles WAL and advances the slot's `confirmed_flush`
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
    /// Advances ONLY the flush position. Monotonic, but never past the
    /// received position: `PostgreSQL` trusts `flush_lsn` enough to recycle WAL,
    /// and a flush beyond `write_lsn` is a protocol state this stream cannot
    /// truthfully report.
    const fn advance_processed(&mut self, lsn: u64) {
        let lsn = if lsn > self.received {
            self.received
        } else {
            lsn
        };
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
        /// Starting WAL position reported for this message.
        wal_start: u64,
        /// Current end of WAL on the server when this message was sent.
        wal_end: u64,
        /// Server clock in microseconds since the PG epoch
        /// (2000-01-01 00:00:00 UTC).
        timestamp: i64,
        /// pgoutput frame bytes.
        body: bytes::Bytes,
    },
    /// Periodic keepalive - the server's current `wal_end`, plus whether it
    /// asked for an immediate StandbyStatusUpdate. When this variant is
    /// yielded, that requested update has already been sent.
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
    /// Returns a token which sends an out-of-band CancelRequest for this
    /// CopyBoth session. An effective cancel is returned as SQLSTATE `57014`
    /// by [`next`](Self::next), after which the stream is retired.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel_token.clone()
    }

    /// The overall format selected by PostgreSQL for the CopyBoth stream.
    pub fn format(&self) -> crate::CopyFormat {
        self.copy_response.format()
    }

    /// The format PostgreSQL selected for each CopyBoth column.
    pub fn column_formats(&self) -> &[crate::CopyFormat] {
        self.copy_response.column_formats()
    }

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
    ///
    /// A configured socket-read timeout is disarmed while no CopyBoth frame
    /// has begun. Once the first byte arrives, it bounds completion of that
    /// frame; indefinite WAL silence at a frame boundary remains healthy.
    pub async fn next(&mut self) -> Result<Option<ReplicationMessage>, Error> {
        self.in_flight.enter(&self.cancel_token)?;
        let result = self.next_inner().await;
        if result.as_ref().is_err_and(Error::is_read_timeout) {
            self.in_flight.poison();
            if let Some(release) = &self.release {
                release.shutdown();
            }
        }
        self.in_flight.leave();
        result
    }

    async fn next_inner(&mut self) -> Result<Option<ReplicationMessage>, Error> {
        loop {
            // A walsender owes no bytes at a frame boundary: an unchanged
            // primary can remain quiet indefinitely. Wait for byte one with
            // no response obligation, then bound only completion of the frame
            // that byte started. `read_header` fills the entire declared body,
            // so finishing immediately after it returns also disarms the wait
            // after a NoticeResponse that this loop skips.
            let frame = match self.stream.fill(1).await {
                Ok(()) => {
                    self.stream.begin_read_response();
                    let result = read_header(&mut self.stream).await;
                    self.stream.finish_read_response();
                    result
                }
                Err(error) => Err(error),
            };

            // Any failure while acquiring or completing the next frame is
            // terminal. Before byte one it means the socket itself failed;
            // after byte one the next frame boundary may already be lost.
            //
            // The test is ALIGNMENT, not where the error came from.
            //
            // Poison whenever the wire is left somewhere the next call cannot
            // read a header from. That is true here - `read_header`'s floor and
            // ceiling checks return before consuming the five bytes, so the
            // next call re-reads them for the identical error forever, which a
            // caller that retries cannot tell from something transient. It is
            // ALSO true of the unhandled-tag arm below, which returns with a
            // body it did not consume; an earlier version of this comment said
            // everything below this point was well-framed, and that arm is the
            // counterexample sitting in the same `match`.
            //
            // A server-sent `ErrorResponse` is frame-aligned but not reusable:
            // it ends CopyBoth protocol state. PostgreSQL may follow it with
            // ReadyForQuery, but this API cannot send Sync, return to a
            // ReplicationConnection, or interpret ordinary-query frames. Its
            // arm therefore preserves the server error and retires the stream.
            let header = match frame {
                Ok(header) => header,
                Err(e) => {
                    // `next` owns the timeout path so logical poisoning and
                    // physical shutdown cannot drift apart. Other framing I/O
                    // failures still retire the stream here.
                    if !e.is_read_timeout() {
                        self.in_flight.poison();
                    }
                    return Err(e);
                }
            };
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
                            if body.len() != 18 {
                                return Err(Error::io(std::io::Error::other(format!(
                                    "PrimaryKeepalive must be exactly 18 bytes, but the server sent {}",
                                    body.len()
                                ))));
                            }
                            let wal_end = u64::from_be_bytes([
                                body[1], body[2], body[3], body[4], body[5], body[6], body[7],
                                body[8],
                            ]);
                            let timestamp = i64::from_be_bytes([
                                body[9], body[10], body[11], body[12], body[13], body[14],
                                body[15], body[16],
                            ]);
                            let reply_requested = match body[17] {
                                0 => false,
                                1 => true,
                                other => {
                                    return Err(Error::io(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!(
                                            "PrimaryKeepalive reply_requested is 0x{other:02x}; expected 0 or 1"
                                        ),
                                    )));
                                }
                            };
                            self.lsn.observe_received(wal_end);
                            if reply_requested {
                                if let Err(error) =
                                    self.send_standby_status_update_inner(false).await
                                {
                                    // The response may have been written only
                                    // partially, so no later frontend frame can
                                    // be aligned safely after this failure.
                                    self.in_flight.poison();
                                    if let Some(release) = &self.release {
                                        release.shutdown();
                                    }
                                    return Err(error);
                                }
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
                    let body_len = header.body_len();
                    let _ = self.stream.buf().split_to(body_len).freeze();
                    if body_len != 0 {
                        self.in_flight.poison();
                        if let Some(release) = &self.release {
                            release.shutdown();
                        }
                        return Err(Error::io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "CopyDone must have an empty body, but the server sent {body_len} bytes"
                            ),
                        )));
                    }

                    // CopyBoth is bidirectional: backend CopyDone closes only
                    // PostgreSQL's sending direction. Acknowledge it, then
                    // consume the simple-query completion before reporting a
                    // clean end. Executor cleanup can still fail in that
                    // interval, and returning here used to discard that
                    // ErrorResponse as well as leave the server waiting for
                    // our half-close.
                    let result = self.finish_copy_both().await;
                    self.in_flight.poison();
                    if let Some(release) = &self.release {
                        release.shutdown();
                    }
                    return result.map(|()| None);
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
                    let error = error_from_error_response_frame(&header, &bytes);
                    self.in_flight.poison();
                    if let Some(release) = &self.release {
                        release.shutdown();
                    }
                    return Err(error);
                }
                NOTICE_RESPONSE_TAG | PARAMETER_STATUS_TAG | NOTIFICATION_RESPONSE_TAG => {
                    let _ = self.stream.buf().split_to(header.body_len()).freeze();
                }
                other => {
                    // POISON. `read_header` has taken the five header bytes,
                    // and an unknown tag means the length that came with them
                    // was never validated against a shape we understand - so
                    // the body cannot be skipped on trust either. Every arm
                    // above consumes `header.body_len()` because it knows what
                    // the frame is; here we do not.
                    //
                    // Without this the next call reads its length field out of
                    // this frame's unconsumed body, and a framer reading
                    // payload as a header can synthesise an XLogData whose
                    // `wal_end` becomes a flush position the server acts on.
                    self.in_flight.poison();
                    return Err(Error::io(std::io::Error::other(format!(
                        "unexpected tag in replication stream: 0x{other:02x}"
                    ))));
                }
            }
        }
    }

    /// Acknowledge backend CopyDone and consume the ordinary response which
    /// finishes the START_REPLICATION simple-query phase.
    async fn finish_copy_both(&mut self) -> Result<(), Error> {
        let mut bytes = BytesMut::new();
        frontend::copy_done(&mut bytes);
        crate::codec::write_frontend(&mut self.stream, FrontendMessage::Raw(bytes.freeze()))?;
        if let Err(error) = self.stream.flush().await {
            return Err(take_buffered_server_error(&mut self.stream).unwrap_or(error));
        }

        // PostgreSQL owes a terminal CommandComplete/ErrorResponse plus
        // ReadyForQuery only after the frontend half-close reaches it.
        self.stream.begin_read_response();
        let result = self.drain_copy_both_completion().await;
        self.stream.finish_read_response();
        result
    }

    async fn drain_copy_both_completion(&mut self) -> Result<(), Error> {
        let mut failure: Option<Error> = None;
        let mut confirmation_failure: Option<Error> = None;
        let confirmations = ["COPY 0", "START_REPLICATION"];
        let mut next_confirmation = 0usize;

        loop {
            let message = match read_one_message(&mut self.stream).await {
                Ok(message) => message,
                // PostgreSQL sends no ReadyForQuery after FATAL. In that case
                // EOF closes the response, but the ErrorResponse already read
                // from the wire remains the useful diagnosis.
                Err(error) => return Err(failure.unwrap_or(error)),
            };

            match message {
                Message::CommandComplete(body)
                    if failure.is_none() && confirmation_failure.is_none() =>
                {
                    confirmation_failure = match confirmations.get(next_confirmation) {
                        Some(expected) => match body.tag() {
                            Ok(tag) if tag == *expected => {
                                next_confirmation += 1;
                                None
                            }
                            Ok(_) => Some(Error::unexpected_message()),
                            Err(error) => Some(Error::parse(error)),
                        },
                        None => Some(Error::unexpected_message()),
                    };
                }
                Message::CommandComplete(_) => {}
                Message::ReadyForQuery(_) => {
                    if let Some(error) = failure.or(confirmation_failure) {
                        return Err(error);
                    }
                    return if next_confirmation == confirmations.len() {
                        Ok(())
                    } else {
                        Err(Error::unexpected_message())
                    };
                }
                Message::ErrorResponse(body) => failure = failure.or(Some(Error::db(body))),
                Message::NoticeResponse(_)
                | Message::ParameterStatus(_)
                | Message::NotificationResponse(_) => {}
                _ => {
                    return Err(failure.unwrap_or_else(Error::unexpected_message));
                }
            }
        }
    }

    /// Highest WAL LSN seen on the wire so far (advances as the
    /// stream yields XLogData / PrimaryKeepalive).
    pub fn last_received_lsn(&self) -> u64 {
        self.lsn.received
    }

    /// Highest WAL LSN the caller has confirmed it processed -
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
    /// mere receipt - a concern that lives in the consumer
    /// (`wal_consumer.rs`), intentionally out of scope of this driver.
    /// A value above [`last_received_lsn`](Self::last_received_lsn) is capped
    /// there: `flush_lsn` and `apply_lsn` cannot truthfully exceed the
    /// `write_lsn` this stream reports, and `PostgreSQL` uses an overreported
    /// flush position to advance `confirmed_flush_lsn` and recycle WAL.
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
    /// immediate PrimaryKeepalive - typically left `false`.
    ///
    /// NOT CANCEL-SAFE. Dropping this future before it resolves can leave a
    /// fraction of the frame on the wire, which no later frame can repair;
    /// every later call on the stream then fails. See [`InFlight`].
    pub async fn send_standby_status_update(&mut self, reply_requested: bool) -> Result<(), Error> {
        self.in_flight.enter(&self.cancel_token)?;
        let result = self.send_standby_status_update_inner(reply_requested).await;
        if result.is_err() {
            // ANY write failure retires the stream, not just a cancelled one.
            // `BufStream::flush` takes the frame out of the write buffer before
            // it awaits, so a `write_all` that wrote part of the frame and then
            // failed has discarded the rest: the peer holds a fragment and the
            // remainder exists nowhere. Sending the next update would append a
            // whole frame after it and leave the walsender parsing our frame's
            // middle as a frame's start.
            //
            // Not conditioned on the error kind, because nothing here can tell
            // a failure that wrote nothing from one that wrote half. On a dead
            // socket this costs nothing - every later call fails regardless -
            // and it is the only correct answer in the case that matters. This
            // mirrors `next`, which already retires the stream on framing I/O
            // failures.
            self.in_flight.poison();
            if let Some(release) = &self.release {
                release.shutdown();
            }
        }
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
        if let Err(error) = self.stream.flush().await {
            return Err(take_buffered_server_error(&mut self.stream).unwrap_or(error));
        }
        Ok(())
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
/// IDENTIFY_SYSTEM (DataRow / CommandComplete / ReadyForQuery - all
/// regular tags Message::parse already knows).
async fn read_one_message<S>(stream: &mut BufStream<S>) -> Result<Message, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        // Validate the DECLARED length before the parser is allowed to act on
        // it. When the body is short, `Message::parse` does
        // `buf.reserve(total_len - buf.len())` before returning `None`
        // (postgres-protocol 0.6.12, backend.rs:133), so `D ff ff ff ff` asks
        // the allocator for about 4 GiB from a five-byte frame.
        //
        // `fill` below does NOT bound that, which is the part worth stating
        // because its guard looks like it would: `fill` checks the bytes the
        // CALLER requests, and this loop requests `buf.len() + 1`. So the
        // check passes on a handful of bytes while the reservation has already
        // happened, and the configured ceiling governed nothing on this path.
        //
        // Peeking is O(1) and returns `None` until five bytes are buffered, at
        // which point `parse` would return `None` too and the `fill` below
        // makes progress. This mirrors what `read_header` already does on the
        // streaming path.
        if let Some(length) = stream.peek_u32_be(1) {
            stream.validate_length(length)?;
        }
        if let Some(m) = Message::parse(stream.buf()).map_err(Error::io)? {
            return Ok(m);
        }
        let need = stream.buf().len() + 1;
        stream.fill(need).await?;
    }
}

/// Consume a complete ErrorResponse already over-read behind the response
/// which made the caller attempt a frontend write, skipping only complete
/// asynchronous messages before it.
///
/// This deliberately performs no socket read. A failed write retires the
/// session, and waiting for bytes which are not already buffered could hang on
/// a one-way transport failure. The common coalesced-read case still gets the
/// server diagnosis which is already locally available.
fn take_buffered_server_error<S>(stream: &mut BufStream<S>) -> Option<Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let length = stream.peek_u32_be(1)?;
        if length < 4 || stream.validate_length(length).is_err() {
            return None;
        }
        let total_len = usize::try_from(length).ok()?.checked_add(1)?;
        if stream.buf().len() < total_len {
            return None;
        }

        match stream.buf()[0] {
            ERROR_RESPONSE_TAG => {
                let frame = stream.buf().split_to(total_len);
                return Some(error_from_error_response_body(frame));
            }
            NOTICE_RESPONSE_TAG | NOTIFICATION_RESPONSE_TAG | PARAMETER_STATUS_TAG => {
                let _ = stream.buf().split_to(total_len);
            }
            _ => return None,
        }
    }
}

/// Send a simple-query (`Q`) frontend message - used to issue both
/// `IDENTIFY_SYSTEM` and `START_REPLICATION`. The walsender accepts
/// the simple-query path for the replication command grammar.
async fn send_simple_query<S>(stream: &mut BufStream<S>, query: &str) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    frontend::query(query, &mut buf).map_err(Error::encode)?;
    crate::codec::write_frontend(stream, FrontendMessage::Raw(buf.freeze()))?;
    if let Err(error) = stream.flush().await {
        return Err(take_buffered_server_error(stream).unwrap_or(error));
    }
    Ok(())
}

/// Encode a `StandbyStatusUpdate` frame into the stream's write
/// buffer.
///
/// Wire layout (frontend -> backend, inside CopyData):
///
/// ```text
///   'd' tag, length, [
///       'r' sub-tag,
///       i64 write_lsn,
///       i64 flush_lsn,
///       i64 apply_lsn,
///       i64 timestamp_us_from_2000,
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
    // Body length = sub-tag(1) + 4xi64(32) + reply(1) = 34 bytes
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
/// severity, and message survive as a [`crate::error::DbError`] - the
/// same shape the `IDENTIFY_SYSTEM` loop produces via
/// `Message::ErrorResponse(body) => Error::db(body)`. A byte count alone
/// (the former behaviour) made `START_REPLICATION` failures
/// undebuggable. If the framer can't parse the bytes, fall back to a
/// parse error rather than silently dropping the failure.
fn error_from_error_response_body(mut body: BytesMut) -> Error {
    match Message::parse(&mut body) {
        Ok(Some(Message::ErrorResponse(b))) => Error::db(b),
        // UNREACHABLE from both call sites, and the arm stays only because the
        // match must be exhaustive. `read_header` rejects `length < 4` and
        // fills the whole body before returning, so each caller hands
        // `error_from_error_response_frame` a payload whose length equals the
        // declared one; it rebuilds `[E][length][payload]` exactly. Then
        // `Message::parse` returns `Ok(None)` only when the buffer is SHORT
        // and `Err` only when `len < 4`, and its `ERROR_RESPONSE_TAG` arm is
        // lazy (`read_all`, no field validation), so every payload parses.
        //
        // The text used to name START_REPLICATION, which was wrong for the
        // mid-stream caller (`next_inner`) even if it could fire. Naming no
        // phase is the honest version: nothing here knows which one it is.
        _ => Error::parse(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed ErrorResponse",
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

    // Refused rather than defaulted. Every one of these used to fall back to a
    // plausible-looking value -- `""` for the two strings, `0` for the
    // timeline -- and none of those is neutral. `systemid` is the CLUSTER
    // identity, which callers compare to notice they have been failed over
    // onto a different cluster; two empty strings compare EQUAL, so the check
    // passes silently in exactly the case it exists to catch. PostgreSQL
    // numbers timelines from 1, so `0` is not a timeline at all, and an
    // unparseable one became `0` as well.
    //
    // A conforming server sends all three non-NULL, so this only fires for a
    // broken or hostile peer -- which is the threat model the rest of this
    // module already works in. `dbname` stays optional because it is genuinely
    // NULL on a replication connection that is not database-specific.
    let required = |index: usize, name: &'static str| -> Result<&str, Error> {
        fields
            .get(index)
            .and_then(|field| *field)
            .ok_or_else(|| missing_identify_field(name))
    };

    let timeline = required(1, "timeline")?;
    Ok(IdentifySystem {
        systemid: required(0, "systemid")?.to_string(),
        timeline: timeline
            .parse()
            .map_err(|_| missing_identify_field("timeline"))?,
        xlogpos: required(2, "xlogpos")?.to_string(),
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

/// `IDENTIFY_SYSTEM` completed its response without returning the row it is
/// defined to return.
///
/// Refused rather than answered with an empty identity, for the same reason
/// [`missing_identify_field`] refuses one inside a row: `systemid` is the
/// CLUSTER identity a caller compares to notice it has been failed over, and
/// two empty strings compare EQUAL.
fn identify_system_returned_no_row() -> Error {
    Error::parse(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "IDENTIFY_SYSTEM completed without returning a row",
    ))
}

/// A field `IDENTIFY_SYSTEM` must supply was absent, NULL, or unreadable.
///
/// Named so the caller learns WHICH field, because the three that are required
/// mean quite different things and a caller cannot tell them apart from a
/// generic parse failure.
fn missing_identify_field(name: &str) -> Error {
    Error::parse(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("IDENTIFY_SYSTEM did not return a usable {name}"),
    ))
}

/// Parse a Postgres text LSN like `"0/16B3750"` into a `u64`.
///
/// Returns `None` on malformed input rather than erroring - the
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
// pgoutput - logical-decoding payload decoder
// ---------------------------------------------------------------------------

/// pgoutput logical-decoding message decoder.
///
/// The replication stream layer hands the caller raw pgoutput frames (the
/// body of each XLogData). When streaming is disabled, feed individual frames
/// to [`pgoutput::decode`]. Protocols 2 through 4 can interleave chunks from
/// in-progress transactions when streaming is enabled, so feed their frames in
/// order to a [`pgoutput::Decoder`], which retains the current chunk's
/// transaction id.
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
            /// Flags - currently always 0.
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
        /// A relation (table) descriptor - emitted before the first
        /// Insert/Update/Delete on that relation. Cache the mapping
        /// `rel_id -> (namespace, name, columns)` for use when the
        /// tuple messages reference it.
        Relation {
            /// Transaction or subtransaction xid when streamed.
            xid: Option<u32>,
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
            /// Transaction or subtransaction xid when streamed.
            xid: Option<u32>,
            type_id: u32,
            namespace: String,
            name: String,
        },
        /// INSERT.
        Insert {
            /// Transaction or subtransaction xid when streamed.
            xid: Option<u32>,
            rel_id: u32,
            new_tuple: TupleData,
        },
        /// UPDATE. `old_tuple` is absent unless the replica identity
        /// columns changed (`DEFAULT`/`INDEX`) or the relation is
        /// `REPLICA IDENTITY FULL`.
        Update {
            /// Transaction or subtransaction xid when streamed.
            xid: Option<u32>,
            rel_id: u32,
            old_tuple: Option<OldTuple>,
            new_tuple: TupleData,
        },
        /// DELETE. Under the default replica identity `old_tuple` is
        /// [`OldTuple::Key`] - the row's identity, NOT its contents.
        Delete {
            /// Transaction or subtransaction xid when streamed.
            xid: Option<u32>,
            rel_id: u32,
            old_tuple: OldTuple,
        },
        /// TRUNCATE.
        Truncate {
            /// Transaction or subtransaction xid when streamed.
            xid: Option<u32>,
            /// Bit 0 = CASCADE, bit 1 = RESTART IDENTITY.
            options: u8,
            relation_ids: Vec<u32>,
        },
        /// A prepared transaction begins. Its changes follow before
        /// [`Self::Prepare`].
        BeginPrepare {
            prepare_lsn: u64,
            end_lsn: u64,
            prepare_timestamp: i64,
            xid: u32,
            gid: String,
        },
        /// A non-streamed transaction has been prepared.
        Prepare {
            flags: u8,
            prepare_lsn: u64,
            end_lsn: u64,
            prepare_timestamp: i64,
            xid: u32,
            gid: String,
        },
        /// A prepared transaction has been committed.
        CommitPrepared {
            flags: u8,
            commit_lsn: u64,
            end_lsn: u64,
            commit_timestamp: i64,
            xid: u32,
            gid: String,
        },
        /// A prepared transaction has been rolled back.
        RollbackPrepared {
            flags: u8,
            prepare_end_lsn: u64,
            rollback_end_lsn: u64,
            prepare_timestamp: i64,
            rollback_timestamp: i64,
            xid: u32,
            gid: String,
        },
        /// `S` - a chunk of a not-yet-committed transaction begins.
        ///
        /// Arrives only under [`super::Streaming`]. Layout measured on 16.14:
        /// `53 000a8649 01` - tag, xid, then the first-segment flag, which is
        /// `01` on the first chunk of a transaction and `00` on every later
        /// one.
        StreamStart {
            xid: u32,
            /// True on the first chunk of this transaction only.
            first_segment: bool,
        },
        /// `E` - the current chunk ends. Carries no payload at all: the whole
        /// message is the single byte `45`.
        StreamStop,
        /// `c` - a streamed transaction committed. Replaces [`Self::Commit`]
        /// when streaming is on, and adds the `xid`, which `Commit` has no
        /// room for because a non-streamed commit needs no correlation.
        StreamCommit {
            xid: u32,
            flags: u8,
            commit_lsn: u64,
            end_lsn: u64,
            commit_timestamp: i64,
        },
        /// `p` - a streamed transaction has been prepared.
        StreamPrepare {
            flags: u8,
            prepare_lsn: u64,
            end_lsn: u64,
            prepare_timestamp: i64,
            xid: u32,
            gid: String,
        },
        /// `A` - a streamed transaction, or one of its subtransactions,
        /// aborted. Everything already delivered for `subxid` must be
        /// discarded.
        ///
        /// Measured 9 bytes on 16.14 (`41 000a864b 000a864b`) with streaming
        /// `on`. Parallel streaming under protocol 4 appends the abort LSN and
        /// timestamp, making the frame 25 bytes.
        StreamAbort {
            xid: u32,
            /// The subtransaction that aborted. Equals `xid` when the whole
            /// transaction did.
            subxid: u32,
            /// Present only for protocol-4 parallel streaming.
            abort_lsn: Option<u64>,
            /// Present only for protocol-4 parallel streaming.
            abort_timestamp: Option<i64>,
        },
        /// A logical-replication "Message" (pg_logical_emit_message).
        Message {
            /// Transaction or subtransaction xid when streamed.
            xid: Option<u32>,
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

    /// The pre-change tuple on an UPDATE or DELETE, and WHICH KIND it is.
    ///
    /// pgoutput tags this tuple `K` or `O` and the two mean different things.
    /// Collapsing them loses the only signal that says whether a NULL in the
    /// tuple is a value or a placeholder:
    ///
    /// ```text
    /// DELETE, REPLICA IDENTITY DEFAULT      -> 44 <rel> 4b 0002 74 ..."7" 6e
    /// DELETE, REPLICA IDENTITY FULL, of a      44 <rel> 4f 0002 74 ..."7" 6e
    ///   row whose second column IS NULL
    /// ```
    ///
    /// Those two frames differ in exactly one byte - `4b` vs `4f`, `K` vs `O`
    /// - and decode to the same columns. A consumer handed only the columns
    /// cannot tell "this column was NULL" from "this column was not sent",
    /// so a predicate evaluated against it silently answers about a value the
    /// server never claimed. This enum is that byte, kept.
    #[derive(Debug, Clone, PartialEq)]
    pub enum OldTuple {
        /// `K` - the replica-identity columns only. Every other column is a
        /// NULL PLACEHOLDER, not a NULL value. This is what the DEFAULT
        /// replica identity sends on every DELETE, and on an UPDATE that
        /// changed the key. Use it to IDENTIFY the row; do not read the
        /// non-key columns as the row's prior contents.
        Key(TupleData),
        /// `O` - the complete pre-change row, sent when the relation is
        /// `REPLICA IDENTITY FULL`. Here a NULL is a value.
        Full(TupleData),
    }

    impl OldTuple {
        /// The tuple, whichever kind this is. Callers that genuinely only
        /// need the row's identity can use this; callers reading prior values
        /// must use [`OldTuple::full`], because [`OldTuple::Key`] does not
        /// carry them.
        pub fn tuple(&self) -> &TupleData {
            match self {
                Self::Key(tuple) | Self::Full(tuple) => tuple,
            }
        }

        /// The full pre-change row, or `None` when only the identity was sent.
        pub fn full(&self) -> Option<&TupleData> {
            match self {
                Self::Full(tuple) => Some(tuple),
                Self::Key(_) => None,
            }
        }
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
        /// Binary-format value, in the column type's `send` representation.
        ///
        /// Sent only when the STREAM was started with
        /// [`super::StartReplicationOptions::binary`]. It is not a property
        /// of the publication - this doc said "when the publication was
        /// created with the `binary` option" until 2026-08-24, and no such
        /// publication option exists; `CREATE PUBLICATION` would reject it.
        /// The option belongs to pgoutput, which is why it travels on
        /// `START_REPLICATION`.
        Binary(Bytes),
    }

    /// A tuple - one ordered list of column values.
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
        /// Bytes remained after a known message's documented field list.
        TrailingData {
            message: &'static str,
            remaining: usize,
        },
        /// A known field carried a byte outside its documented value set.
        InvalidField {
            message: &'static str,
            field: &'static str,
            value: u8,
            expected: &'static str,
        },
        /// A stream boundary contradicted the currently open block.
        InvalidStreamSequence {
            message: &'static str,
            reason: &'static str,
        },
        /// A streaming frame reached the stateless [`decode`], which cannot
        /// track the chunk state the rest of the transaction needs. Use
        /// [`Decoder`].
        StreamingNeedsDecoder,
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
                DecodeError::TrailingData { message, remaining } => {
                    write!(f, "pgoutput: {message} has {remaining} trailing byte(s)")
                }
                DecodeError::InvalidField {
                    message,
                    field,
                    value,
                    expected,
                } => write!(
                    f,
                    "pgoutput: {message} {field} is 0x{value:02x}; expected {expected}"
                ),
                DecodeError::InvalidStreamSequence { message, reason } => {
                    write!(f, "pgoutput: invalid {message}: {reason}")
                }
                DecodeError::StreamingNeedsDecoder => write!(
                    f,
                    "pgoutput: this frame belongs to a streamed transaction, whose framing is \
                     stateful; decode the stream with pgoutput::Decoder rather than the \
                     stateless decode()"
                ),
            }
        }
    }

    impl std::error::Error for DecodeError {}

    fn read_cstr(buf: &mut &[u8]) -> Result<String, DecodeError> {
        let pos = buf
            .iter()
            .position(|&b| b == 0)
            .ok_or(DecodeError::UnexpectedEof)?;
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

    fn read_constrained_u8(
        buf: &mut &[u8],
        message: &'static str,
        field: &'static str,
        allowed: &[u8],
        expected: &'static str,
    ) -> Result<u8, DecodeError> {
        let value = read_u8(buf)?;
        if !allowed.contains(&value) {
            return Err(DecodeError::InvalidField {
                message,
                field,
                value,
                expected,
            });
        }
        Ok(value)
    }

    const fn message_name(tag: u8) -> &'static str {
        match tag {
            b'B' => "Begin",
            b'C' => "Commit",
            b'O' => "Origin",
            b'R' => "Relation",
            b'Y' => "Type",
            b'I' => "Insert",
            b'U' => "Update",
            b'D' => "Delete",
            b'T' => "Truncate",
            b'M' => "Message",
            b'b' => "BeginPrepare",
            b'P' => "Prepare",
            b'K' => "CommitPrepared",
            b'r' => "RollbackPrepared",
            b'S' => "StreamStart",
            b'E' => "StreamStop",
            b'c' => "StreamCommit",
            b'A' => "StreamAbort",
            b'p' => "StreamPrepare",
            _ => "unknown pgoutput message",
        }
    }

    /// Decode a tuple - a u16 column count followed by per-column
    /// `(format_byte, [u32 len + bytes])`.
    fn read_tuple(buf: &mut &[u8]) -> Result<TupleData, DecodeError> {
        let n = read_u16(buf)? as usize;
        // Reserve for what the frame can still CONTAIN, not for what it
        // claims. Every column costs at least its one format byte, so the
        // remaining length is a sound cap that can never refuse valid input -
        // the same shape as the `nrelations.min(cur.len() / 4)` clamp on
        // Truncate below. Unclamped, an eight-byte Insert claiming 65535
        // columns reserved about 2.6 MB before the first column byte was even
        // read, which is a ~300000x amplification a peer can repeat per frame.
        //
        // NO TEST DISCRIMINATES THIS, and that is measured rather than assumed.
        // A `u16` count caps the over-reservation at ~2.6 MB where Truncate's
        // `u32` reaches ~17 GB, and 2.6 MB is below what the only instrument
        // available here can see: `tests/suite/pgoutput_allocation.rs` reads
        // `VmPeak`, which its own header says "distinguishes gigabytes from
        // nothing", and a counting `#[global_allocator]` is ruled out because
        // the workspace sets `unsafe_code = "deny"`. A VmPeak assertion written
        // for these two sites PASSES with the clamps deleted - checked
        // 2026-08-26 - so shipping one would have claimed cover it does not
        // give. This is defence in depth on the same argument as Truncate's
        // clamp, not a fix with a regression test behind it.
        let mut columns = Vec::with_capacity(n.min(buf.len()));
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

    /// Decode ONE pgoutput frame from a stream that is not using
    /// [`super::Streaming`].
    ///
    /// Streamed transactions cannot be decoded here, and this refuses them
    /// rather than guessing: inside a stream chunk every transactional
    /// message carries a 4-byte xid between the tag and its body, and NOTHING
    /// IN THE BYTES SAYS SO. A `Relation` is 45 bytes outside a stream and 49
    /// inside it, same relation. Reading the longer form with the shorter
    /// layout walks the namespace string four bytes early, which surfaces as
    /// `InvalidUtf8` if you are lucky and as a plausible wrong answer if you
    /// are not.
    ///
    /// Whether the prefix is there is a property of the CONVERSATION, so it
    /// takes a decoder that remembers one: [`Decoder`].
    pub fn decode(input: &[u8]) -> Result<PgOutputMessage, DecodeError> {
        match input.first() {
            Some(b'S' | b'E' | b'c' | b'A' | b'p') => Err(DecodeError::StreamingNeedsDecoder),
            _ => Decoder::new().decode(input),
        }
    }

    /// A pgoutput decoder that tracks whether it is inside a stream chunk.
    ///
    /// Hold ONE of these per replication stream and feed it every frame in
    /// order. It is only stateful because the protocol is: `StreamStart`
    /// opens a chunk in which transactional messages gain an xid prefix, and
    /// `StreamStop` closes it. The prefix is retained in those messages'
    /// `xid: Option<u32>` field and may name a subtransaction. It also refuses
    /// boundaries that would make this framing state ambiguous: nested starts,
    /// a stop without a start, and terminal messages before the current block
    /// has stopped.
    #[derive(Debug, Default)]
    pub struct Decoder {
        /// The transaction whose chunk is open, if any.
        stream_xid: Option<u32>,
    }

    impl Decoder {
        pub fn new() -> Self {
            Self::default()
        }

        /// The transaction whose chunk is currently open.
        pub fn stream_xid(&self) -> Option<u32> {
            self.stream_xid
        }

        /// Decode one frame, updating the stream state.
        pub fn decode(&mut self, input: &[u8]) -> Result<PgOutputMessage, DecodeError> {
            let message = decode_frame(input, self.stream_xid)?;
            self.validate_stream_sequence(&message)?;
            match &message {
                PgOutputMessage::StreamStart { xid, .. } => self.stream_xid = Some(*xid),
                PgOutputMessage::StreamStop
                | PgOutputMessage::StreamCommit { .. }
                | PgOutputMessage::StreamPrepare { .. }
                | PgOutputMessage::StreamAbort { .. } => self.stream_xid = None,
                _ => {}
            }
            Ok(message)
        }

        fn validate_stream_sequence(&self, message: &PgOutputMessage) -> Result<(), DecodeError> {
            let violation = match (self.stream_xid, message) {
                (Some(_), PgOutputMessage::StreamStart { .. }) => {
                    Some(("StreamStart", "a stream block is already open"))
                }
                (None, PgOutputMessage::StreamStop) => {
                    Some(("StreamStop", "no stream block is open"))
                }
                (Some(_), PgOutputMessage::StreamCommit { .. }) => Some((
                    "StreamCommit",
                    "StreamStop must close the current block first",
                )),
                (Some(_), PgOutputMessage::StreamAbort { .. }) => Some((
                    "StreamAbort",
                    "StreamStop must close the current block first",
                )),
                (Some(_), PgOutputMessage::StreamPrepare { .. }) => Some((
                    "StreamPrepare",
                    "StreamStop must close the current block first",
                )),
                _ => None,
            };

            match violation {
                Some((message, reason)) => {
                    Err(DecodeError::InvalidStreamSequence { message, reason })
                }
                None => Ok(()),
            }
        }
    }

    fn decode_frame(input: &[u8], stream_xid: Option<u32>) -> Result<PgOutputMessage, DecodeError> {
        let mut cur = input;
        let tag = read_u8(&mut cur)?;

        // Inside a chunk, every transactional message carries its own xid.
        // This is NOT necessarily the top-level xid in StreamStart: changes
        // made under a SAVEPOINT carry their subtransaction xid, which a later
        // StreamAbort can name independently. Measured on 16.14, one streamed
        // transaction had 21 StreamStarts carrying 000ab400 while its 4000
        // Inserts carried both 000ab400 and 000ab401. Preserve that identity
        // around the decoded payload instead of comparing it to the chunk's
        // top-level xid or silently throwing it away.
        let carried_xid = if stream_xid.is_some()
            && matches!(tag, b'R' | b'Y' | b'I' | b'U' | b'D' | b'T' | b'M')
        {
            Some(read_u32(&mut cur)?)
        } else {
            None
        };

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
                let flags = read_constrained_u8(&mut cur, "Commit", "flags", &[0], "0")?;
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
            b'b' => PgOutputMessage::BeginPrepare {
                prepare_lsn: read_u64(&mut cur)?,
                end_lsn: read_u64(&mut cur)?,
                prepare_timestamp: read_i64(&mut cur)?,
                xid: read_u32(&mut cur)?,
                gid: read_cstr(&mut cur)?,
            },
            b'P' => PgOutputMessage::Prepare {
                flags: read_constrained_u8(&mut cur, "Prepare", "flags", &[0], "0")?,
                prepare_lsn: read_u64(&mut cur)?,
                end_lsn: read_u64(&mut cur)?,
                prepare_timestamp: read_i64(&mut cur)?,
                xid: read_u32(&mut cur)?,
                gid: read_cstr(&mut cur)?,
            },
            b'K' => PgOutputMessage::CommitPrepared {
                flags: read_constrained_u8(&mut cur, "CommitPrepared", "flags", &[0], "0")?,
                commit_lsn: read_u64(&mut cur)?,
                end_lsn: read_u64(&mut cur)?,
                commit_timestamp: read_i64(&mut cur)?,
                xid: read_u32(&mut cur)?,
                gid: read_cstr(&mut cur)?,
            },
            b'r' => PgOutputMessage::RollbackPrepared {
                flags: read_constrained_u8(&mut cur, "RollbackPrepared", "flags", &[0], "0")?,
                prepare_end_lsn: read_u64(&mut cur)?,
                rollback_end_lsn: read_u64(&mut cur)?,
                prepare_timestamp: read_i64(&mut cur)?,
                rollback_timestamp: read_i64(&mut cur)?,
                xid: read_u32(&mut cur)?,
                gid: read_cstr(&mut cur)?,
            },
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
                // Same clamp as `read_tuple`: a Relation column costs at least
                // its flags byte, so the remaining length bounds how many can
                // really follow.
                let mut columns = Vec::with_capacity(ncols.min(cur.len()));
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
                    xid: carried_xid,
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
                    xid: carried_xid,
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
                PgOutputMessage::Insert {
                    xid: carried_xid,
                    rel_id,
                    new_tuple,
                }
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
                        let tuple = read_tuple(&mut cur)?;
                        // Which of the two it was decides whether the
                        // non-key columns are values or placeholders, so it
                        // is carried, not just validated.
                        let old = if kind == b'K' {
                            OldTuple::Key(tuple)
                        } else {
                            OldTuple::Full(tuple)
                        };
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
                    xid: carried_xid,
                    rel_id,
                    old_tuple,
                    new_tuple,
                }
            }
            b'D' => {
                let rel_id = read_u32(&mut cur)?;
                // 'K' (replica identity key) or 'O' (full row). Which one
                // decides whether a NULL in the tuple is a value, so the
                // answer travels with it.
                let kind = read_u8(&mut cur)?;
                let old_tuple = match kind {
                    b'K' => OldTuple::Key(read_tuple(&mut cur)?),
                    b'O' => OldTuple::Full(read_tuple(&mut cur)?),
                    other => return Err(DecodeError::UnknownTupleFormat(other)),
                };
                PgOutputMessage::Delete {
                    xid: carried_xid,
                    rel_id,
                    old_tuple,
                }
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
                    xid: carried_xid,
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
                cur = &cur[len..];
                PgOutputMessage::Message {
                    xid: carried_xid,
                    flags,
                    lsn,
                    prefix,
                    content,
                }
            }
            b'S' => {
                let xid = read_u32(&mut cur)?;
                let first_segment = read_constrained_u8(
                    &mut cur,
                    "StreamStart",
                    "first_segment",
                    &[0, 1],
                    "0 or 1",
                )? == 1;
                PgOutputMessage::StreamStart { xid, first_segment }
            }
            b'E' => PgOutputMessage::StreamStop,
            b'c' => PgOutputMessage::StreamCommit {
                xid: read_u32(&mut cur)?,
                flags: read_constrained_u8(&mut cur, "StreamCommit", "flags", &[0], "0")?,
                commit_lsn: read_u64(&mut cur)?,
                end_lsn: read_u64(&mut cur)?,
                commit_timestamp: read_i64(&mut cur)?,
            },
            b'p' => PgOutputMessage::StreamPrepare {
                flags: read_constrained_u8(&mut cur, "StreamPrepare", "flags", &[0], "0")?,
                prepare_lsn: read_u64(&mut cur)?,
                end_lsn: read_u64(&mut cur)?,
                prepare_timestamp: read_i64(&mut cur)?,
                xid: read_u32(&mut cur)?,
                gid: read_cstr(&mut cur)?,
            },
            b'A' => {
                let xid = read_u32(&mut cur)?;
                let subxid = read_u32(&mut cur)?;
                let (abort_lsn, abort_timestamp) = match cur.len() {
                    0 => (None, None),
                    16 => (Some(read_u64(&mut cur)?), Some(read_i64(&mut cur)?)),
                    remaining => {
                        return Err(DecodeError::TrailingData {
                            message: message_name(tag),
                            remaining,
                        });
                    }
                };
                PgOutputMessage::StreamAbort {
                    xid,
                    subxid,
                    abort_lsn,
                    abort_timestamp,
                }
            }
            other => return Err(DecodeError::UnknownTag(other)),
        };
        if !cur.is_empty() {
            return Err(DecodeError::TrailingData {
                message: message_name(tag),
                remaining: cur.len(),
            });
        }
        Ok(msg)
    }

    // ----- Test-only encoders -----
    //
    // pgoutput messages are server -> client only; nobody sends them
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

        pub fn commit(flags: u8, commit_lsn: u64, end_lsn: u64, commit_timestamp: i64) -> Vec<u8> {
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
    use crate::NoTls;
    use crate::config::{SslCertMode, SslMode, SslNegotiation, SslRootCert};
    use crate::test_utils::paired_loopback_port;
    use crate::tls::{NoTlsStream, TlsConnect};
    use compio::io::{AsyncReadExt, AsyncWriteExt};
    use pgoutput::{OldTuple, PgOutputMessage, TupleColumn};
    use std::error::Error as _;
    use std::future;

    fn test_cancel_token() -> CancelToken {
        CancelToken {
            socket_config: None,
            encryption: Encryption::Plaintext,
            ssl_sni: true,
            ssl_cert_mode: SslCertMode::Allow,
            server_verification: ServerVerification::None,
            tls_policy_identity: None,
            ssl_mode: SslMode::Disable,
            ssl_negotiation: SslNegotiation::Postgres,
            process_id: 0,
            secret_key: Some(0.into()),
            pool_lease: None,
            drop_target: Some(crate::cancel_token::CancelDropTarget::Replication(
                Arc::new(AtomicBool::new(false)),
            )),
        }
    }

    fn startup_frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + body.len());
        frame.push(tag);
        frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    fn successful_replication_handshake() -> Vec<u8> {
        let mut response = startup_frame(b'R', &0u32.to_be_bytes());
        response.extend_from_slice(&startup_frame(b'K', &[0; 8]));
        response.extend_from_slice(&startup_frame(b'Z', b"I"));
        response
    }

    fn refused_replication_handshake() -> Vec<u8> {
        startup_frame(b'E', b"SERROR\0C57P03\0Mscripted refusal\0\0")
    }

    async fn scripted_replication_server() -> std::net::SocketAddr {
        scripted_replication_server_bound(
            "127.0.0.1:0".parse().unwrap(),
            successful_replication_handshake(),
        )
        .await
        .0
    }

    async fn scripted_replication_server_bound(
        bind: std::net::SocketAddr,
        response: Vec<u8>,
    ) -> (std::net::SocketAddr, futures_channel::oneshot::Receiver<()>) {
        let listener = compio::net::TcpListener::bind(bind)
            .await
            .expect("bind scripted replication server");
        let addr = listener.local_addr().expect("scripted server address");
        let (startup_seen, startup_observed) = futures_channel::oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept replication connection");
            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.expect("read startup length");
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            assert!(length >= 4, "startup packet length includes its header");
            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
            result.expect("read startup body");
            let _ = startup_seen.send(());

            let compio::BufResult(result, _) = socket.write_all(response).await;
            result.expect("write scripted startup response");
            socket.flush().await.expect("flush startup response");
        })
        .detach();

        (addr, startup_observed)
    }

    /// Accept one PostgreSQL SSLRequest and agree to TLS. The test connector
    /// then fails its handshake, ending this address attempt immediately.
    async fn replication_tls_handshake_server_bound(
        bind: std::net::SocketAddr,
    ) -> (
        std::net::SocketAddr,
        futures_channel::oneshot::Receiver<u32>,
    ) {
        let listener = compio::net::TcpListener::bind(bind)
            .await
            .expect("bind TLS replication probe");
        let addr = listener
            .local_addr()
            .expect("TLS replication probe address");
        let (opening_seen, opening_observed) = futures_channel::oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept TLS replication probe");
            let compio::BufResult(result, opening) = socket.read_exact(vec![0u8; 8]).await;
            result.expect("read replication SSLRequest");
            assert_eq!(u32::from_be_bytes(opening[..4].try_into().unwrap()), 8);
            let code = u32::from_be_bytes(opening[4..].try_into().unwrap());
            let _ = opening_seen.send(code);

            let compio::BufResult(result, _) = socket.write_all(vec![b'S']).await;
            result.expect("accept scripted TLS negotiation");
            socket.flush().await.expect("flush TLS acceptance");
        })
        .detach();

        (addr, opening_observed)
    }

    struct HandshakeFailingTls;

    impl<S> MakeTlsConnect<S> for HandshakeFailingTls {
        type Stream = NoTlsStream;
        type TlsConnect = HandshakeFailingTls;
        type Error = std::io::Error;

        fn make_tls_connect(&mut self, _domain: &str) -> Result<Self::TlsConnect, Self::Error> {
            Ok(HandshakeFailingTls)
        }
    }

    impl<S> TlsConnect<S> for HandshakeFailingTls {
        type Stream = NoTlsStream;
        type Error = std::io::Error;
        type Future = future::Ready<Result<NoTlsStream, std::io::Error>>;

        fn connect(self, _stream: S) -> Self::Future {
            future::ready(Err(std::io::Error::other("scripted TLS handshake failure")))
        }
    }

    /// A replication server that accepts, reads the startup packet, and then
    /// never answers. Bound to a caller-chosen address so two of them can
    /// share one port across two loopback IPs -- `Endpoint::addresses`
    /// discards the resolved port and the walk dials `endpoint.port()`.
    async fn stalled_replication_server(
        bind: std::net::SocketAddr,
    ) -> (std::net::SocketAddr, futures_channel::oneshot::Receiver<()>) {
        let listener = compio::net::TcpListener::bind(bind)
            .await
            .expect("bind stalled replication server");
        let addr = listener.local_addr().expect("stalled server address");
        let (startup_seen, startup_observed) = futures_channel::oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.expect("read startup length");
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
            result.expect("read startup body");
            let _ = startup_seen.send(());

            // Never answer: the client must time out on THIS address and move
            // on to the next one with a fresh budget.
            let compio::BufResult(_, _) = socket.read(vec![0u8; 1]).await;
        })
        .detach();

        (addr, startup_observed)
    }

    /// The protocol code in the first 8 bytes a client writes: either an
    /// `SSLRequest` or a 3.2 `StartupMessage`. That single u32 is what says
    /// which transport the driver chose, without needing a TLS stack.
    const SSL_REQUEST_CODE: u32 = 80_877_103;
    #[cfg(unix)]
    const STARTUP_V3_2_CODE: u32 = 196_610;
    /// Not a protocol code: the client closed without writing anything.
    #[cfg(unix)]
    const NOTHING_WRITTEN: u32 = 0;

    /// A walsender on a UNIX socket that reports which opening message it got.
    ///
    /// `/tmp` literally, not `std::env::temp_dir()`: Linux caps a
    /// `sockaddr_un` path at 108 bytes including the `.s.PGSQL.<port>` suffix,
    /// and this repo's agent scratchpad root alone is 79 characters, which
    /// overruns it and fails `bind` with ENAMETOOLONG on a healthy machine.
    #[cfg(unix)]
    struct UnixProbe {
        dir: std::path::PathBuf,
        seen: futures_channel::oneshot::Receiver<u32>,
    }

    #[cfg(unix)]
    async fn unix_probe(port: u16) -> UnixProbe {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let dir = std::path::PathBuf::from("/tmp").join(format!(
            "cpg-repl-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("create temporary socket directory");
        let listener = compio::net::UnixListener::bind(dir.join(format!(".s.PGSQL.{port}")))
            .await
            .expect("bind unix walsender");
        let (tx, seen) = futures_channel::oneshot::channel::<u32>();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept unix connection");
            let compio::BufResult(result, head) = socket.read_exact(vec![0u8; 8]).await;
            // A client that picks a transport it cannot build closes without
            // writing. Report that as its own code rather than panicking in a
            // detached task, so the assertion below can name what happened.
            let code = match result {
                Ok(_) => u32::from_be_bytes(head[4..8].try_into().unwrap()),
                Err(_) => NOTHING_WRITTEN,
            };
            let _ = tx.send(code);
        })
        .detach();

        UnixProbe { dir, seen }
    }

    /// `sslmode` is ignored for Unix-domain sockets on the replication path,
    /// exactly as it is on the query path.
    ///
    /// libpq: "sslmode is ignored for Unix domain socket communication." A
    /// local socket has no network to eavesdrop on and no host name to put in
    /// a certificate. `connect_replication_addr` chose its transport from the
    /// MODE alone, so `host=/path sslmode=require` sent an `SSLRequest` down a
    /// Unix socket -- a configuration that connects fine as an ordinary query.
    ///
    /// Asserted on the protocol code actually written to the socket, because
    /// that is the thing that differs; a test that only asserted "did not
    /// succeed" would pass on both sides of the fix.
    #[cfg(unix)]
    #[compio::test]
    async fn unix_socket_replication_ignores_sslmode() {
        let port = 5432;
        let probe = unix_probe(port).await;

        let mut cfg = Config::new();
        cfg.user("scripted-user")
            .ssl_mode(SslMode::Require)
            .replication(ReplicationMode::Logical);

        // The connect will not complete -- the probe never answers -- so the
        // result is deliberately ignored. What is under test is the opening
        // message, which has been written by then.
        let _ = compio::time::timeout(
            std::time::Duration::from_secs(5),
            connect_replication_addr(Addr::Unix(probe.dir.clone()), None, port, &mut NoTls, &cfg),
        )
        .await;

        let code = compio::time::timeout(std::time::Duration::from_secs(5), probe.seen)
            .await
            .expect("the driver never opened the unix socket")
            .expect("the probe never reported an opening message");
        let _ = std::fs::remove_dir_all(&probe.dir);

        assert_eq!(
            code,
            STARTUP_V3_2_CODE,
            "expected a plaintext StartupMessage over the unix socket, got {}",
            match code {
                // What this test sees pre-fix, because `NoTls` cannot build the
                // TLS transport that `sslmode=require` selected, so the attempt
                // dies before a byte is written.
                NOTHING_WRITTEN =>
                    "nothing: sslmode selected a transport this \
                                    connector cannot build over a local socket",
                // What a real TLS connector would send pre-fix.
                SSL_REQUEST_CODE => "an SSLRequest: sslmode was applied to a local socket",
                _ => "an unrecognised opening message",
            }
        );
    }

    struct ListResolver(Vec<std::net::SocketAddr>);

    impl Resolver for ListResolver {
        async fn resolve(
            &mut self,
            _host: &str,
            _port: u16,
        ) -> std::io::Result<Vec<std::net::SocketAddr>> {
            Ok(self.0.clone())
        }
    }

    /// The replication address walk must continue after a real TLS-handshake
    /// failure. Seeing SSLRequest on both probes proves neither attempt fell
    /// through to plaintext.
    #[compio::test]
    async fn replication_tls_failure_advances_to_second_resolved_address() {
        let port = paired_loopback_port();
        let (first, first_opening) = replication_tls_handshake_server_bound(
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        )
        .await;
        let second_bind = std::net::SocketAddr::from(([127, 0, 0, 2], port));
        let (second, second_opening) = replication_tls_handshake_server_bound(second_bind).await;

        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("scripted.example")
            .port(first.port())
            .ssl_mode(SslMode::Require)
            .replication(ReplicationMode::Logical);
        let endpoint = endpoints(&config)
            .expect("one hostname is a valid endpoint list")
            .pop()
            .expect("the endpoint list contains the hostname");
        let mut resolver = ListResolver(vec![first, second]);
        let mut tls = HandshakeFailingTls;

        let result = compio::time::timeout(
            std::time::Duration::from_secs(5),
            connect_replication_host(&endpoint, &mut resolver, &mut tls, &config),
        )
        .await
        .expect("the TLS replication address walk hung");
        assert!(result.is_err(), "both scripted TLS handshakes fail");

        async fn opening_code(seen: futures_channel::oneshot::Receiver<u32>) -> u32 {
            compio::time::timeout(std::time::Duration::from_secs(2), seen)
                .await
                .expect("the resolved replication address was never dialled")
                .expect("the TLS probe closed without reporting its opening message")
        }

        assert_eq!(opening_code(first_opening).await, SSL_REQUEST_CODE);
        assert_eq!(
            opening_code(second_opening).await,
            SSL_REQUEST_CODE,
            "the first TLS failure stopped the replication address walk"
        );
    }

    /// A successful second address must win even when the first address has
    /// already produced a valid PostgreSQL startup error.
    #[compio::test]
    async fn replication_connect_succeeds_via_second_resolved_address() {
        let port = paired_loopback_port();
        let (first, first_seen) = scripted_replication_server_bound(
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            refused_replication_handshake(),
        )
        .await;
        let second_bind = std::net::SocketAddr::from(([127, 0, 0, 2], port));
        let (second, second_seen) =
            scripted_replication_server_bound(second_bind, successful_replication_handshake())
                .await;

        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("scripted.example")
            .port(first.port())
            .ssl_mode(SslMode::Disable)
            .replication(ReplicationMode::Logical);
        let endpoint = endpoints(&config)
            .expect("one hostname is a valid endpoint list")
            .pop()
            .expect("the endpoint list contains the hostname");
        let mut resolver = ListResolver(vec![first, second]);
        let mut tls = NoTls;

        let connection = compio::time::timeout(
            std::time::Duration::from_secs(5),
            connect_replication_host(&endpoint, &mut resolver, &mut tls, &config),
        )
        .await
        .expect("the replication address walk hung")
        .expect("the healthy second address must complete replication startup");

        async fn startup_seen(seen: futures_channel::oneshot::Receiver<()>, address: &str) {
            compio::time::timeout(std::time::Duration::from_secs(2), seen)
                .await
                .unwrap_or_else(|_| panic!("the {address} replication server was never dialled"))
                .unwrap_or_else(|_| {
                    panic!("the {address} replication server closed without observing startup")
                });
        }

        startup_seen(first_seen, "first").await;
        startup_seen(second_seen, "healthy second").await;
        drop(connection);
    }

    /// The replication walk budgets `connect_timeout` per ADDRESS, exactly as
    /// `connect::connect_host` does.
    ///
    /// Both walks iterate the same `Endpoint` list, so a divergence in the
    /// BUDGET means a replication client and a query client disagree about
    /// what `connect_timeout` means. (They still differ elsewhere -- see
    /// `connect_replication_host` -- so this says nothing about the rest.)
    ///
    /// What this test actually pins: that the deadline restarts for the
    /// SECOND address rather than being spent once for the whole walk. It
    /// does NOT prove the second address received a FULL fresh budget, only
    /// that it was dialled at all -- a residual-budget regression would still
    /// pass here.
    #[compio::test]
    async fn replication_connect_timeout_restarts_for_each_resolved_address() {
        let port = paired_loopback_port();
        let (first, first_seen) =
            stalled_replication_server(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await;
        let second_bind = std::net::SocketAddr::from(([127, 0, 0, 2], port));
        let (second, second_seen) = stalled_replication_server(second_bind).await;

        let mut config = Config::new();
        config
            .user("scripted-user")
            .host("scripted.example")
            .port(first.port())
            .ssl_mode(SslMode::Disable)
            .replication(ReplicationMode::Logical)
            .connect_timeout(std::time::Duration::from_millis(150));

        let endpoint = endpoints(&config)
            .expect("one hostname is a valid endpoint list")
            .pop()
            .expect("the endpoint list contains the hostname");
        let mut resolver = ListResolver(vec![first, second]);
        let mut tls = NoTls;

        let result = compio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_replication_host(&endpoint, &mut resolver, &mut tls, &config),
        )
        .await
        .expect("the outer watchdog expired, so some leg had no deadline at all");
        assert!(
            result.is_err(),
            "both addresses stall forever, so this cannot connect"
        );

        // Bounded: an address that was never dialled leaves its server parked
        // in `accept()` still holding the sender, so a bare await would hang.
        async fn dialled(seen: futures_channel::oneshot::Receiver<()>) -> bool {
            compio::time::timeout(std::time::Duration::from_secs(2), seen)
                .await
                .is_ok_and(|received| received.is_ok())
        }

        assert!(
            dialled(first_seen).await,
            "the first address was never dialled"
        );
        assert!(
            dialled(second_seen).await,
            "the second address was never dialled: one budget was shared across the whole \
             host walk instead of restarting per address"
        );
    }

    /// Every connection entry point rejects contradictory TLS settings before
    /// it tries an endpoint. In particular, trusting the system roots without
    /// verifying the hostname is invalid for replication just as it is for an
    /// ordinary connection.
    #[compio::test]
    async fn replication_connect_rejects_system_roots_with_weak_sslmode() {
        let addr = scripted_replication_server().await;

        let mut config = Config::new();
        config
            .hostaddr(addr.ip())
            .port(addr.port())
            .ssl_mode(SslMode::Prefer)
            .ssl_root_cert(SslRootCert::System)
            .connect_timeout(std::time::Duration::from_millis(100));

        let query_error = match config.connect(NoTls).await {
            Ok(_) => panic!("ordinary connect accepted contradictory TLS settings"),
            Err(error) => error,
        };
        assert_eq!(query_error.to_string(), "invalid configuration");

        let replication_error = match compio::time::timeout(
            std::time::Duration::from_secs(2),
            connect_replication(NoTls, &config),
        )
        .await
        .expect("replication validation or scripted startup hung")
        {
            Ok(_) => panic!("replication connect accepted contradictory TLS settings"),
            Err(error) => error,
        };
        assert_eq!(replication_error.to_string(), "invalid configuration");
        assert!(
            replication_error
                .source()
                .is_some_and(|cause| cause.to_string().contains("sslrootcert=system")),
            "the error must name the contradictory setting: {replication_error:?}"
        );
    }

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

        // A row with no fields is REFUSED. This assertion used to say the
        // opposite -- that an empty row "yields the empty identity rather than
        // an error: there is nothing malformed about it, and `identify_system`
        // reports the absence through its own values". That reasoning was
        // wrong on its own terms, which is why it is reversed here rather than
        // merely adjusted.
        //
        // An empty string does not report an absence; it is a VALUE, and two
        // of them compare equal. `systemid` is the cluster identity a caller
        // compares to notice it has been failed over onto a different cluster,
        // so an identity that defaults to `""` makes that comparison succeed
        // in precisely the case it exists to catch. `IDENTIFY_SYSTEM` is
        // specified to return four columns, so a row with none is malformed
        // for this command whatever it might mean for some other one.
        //
        // Nothing depended on the old shape: the sole caller in the workspace
        // (`crates/plugin-db/src/wal_consumer.rs`) uses `identify_system` as a
        // health check and discards the value, so this change only makes that
        // check harder to pass with a broken peer.
        let row = identify_row(&[]);
        let error = parse_identify_system_row(&row)
            .expect_err("a row with no fields cannot be an IDENTIFY_SYSTEM identity");
        assert!(
            error
                .to_string()
                .contains("error parsing response from server"),
            "unexpected error: {error}"
        );
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
        assert_eq!(err.code().map(crate::error::SqlState::code), Some("42501"));
    }

    #[test]
    fn lsn_tracker_reports_flush_below_received() {
        // The walsender protocol distinguishes write (received) from
        // flush (durably processed). A careful caller that has *seen*
        // up to LSN 100 on the wire but only durably *flushed* 50 must
        // be able to report write=100, flush=50, apply=50 - otherwise
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

        // A caller-supplied checkpoint cannot make flush/apply overtake
        // write. PostgreSQL trusts flush enough to recycle WAL, so emitting
        // (100, 120, 120) would turn a local bookkeeping error into data loss.
        t.advance_processed(120);
        assert_eq!(t.standby_lsns(), (100, 100, 100));

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
        assert_eq!(
            parse_lsn("FF/FFFFFFFF").unwrap(),
            (0xFFu64 << 32) | 0xFFFFFFFF
        );
        assert_eq!(format_lsn(0x16B3750), "0/16B3750");
        assert_eq!(format_lsn((1u64 << 32) | 0x16B3750), "1/16B3750");
    }

    #[test]
    fn parse_lsn_rejects_bad_input() {
        assert!(parse_lsn("not-an-lsn").is_none());
        assert!(parse_lsn("0").is_none());
        assert!(parse_lsn("0/zz").is_none());
    }

    // THE pgoutput TESTS BELOW FEED OUR OWN ENCODER TO OUR OWN DECODER, so on
    // their own they would hold for a pair that is wrong in the same
    // direction - pgoutput is PostgreSQL's format, not ours. They are
    // supplementary, and that is only safe because every variant is also
    // decoded from a real server somewhere in `tests/pgoutput_*.rs` /
    // `tests/replication_*.rs`.
    //
    // AUDITED 2026-08-25: all 19 `PgOutputMessage` variants appear in a live
    // assertion - the enum's variant list and the set of `PgOutputMessage::*`
    // named across those suites were compared and are identical, including the
    // two-phase (BeginPrepare, Prepare, CommitPrepared, RollbackPrepared,
    // StreamPrepare) and streaming (StreamStart/Stop/Commit/Abort) arms.
    //
    // So: adding a variant means adding a LIVE test for it, not just a round
    // trip here. `parse_lsn`/`format_lsn` above are the cautionary case - they
    // had only the self-referential test until `tests/lsn_server_parity.rs`.
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
                (1, "id", 20, -1),    // BIGINT, REPLICA IDENTITY KEY
                (0, "title", 25, -1), // TEXT
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
                ..
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
    fn pgoutput_relation_type_modifier_reinterprets_the_signed_i32_boundary() {
        let bytes = pgoutput::encode::relation(
            16384,
            "public",
            "boundary",
            b'd',
            &[(0, "value", 25, i32::MIN)],
        );
        let PgOutputMessage::Relation { columns, .. } = pgoutput::decode(&bytes).unwrap() else {
            panic!("expected Relation");
        };

        assert_eq!(columns[0].type_modifier, i32::MIN);
    }

    #[test]
    fn pgoutput_decode_insert() {
        let bytes = pgoutput::encode::insert(16384, &[Some("42"), Some("hello"), None]);
        let msg = pgoutput::decode(&bytes).unwrap();
        match msg {
            PgOutputMessage::Insert {
                rel_id, new_tuple, ..
            } => {
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
                ..
            } => {
                assert_eq!(rel_id, 16384);
                assert!(old_tuple.is_none());
                assert_eq!(new_tuple.columns.len(), 2);
                assert_eq!(new_tuple.columns[1], TupleColumn::Text("world".into()));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    /// TupleData's `u` marker means "reuse the unchanged TOAST value", not
    /// SQL NULL. A caller must be able to tell those states apart while still
    /// receiving text and binary values from the same tuple.
    #[test]
    fn pgoutput_preserves_every_tuple_column_kind() {
        let mut bytes = vec![b'U'];
        bytes.extend_from_slice(&16_384u32.to_be_bytes());
        bytes.push(b'N');
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.push(b'n');
        bytes.push(b'u');
        bytes.push(b't');
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(b"abc");
        bytes.push(b'b');
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&[0x00, 0xFF]);

        let PgOutputMessage::Update { new_tuple, .. } = pgoutput::decode(&bytes).unwrap() else {
            panic!("the tuple fixture did not decode as Update");
        };
        assert_eq!(
            new_tuple.columns,
            vec![
                TupleColumn::Null,
                TupleColumn::Toasted,
                TupleColumn::Text("abc".into()),
                TupleColumn::Binary(bytes::Bytes::from_static(&[0x00, 0xFF])),
            ]
        );
    }

    #[test]
    fn pgoutput_decode_delete_key() {
        let bytes = pgoutput::encode::delete_key(16384, &[Some("42")]);
        let msg = pgoutput::decode(&bytes).unwrap();
        match msg {
            PgOutputMessage::Delete {
                rel_id, old_tuple, ..
            } => {
                assert_eq!(rel_id, 16384);
                // The encoder writes a 'K' frame, so this must come back as
                // Key - a Full here would mean the kind byte was ignored.
                let OldTuple::Key(tuple) = &old_tuple else {
                    panic!("a 'K' frame must decode as OldTuple::Key, got {old_tuple:?}");
                };
                assert_eq!(tuple.columns.len(), 1);
                assert_eq!(tuple.columns[0], TupleColumn::Text("42".into()));
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

    /// pgoutput versions are selected explicitly, and each selected version
    /// defines a complete field list for every message. Bytes after that list
    /// are unsupported structure, not an extension the decoder may discard.
    /// Exercise all 19 legal tags so adding an exhaustion check to only the
    /// common fixed-layout cases cannot print a clean result.
    #[test]
    fn every_pgoutput_message_refuses_trailing_bytes_by_name() {
        fn fixed(tag: u8, body_len: usize) -> Vec<u8> {
            let mut frame = vec![tag];
            frame.resize(body_len + 1, 0);
            frame
        }

        let mut relation = fixed(b'R', 4);
        relation.extend_from_slice(b"public\0t\0d\0\0");
        let mut type_message = fixed(b'Y', 4);
        type_message.extend_from_slice(b"public\0t\0");
        let empty_tuple = [0u8, 0];
        let mut insert = fixed(b'I', 4);
        insert.push(b'N');
        insert.extend_from_slice(&empty_tuple);
        let mut update = fixed(b'U', 4);
        update.push(b'N');
        update.extend_from_slice(&empty_tuple);
        let mut delete = fixed(b'D', 4);
        delete.push(b'K');
        delete.extend_from_slice(&empty_tuple);
        let mut origin = fixed(b'O', 8);
        origin.extend_from_slice(b"origin\0");
        let mut message = fixed(b'M', 1 + 8);
        message.extend_from_slice(b"prefix\0");
        message.extend_from_slice(&0u32.to_be_bytes());
        let mut begin_prepare = fixed(b'b', 8 + 8 + 8 + 4);
        begin_prepare.extend_from_slice(b"g\0");
        let mut prepare = fixed(b'P', 1 + 8 + 8 + 8 + 4);
        prepare.extend_from_slice(b"g\0");
        let mut commit_prepared = fixed(b'K', 1 + 8 + 8 + 8 + 4);
        commit_prepared.extend_from_slice(b"g\0");
        let mut rollback_prepared = fixed(b'r', 1 + 8 + 8 + 8 + 8 + 4);
        rollback_prepared.extend_from_slice(b"g\0");
        let mut stream_prepare = fixed(b'p', 1 + 8 + 8 + 8 + 4);
        stream_prepare.extend_from_slice(b"g\0");

        let frames = [
            ("Begin", fixed(b'B', 8 + 8 + 4)),
            ("Commit", fixed(b'C', 1 + 8 + 8 + 8)),
            ("Origin", origin),
            ("Relation", relation),
            ("Type", type_message),
            ("Insert", insert),
            ("Update", update),
            ("Delete", delete),
            ("Truncate", fixed(b'T', 4 + 1)),
            ("Message", message),
            ("BeginPrepare", begin_prepare),
            ("Prepare", prepare),
            ("CommitPrepared", commit_prepared),
            ("RollbackPrepared", rollback_prepared),
            ("StreamStart", fixed(b'S', 4 + 1)),
            ("StreamStop", fixed(b'E', 0)),
            ("StreamCommit", fixed(b'c', 4 + 1 + 8 + 8 + 8)),
            ("StreamAbort", fixed(b'A', 4 + 4)),
            ("StreamPrepare", stream_prepare),
        ];

        let mut ruled_on = 0;
        for (expected_name, mut frame) in frames {
            let seed_stream_stop = |decoder: &mut pgoutput::Decoder| {
                if expected_name == "StreamStop" {
                    decoder
                        .decode(&[b'S', 0, 0, 0, 1, 1])
                        .expect("StreamStart fixture opens the block");
                }
            };

            let mut clean_decoder = pgoutput::Decoder::new();
            seed_stream_stop(&mut clean_decoder);
            clean_decoder
                .decode(&frame)
                .unwrap_or_else(|error| panic!("valid {expected_name} fixture failed: {error}"));
            frame.push(0xAA);
            let mut trailing_decoder = pgoutput::Decoder::new();
            seed_stream_stop(&mut trailing_decoder);
            let error = trailing_decoder.decode(&frame).unwrap_err();
            match error {
                pgoutput::DecodeError::TrailingData { message, remaining } => {
                    assert_eq!(message, expected_name);
                    assert_eq!(remaining, 1);
                }
                other => panic!("{expected_name} was not refused by name: {other}"),
            }
            ruled_on += 1;
        }
        assert_eq!(ruled_on, 19, "the complete legal tag set must be ruled on");
    }

    /// PostgreSQL rejects the six currently-zero flags in its own pgoutput
    /// reader. StreamStart is also constrained to 0 or 1; coercing byte 2 to
    /// true here disagrees with PostgreSQL's `byte == 1` interpretation. Keep
    /// the seven sites in one counted matrix so a newly strict subset cannot
    /// leave an untested raw-byte arm behind.
    #[test]
    fn pgoutput_refuses_invalid_fixed_fields_by_message_name() {
        fn fixed(tag: u8, body_len: usize) -> Vec<u8> {
            let mut frame = vec![tag];
            frame.resize(body_len + 1, 0);
            frame
        }

        let mut commit = fixed(b'C', 1 + 8 + 8 + 8);
        commit[1] = 1;
        let mut prepare = fixed(b'P', 1 + 8 + 8 + 8 + 4);
        prepare[1] = 1;
        prepare.extend_from_slice(b"g\0");
        let mut commit_prepared = fixed(b'K', 1 + 8 + 8 + 8 + 4);
        commit_prepared[1] = 1;
        commit_prepared.extend_from_slice(b"g\0");
        let mut rollback_prepared = fixed(b'r', 1 + 8 + 8 + 8 + 8 + 4);
        rollback_prepared[1] = 1;
        rollback_prepared.extend_from_slice(b"g\0");
        let mut stream_start = fixed(b'S', 4 + 1);
        stream_start[5] = 2;
        let mut stream_commit = fixed(b'c', 4 + 1 + 8 + 8 + 8);
        stream_commit[5] = 1;
        let mut stream_prepare = fixed(b'p', 1 + 8 + 8 + 8 + 4);
        stream_prepare[1] = 1;
        stream_prepare.extend_from_slice(b"g\0");

        let cases = [
            ("Commit", "flags", 1, commit),
            ("Prepare", "flags", 1, prepare),
            ("CommitPrepared", "flags", 1, commit_prepared),
            ("RollbackPrepared", "flags", 1, rollback_prepared),
            ("StreamStart", "first_segment", 2, stream_start),
            ("StreamCommit", "flags", 1, stream_commit),
            ("StreamPrepare", "flags", 1, stream_prepare),
        ];

        let mut refused = 0;
        let mut accepted = Vec::new();
        for (expected_message, expected_field, expected_value, frame) in cases {
            match pgoutput::Decoder::new().decode(&frame) {
                Err(pgoutput::DecodeError::InvalidField {
                    message,
                    field,
                    value,
                    ..
                }) => {
                    assert_eq!(message, expected_message);
                    assert_eq!(field, expected_field);
                    assert_eq!(value, expected_value);
                    refused += 1;
                }
                Ok(_) => accepted.push(expected_message),
                Err(other) => panic!(
                    "{expected_message} reached the wrong refusal instead of its field: {other}"
                ),
            }
        }
        assert!(
            accepted.is_empty(),
            "{} of 7 invalid fields were accepted: {accepted:?}",
            accepted.len()
        );
        assert_eq!(refused, 7, "every constrained-byte site must be ruled on");
    }

    /// These five boundaries change the state that decides whether the next
    /// transactional frame contains a four-byte xid prefix. PostgreSQL names
    /// each as a protocol violation; accepting one can therefore parse every
    /// later field four bytes out of phase. Lifecycle checks that require xid
    /// history remain caller policy, but the currently open block is decoder
    /// framing state and must be protected here.
    #[test]
    fn pgoutput_refuses_invalid_stream_boundaries_without_losing_state() {
        fn stream_start(xid: u32) -> Vec<u8> {
            let mut frame = vec![b'S'];
            frame.extend_from_slice(&xid.to_be_bytes());
            frame.push(1);
            frame
        }

        fn fixed(tag: u8, body_len: usize) -> Vec<u8> {
            let mut frame = vec![tag];
            frame.resize(body_len + 1, 0);
            frame
        }

        let mut stream_prepare = fixed(b'p', 1 + 8 + 8 + 8 + 4);
        stream_prepare.extend_from_slice(b"g\0");
        let cases = [
            ("StreamStart", true, stream_start(42)),
            ("StreamStop", false, vec![b'E']),
            ("StreamCommit", true, fixed(b'c', 4 + 1 + 8 + 8 + 8)),
            ("StreamAbort", true, fixed(b'A', 4 + 4)),
            ("StreamPrepare", true, stream_prepare),
        ];

        let mut refused = 0;
        let mut accepted = Vec::new();
        for (expected_name, open_first, frame) in cases {
            let mut decoder = pgoutput::Decoder::new();
            if open_first {
                decoder
                    .decode(&stream_start(41))
                    .expect("the control StreamStart opens one block");
            }
            let state_before = decoder.stream_xid();

            match decoder.decode(&frame) {
                Err(pgoutput::DecodeError::InvalidStreamSequence { message, .. }) => {
                    assert_eq!(message, expected_name);
                    assert_eq!(
                        decoder.stream_xid(),
                        state_before,
                        "refusing {expected_name} must not change xid-prefix state"
                    );
                    refused += 1;

                    if let Some(open_xid) = state_before {
                        let carried_xid = open_xid + 1;
                        let mut insert = vec![b'I'];
                        insert.extend_from_slice(&carried_xid.to_be_bytes());
                        insert.extend_from_slice(&7u32.to_be_bytes());
                        insert.push(b'N');
                        insert.extend_from_slice(&0u16.to_be_bytes());
                        match decoder
                            .decode(&insert)
                            .expect("the open block stays usable")
                        {
                            PgOutputMessage::Insert { xid, .. } => {
                                assert_eq!(xid, Some(carried_xid));
                            }
                            other => panic!("expected an in-block Insert, got {other:?}"),
                        }
                        decoder.decode(b"E").expect("the block can still stop");
                    } else {
                        decoder
                            .decode(&stream_start(43))
                            .expect("a fresh block can still start");
                        decoder.decode(b"E").expect("the fresh block can stop");
                    }
                }
                Ok(_) => accepted.push(expected_name),
                Err(other) => panic!(
                    "{expected_name} reached the wrong refusal instead of stream state: {other}"
                ),
            }
        }

        assert!(
            accepted.is_empty(),
            "{} of 5 invalid boundaries were accepted: {accepted:?}",
            accepted.len()
        );
        assert_eq!(refused, 5, "every stream boundary must be ruled on");
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
    /// rejected.
    ///
    /// It does NOT rule on the reservation, and the second half of this
    /// sentence used to say it did. The `.min(cur.len() / 4)` clamp is
    /// invisible from here: `decode` answers `UnexpectedEof` on this frame
    /// whether the capacity came from the frame or from the claimed count,
    /// so deleting the clamp leaves the assertion below green. The
    /// reservation is measured in `tests/pgoutput_allocation.rs`, which
    /// watches the process's own peak address space instead - the sibling
    /// above (`pgoutput_decode_accepts_a_truncate_larger_than_any_fixed_cap`)
    /// already carried that exclusion; this one kept the claim.
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

    fn replication_connection_over(
        unread: Vec<u8>,
    ) -> ReplicationConnection<ScriptedPeer, ScriptedPeer> {
        ReplicationConnection {
            stream: BufStream::new(MaybeTlsStream::Raw(ScriptedPeer { unread })),
            parameters: HashMap::new(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        }
    }

    fn copy_both_response(body: &[u8]) -> Vec<u8> {
        let mut frame = vec![COPY_BOTH_RESPONSE_TAG];
        frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    /// CopyBothResponse has the same metadata fields as CopyInResponse and
    /// CopyOutResponse. This path owns its framer because postgres-protocol
    /// does not decode `W`, so it must apply the same validation itself.
    #[compio::test]
    async fn malformed_copy_both_response_metadata_is_rejected() {
        let malformed: &[(&str, &[u8])] = &[
            ("empty body", b""),
            ("short fixed fields", b"\x00\x00"),
            ("missing column code", b"\x00\x00\x01"),
            ("surplus column code", b"\x00\x00\x00\x00\x00"),
            ("invalid overall code", b"\x02\x00\x00"),
            ("invalid column code", b"\x00\x00\x01\x00\x02"),
            ("binary column in text copy", b"\x00\x00\x01\x00\x01"),
        ];

        let mut ruled_on = 0usize;
        for (case, body) in malformed {
            ruled_on += 1;
            let connection = replication_connection_over(copy_both_response(body));
            let outcome = connection
                .start_logical_replication(StartReplicationOptions {
                    slot_name: "slot",
                    ..Default::default()
                })
                .await;
            let error = match outcome {
                Ok(_) => panic!("{case} was accepted in CopyBothResponse"),
                Err(error) => error,
            };
            let chain = std::iter::successors(std::error::Error::source(&error), |source| {
                std::error::Error::source(*source)
            })
            .fold(error.to_string(), |chain, source| {
                format!("{chain}: {source}")
            });
            assert!(
                chain.contains("COPY") && chain.contains("response"),
                "{case} produced an unnamed CopyBoth error: {chain}"
            );
        }
        assert_eq!(ruled_on, 7, "the malformed CopyBoth matrix shrank");
    }

    /// The returned stream retains the format metadata instead of discarding
    /// it, matching the information libpq keeps on its COPY result.
    #[compio::test]
    async fn copy_both_response_metadata_is_exposed() {
        let connection =
            replication_connection_over(copy_both_response(b"\x01\x00\x02\x00\x00\x00\x01"));
        let stream = connection
            .start_logical_replication(StartReplicationOptions {
                slot_name: "slot",
                ..Default::default()
            })
            .await
            .expect("a well-formed CopyBothResponse was rejected");
        assert_eq!(stream.format(), crate::CopyFormat::Binary);
        assert_eq!(
            stream.column_formats(),
            &[crate::CopyFormat::Text, crate::CopyFormat::Binary]
        );
    }

    /// A peer that accepts a configured number of writes, writes part of the
    /// next frame, fails the remainder, and is healthy for every write after
    /// that.
    ///
    /// The recovery is the whole point. A peer that simply stayed broken would
    /// make the test below pass on the unfixed driver too, because the second
    /// call would fail on its own write rather than on the refusal -- and a
    /// dead socket is the case where none of this matters. Healing the peer
    /// isolates the only question worth asking: does the DRIVER still consider
    /// the stream usable after it left a fragment of a frame on the wire.
    ///
    /// This does not contradict the note on `ScriptedPeer` above. The subject
    /// here is not how many bytes reached a server; it is what the driver does
    /// with a write error, which no real peer is needed to produce.
    struct WriteFailingPeer {
        writes: usize,
        writes_before_failure: usize,
        unread: Vec<u8>,
    }

    impl compio::io::AsyncRead for WriteFailingPeer {
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

    impl compio::io::AsyncWrite for WriteFailingPeer {
        async fn write<B: compio::buf::IoBuf>(
            &mut self,
            buf: B,
        ) -> compio::buf::BufResult<usize, B> {
            self.writes += 1;
            let len = compio::buf::IoBuf::buf_len(&buf);
            let partial_write = self.writes_before_failure + 1;
            match self.writes {
                // Accept half the frame, so `write_all` comes back for the rest.
                write if write == partial_write => compio::buf::BufResult(Ok(len / 2), buf),
                // ... and fail it. `BufStream::flush` took the frame out of the
                // write buffer before awaiting, so the tail is now gone.
                write if write == partial_write + 1 => compio::buf::BufResult(
                    Err(std::io::Error::other("scripted mid-frame write failure")),
                    buf,
                ),
                _ => compio::buf::BufResult(Ok(len), buf),
            }
        }
        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn stream_over_failing_writer() -> ReplicationStream<WriteFailingPeer, WriteFailingPeer> {
        ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(WriteFailingPeer {
                writes: 0,
                writes_before_failure: 0,
                unread: Vec::new(),
            })),
            lsn: LsnTracker::new(0),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        }
    }

    fn stream_over_failing_writer_with_unread(
        unread: Vec<u8>,
    ) -> ReplicationStream<WriteFailingPeer, WriteFailingPeer> {
        ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(WriteFailingPeer {
                writes: 0,
                writes_before_failure: 0,
                unread,
            })),
            lsn: LsnTracker::new(0),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        }
    }

    fn replication_connection_over_failing_writer(
        unread: Vec<u8>,
        writes_before_failure: usize,
    ) -> ReplicationConnection<WriteFailingPeer, WriteFailingPeer> {
        ReplicationConnection {
            stream: BufStream::new(MaybeTlsStream::Raw(WriteFailingPeer {
                writes: 0,
                writes_before_failure,
                unread,
            })),
            parameters: HashMap::new(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        }
    }

    /// A real socket whose first write fails locally while its read direction
    /// remains live. `ConnectionRelease` duplicates this socket before it is
    /// wrapped, so a test can distinguish logical poison from physical
    /// shutdown while the owning replication object is still alive.
    struct ReleaseBackedWriteFailure {
        socket: compio::net::TcpStream,
        failed: bool,
    }

    impl compio::io::AsyncRead for ReleaseBackedWriteFailure {
        async fn read<B: compio::buf::IoBufMut>(
            &mut self,
            buf: B,
        ) -> compio::buf::BufResult<usize, B> {
            self.socket.read(buf).await
        }
    }

    impl compio::io::AsyncWrite for ReleaseBackedWriteFailure {
        async fn write<B: compio::buf::IoBuf>(
            &mut self,
            buf: B,
        ) -> compio::buf::BufResult<usize, B> {
            if !self.failed {
                self.failed = true;
                return compio::buf::BufResult(
                    Err(std::io::Error::other("scripted socket write failure")),
                    buf,
                );
            }
            self.socket.write(buf).await
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.socket.flush().await
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            self.socket.shutdown().await
        }
    }

    async fn release_backed_failing_transport() -> (
        ReleaseBackedWriteFailure,
        ConnectionRelease,
        compio::net::TcpStream,
    ) {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind release-backed writer listener");
        let address = listener
            .local_addr()
            .expect("release-backed writer address");
        let accepting = compio::runtime::spawn(async move {
            listener
                .accept()
                .await
                .expect("accept release-backed writer peer")
                .0
        });
        let socket = compio::net::TcpStream::connect(address)
            .await
            .expect("connect release-backed writer");
        let peer = accepting.await.expect("accept release-backed writer task");
        let release =
            ConnectionRelease::dup_of(&socket).expect("duplicate release-backed writer socket");
        (
            ReleaseBackedWriteFailure {
                socket,
                failed: false,
            },
            release,
            peer,
        )
    }

    async fn assert_release_shut_down_peer(peer: &mut compio::net::TcpStream, arm: &str) {
        let compio::buf::BufResult(result, _) =
            compio::time::timeout(std::time::Duration::from_secs(1), peer.read(vec![0u8; 1]))
                .await
                .unwrap_or_else(|_| panic!("{arm} left its release-backed peer open"));
        match result {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::NotConnected
                ) => {}
            Ok(read) => panic!("{arm} sent {read} byte(s) instead of shutting down"),
            Err(error) => panic!("{arm} peer shutdown produced {error}"),
        }
    }

    /// CopyBoth remains an ordinary protocol response boundary for
    /// asynchronous backend messages. A status change or notification can be
    /// coalesced ahead of the ErrorResponse which actually ends replication;
    /// neither asynchronous frame may replace that server diagnosis.
    #[compio::test]
    async fn copy_both_preserves_an_error_behind_asynchronous_messages() {
        let mut notification = 17i32.to_be_bytes().to_vec();
        notification.extend_from_slice(b"scripted_channel\0scripted payload\0");

        let mut wire = startup_frame(PARAMETER_STATUS_TAG, b"TimeZone\0UTC\0");
        wire.extend_from_slice(&startup_frame(NOTIFICATION_RESPONSE_TAG, &notification));
        wire.extend_from_slice(&error_response_message(&[
            (b'S', "ERROR"),
            (b'C', "55000"),
            (b'M', "scripted replication stream refusal"),
        ]));
        let mut stream = stream_over_failing_writer_with_unread(wire);

        let error = stream
            .next()
            .await
            .expect_err("the scripted replication stream ended with an error");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("55000"),
            "replication stream discarded SQLSTATE 55000 behind asynchronous messages: {chain}"
        );
    }

    #[compio::test]
    async fn identify_system_write_failure_shuts_down_its_release_handle() {
        let (transport, release, mut peer) = release_backed_failing_transport().await;
        let mut connection = ReplicationConnection {
            stream: BufStream::new(MaybeTlsStream::<_, ReleaseBackedWriteFailure>::Raw(
                transport,
            )),
            parameters: HashMap::new(),
            in_flight: InFlight::default(),
            release: Some(release),
            cancel_token: test_cancel_token(),
        };

        let error = connection
            .identify_system()
            .await
            .expect_err("the scripted IDENTIFY_SYSTEM write succeeded");
        assert!(!error.is_cancelled(), "the first failure must be the write");
        assert_release_shut_down_peer(&mut peer, "IDENTIFY_SYSTEM write failure").await;
        assert!(
            connection.in_flight.poisoned,
            "the connection was dropped before its release handle was observed"
        );
    }

    #[compio::test]
    async fn automatic_keepalive_write_failure_shuts_down_its_release_handle() {
        let (transport, release, mut peer) = release_backed_failing_transport().await;
        let mut stream = ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::<_, ReleaseBackedWriteFailure>::Raw(
                transport,
            )),
            lsn: LsnTracker::new(0),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: Some(release),
            cancel_token: test_cancel_token(),
        };
        let mut keepalive = vec![PRIMARY_KEEPALIVE_TAG];
        keepalive.extend_from_slice(&0x16B_4000u64.to_be_bytes());
        keepalive.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        keepalive.push(1);
        let compio::buf::BufResult(written, _) = peer
            .write_all(startup_frame(COPY_DATA_TAG, &keepalive))
            .await;
        written.expect("write reply-requesting keepalive");
        peer.flush()
            .await
            .expect("flush reply-requesting keepalive");

        let error = stream
            .next()
            .await
            .expect_err("the automatic keepalive reply write succeeded");
        assert!(!error.is_cancelled(), "the first failure must be the write");
        assert_release_shut_down_peer(&mut peer, "automatic keepalive write failure").await;
        assert!(
            stream.in_flight.poisoned,
            "the stream was dropped before its release handle was observed"
        );
    }

    #[compio::test]
    async fn manual_status_write_failure_shuts_down_its_release_handle() {
        let (transport, release, mut peer) = release_backed_failing_transport().await;
        let mut stream = ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::<_, ReleaseBackedWriteFailure>::Raw(
                transport,
            )),
            lsn: LsnTracker::new(0),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: Some(release),
            cancel_token: test_cancel_token(),
        };

        let error = stream
            .send_standby_status_update(false)
            .await
            .expect_err("the scripted manual status write succeeded");
        assert!(!error.is_cancelled(), "the first failure must be the write");
        assert_release_shut_down_peer(&mut peer, "manual status write failure").await;
        assert!(
            stream.in_flight.poisoned,
            "the stream was dropped before its release handle was observed"
        );
    }

    /// A standby status update that fails part-way through its flush must
    /// retire the stream, exactly as a failed read does.
    ///
    /// `BufStream::flush` calls `write_buf.split()` BEFORE it awaits, so a
    /// `write_all` that consumes part of the frame and then errors discards the
    /// remainder: a fragment of a 39-byte CopyData frame is on the wire and the
    /// rest exists nowhere. `send_standby_status_update` nonetheless called
    /// `in_flight.leave()` unconditionally and handed the stream back as if
    /// nothing had happened, so the next update appended a whole frame after
    /// that fragment and the walsender read our frame's middle as a frame's
    /// start.
    ///
    /// `InFlight`'s own documentation describes this hazard for a CANCELLED
    /// write, and cancellation is handled -- the dropped future never reaches
    /// the clear, so `busy` stays set. The error return was the same hazard
    /// through a path the guard did not cover. `next` already retires the
    /// stream on framing I/O failures; this is that rule applied to the other
    /// direction.
    #[compio::test]
    async fn a_standby_update_that_fails_mid_flush_retires_the_stream() {
        let mut stream = stream_over_failing_writer();

        let first = stream
            .send_standby_status_update(false)
            .await
            .expect_err("the scripted peer failed the second half of the frame");
        assert!(
            !first.is_cancelled(),
            "the first failure is the write itself, not a refusal: {first}"
        );

        let second = stream
            .send_standby_status_update(false)
            .await
            .expect_err("a stream carrying half a frame must not accept another");
        assert!(
            second.is_cancelled(),
            "the driver wrote a second frame after a fragment instead of refusing: {second}"
        );
    }

    /// The read which delivered a reply-requesting keepalive can over-read a
    /// following ErrorResponse into the userspace buffer. If the automatic
    /// StandbyStatusUpdate then fails to flush, that already-available server
    /// diagnosis outranks the local write symptom.
    #[compio::test]
    async fn an_automatic_keepalive_write_failure_preserves_a_buffered_server_error() {
        let mut keepalive = vec![PRIMARY_KEEPALIVE_TAG];
        keepalive.extend_from_slice(&0x16B_4000u64.to_be_bytes());
        keepalive.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        keepalive.push(1);

        let mut wire = startup_frame(COPY_DATA_TAG, &keepalive);
        wire.extend_from_slice(&error_response_message(&[
            (b'S', "FATAL"),
            (b'C', "57P01"),
            (b'M', "terminating connection due to administrator command"),
        ]));
        let mut stream = stream_over_failing_writer_with_unread(wire);

        let error = stream
            .next()
            .await
            .expect_err("the automatic keepalive reply write was scripted to fail");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "automatic keepalive reply discarded SQLSTATE 57P01: {chain}"
        );

        let retry = stream
            .next()
            .await
            .expect_err("a failed automatic keepalive reply left the stream reusable");
        assert!(
            retry.is_cancelled(),
            "the failed automatic keepalive reply did not retire the stream: {retry}"
        );
    }

    /// A preceding `next` can over-read an ErrorResponse behind the CopyData
    /// it returns. If a caller then sends a manual StandbyStatusUpdate and the
    /// write fails, that buffered server diagnosis outranks the local write
    /// symptom just as it does for an automatic keepalive reply.
    #[compio::test]
    async fn a_manual_status_write_failure_preserves_a_buffered_server_error() {
        let mut xlog_data = vec![XLOG_DATA_TAG];
        xlog_data.extend_from_slice(&0x16B_3000u64.to_be_bytes());
        xlog_data.extend_from_slice(&0x16B_4000u64.to_be_bytes());
        xlog_data.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        xlog_data.extend_from_slice(b"payload");

        let mut wire = startup_frame(COPY_DATA_TAG, &xlog_data);
        wire.extend_from_slice(&error_response_message(&[
            (b'S', "FATAL"),
            (b'C', "57P01"),
            (b'M', "terminating connection due to administrator command"),
        ]));
        let mut stream = stream_over_failing_writer_with_unread(wire);

        let message = stream
            .next()
            .await
            .expect("the XLogData preceding the buffered error was rejected")
            .expect("the XLogData preceding the buffered error ended the stream");
        assert!(matches!(message, ReplicationMessage::XLogData { .. }));

        let error = stream
            .send_standby_status_update(false)
            .await
            .expect_err("the manual status write was scripted to fail");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "manual standby status discarded SQLSTATE 57P01: {chain}"
        );

        let retry = stream
            .next()
            .await
            .expect_err("a failed manual status write left the stream reusable");
        assert!(
            retry.is_cancelled(),
            "the failed manual status write did not retire the stream: {retry}"
        );
    }

    /// A command response can end at ReadyForQuery while the same read has
    /// already buffered a later ErrorResponse. If the next simple-query write
    /// fails, report that server diagnosis instead of the local write symptom.
    #[compio::test]
    async fn a_simple_query_write_failure_preserves_a_buffered_server_error() {
        let fields = [
            Some("7012345678901234567"),
            Some("3"),
            Some("0/16B3750"),
            Some("zeroship"),
        ];
        let mut row = Vec::new();
        row.extend_from_slice(&u16::try_from(fields.len()).unwrap().to_be_bytes());
        for field in fields {
            let field = field.expect("the fixture has no NULL fields");
            row.extend_from_slice(&i32::try_from(field.len()).unwrap().to_be_bytes());
            row.extend_from_slice(field.as_bytes());
        }

        let mut wire = startup_frame(b'D', &row);
        wire.extend_from_slice(&startup_frame(b'C', b"IDENTIFY_SYSTEM\0"));
        wire.extend_from_slice(&startup_frame(b'Z', b"I"));
        wire.extend_from_slice(&error_response_message(&[
            (b'S', "FATAL"),
            (b'C', "57P01"),
            (b'M', "terminating connection due to administrator command"),
        ]));
        let mut connection = replication_connection_over_failing_writer(wire, 1);

        let identity = connection
            .identify_system()
            .await
            .expect("the first IDENTIFY_SYSTEM response was rejected");
        assert_eq!(identity.systemid, "7012345678901234567");

        let error = connection
            .identify_system()
            .await
            .expect_err("the second simple-query write was scripted to fail");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "replication command write discarded SQLSTATE 57P01: {chain}"
        );

        let retry = connection
            .identify_system()
            .await
            .expect_err("a failed replication command write left the connection reusable");
        assert!(
            retry.is_cancelled(),
            "the failed replication command write did not retire the connection: {retry}"
        );
    }

    /// Backend CopyDone and the ErrorResponse raised while ending CopyBoth can
    /// arrive in one read. If the frontend CopyDone acknowledgment then fails
    /// to flush, the already-buffered server diagnosis still outranks that
    /// local write symptom.
    #[compio::test]
    async fn a_copy_done_write_failure_preserves_a_buffered_server_error() {
        let mut wire = startup_frame(COPY_DONE_TAG, b"");
        wire.extend_from_slice(&error_response_message(&[
            (b'S', "FATAL"),
            (b'C', "57P01"),
            (b'M', "terminating connection due to administrator command"),
        ]));
        let mut stream = stream_over_failing_writer_with_unread(wire);

        let error = stream
            .next()
            .await
            .expect_err("the CopyDone acknowledgment write was scripted to fail");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "CopyDone acknowledgment discarded SQLSTATE 57P01: {chain}"
        );

        let retry = stream
            .next()
            .await
            .expect_err("a failed CopyDone acknowledgment left the stream reusable");
        assert!(
            retry.is_cancelled(),
            "the failed CopyDone acknowledgment did not retire the stream: {retry}"
        );
    }

    /// Randomised CopyBoth frames against the bespoke replication framer.
    ///
    /// `codec.rs` has `tests/frame_fuzz.rs`; this framer has had nothing. It is
    /// a SEPARATE, hand-rolled framer -- [`read_header`] plus `next_inner` --
    /// with its own length arithmetic ([`WireHeader::body_len`] subtracts 4 and
    /// documents that underflowing it hands a `usize::MAX`-ish size to
    /// `split_to`) and its own index maths on the `XLogData` and
    /// `PrimaryKeepalive` bodies. None of it is reachable from the codec
    /// corpus, because a replication stream never goes through
    /// `Message::parse`.
    ///
    /// Same bargain as `tests/frame_fuzz.rs`: weak per-case assertions, many
    /// cases. ASSERTED -- the framer terminates, does not panic, and once it
    /// has REFUSED the stream (`Error::cancelled`, which only `InFlight::enter`
    /// produces and which nothing clears) it never decodes another message.
    /// NOT asserted: that random bytes produce an error, because some generated
    /// frames are legitimate and a test demanding failure would be wrong about
    /// the protocol rather than about the driver.
    ///
    /// THE INVARIANT IS KEYED TO THE REFUSAL, NOT TO ERRORS IN GENERAL, and
    /// getting that wrong is how this test was first written. "An error means
    /// the framer lost sync" is false for a whole class of arms: an empty
    /// `CopyData`, an `XLogData` or `PrimaryKeepalive` under its size floor, an
    /// unknown sub-tag, and a server-sent `ErrorResponse` all return `Err`
    /// AFTER `split_to` has consumed the body, so the wire is still aligned and
    /// the next frame legitimately decodes. The first version of this generator
    /// failed on case 134 for exactly that, and the driver was right.
    /// `Error::cancelled` is the only signal that means "this stream is
    /// finished", because it is the one the poison flag produces.
    #[compio::test]
    async fn generated_copyboth_frames_never_hang_or_panic_the_framer() {
        /// Fixed so the corpus is identical on every machine. Changing it
        /// explores a different corpus; it cannot hide a failure, because the
        /// failing case prints its own seed.
        const ROOT_SEED: u64 = 0x7a1e_c0de_5eed_1234;
        const CASES: u32 = 192;
        /// A generated stream is finite, so the framer reaches EOF well inside
        /// this. It turns a hang into a failure; it does not pace anything.
        const MAX_FRAMES_READ: usize = 24;

        /// xorshift64*, inline so this adds no dependency (as in
        /// `tests/frame_fuzz.rs`).
        struct Rng(u64);
        impl Rng {
            fn new(seed: u64) -> Self {
                Self(if seed == 0 {
                    0x9e37_79b9_7f4a_7c15
                } else {
                    seed
                })
            }
            fn next_u64(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.0 = x;
                x.wrapping_mul(0x2545_f491_4f6c_dd1d)
            }
            fn below(&mut self, bound: u32) -> u32 {
                u32::try_from(self.next_u64() % u64::from(bound)).expect("bound fits")
            }
            fn byte(&mut self) -> u8 {
                u8::try_from(self.next_u64() & 0xff).expect("masked to a byte")
            }
        }

        /// The tags this framer dispatches on, so most of the budget lands
        /// inside the arms rather than on the unknown-tag refusal.
        const FRAMER_TAGS: &[u8] = &[
            COPY_DATA_TAG,
            COPY_DONE_TAG,
            ERROR_RESPONSE_TAG,
            NOTICE_RESPONSE_TAG,
        ];
        /// `CopyData` sub-tags, including the two frontend-only ones a server
        /// has no business sending back.
        const SUB_TAGS: &[u8] = &[
            XLOG_DATA_TAG,
            PRIMARY_KEEPALIVE_TAG,
            STANDBY_STATUS_UPDATE_TAG,
            HOT_STANDBY_FEEDBACK_TAG,
        ];

        fn generate(rng: &mut Rng) -> Vec<u8> {
            let mut out = Vec::new();
            for _ in 0..1 + rng.below(4) {
                let tag = if rng.below(5) == 0 {
                    rng.byte()
                } else {
                    FRAMER_TAGS[rng.below(4) as usize]
                };

                let mut body = Vec::new();
                if tag == COPY_DATA_TAG && rng.below(4) != 0 {
                    body.push(SUB_TAGS[rng.below(4) as usize]);
                    // Land on, just under and just over the 25- and 18-byte
                    // floors the XLogData and PrimaryKeepalive arms check: the
                    // interesting index maths is exactly at those boundaries.
                    let payload = match rng.below(4) {
                        0 => 0,
                        1 => 16 + rng.below(3) as usize,
                        2 => 23 + rng.below(4) as usize,
                        _ => rng.below(40) as usize,
                    };
                    for _ in 0..payload {
                        body.push(rng.byte());
                    }
                } else {
                    for _ in 0..rng.below(24) {
                        body.push(rng.byte());
                    }
                }

                // Usually honest, sometimes a lie either way, and sometimes
                // below 4 -- the underflow `read_header`'s floor check exists
                // to refuse.
                let honest = u32::try_from(body.len() + 4).expect("body fits");
                let declared = match rng.below(8) {
                    0 => rng.below(4),
                    1 => honest + 1 + rng.below(64),
                    2 => honest.saturating_sub(1 + rng.below(4)),
                    _ => honest,
                };

                out.push(tag);
                out.extend_from_slice(&declared.to_be_bytes());
                out.extend_from_slice(&body);
            }
            out
        }

        /// Anchor the random corpus with both legal CopyData forms followed by
        /// a body that ends one byte early. The first call must decode, the
        /// second must report the framing error, and every later call must see
        /// the poisoned stream. Random bytes rarely satisfy the exact
        /// PrimaryKeepalive layout, so relying on them alone lets stricter
        /// protocol validation silently erase the invariant's positive cases.
        fn decode_then_refuse(case: u32) -> Vec<u8> {
            let mut body = if case.is_multiple_of(2) {
                let mut keepalive = vec![PRIMARY_KEEPALIVE_TAG];
                keepalive.extend_from_slice(&0x16B_4000u64.to_be_bytes());
                keepalive.extend_from_slice(&700_000_000_000i64.to_be_bytes());
                keepalive.push(0);
                keepalive
            } else {
                let mut xlog = vec![XLOG_DATA_TAG];
                xlog.extend_from_slice(&0x16B_4000u64.to_be_bytes());
                xlog.extend_from_slice(&0x16B_4000u64.to_be_bytes());
                xlog.extend_from_slice(&700_000_000_000i64.to_be_bytes());
                xlog
            };

            let mut wire = vec![COPY_DATA_TAG];
            wire.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
            wire.append(&mut body);

            wire.push(COPY_DATA_TAG);
            wire.extend_from_slice(&6u32.to_be_bytes());
            wire.push(XLOG_DATA_TAG);
            wire
        }

        let mut total_decoded = 0usize;
        let mut xlog_decoded = 0usize;
        let mut keepalives_decoded = 0usize;
        let mut total_refused = 0usize;
        let mut cases_with_a_decode = 0usize;
        for case in 0..CASES {
            let seed = ROOT_SEED ^ u64::from(case).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let mut rng = Rng::new(seed);
            let wire = if case < 8 {
                decode_then_refuse(case)
            } else {
                generate(&mut rng)
            };

            let mut stream = stream_over(wire.clone());
            let mut refused = false;
            let mut counted_this_case = false;
            for step in 0..MAX_FRAMES_READ {
                let outcome =
                    compio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                        .await
                        .unwrap_or_else(|_| {
                            panic!(
                                "case {case} (seed {seed:#x}) hung at step {step} on {wire:02x?}"
                            )
                        });

                match outcome {
                    Ok(message) => {
                        assert!(
                            !refused,
                            "case {case} (seed {seed:#x}) decoded a message after refusing \
                             the stream; the poison flag is documented as unclearable"
                        );
                        match message {
                            Some(ReplicationMessage::XLogData { .. }) => xlog_decoded += 1,
                            Some(ReplicationMessage::PrimaryKeepalive { .. }) => {
                                keepalives_decoded += 1;
                            }
                            None => break,
                        }
                        total_decoded += 1;
                        if !std::mem::replace(&mut counted_this_case, true) {
                            cases_with_a_decode += 1;
                        }
                    }
                    Err(error) => {
                        if error.is_cancelled() {
                            if !refused {
                                total_refused += 1;
                            }
                            refused = true;
                        }
                    }
                }
            }
        }
        // WHAT THE CORPUS RULED ON. The only assertion inside the loop is "no
        // message decodes after a refusal", which is vacuous unless BOTH a
        // decode and a refusal actually happen - and until 2026-08-23 there was
        // nothing after the loop, so 192 cases that all failed to produce
        // either printed exactly what a working corpus prints. That is the
        // failure the repo's own gate convention exists for (every arm declares
        // what it ruled on and a floor it must clear); this is that convention
        // applied to a fuzz loop in Rust.
        //
        // Measured 2026-08-26 over these 192 cases: 6 XLogData and 4
        // PrimaryKeepalive messages across 10 distinct cases, and 191
        // refusals. The floors sit well under the aggregate counts so ordinary
        // generator drift does not trip them, while the exact anchor counts
        // keep either legal CopyData form from disappearing silently.
        assert!(
            total_decoded >= 5,
            "the corpus decoded {total_decoded} messages, so the after-refusal invariant \
             ruled on almost nothing"
        );
        assert!(
            cases_with_a_decode >= 5,
            "only {cases_with_a_decode} of {CASES} cases decoded anything"
        );
        assert!(
            xlog_decoded >= 4,
            "the corpus decoded only {xlog_decoded} XLogData messages"
        );
        assert!(
            keepalives_decoded >= 4,
            "the corpus decoded only {keepalives_decoded} PrimaryKeepalive messages"
        );
        assert!(
            total_refused >= 50,
            "the corpus refused {total_refused} streams, so the poison path is barely exercised"
        );
    }

    /// A `ReplicationStream` reading the given bytes as if the walsender had
    /// sent them inside the CopyBoth channel.
    fn stream_over(bytes: Vec<u8>) -> ReplicationStream<ScriptedPeer, ScriptedPeer> {
        ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(ScriptedPeer { unread: bytes })),
            lsn: LsnTracker::new(0),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
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
        let mut wire = startup_frame(COPY_DONE_TAG, b"");
        wire.extend_from_slice(&startup_frame(b'C', b"COPY 0\0"));
        wire.extend_from_slice(&startup_frame(b'C', b"START_REPLICATION\0"));
        wire.extend_from_slice(&startup_frame(b'Z', b"I"));

        let mut stream = stream_over(wire);
        assert!(
            stream
                .next()
                .await
                .expect("CopyDone is a valid empty-bodied frame")
                .is_none(),
            "CopyDone ends the stream"
        );

        let retry = stream
            .next()
            .await
            .expect_err("a completed CopyBoth exchange became reusable");
        assert!(
            retry.is_cancelled(),
            "the completed CopyBoth exchange was not retired: {retry}"
        );
    }

    /// Logical replication ends with two distinct confirmations: one closes
    /// COPY and one closes START_REPLICATION. ReadyForQuery cannot make a
    /// missing, reordered, changed, or extra tag into a clean exchange.
    #[compio::test]
    async fn copy_both_completion_requires_exact_server_confirmations() {
        let invalid = [
            ("missing one CommandComplete", vec![b"COPY 0\0".as_slice()]),
            (
                "wrong COPY confirmation",
                vec![b"COPY 1\0".as_slice(), b"START_REPLICATION\0".as_slice()],
            ),
            (
                "wrong START_REPLICATION confirmation",
                vec![b"COPY 0\0".as_slice(), b"SELECT 1\0".as_slice()],
            ),
            (
                "an extra CommandComplete",
                vec![
                    b"COPY 0\0".as_slice(),
                    b"START_REPLICATION\0".as_slice(),
                    b"START_REPLICATION\0".as_slice(),
                ],
            ),
        ];

        for (description, confirmations) in invalid {
            let mut wire = startup_frame(COPY_DONE_TAG, b"");
            for confirmation in confirmations {
                wire.extend_from_slice(&startup_frame(b'C', confirmation));
            }
            wire.extend_from_slice(&startup_frame(b'Z', b"I"));

            let mut stream = stream_over(wire);
            if let Ok(None) = stream.next().await {
                panic!("a CopyBoth exchange {description} was accepted as clean");
            }
        }
    }

    /// CopyDone is exactly a tag plus Int32(4); it has no body. Accepting a
    /// payload silently changes malformed bytes into a clean CopyBoth state
    /// transition, so reject it by name and retire the stream.
    #[compio::test]
    async fn copy_done_with_a_body_is_rejected_and_retires_the_stream() {
        let mut wire = vec![COPY_DONE_TAG];
        wire.extend_from_slice(&5u32.to_be_bytes());
        wire.push(0xAA);

        let mut stream = stream_over(wire);
        let error = stream
            .next()
            .await
            .expect_err("CopyDone with a body was accepted as a clean transition");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert!(
            chain.contains("CopyDone") && chain.contains("empty body"),
            "the malformed CopyDone was not refused by name: {chain}"
        );

        let retry = stream
            .next()
            .await
            .expect_err("a stream with a malformed CopyDone became reusable");
        assert!(retry.is_cancelled(), "the malformed stream was not retired");
    }

    /// PrimaryKeepalive has one fixed 18-byte CopyData body. Accepting a
    /// suffix silently claims support for fields no replication protocol
    /// version defines, so the refusal must name the known message.
    #[compio::test]
    async fn primary_keepalive_with_a_suffix_is_rejected_by_name() {
        let mut body = vec![PRIMARY_KEEPALIVE_TAG];
        body.extend_from_slice(&0x16B_4000u64.to_be_bytes());
        body.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        body.push(0);
        body.push(0xAA);

        let mut wire = vec![COPY_DATA_TAG];
        wire.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);

        let mut stream = stream_over(wire);
        let error = stream
            .next()
            .await
            .expect_err("PrimaryKeepalive with a suffix was accepted");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert!(
            chain.contains("PrimaryKeepalive")
                && chain.contains("exactly 18 bytes")
                && chain.contains("19"),
            "the malformed keepalive was not refused by name and size: {chain}"
        );
    }

    /// The keepalive's last byte is specified as 0 or 1. Treating every
    /// nonzero value as true is conservative about replying, but it also
    /// accepts a peer value no protocol version defines.
    #[compio::test]
    async fn primary_keepalive_rejects_an_invalid_reply_flag_by_name() {
        let mut body = vec![PRIMARY_KEEPALIVE_TAG];
        body.extend_from_slice(&0x16B_4000u64.to_be_bytes());
        body.extend_from_slice(&700_000_000_000i64.to_be_bytes());
        body.push(2);

        let mut wire = vec![COPY_DATA_TAG];
        wire.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        wire.extend_from_slice(&body);

        let mut stream = stream_over(wire);
        let error = stream
            .next()
            .await
            .expect_err("PrimaryKeepalive reply_requested=2 was accepted");
        let chain = std::iter::successors(std::error::Error::source(&error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        });
        assert!(
            chain.contains("PrimaryKeepalive")
                && chain.contains("reply_requested")
                && chain.contains("0x02")
                && chain.contains("0 or 1"),
            "the malformed flag was not refused by name and value: {chain}"
        );
    }

    /// An unhandled tag must retire the stream too, because its body was
    /// never consumed.
    ///
    /// `read_header` has already taken the five header bytes by the time the
    /// tag is matched. Every other arm splits `header.body_len()` off before
    /// returning; this one did not, and did not poison either, so the next
    /// call read its length field out of the SKIPPED BODY. The framer then
    /// resynchronises onto payload, and if those bytes happen to decode as an
    /// XLogData its `wal_end` becomes `observe_received` and, through the next
    /// standby status update, a `flush_lsn` - a durability promise the server
    /// acts on by recycling WAL. Repeating one error forever was safe by
    /// comparison.
    #[compio::test]
    async fn an_unhandled_tag_retires_the_stream_rather_than_resynchronising() {
        // A ReadyForQuery, which has no meaning inside CopyBoth, followed by a
        // well-formed keepalive. If the framer resyncs it will read the second
        // frame from the wrong offset.
        let mut wire = vec![b'Z'];
        wire.extend_from_slice(&5u32.to_be_bytes());
        wire.push(b'I');
        let mut keepalive = vec![PRIMARY_KEEPALIVE_TAG];
        keepalive.extend_from_slice(&0x0000_0000_dead_beefu64.to_be_bytes());
        keepalive.extend_from_slice(&0i64.to_be_bytes());
        keepalive.push(0);
        wire.push(COPY_DATA_TAG);
        wire.extend_from_slice(&(u32::try_from(keepalive.len() + 4).unwrap()).to_be_bytes());
        wire.extend_from_slice(&keepalive);

        let mut stream = stream_over(wire);
        let first = stream
            .next()
            .await
            .expect_err("an unhandled tag is not decodable");
        assert!(
            !first.is_cancelled(),
            "the first failure is the unhandled tag itself, not a refusal"
        );

        let second = stream
            .next()
            .await
            .expect_err("a stream whose framer skipped a body must not be reusable");
        assert!(
            second.is_cancelled(),
            "the framer resynchronised onto the skipped body instead of refusing: {second}"
        );
        assert_eq!(
            stream.last_received_lsn(),
            0,
            "a resynchronised read advanced the received LSN from payload bytes"
        );
    }

    /// A frame the framer cannot resynchronise from must poison the stream,
    /// not repeat forever.
    ///
    /// `read_header` returns the floor error BEFORE consuming the five header
    /// bytes, and `next` gives the stream back unconditionally, so the same
    /// bad header is re-read on the next call: identical error, no forward
    /// progress, no way for a caller to tell it apart from something
    /// transient. A consumer whose policy is "log and retry" spins on it.
    ///
    /// This is the same rule the cancellation guard already applies. A framer
    /// that has lost sync does not know where the next real frame starts, so
    /// there is nothing to resynchronise TO - refusing is the only honest
    /// answer.
    #[compio::test]
    async fn a_frame_the_framer_cannot_resynchronise_from_poisons_the_stream() {
        let mut wire = vec![COPY_DATA_TAG];
        wire.extend_from_slice(&0u32.to_be_bytes());

        let mut stream = stream_over(wire);
        let first = stream
            .next()
            .await
            .expect_err("a length below the protocol minimum is not decodable");
        assert!(
            !first.is_cancelled(),
            "the first failure is the framing error itself, not a refusal"
        );

        let second = stream
            .next()
            .await
            .expect_err("a stream that lost framing must not be reusable");
        assert!(
            second.is_cancelled(),
            "the stream re-read the same bad header instead of refusing: {second}"
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
        let accepting =
            compio::runtime::spawn(async move { listener.accept().await.expect("accept").0 });
        let client = compio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the listener");
        let server = accepting.await.expect("accept task");
        let release = ConnectionRelease::dup_of(&client)
            .expect("duplicate replication timeout release handle");

        (
            ReplicationStream {
                stream: BufStream::new(MaybeTlsStream::Raw(client)),
                lsn: LsnTracker::new(0),
                copy_response: Default::default(),
                in_flight: InFlight::default(),
                release: Some(release),
                cancel_token: test_cancel_token(),
            },
            server,
        )
    }

    #[compio::test]
    async fn an_abandoned_cancel_poisons_the_target_replication_stream() {
        let (mut stream, _wal_peer) = silent_peer().await;

        let listener = compio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replication CancelRequest listener");
        let address = listener
            .local_addr()
            .expect("replication CancelRequest address");
        let accepting = compio::runtime::spawn(async move {
            listener
                .accept()
                .await
                .expect("accept replication CancelRequest")
                .0
        });
        let cancel_socket = compio::net::TcpStream::connect(address)
            .await
            .expect("connect replication CancelRequest");
        let mut cancel_peer = accepting.await.expect("accept CancelRequest task");
        let (packet_seen_tx, packet_seen_rx) = futures_channel::oneshot::channel();
        let (release_peer_tx, release_peer_rx) = futures_channel::oneshot::channel();
        let peer = compio::runtime::spawn(async move {
            let compio::BufResult(result, packet) = cancel_peer.read_exact(vec![0; 16]).await;
            result.expect("read replication CancelRequest packet");
            packet_seen_tx
                .send(packet)
                .expect("report replication CancelRequest packet");
            release_peer_rx
                .await
                .expect("release replication CancelRequest peer");
        });

        let token = stream.cancel_token();
        let cancel = Box::pin(token.cancel_query_raw(cancel_socket, NoTls));
        let packet_seen = Box::pin(packet_seen_rx);
        let cancel = match futures_util::future::select(cancel, packet_seen).await {
            futures_util::future::Either::Left((result, _)) => {
                panic!("replication CancelRequest returned before peer EOF: {result:?}")
            }
            futures_util::future::Either::Right((packet, cancel)) => {
                assert_eq!(
                    packet
                        .expect("CancelRequest peer dropped its packet report")
                        .len(),
                    16
                );
                cancel
            }
        };
        drop(cancel);

        let result = stream.send_standby_status_update(false).await;
        release_peer_tx
            .send(())
            .expect("release replication CancelRequest peer");
        peer.await.expect("replication CancelRequest peer panicked");

        let error = result.expect_err(
            "dropping an in-flight replication CancelRequest left the target stream usable",
        );
        assert!(
            error.is_cancelled(),
            "the abandoned CancelRequest refusal was not cancellation: {error}"
        );
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

    /// A peer that KEEPS what the driver wrote, so a test can assert what
    /// actually went upstream.
    ///
    /// Every other peer in this module throws writes away: `ScriptedPeer`
    /// returns the length and discards the bytes, `WriteFailingPeer` counts
    /// calls without retaining them, and the socket-backed tests never read the
    /// far end. So until 2026-08-23 NOTHING here asserted what a
    /// `StandbyStatusUpdate` looks like on the wire -- measured by corrupting
    /// `encode_standby_status_update` four ways at once (wrong `CopyData` tag,
    /// declared length 999, wrong sub-tag, write and flush LSNs swapped) and
    /// watching all 177 lib tests stay green.
    struct CapturingPeer {
        unread: Vec<u8>,
        written: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }

    impl compio::io::AsyncRead for CapturingPeer {
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

    impl compio::io::AsyncWrite for CapturingPeer {
        async fn write<B: compio::buf::IoBuf>(
            &mut self,
            buf: B,
        ) -> compio::buf::BufResult<usize, B> {
            self.written
                .borrow_mut()
                .extend_from_slice(compio::buf::IoBuf::as_init(&buf));
            let len = compio::buf::IoBuf::buf_len(&buf);
            compio::buf::BufResult(Ok(len), buf)
        }
        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A walsender sets the final byte of PrimaryKeepalive to 1 when it needs
    /// feedback now. Merely exposing that byte makes every generic consumer
    /// responsible for a timing-critical protocol obligation and lets an idle
    /// stream be terminated by `wal_sender_timeout` if the caller overlooks
    /// it. `next` must answer before returning the informational message.
    #[compio::test]
    async fn a_requested_primary_keepalive_is_answered_before_it_is_yielded() {
        let keepalive = |wal_end: u64, reply_requested: u8| {
            let mut body = vec![PRIMARY_KEEPALIVE_TAG];
            body.extend_from_slice(&wal_end.to_be_bytes());
            body.extend_from_slice(&700_000_000_000i64.to_be_bytes());
            body.push(reply_requested);

            let mut frame = vec![COPY_DATA_TAG];
            frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
            frame.extend_from_slice(&body);
            frame
        };

        let start_lsn = 0x16B_3750;
        let wal_end = 0x16B_4000;
        let written = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut stream: ReplicationStream<CapturingPeer, CapturingPeer> = ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(CapturingPeer {
                unread: keepalive(wal_end, 1),
                written: std::rc::Rc::clone(&written),
            })),
            lsn: LsnTracker::new(start_lsn),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        };

        match stream.next().await.expect("keepalive decodes") {
            Some(ReplicationMessage::PrimaryKeepalive {
                wal_end: received,
                reply_requested,
                ..
            }) => {
                assert_eq!(received, wal_end);
                assert!(reply_requested);
            }
            other => panic!("expected PrimaryKeepalive, got {other:?}"),
        }

        let frame = written.borrow().clone();
        assert_eq!(frame.len(), 39, "feedback frame was {frame:02x?}");
        assert_eq!(frame[0], COPY_DATA_TAG);
        assert_eq!(frame[5], STANDBY_STATUS_UPDATE_TAG);
        let lsn = |at: usize| u64::from_be_bytes(frame[at..at + 8].try_into().expect("8 bytes"));
        assert_eq!(lsn(6), wal_end, "write LSN must include the keepalive");
        assert_eq!(lsn(14), start_lsn, "keepalive receipt is not a flush");
        assert_eq!(lsn(22), start_lsn, "keepalive receipt is not an apply");
        assert_eq!(frame[38], 0, "the response must not request another reply");

        let unsolicited = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut stream: ReplicationStream<CapturingPeer, CapturingPeer> = ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(CapturingPeer {
                unread: keepalive(wal_end, 0),
                written: std::rc::Rc::clone(&unsolicited),
            })),
            lsn: LsnTracker::new(start_lsn),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        };
        stream
            .next()
            .await
            .expect("keepalive decodes")
            .expect("one keepalive");
        assert!(
            unsolicited.borrow().is_empty(),
            "reply_requested=0 must not cause unsolicited feedback"
        );
    }

    /// The bytes `send_standby_status_update` puts on the wire are the bytes
    /// PostgreSQL's protocol specifies.
    ///
    /// THIS TEST USED TO ASSERT ITS OWN LITERALS. It hand-built a 39-byte
    /// vector and then checked that vector's length, first byte, length field
    /// and sub-tag -- properties of the three lines above the assertions, not of
    /// the encoder, which it never called. Its only contact with `src/` was two
    /// constants. Its twin `xlog_data_frame_parsing_smoke` did the same in the
    /// read direction, re-extracting fields with hand-written index arithmetic
    /// rather than the parser; it is deleted rather than repaired, because
    /// `a_notice_is_skipped_and_the_next_xlog_frame_decodes` already drives the
    /// real framer over the same frame and asserts the same three fields plus
    /// `last_received_lsn`.
    ///
    /// The distinct LSNs are the point of driving the whole call rather than
    /// the free function: `standby_lsns` returns write, flush and apply in that
    /// order, and three equal values -- which the old fixture used -- cannot
    /// tell a correct encoder from one that emits them in any other order.
    #[compio::test]
    async fn a_standby_status_update_is_encoded_as_postgresql_specifies() {
        let written = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut stream: ReplicationStream<CapturingPeer, CapturingPeer> = ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(CapturingPeer {
                unread: Vec::new(),
                written: std::rc::Rc::clone(&written),
            })),
            lsn: LsnTracker::new(0x16B_3750),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        };

        // THE THREE LSNs MUST NOT ALL BE EQUAL, or their POSITIONS are not
        // pinned: `LsnTracker::new` seeds received and processed alike, so with
        // it alone every field carries the same eight bytes and swapping two of
        // them in the encoder is invisible. Verified: swapping the flush and
        // apply writes left all 37 replication tests green.
        //
        // Advancing the received position separates `write` from the other two.
        // `flush` and `apply` still cannot be told apart, and that is inherent
        // rather than an omission -- `standby_lsns` returns the processed
        // position for BOTH by definition, so no encoding can distinguish them
        // and swapping them changes nothing on the wire.
        stream.lsn.observe_received(0x16B_4000);
        let (write_lsn, flush_lsn, apply_lsn) = stream.lsn.standby_lsns();
        assert_ne!(
            write_lsn, flush_lsn,
            "the fixture must make the write position differ from the flush one, \
             or this test cannot tell the fields apart"
        );
        let before = postgres_microseconds_since_epoch();
        stream
            .send_standby_status_update(true)
            .await
            .expect("a healthy peer accepts a standby status update");
        let after = postgres_microseconds_since_epoch();

        let frame = written.borrow().clone();
        // 'd' + length(4) + 'r' + 4 x i64 + reply = 1 + 4 + 1 + 32 + 1.
        assert_eq!(frame.len(), 39, "frame was {frame:02x?}");
        assert_eq!(frame[0], COPY_DATA_TAG, "not a CopyData frame");
        assert_eq!(
            u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]),
            38,
            "the declared length must cover the body and itself, not the tag"
        );
        assert_eq!(frame[5], STANDBY_STATUS_UPDATE_TAG, "wrong sub-tag");

        let field = |at: usize| u64::from_be_bytes(frame[at..at + 8].try_into().expect("8 bytes"));
        assert_eq!(field(6), write_lsn, "write LSN is not the first field");
        assert_eq!(field(14), flush_lsn, "flush LSN is not the second field");
        assert_eq!(field(22), apply_lsn, "apply LSN is not the third field");

        // The timestamp is read from the clock inside the call, so it is pinned
        // by the interval that brackets it rather than by a literal.
        let timestamp = i64::from_be_bytes(frame[30..38].try_into().expect("8 bytes"));
        assert!(
            (before..=after).contains(&timestamp),
            "timestamp {timestamp} is outside the {before}..={after} the call ran in"
        );
        assert_eq!(frame[38], 1, "reply_requested=true must set the last byte");
    }

    /// The control for the byte above it: the same call with the flag cleared
    /// must differ in exactly that byte.
    ///
    /// Without this, `frame[38] == 1` is satisfied by an encoder that hard-codes
    /// a 1 there and ignores its argument.
    #[compio::test]
    async fn a_standby_status_update_reports_whether_a_reply_is_wanted() {
        let written = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut stream: ReplicationStream<CapturingPeer, CapturingPeer> = ReplicationStream {
            stream: BufStream::new(MaybeTlsStream::Raw(CapturingPeer {
                unread: Vec::new(),
                written: std::rc::Rc::clone(&written),
            })),
            lsn: LsnTracker::new(0x16B_3750),
            copy_response: Default::default(),
            in_flight: InFlight::default(),
            release: None,
            cancel_token: test_cancel_token(),
        };
        stream
            .send_standby_status_update(false)
            .await
            .expect("a healthy peer accepts a standby status update");
        let frame = written.borrow().clone();
        assert_eq!(frame.len(), 39, "frame was {frame:02x?}");
        assert_eq!(
            frame[38], 0,
            "reply_requested=false must clear the last byte"
        );
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
