// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Startup + auth state machine. Swaps `Framed<S, PostgresCodec>` for
// direct `BufStream<MaybeTlsStream<S, T::Stream>>` I/O built on
// `codec::write_frontend` and `codec::read_backend`. All four auth
// paths the source supports are preserved:
//
//   AuthenticationOk
//   AuthenticationCleartextPassword
//   AuthenticationMd5Password
//   AuthenticationSasl { SCRAM-SHA-256, SCRAM-SHA-256-PLUS }
//
// The `StartupStream` struct in the source wraps `Framed` + a
// `BackendMessages` iterator + a `delayed` VecDeque of async messages
// captured mid-handshake. Our equivalent is `Handshake`: same three
// fields, but `stream` is a `BufStream` and `buf` is unused because
// `read_backend` always returns a fresh batch.

use crate::buf_stream::BufStream;
use crate::client::{Client, StatementCacheSettings};
use crate::codec::{BackendMessage, BackendMessages, FrontendMessage, read_backend, write_frontend};
use crate::config::{self, AuthMethod, Config, ReplicationMode, TargetSessionAttrs};
use crate::connect_tls::{Encryption, negotiate_tls};
use crate::connection::Connection;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::{ServerVerification, TlsConnect, TlsStream};
use crate::Error;
use bytes::BytesMut;
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use futures_channel::mpsc;
use postgres_protocol::authentication;
use postgres_protocol::authentication::sasl;
use postgres_protocol::authentication::sasl::ScramSha256;
use postgres_protocol::message::backend::{AuthenticationSaslBody, DataRowBody, Message};
use postgres_protocol::message::frontend;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io;

/// How many notices the handshake will retain.
///
/// A normal PostgreSQL startup emits none, and a new session cannot ordinarily
/// receive a notification before it has issued `LISTEN`. This leaves room for
/// the extension and hook warnings a heavily-configured server emits.
const MAX_DELAYED_HANDSHAKE_MESSAGES: usize = 256;

/// How many BYTES the handshake will retain, which is the bound that matters.
///
/// The two caps are not redundant, and neither substitutes for the other. A
/// count alone leaves the peer free to choose frame SIZE: a single frame may be
/// [`MAX_MESSAGE_SIZE`](crate::buf_stream::MAX_MESSAGE_SIZE), 64 MiB, so 255 of
/// them is about 16 GiB of retained `Bytes` per connection, times the pool's
/// `max_size`. A byte budget alone would let a peer hold a slot open with
/// unlimited tiny frames. Both, or neither is a bound.
///
/// The queue starts before authentication: on the md5 and SCRAM paths no
/// password has been sent yet, and under `sslmode=disable` or a `prefer`
/// downgrade the peer's identity has not been checked at all. It remains in
/// use through the post-authentication target-session probe, so its bounds
/// must still be safe for that earlier, untrusted window.
///
/// CHARGE THE FRAME, NOT THE PARSED VIEW. `Message::parse` splits the whole
/// `tag + length + body` off the read buffer and the message owns all of it,
/// so nothing computed from the fields can stand in. A `NotificationResponse`
/// whose channel and payload are followed by 64 MiB of padding parses
/// successfully and its fields sum to six bytes; an earlier version of this
/// guard measured exactly that and admitted the full 16 GiB it was written to
/// prevent. `read_backend` hands the frame length over for this reason.
///
/// 1 MiB across the whole queue. Real handshake notices are a line of text
/// apiece, so this is orders of magnitude above anything a server sends and far
/// below anything worth holding.
const MAX_DELAYED_HANDSHAKE_BYTES: usize = 1024 * 1024;

/// Carries the handshake-time state: the wrapped stream, a cursor through
/// the currently-in-flight `BackendMessages` batch, and a deferred queue of
/// async messages (NoticeResponse / NotificationResponse) that arrive
/// mid-handshake and must be replayed to the connection task once it
/// starts.
///
/// **Why `pending` exists**: a single `read_backend` can return a batch
/// containing several messages (e.g. on successful auth PG bundles
/// `AuthenticationOk + ParameterStatus* + BackendKeyData + ReadyForQuery`
/// into one TCP segment). The handshake needs to consume them one at a
/// time. Without a persisted iterator we'd drop every message after the
/// first on each `next()` call and re-enter `read_backend`, which would
/// block waiting for messages the server already sent.
struct Handshake<S, T> {
    stream: BufStream<MaybeTlsStream<S, T>>,
    /// Unread messages from the last `read_backend` batch.
    pending: BackendMessages,
    delayed: VecDeque<Message>,
    /// Bytes retained in `delayed`. Tracked rather than recomputed so the
    /// guard stays O(1) per message.
    delayed_bytes: usize,
}

impl<S, T> Handshake<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    async fn send(&mut self, msg: FrontendMessage) -> Result<(), Error> {
        write_frontend(&mut self.stream, msg)?;
        self.stream.flush().await
    }

    /// Read one post-handshake message. Returns `None` on clean EOF.
    ///
    /// Classification of async messages (those `read_backend` returns as
    /// `BackendMessage::Async` because they arrive at the head of a
    /// batch — ParameterStatus, NoticeResponse, NotificationResponse):
    ///
    /// - `NoticeResponse` / `NotificationResponse` are deferred into
    ///   `delayed` so the connection task can replay them on its first
    ///   iteration.
    /// - `ParameterStatus` is returned inline so `read_info` can fold
    ///   it into the parameter map that becomes `Connection.parameters`.
    ///   If we deferred it instead, a caller of `Connection::parameter`
    ///   between `connect()` returning and `Connection::run` draining
    ///   `delayed_notices` would see `None` for keys the server already
    ///   sent (e.g. `server_version`).
    async fn next(&mut self) -> Result<Option<Message>, Error> {
        loop {
            // First, drain any unread messages from the previous batch —
            // only pull a fresh batch off the wire when the pending
            // iterator is empty.
            if let Some(m) = self.pending.next().map_err(Error::parse)? {
                return Ok(Some(m));
            }

            let batch = read_backend(&mut self.stream).await?;
            match batch {
                BackendMessage::Async { message: msg, frame_len } => match msg {
                    Message::NoticeResponse(_) | Message::NotificationResponse(_) => {
                        // Preserve ordering — the connection task
                        // will replay these in front of its first
                        // real read.
                        self.delay(msg, frame_len)?;
                    }
                    // ParameterStatus must be surfaced to the handshake
                    // caller so `read_info` updates the parameter map
                    // directly. Every other async-tagged message is
                    // unexpected here; return it and let the caller
                    // produce an `unexpected_message` error.
                    _ => return Ok(Some(msg)),
                },
                BackendMessage::Normal { messages, .. } => {
                    // Stash the iterator; the top of the loop will drain
                    // it one call at a time.
                    self.pending = messages;
                }
            }
        }
    }

    fn delay(&mut self, msg: Message, frame_len: usize) -> Result<(), Error> {
        if self.delayed.len() >= MAX_DELAYED_HANDSHAKE_MESSAGES {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "too many asynchronous messages during connection handshake (max \
                     {MAX_DELAYED_HANDSHAKE_MESSAGES})"
                ),
            )));
        }

        // The frame length as it came off the wire. NOT a size derived from the
        // parsed message - see `MAX_DELAYED_HANDSHAKE_BYTES` for why that
        // measures a quantity the peer controls independently of what is held.
        let retained = self.delayed_bytes.saturating_add(frame_len);
        if retained > MAX_DELAYED_HANDSHAKE_BYTES {
            return Err(Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "asynchronous messages during connection handshake exceed \
                     {MAX_DELAYED_HANDSHAKE_BYTES} bytes"
                ),
            )));
        }

        self.delayed_bytes = retained;
        self.delayed.push_back(msg);
        Ok(())
    }
}

/// Negotiate TLS if configured, drive the startup + auth exchange,
/// capture `ParameterStatus` + `BackendKeyData` up to `ReadyForQuery`,
/// and return a wired-up `(Client, Connection)` pair.
pub(crate) async fn connect_raw<S, T>(
    stream: S,
    tls: T,
    encryption: Encryption,
    has_hostname: bool,
    config: &Config,
    // Taken from the socket BEFORE it was handed to this function, because
    // `S` is generic here and only the caller knows whether it is one. The
    // client half stores it so the session ends when the client does; see
    // `crate::release`.
    release: Option<crate::release::ConnectionRelease>,
) -> Result<(Client, Connection<S, T::Stream>), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    // The negotiated transport is dropped here on purpose: a caller-owned
    // stream has no `SocketConfig`, so it can never issue a `CancelToken`
    // cancel that would need to reproduce the transport.
    let (client, connection, _negotiated) = connect_raw_with_target_session_attrs(
        stream,
        tls,
        encryption,
        has_hostname,
        config,
        TargetSessionAttrs::Any,
        release,
    )
    .await?;
    Ok((client, connection))
}

/// The normal host-routing connection path, including its session-property
/// check before the raw stream is packaged into a `Connection`.
///
/// Reports the transport the session ACTUALLY negotiated alongside the pair.
/// `encryption` is only what was attempted; a server that answers `N` to
/// `SSLRequest` continues in plaintext on the same socket, so the caller cannot
/// recover the answer from its own argument. `cancel_query` needs the real one.
pub(crate) async fn connect_raw_with_target_session_attrs<S, T>(
    stream: S,
    tls: T,
    encryption: Encryption,
    has_hostname: bool,
    config: &Config,
    target_session_attrs: TargetSessionAttrs,
    release: Option<crate::release::ConnectionRelease>,
) -> Result<(Client, Connection<S, T::Stream>, Encryption), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    validate_tls_connector_parameters(&tls, encryption, config)?;
    let stream = negotiate_tls(
        stream,
        encryption,
        config.get_ssl_mode(),
        config.get_ssl_negotiation(),
        tls,
        has_hostname,
    )
    .await?;
    let negotiated = stream.negotiated_encryption();

    let mut handshake = Handshake {
        stream: BufStream::new(stream),
        pending: BackendMessages::empty(),
        delayed: VecDeque::new(),
        delayed_bytes: 0,
    };

    let user = match config.get_user() {
        Some(user) => Cow::Borrowed(user),
        None => Cow::Owned(whoami::username().map_err(|err| Error::io(err.into()))?),
    };

    startup(&mut handshake, config, &user).await?;
    authenticate(&mut handshake, config, &user).await?;
    check_ssl_cert_mode(config, handshake.stream.get_mut().client_cert_status())?;
    let (process_id, secret_key, mut parameters) = read_info(&mut handshake).await?;
    probe_target_session_attrs(
        &mut handshake,
        target_session_attrs,
        &mut parameters,
    )
    .await?;

    // `connect_timeout` owns negotiation, startup, authentication, and the
    // target-session probe above. The socket-read inactivity clock is a
    // different, post-startup clock and begins only when Connection::run owes
    // an application response.
    handshake
        .stream
        .set_read_timeout(config.get_read_timeout().copied());

    let (sender, receiver) = mpsc::unbounded();
    let drop_release = release.as_ref().map(|release| release.connection_guard());
    let client = Client::new_with_statement_cache(
        sender,
        config.get_ssl_mode(),
        config.get_ssl_negotiation(),
        process_id,
        secret_key,
        release,
        StatementCacheSettings::new(
            config.get_statement_cache_capacity(),
            config.get_statement_cache_execution_threshold(),
        ),
    );
    let connection = Connection::new(
        handshake.stream,
        handshake.delayed,
        parameters,
        client.parameters_handle(),
        receiver,
        client.tx_status_handle(),
        client.in_flight_requests_handle(),
        drop_release,
    );

    Ok((client, connection, negotiated))
}

/// Refuse a TLS parameter before the connector can emit a ClientHello.
///
/// The generic connector traits deliberately support implementations other
/// than rustls. Those implementations must opt in to nondefault policies;
/// otherwise accepting the connection string would falsely claim the policy
/// was active.
pub(crate) fn validate_tls_connector_parameters<S, T>(
    tls: &T,
    encryption: Encryption,
    config: &Config,
) -> Result<(), Error>
where
    T: TlsConnect<S>,
{
    if encryption == Encryption::Plaintext {
        return Ok(());
    }

    let ssl_sni = config.get_ssl_sni();
    if !tls.can_honor_sslsni(ssl_sni) {
        return Err(Error::tls(
            format!(
                "sslsni={} cannot be honoured by the supplied TLS connector",
                u8::from(ssl_sni)
            )
            .into(),
        ));
    }

    let ssl_cert_mode = config.get_ssl_cert_mode();
    if !tls.can_honor_sslcertmode(ssl_cert_mode) {
        return Err(Error::tls(
            format!(
                "sslcertmode={} cannot be honoured by the supplied TLS connector",
                ssl_cert_mode.as_str()
            )
            .into(),
        ));
    }

    // Server authentication last, because it is the one whose absence is
    // silent: an unhonoured `sslsni` or `sslcertmode` changes what the wire
    // carries, while an unhonoured `sslmode=verify-full` produces a session
    // that looks exactly like a verified one.
    let ssl_mode = config.get_ssl_mode();
    let verification = ServerVerification::demanded_by(ssl_mode, config.get_ssl_root_cert())?;
    if !tls.can_honor_server_verification(verification) {
        return Err(Error::tls(
            format!(
                "sslmode={} asks for server-certificate verification the supplied TLS connector \
                 does not attest to performing",
                ssl_mode.as_str()
            )
            .into(),
        ));
    }

    Ok(())
}

/// Check the requested session property while the startup stream is still
/// undivided, then leave it aligned immediately after `ReadyForQuery` for the
/// regular connection task.
async fn probe_target_session_attrs<S, T>(
    handshake: &mut Handshake<S, T>,
    target: TargetSessionAttrs,
    parameters: &mut HashMap<String, String>,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    let probe = match target {
        TargetSessionAttrs::Any => return Ok(()),
        TargetSessionAttrs::ReadWrite | TargetSessionAttrs::ReadOnly => {
            TargetSessionProbe::TransactionReadOnly
        }
        TargetSessionAttrs::Primary
        | TargetSessionAttrs::Standby
        | TargetSessionAttrs::PreferStandby => TargetSessionProbe::InRecovery,
    };

    let mut buf = BytesMut::new();
    frontend::query(probe.query(), &mut buf).map_err(Error::encode)?;
    handshake.send(FrontendMessage::Raw(buf.freeze())).await?;

    let mut saw_row_description = false;
    let mut state = None;
    let mut saw_command_complete = false;

    loop {
        match handshake.next().await? {
            Some(Message::RowDescription(_))
                if !saw_row_description && state.is_none() && !saw_command_complete =>
            {
                saw_row_description = true;
            }
            Some(Message::DataRow(row))
                if saw_row_description && state.is_none() && !saw_command_complete =>
            {
                state = Some(probe.parse(&row)?);
            }
            Some(Message::CommandComplete(_))
                if saw_row_description && state.is_some() && !saw_command_complete =>
            {
                saw_command_complete = true;
            }
            Some(Message::ParameterStatus(body)) => {
                record_parameter_status(
                    parameters,
                    body.name().map_err(Error::parse)?.to_string(),
                    body.value().map_err(Error::parse)?.to_string(),
                )?;
            }
            Some(Message::ReadyForQuery(_))
                if saw_row_description && saw_command_complete =>
            {
                let state = state.ok_or_else(Error::unexpected_message)?;
                return require_target_session_attrs(target, state);
            }
            Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
            Some(_) => return Err(Error::unexpected_message()),
            None => return Err(Error::closed()),
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum TargetSessionProbe {
    TransactionReadOnly,
    InRecovery,
}

impl TargetSessionProbe {
    const fn query(self) -> &'static str {
        match self {
            Self::TransactionReadOnly => "SHOW transaction_read_only",
            Self::InRecovery => "SELECT pg_catalog.pg_is_in_recovery()",
        }
    }

    fn parse(self, row: &DataRowBody) -> Result<TargetSessionState, Error> {
        let value = single_text_value(row)?;
        match (self, value) {
            (Self::TransactionReadOnly, b"on") => {
                Ok(TargetSessionState::TransactionReadOnly(true))
            }
            (Self::TransactionReadOnly, b"off") => {
                Ok(TargetSessionState::TransactionReadOnly(false))
            }
            (Self::InRecovery, b"t") => Ok(TargetSessionState::InRecovery(true)),
            (Self::InRecovery, b"f") => Ok(TargetSessionState::InRecovery(false)),
            (Self::TransactionReadOnly, _) => Err(Error::connect(io::Error::new(
                io::ErrorKind::InvalidData,
                "server returned an invalid transaction_read_only value",
            ))),
            (Self::InRecovery, _) => Err(Error::connect(io::Error::new(
                io::ErrorKind::InvalidData,
                "server returned an invalid pg_is_in_recovery value",
            ))),
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum TargetSessionState {
    TransactionReadOnly(bool),
    InRecovery(bool),
}

fn single_text_value(row: &DataRowBody) -> Result<&[u8], Error> {
    let mut ranges = row.ranges();
    let Some(Some(range)) = ranges.next().map_err(Error::parse)? else {
        return Err(Error::unexpected_message());
    };
    if ranges.next().map_err(Error::parse)?.is_some() {
        return Err(Error::unexpected_message());
    }

    row.buffer()
        .get(range)
        .ok_or_else(Error::unexpected_message)
}

fn require_target_session_attrs(
    target: TargetSessionAttrs,
    state: TargetSessionState,
) -> Result<(), Error> {
    match (target, state) {
        (TargetSessionAttrs::ReadWrite, TargetSessionState::TransactionReadOnly(true)) => {
            Err(target_session_attrs_mismatch(
                "database does not allow writes",
            ))
        }
        (TargetSessionAttrs::ReadOnly, TargetSessionState::TransactionReadOnly(false)) => {
            Err(target_session_attrs_mismatch("database is not read only"))
        }
        (TargetSessionAttrs::Primary, TargetSessionState::InRecovery(true)) => {
            Err(target_session_attrs_mismatch(
                "database server is in recovery",
            ))
        }
        (
            TargetSessionAttrs::Standby | TargetSessionAttrs::PreferStandby,
            TargetSessionState::InRecovery(false),
        ) => Err(target_session_attrs_mismatch(
            "database server is not in recovery",
        )),
        (TargetSessionAttrs::Any, _)
        | (
            TargetSessionAttrs::ReadWrite,
            TargetSessionState::TransactionReadOnly(false),
        )
        | (
            TargetSessionAttrs::ReadOnly,
            TargetSessionState::TransactionReadOnly(true),
        )
        | (TargetSessionAttrs::Primary, TargetSessionState::InRecovery(false))
        | (
            TargetSessionAttrs::Standby | TargetSessionAttrs::PreferStandby,
            TargetSessionState::InRecovery(true),
        ) => Ok(()),
        _ => Err(Error::connect(io::Error::new(
            io::ErrorKind::InvalidData,
            "target session attribute probe returned the wrong property",
        ))),
    }
}

fn target_session_attrs_mismatch(message: &'static str) -> Error {
    Error::target_session_attrs(Error::connect(io::Error::new(
        io::ErrorKind::PermissionDenied,
        message,
    )))
}

/// Run the regular startup + auth + parameter-read handshake against
/// an already-TLS-wrapped stream, but return the post-handshake stream
/// (and parameter map) instead of wiring it into a Client/Connection
/// pair.
///
/// Used by [`crate::replication::connect_replication`] — the
/// replication-mode connection does NOT spawn a `Connection::run` task
/// because the wire protocol after `START_REPLICATION` is bespoke
/// (`CopyBothResponse` is not in postgres-protocol's tag list).
///
/// This is exported `pub(crate)` so the replication module can reuse
/// the handshake state machine without duplicating ~250 LOC of
/// auth/SASL code.
pub(crate) async fn handshake_for_replication<S, T>(
    stream: MaybeTlsStream<S, T>,
    config: &Config,
) -> Result<(MaybeTlsStream<S, T>, std::collections::HashMap<String, String>), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: crate::tls::TlsStream + Unpin,
{
    let mut handshake = Handshake {
        stream: BufStream::new(stream),
        pending: BackendMessages::empty(),
        delayed: VecDeque::new(),
        delayed_bytes: 0,
    };

    let user = match config.get_user() {
        Some(user) => Cow::Borrowed(user),
        None => Cow::Owned(whoami::username().map_err(|err| Error::io(err.into()))?),
    };

    startup(&mut handshake, config, &user).await?;
    authenticate(&mut handshake, config, &user).await?;
    check_ssl_cert_mode(config, handshake.stream.get_mut().client_cert_status())?;
    let (_pid, _key, parameters) = read_info(&mut handshake).await?;

    Ok((handshake.stream.into_inner(), parameters))
}

/// Enforce `sslcertmode=require` only after PostgreSQL authentication succeeds.
///
/// That timing matches libpq: an earlier server authentication failure remains
/// the primary error, while a server that would otherwise accept the session
/// cannot quietly omit the requested TLS client-certificate exchange.
fn check_ssl_cert_mode(
    config: &Config,
    status: crate::tls::ClientCertStatus,
) -> Result<(), Error> {
    use crate::tls::ClientCertStatus;

    if config.get_ssl_cert_mode() != config::SslCertMode::Require {
        return Ok(());
    }

    match status {
        ClientCertStatus::Sent => Ok(()),
        ClientCertStatus::NotApplicable | ClientCertStatus::NotRequested => {
            Err(Error::authentication(
                "sslcertmode=require: server did not request an SSL certificate".into(),
            ))
        }
        ClientCertStatus::NotSent => Err(Error::authentication(
            "sslcertmode=require: server accepted connection without a valid SSL certificate"
                .into(),
        )),
        ClientCertStatus::Unknown => Err(Error::authentication(
            "sslcertmode=require cannot be honoured: the TLS connector did not report whether \
             it sent a client certificate"
                .into(),
        )),
    }
}

async fn startup<S, T>(
    handshake: &mut Handshake<S, T>,
    config: &Config,
    user: &str,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut params = vec![("client_encoding", "UTF8")];
    params.push(("user", user));
    if let Some(dbname) = config.get_dbname() {
        params.push(("database", dbname));
    }
    if let Some(options) = config.get_options() {
        params.push(("options", options));
    }
    if let Some(application_name) = config.resolved_application_name() {
        params.push(("application_name", application_name));
    }
    // Streaming-replication protocol opt-in. `replication=database`
    // selects the logical-decoding walsender (required for pgoutput +
    // START_REPLICATION SLOT ... LOGICAL); `replication=true` selects
    // the physical walsender. Either value puts the connection in
    // walsender mode, restricting the post-handshake grammar to the
    // replication subset documented in
    // https://www.postgresql.org/docs/16/protocol-replication.html.
    if let Some(mode) = config.get_replication() {
        params.push((
            "replication",
            match mode {
                ReplicationMode::Logical => "database",
                ReplicationMode::Physical => "true",
            },
        ));
    }

    let mut buf = BytesMut::new();
    frontend::startup_message(params, &mut buf).map_err(Error::encode)?;

    handshake
        .send(FrontendMessage::Raw(buf.freeze()))
        .await
}

async fn authenticate<S, T>(
    handshake: &mut Handshake<S, T>,
    config: &Config,
    user: &str,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsStream + Unpin,
{
    match handshake.next().await? {
        Some(Message::AuthenticationOk) => {
            check_require_auth(config, AuthMethod::None)?;
            can_skip_channel_binding(config)?;
            return Ok(());
        }
        Some(Message::AuthenticationCleartextPassword) => {
            check_require_auth(config, AuthMethod::Password)?;
            can_skip_channel_binding(config)?;

            let pass = config
                .get_password()
                .ok_or_else(|| Error::config("password missing".into()))?;

            authenticate_password(handshake, pass).await?;
        }
        Some(Message::AuthenticationMd5Password(body)) => {
            check_require_auth(config, AuthMethod::Md5)?;
            can_skip_channel_binding(config)?;

            let pass = config
                .get_password()
                .ok_or_else(|| Error::config("password missing".into()))?;

            let output = authentication::md5_hash(user.as_bytes(), pass, body.salt());
            authenticate_password(handshake, output.as_bytes()).await?;
        }
        Some(Message::AuthenticationSasl(body)) => {
            // PostgreSQL 16's only SASL authentication family is SCRAM; both
            // SCRAM-SHA-256 and SCRAM-SHA-256-PLUS map to this policy name.
            // Check before constructing or writing the client-first message.
            check_require_auth(config, AuthMethod::ScramSha256)?;
            authenticate_sasl(handshake, body, config).await?;
        }
        // The four methods this driver does not implement. Each names ITSELF:
        // all four used to return the same "unsupported authentication method",
        // which tells an operator nothing about what their server asked for or
        // what to reconfigure. This crate already holds refusals to that
        // standard elsewhere -- `libpq_parameter_parity` requires a refused
        // connection parameter to be named through the error's source chain,
        // for exactly this reason.
        Some(Message::AuthenticationGss) => {
            check_require_auth(config, AuthMethod::Gss)?;
            return Err(unsupported_authentication("GSSAPI"));
        }
        Some(Message::AuthenticationSspi) => {
            check_require_auth(config, AuthMethod::Sspi)?;
            return Err(unsupported_authentication("SSPI"));
        }
        // Neither of these has an `AuthMethod` variant, so neither consults
        // `require_auth`: there is no policy that could permit a method the
        // driver cannot perform.
        Some(Message::AuthenticationKerberosV5) => {
            return Err(unsupported_authentication("Kerberos V5"));
        }
        Some(Message::AuthenticationScmCredential) => {
            return Err(unsupported_authentication("SCM credential"));
        }
        Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
        Some(_) => return Err(Error::unexpected_message()),
        None => return Err(Error::closed()),
    }

    // After sending our credentials, expect an AuthenticationOk.
    match handshake.next().await? {
        Some(Message::AuthenticationOk) => Ok(()),
        Some(Message::ErrorResponse(body)) => Err(Error::db(body)),
        Some(_) => Err(Error::unexpected_message()),
        None => Err(Error::closed()),
    }
}

/// A method this driver does not implement, named so the operator knows what
/// their server asked for.
fn unsupported_authentication(method: &str) -> Error {
    Error::authentication(format!("unsupported authentication method: {method}").into())
}

fn check_require_auth(config: &Config, method: AuthMethod) -> Result<(), Error> {
    let policy = config.get_require_auth();
    if policy.allows(method) {
        return Ok(());
    }

    let reason = match method {
        AuthMethod::Password => "server requested a cleartext password",
        AuthMethod::Md5 => "server requested a hashed password",
        AuthMethod::Gss => "server requested GSSAPI authentication",
        AuthMethod::Sspi => "server requested SSPI authentication",
        AuthMethod::ScramSha256 => "server requested SASL authentication",
        AuthMethod::None => "server did not complete authentication",
    };
    Err(Error::authentication(
        format!(
            "authentication method requirement \"{policy}\" failed: {reason}"
        )
        .into(),
    ))
}

fn can_skip_channel_binding(config: &Config) -> Result<(), Error> {
    match config.get_channel_binding() {
        config::ChannelBinding::Disable | config::ChannelBinding::Prefer => Ok(()),
        config::ChannelBinding::Require => Err(Error::authentication(
            "server did not use channel binding".into(),
        )),
    }
}

async fn authenticate_password<S, T>(
    handshake: &mut Handshake<S, T>,
    password: &[u8],
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = BytesMut::new();
    frontend::password_message(password, &mut buf).map_err(Error::encode)?;
    handshake.send(FrontendMessage::Raw(buf.freeze())).await
}

async fn authenticate_sasl<S, T>(
    handshake: &mut Handshake<S, T>,
    body: AuthenticationSaslBody,
    config: &Config,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsStream + Unpin,
{
    let password = config
        .get_password()
        .ok_or_else(|| Error::config("password missing".into()))?;

    let mut has_scram = false;
    let mut has_scram_plus = false;
    let mut mechanisms = body.mechanisms();
    while let Some(mechanism) = mechanisms.next().map_err(Error::parse)? {
        match mechanism {
            sasl::SCRAM_SHA_256 => has_scram = true,
            sasl::SCRAM_SHA_256_PLUS => has_scram_plus = true,
            _ => {}
        }
    }

    // Channel binding hook: the source introspects the TlsStream via
    // `get_ref()` on the Framed wrapper. Our BufStream exposes
    // `get_mut()` which yields &mut MaybeTlsStream<_, T>. MaybeTlsStream
    // implements TlsStream (forwarding to the inner T), so we can query
    // channel_binding() directly.
    let tls_server_end_point = handshake
        .stream
        .get_mut()
        .channel_binding()
        .tls_server_end_point;

    let channel_binding_cfg = config.get_channel_binding();

    // Enforce `ChannelBinding::Require` *before* the mechanism is picked
    // so we reject both downgrade shapes with a precise error instead of
    // relying on the post-hoc `can_skip_channel_binding` check:
    //
    //   1. Server advertises SCRAM-SHA-256-PLUS but the TLS backend
    //      cannot export the server endpoint (no tls-server-end-point
    //      channel binding). Silently falling back to plain
    //      SCRAM-SHA-256 under `prefer` would be a downgrade surface.
    //   2. Server does not advertise SCRAM-SHA-256-PLUS at all.
    //
    // Both are fatal when the user asked for `require`.
    if channel_binding_cfg == config::ChannelBinding::Require {
        if !has_scram_plus {
            return Err(Error::authentication(
                "server did not offer SCRAM-SHA-256-PLUS but channel binding was required"
                    .into(),
            ));
        }
        if tls_server_end_point.is_none() {
            return Err(Error::tls(
                "channel binding requested but backend does not support it".into(),
            ));
        }
    }

    // Under `prefer`, log an operator-visible warning when the server
    // offered -PLUS but the backend can't bind, so the silent fallback
    // to plain SCRAM-SHA-256 is still traceable.
    if has_scram_plus
        && tls_server_end_point.is_none()
        && channel_binding_cfg == config::ChannelBinding::Prefer
    {
        log::warn!(
            "server offered SCRAM-SHA-256-PLUS but TLS backend did not expose \
             tls-server-end-point channel binding; falling back to plain SCRAM-SHA-256"
        );
    }

    let channel_binding = tls_server_end_point
        .filter(|_| channel_binding_cfg != config::ChannelBinding::Disable)
        .map(sasl::ChannelBinding::tls_server_end_point);

    let (channel_binding, mechanism) = if has_scram_plus {
        match channel_binding {
            Some(channel_binding) => (channel_binding, sasl::SCRAM_SHA_256_PLUS),
            None => (sasl::ChannelBinding::unsupported(), sasl::SCRAM_SHA_256),
        }
    } else if has_scram {
        match channel_binding {
            Some(_) => (sasl::ChannelBinding::unrequested(), sasl::SCRAM_SHA_256),
            None => (sasl::ChannelBinding::unsupported(), sasl::SCRAM_SHA_256),
        }
    } else {
        return Err(Error::authentication("unsupported SASL mechanism".into()));
    };

    if mechanism != sasl::SCRAM_SHA_256_PLUS {
        can_skip_channel_binding(config)?;
    }

    let mut scram = ScramSha256::new(password, channel_binding);

    let mut buf = BytesMut::new();
    frontend::sasl_initial_response(mechanism, scram.message(), &mut buf).map_err(Error::encode)?;
    handshake.send(FrontendMessage::Raw(buf.freeze())).await?;

    let body = match handshake.next().await? {
        Some(Message::AuthenticationSaslContinue(body)) => body,
        Some(Message::AuthenticationOk) => {
            return Err(incomplete_authentication_exchange(config));
        }
        Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
        Some(_) => return Err(Error::unexpected_message()),
        None => return Err(Error::closed()),
    };

    scram
        .update(body.data())
        .map_err(|e| Error::authentication(e.into()))?;

    let mut buf = BytesMut::new();
    frontend::sasl_response(scram.message(), &mut buf).map_err(Error::encode)?;
    handshake.send(FrontendMessage::Raw(buf.freeze())).await?;

    let body = match handshake.next().await? {
        Some(Message::AuthenticationSaslFinal(body)) => body,
        Some(Message::AuthenticationOk) => {
            return Err(incomplete_authentication_exchange(config));
        }
        Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
        Some(_) => return Err(Error::unexpected_message()),
        None => return Err(Error::closed()),
    };

    scram
        .finish(body.data())
        .map_err(|e| Error::authentication(e.into()))?;

    Ok(())
}

fn incomplete_authentication_exchange(config: &Config) -> Error {
    let policy = config.get_require_auth();
    if matches!(policy, config::RequireAuth::Any) {
        // Preserve the default policy's pre-require_auth behavior for a
        // malformed SCRAM exchange.
        return Error::unexpected_message();
    }

    Error::authentication(
        format!(
            "authentication method requirement \"{policy}\" failed: server did not complete authentication"
        )
        .into(),
    )
}

/// Fold one `ParameterStatus` from the handshake into `parameters`, refusing a
/// text encoding this driver cannot decode.
///
/// THE HANDSHAKE IS THE DOOR THIS CHECK WAS MISSING FROM. `Config::param`
/// refuses a `client_encoding` named in the connection string, and
/// `connection::route_async` retires a session that changes it later, but a
/// server was free to ANNOUNCE a different encoding during startup and be
/// believed. Every string on that connection would then be decoded as UTF-8
/// regardless -- and where the foreign bytes are themselves valid UTF-8, decoded
/// silently as a different string.
///
/// Both `ParameterStatus` arms in this file route through here: the startup
/// read and the `target_session_attrs` probe, which reads its own frames and
/// would otherwise have been a fourth unguarded path.
fn record_parameter_status(
    parameters: &mut HashMap<String, String>,
    name: String,
    value: String,
) -> Result<(), Error> {
    if name.eq_ignore_ascii_case("client_encoding") && !crate::config::is_decodable_encoding(&value)
    {
        return Err(Error::config(
            format!(
                "server announced client_encoding {value} during startup; this \
                 driver decodes text as UTF-8 and cannot read that encoding"
            )
            .into(),
        ));
    }
    parameters.insert(name, value);
    Ok(())
}

async fn read_info<S, T>(
    handshake: &mut Handshake<S, T>,
) -> Result<(i32, i32, HashMap<String, String>), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut process_id = 0;
    let mut secret_key = 0;
    let mut parameters = HashMap::new();

    loop {
        match handshake.next().await? {
            Some(Message::BackendKeyData(body)) => {
                process_id = body.process_id();
                secret_key = body.secret_key();
            }
            Some(Message::ParameterStatus(body)) => {
                record_parameter_status(
                    &mut parameters,
                    body.name().map_err(Error::parse)?.to_string(),
                    body.value().map_err(Error::parse)?.to_string(),
                )?;
            }
            // NO `NoticeResponse` ARM HERE, and its absence is deliberate.
            // `Handshake::next` only yields what `read_backend` did not classify
            // as async, and `read_backend` never leaves a notice inside a
            // `Normal` batch: at offset zero it returns `Async`, and further in
            // it ends the batch before the frame. So a notice cannot reach this
            // match, and the arm that used to sit here was unreachable - it
            // queued into `delayed` a second time, which is why the byte budget
            // has exactly one enforcement point rather than two.
            Some(Message::ReadyForQuery(_)) => return Ok((process_id, secret_key, parameters)),
            Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
            Some(_) => return Err(Error::unexpected_message()),
            None => return Err(Error::closed()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncMessage;
    use crate::config::{AuthMethod, AuthMethods, RequireAuth, SslMode};
    use crate::tls::NoTls;
    use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use compio::net::{TcpListener, TcpStream};
    use futures_channel::oneshot;
    use futures_util::StreamExt;
    use std::time::Duration;

    const EXPECTED_DELAYED_MESSAGE_LIMIT: usize = 256;

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + body.len());
        frame.push(tag);
        frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    fn notice(message: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"SNOTICE\0");
        body.extend_from_slice(b"VNOTICE\0");
        body.extend_from_slice(b"C00000\0");
        body.push(b'M');
        body.extend_from_slice(message.as_bytes());
        body.extend_from_slice(b"\0\0");
        frame(b'N', &body)
    }

    fn successful_handshake(notices: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let mut script = frame(b'R', &0u32.to_be_bytes());
        for notice in notices {
            script.extend_from_slice(&notice);
        }
        script.extend_from_slice(&frame(b'K', &[0; 8]));
        script.extend_from_slice(&frame(b'Z', b"I"));
        script
    }

    async fn scripted_server_after_startup(
        server_says: Option<Vec<u8>>,
    ) -> (crate::Socket, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (startup_seen, startup_observed) = oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.unwrap();
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            assert!(length >= 4, "startup packet length must include its header");

            let compio::BufResult(result, _) =
                socket.read_exact(vec![0u8; length - 4]).await;
            result.unwrap();
            let _ = startup_seen.send(());

            if let Some(server_says) = server_says {
                let compio::BufResult(result, _) = socket.write_all(server_says).await;
                result.unwrap();
                socket.flush().await.unwrap();
            } else {
                // Startup is on the wire and the client is waiting for
                // authentication. Stay silent until it drops the stream.
                let compio::BufResult(_, _) = socket.read(vec![0u8; 1]).await;
            }
        })
        .detach();

        (
            crate::Socket::new_tcp(TcpStream::connect(addr).await.unwrap()),
            startup_observed,
        )
    }

    async fn scripted_password_auth_server(
        auth_request: Vec<u8>,
        complete_after_response: bool,
    ) -> (
        crate::Socket,
        oneshot::Receiver<Result<Vec<u8>, String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client_bytes_tx, client_bytes_rx) = oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            if let Err(error) = result {
                let _ = client_bytes_tx.send(Err(error.to_string()));
                return;
            }
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            let compio::BufResult(result, _) =
                socket.read_exact(vec![0u8; length - 4]).await;
            if let Err(error) = result {
                let _ = client_bytes_tx.send(Err(error.to_string()));
                return;
            }

            let compio::BufResult(result, _) = socket.write_all(frame(b'R', &auth_request)).await;
            if let Err(error) = result {
                let _ = client_bytes_tx.send(Err(error.to_string()));
                return;
            }
            if let Err(error) = socket.flush().await {
                let _ = client_bytes_tx.send(Err(error.to_string()));
                return;
            }

            let client_bytes = if complete_after_response {
                // TCP reads may be partial. Observe one complete frontend
                // frame before allowing the scripted authentication to
                // finish, so the permitted-path assertion never relies on
                // packet boundaries.
                let compio::BufResult(result, tag) = socket.read_exact(vec![0u8; 1]).await;
                if let Err(error) = result {
                    let _ = client_bytes_tx.send(Err(error.to_string()));
                    return;
                }
                let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
                if let Err(error) = result {
                    let _ = client_bytes_tx.send(Err(error.to_string()));
                    return;
                }
                let frame_length =
                    u32::from_be_bytes(length.as_slice().try_into().unwrap()) as usize;
                if frame_length < 4 {
                    let _ = client_bytes_tx.send(Err(format!(
                        "invalid frontend frame length {frame_length}"
                    )));
                    return;
                }
                let compio::BufResult(result, body) =
                    socket.read_exact(vec![0u8; frame_length - 4]).await;
                if let Err(error) = result {
                    let _ = client_bytes_tx.send(Err(error.to_string()));
                    return;
                }
                let mut frame = Vec::with_capacity(1 + frame_length);
                frame.extend_from_slice(&tag);
                frame.extend_from_slice(&length);
                frame.extend_from_slice(&body);
                frame
            } else {
                // A policy rejection closes the client side before writing a
                // PasswordMessage, so this ordinary read returns zero bytes.
                // If the implementation leaks any credential bytes first,
                // retain them for the assertion below.
                let compio::BufResult(result, mut client_bytes) =
                    socket.read(vec![0u8; 1024]).await;
                let read = match result {
                    Ok(read) => read,
                    Err(error) => {
                        let _ = client_bytes_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                client_bytes.truncate(read);
                client_bytes
            };

            if complete_after_response {
                let compio::BufResult(result, _) = socket
                    .write_all(successful_handshake(std::iter::empty()))
                    .await;
                if let Err(error) = result {
                    let _ = client_bytes_tx.send(Err(error.to_string()));
                    return;
                }
                if let Err(error) = socket.flush().await {
                    let _ = client_bytes_tx.send(Err(error.to_string()));
                    return;
                }
            }

            let _ = client_bytes_tx.send(Ok(client_bytes));
        })
        .detach();

        (
            crate::Socket::new_tcp(TcpStream::connect(addr).await.unwrap()),
            client_bytes_rx,
        )
    }

    fn authentication_error_chain(error: Error) -> String {
        let message = error.to_string();
        let cause = error
            .into_source()
            .map(|cause| cause.to_string())
            .unwrap_or_default();
        format!("{message}: {cause}")
    }

    fn sha256(input: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
            0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
            0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
            0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
            0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
            0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
            0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
            0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
            0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
            0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
        ];

        let mut state = [
            0x6a09e667u32,
            0xbb67ae85,
            0x3c6ef372,
            0xa54ff53a,
            0x510e527f,
            0x9b05688c,
            0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut padded = input.to_vec();
        padded.push(0x80);
        while padded.len() % 64 != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&(input.len() as u64 * 8).to_be_bytes());

        for chunk in padded.chunks_exact(64) {
            let mut words = [0u32; 64];
            for (word, bytes) in words.iter_mut().zip(chunk.chunks_exact(4)) {
                *word = u32::from_be_bytes(bytes.try_into().unwrap());
            }
            for index in 16..64 {
                let s0 = words[index - 15].rotate_right(7)
                    ^ words[index - 15].rotate_right(18)
                    ^ (words[index - 15] >> 3);
                let s1 = words[index - 2].rotate_right(17)
                    ^ words[index - 2].rotate_right(19)
                    ^ (words[index - 2] >> 10);
                words[index] = words[index - 16]
                    .wrapping_add(s0)
                    .wrapping_add(words[index - 7])
                    .wrapping_add(s1);
            }

            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
            for index in 0..64 {
                let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let choice = (e & f) ^ (!e & g);
                let temp1 = h
                    .wrapping_add(sum1)
                    .wrapping_add(choice)
                    .wrapping_add(K[index])
                    .wrapping_add(words[index]);
                let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let majority = (a & b) ^ (a & c) ^ (b & c);
                let temp2 = sum0.wrapping_add(majority);

                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(temp1);
                d = c;
                c = b;
                b = a;
                a = temp1.wrapping_add(temp2);
            }

            for (current, compressed) in state
                .iter_mut()
                .zip([a, b, c, d, e, f, g, h])
            {
                *current = current.wrapping_add(compressed);
            }
        }

        let mut digest = [0u8; 32];
        for (output, word) in digest.chunks_exact_mut(4).zip(state) {
            output.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
        let mut key_block = [0u8; 64];
        if key.len() > key_block.len() {
            key_block[..32].copy_from_slice(&sha256(key));
        } else {
            key_block[..key.len()].copy_from_slice(key);
        }

        let mut inner = Vec::with_capacity(64 + message.len());
        inner.extend(key_block.iter().map(|byte| byte ^ 0x36));
        inner.extend_from_slice(message);
        let inner = sha256(&inner);

        let mut outer = Vec::with_capacity(64 + inner.len());
        outer.extend(key_block.iter().map(|byte| byte ^ 0x5c));
        outer.extend_from_slice(&inner);
        sha256(&outer)
    }

    fn base64(input: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);

        for chunk in input.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            encoded.push(ALPHABET[(first >> 2) as usize] as char);
            encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            if chunk.len() > 1 {
                encoded.push(
                    ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char,
                );
            } else {
                encoded.push('=');
            }
            if chunk.len() > 2 {
                encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }

        encoded
    }

    fn plaintext_config() -> Config {
        let mut config = Config::new();
        config
            .user("scripted-user")
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_secs(5));
        config
    }

    fn scram_config() -> Config {
        let mut config = Config::new();
        config
            .user("scripted-user")
            .password("scripted-password")
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_secs(5));
        config
    }

    async fn scripted_successful_scram_server() -> (
        crate::Socket,
        oneshot::Receiver<Result<(), String>>,
    ) {
        // PBKDF2-HMAC-SHA-256("scripted-password", "salt", 1), followed by
        // HMAC-SHA-256(salted_password, "Server Key"). Keeping the derived
        // verifier here lets the scripted peer compute its nonce-dependent
        // server signature without adding a test-only crypto dependency.
        const SERVER_KEY: [u8; 32] = [
            0xf1, 0x83, 0x92, 0xfc, 0x94, 0x37, 0xe6, 0x33, 0x43, 0x42, 0x26, 0x63,
            0x9a, 0xba, 0x84, 0x91, 0x32, 0xa6, 0x3e, 0x12, 0x47, 0xbf, 0x0f, 0x07,
            0x58, 0x71, 0x43, 0xa4, 0x57, 0x01, 0x2d, 0xc7,
        ];

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server_done_tx, server_done_rx) = oneshot::channel();

        compio::runtime::spawn(async move {
            let outcome: Result<(), String> = async {
                let (mut socket, _) = listener.accept().await.map_err(|e| e.to_string())?;

                let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
                result.map_err(|e| e.to_string())?;
                let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
                let compio::BufResult(result, _) =
                    socket.read_exact(vec![0u8; length - 4]).await;
                result.map_err(|e| e.to_string())?;

                let mut auth_sasl = 10i32.to_be_bytes().to_vec();
                auth_sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
                let compio::BufResult(result, _) =
                    socket.write_all(frame(b'R', &auth_sasl)).await;
                result.map_err(|e| e.to_string())?;
                socket.flush().await.map_err(|e| e.to_string())?;

                let compio::BufResult(result, tag) = socket.read_exact(vec![0u8; 1]).await;
                result.map_err(|e| e.to_string())?;
                if tag != [b'p'] {
                    return Err(format!("expected SASLInitialResponse, got tag {tag:?}"));
                }
                let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
                result.map_err(|e| e.to_string())?;
                let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
                let compio::BufResult(result, initial) =
                    socket.read_exact(vec![0u8; length - 4]).await;
                result.map_err(|e| e.to_string())?;
                let mechanism_end = initial
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(|| "SASL mechanism was not terminated".to_string())?;
                if &initial[..mechanism_end] != b"SCRAM-SHA-256" {
                    return Err(format!(
                        "client chose an unexpected SASL mechanism: {:?}",
                        &initial[..mechanism_end]
                    ));
                }
                let response_length_start = mechanism_end + 1;
                let response_start = response_length_start + 4;
                if initial.len() < response_start {
                    return Err("SASL initial response was truncated".to_string());
                }
                let response_length = i32::from_be_bytes(
                    initial[response_length_start..response_start]
                        .try_into()
                        .unwrap(),
                );
                if response_length < 0
                    || response_length as usize != initial.len() - response_start
                {
                    return Err("SASL initial response length was invalid".to_string());
                }
                let client_first = std::str::from_utf8(&initial[response_start..])
                    .map_err(|e| e.to_string())?;
                let client_first_bare = client_first
                    .strip_prefix("n,,")
                    .ok_or_else(|| format!("unexpected client-first message: {client_first}"))?;
                let client_nonce = client_first_bare
                    .strip_prefix("n=,r=")
                    .ok_or_else(|| format!("unexpected client-first-bare: {client_first_bare}"))?;

                let server_first = format!("r={client_nonce}scripted-server,s=c2FsdA==,i=1");
                let mut auth_continue = 11i32.to_be_bytes().to_vec();
                auth_continue.extend_from_slice(server_first.as_bytes());
                let compio::BufResult(result, _) =
                    socket.write_all(frame(b'R', &auth_continue)).await;
                result.map_err(|e| e.to_string())?;
                socket.flush().await.map_err(|e| e.to_string())?;

                let compio::BufResult(result, tag) = socket.read_exact(vec![0u8; 1]).await;
                result.map_err(|e| e.to_string())?;
                if tag != [b'p'] {
                    return Err(format!("expected SASLResponse, got tag {tag:?}"));
                }
                let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
                result.map_err(|e| e.to_string())?;
                let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
                let compio::BufResult(result, client_final) =
                    socket.read_exact(vec![0u8; length - 4]).await;
                result.map_err(|e| e.to_string())?;
                let client_final =
                    std::str::from_utf8(&client_final).map_err(|e| e.to_string())?;
                let proof_start = client_final
                    .rfind(",p=")
                    .ok_or_else(|| format!("client-final message had no proof: {client_final}"))?;
                let client_final_without_proof = &client_final[..proof_start];
                let auth_message = format!(
                    "{client_first_bare},{server_first},{client_final_without_proof}"
                );
                let server_signature = hmac_sha256(&SERVER_KEY, auth_message.as_bytes());
                let mut auth_final = 12i32.to_be_bytes().to_vec();
                auth_final.extend_from_slice(format!("v={}", base64(&server_signature)).as_bytes());
                let compio::BufResult(result, _) =
                    socket.write_all(frame(b'R', &auth_final)).await;
                result.map_err(|e| e.to_string())?;
                let compio::BufResult(result, _) = socket
                    .write_all(successful_handshake(std::iter::empty()))
                    .await;
                result.map_err(|e| e.to_string())?;
                socket.flush().await.map_err(|e| e.to_string())?;
                Ok(())
            }
            .await;

            let _ = server_done_tx.send(outcome);
        })
        .detach();

        (
            crate::Socket::new_tcp(TcpStream::connect(addr).await.unwrap()),
            server_done_rx,
        )
    }

    /// A peer that opens SCRAM, waits for the client's first message, and then
    /// answers `AuthenticationOk` instead of continuing the exchange.
    ///
    /// RETURNS THE CLIENT-FIRST MESSAGE, and that is not a convenience. This
    /// helper used to `.detach()` its task and return only the socket, so the
    /// one thing separating this scenario from
    /// `require_scram_refuses_authentication_ok_without_an_exchange` -- that a
    /// SCRAM exchange was demonstrably STARTED -- lived in `unwrap()`s inside a
    /// detached task, where a failure cannot fail a test. Both tests then
    /// asserted the same two substrings on the same error, and measured
    /// 2026-08-23 the driver produces a BYTE-IDENTICAL chain for the two:
    /// `authentication method requirement "scram-sha-256" failed: server did
    /// not complete authentication`. So if this peer had quietly stopped
    /// reaching client-first, the test would have become a duplicate of its
    /// sibling and stayed green. The caller now asserts the exchange began.
    async fn scripted_scram_server_sending_early_ok()
    -> (crate::Socket, oneshot::Receiver<Result<Vec<u8>, String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client_first_tx, client_first_rx) = oneshot::channel();

        compio::runtime::spawn(async move {
            let outcome: Result<Vec<u8>, String> = async {
                let (mut socket, _) = listener.accept().await.map_err(|e| e.to_string())?;

                let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
                result.map_err(|e| e.to_string())?;
                let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
                let compio::BufResult(result, _) =
                    socket.read_exact(vec![0u8; length - 4]).await;
                result.map_err(|e| e.to_string())?;

                let mut auth_sasl = 10i32.to_be_bytes().to_vec();
                auth_sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
                let compio::BufResult(result, _) =
                    socket.write_all(frame(b'R', &auth_sasl)).await;
                result.map_err(|e| e.to_string())?;
                socket.flush().await.map_err(|e| e.to_string())?;

                // Wait for the client-first message so AuthenticationOk is
                // demonstrably ending a started, incomplete SCRAM exchange.
                let compio::BufResult(result, tag) = socket.read_exact(vec![0u8; 1]).await;
                result.map_err(|e| e.to_string())?;
                if tag[0] != b'p' {
                    return Err(format!("expected a SASL response, got tag {:?}", tag[0]));
                }
                let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
                result.map_err(|e| e.to_string())?;
                let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
                let compio::BufResult(result, body) =
                    socket.read_exact(vec![0u8; length - 4]).await;
                result.map_err(|e| e.to_string())?;

                let compio::BufResult(result, _) = socket
                    .write_all(successful_handshake(std::iter::empty()))
                    .await;
                result.map_err(|e| e.to_string())?;
                socket.flush().await.map_err(|e| e.to_string())?;
                Ok(body)
            }
            .await;
            let _ = client_first_tx.send(outcome);
        })
        .detach();

        (
            crate::Socket::new_tcp(TcpStream::connect(addr).await.unwrap()),
            client_first_rx,
        )
    }

    #[compio::test]
    async fn require_scram_refuses_cleartext_without_sending_the_password() {
        let auth_request = 3i32.to_be_bytes().to_vec();
        let (stream, client_bytes) =
            scripted_password_auth_server(auth_request, false).await;
        let mut config = scram_config();
        config.require_auth(RequireAuth::Require(AuthMethods::new(
            AuthMethod::ScramSha256,
        )));

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("require_auth=scram-sha-256 accepted cleartext password auth"),
            Err(error) => error,
        };
        let client_bytes = compio::time::timeout(Duration::from_secs(5), client_bytes)
            .await
            .expect("scripted cleartext server did not finish")
            .expect("scripted cleartext server dropped its observation")
            .expect("scripted cleartext server failed");
        let chain = authentication_error_chain(error);

        assert!(
            client_bytes.is_empty(),
            "the password reached the rejected server: {client_bytes:?}"
        );
        assert!(
            chain.contains("scram-sha-256") && chain.contains("cleartext password"),
            "the refusal must name the requirement and server request: {chain}"
        );
    }

    #[compio::test]
    async fn require_scram_refuses_authentication_ok_without_an_exchange() {
        let (stream, _) =
            scripted_server_after_startup(Some(successful_handshake(std::iter::empty()))).await;
        let mut config = scram_config();
        config.require_auth(RequireAuth::Require(AuthMethods::new(
            AuthMethod::ScramSha256,
        )));

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("require_auth=scram-sha-256 accepted an unauthenticated connection"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("scram-sha-256")
                && chain.contains("did not complete authentication"),
            "the refusal must name the requirement and incomplete exchange: {chain}"
        );
    }

    #[compio::test]
    async fn require_scram_names_an_authentication_ok_that_ends_scram_early() {
        let (stream, client_first) = scripted_scram_server_sending_early_ok().await;
        let mut config = scram_config();
        config.require_auth(RequireAuth::Require(AuthMethods::new(
            AuthMethod::ScramSha256,
        )));

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("require_auth=scram-sha-256 accepted partial SCRAM"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("scram-sha-256")
                && chain.contains("did not complete authentication"),
            "the refusal must name the requirement and incomplete exchange: {chain}"
        );

        // WHAT MAKES THIS TEST DIFFERENT FROM ITS SIBLING, asserted rather than
        // scripted. The error above is byte-identical to the one
        // `require_scram_refuses_authentication_ok_without_an_exchange` gets,
        // so it cannot distinguish "SCRAM never started" from "SCRAM started
        // and was cut short" -- only this can. `n,,n=` is the SASL initial
        // response's channel-binding and username prefix, so its presence
        // means the client really did send client-first before the peer
        // answered `AuthenticationOk`.
        let client_first = compio::time::timeout(Duration::from_secs(5), client_first)
            .await
            .expect("scripted early-ok SCRAM server did not finish")
            .expect("scripted early-ok SCRAM server dropped its signal")
            .expect("scripted early-ok SCRAM server failed");
        let mechanism_and_response = String::from_utf8_lossy(&client_first).into_owned();
        assert!(
            mechanism_and_response.contains("SCRAM-SHA-256")
                && mechanism_and_response.contains("n,,n="),
            "the client never sent a SCRAM client-first message, so this test is a duplicate \
             of the no-exchange one: {mechanism_and_response:?}"
        );
    }

    #[compio::test]
    async fn require_scram_accepts_a_completed_scram_exchange() {
        let (stream, server_done) = scripted_successful_scram_server().await;
        let mut config = scram_config();
        config.require_auth(RequireAuth::Require(AuthMethods::new(
            AuthMethod::ScramSha256,
        )));

        let (client, connection) = config
            .connect_raw(stream, NoTls)
            .await
            .expect("require_auth=scram-sha-256 rejected a completed SCRAM exchange");
        compio::time::timeout(Duration::from_secs(5), server_done)
            .await
            .expect("scripted SCRAM server did not finish")
            .expect("scripted SCRAM server dropped its completion signal")
            .expect("scripted SCRAM server failed");
        drop(client);
        drop(connection);
    }

    #[compio::test]
    async fn a_negated_list_rejects_the_named_method_and_permits_another() {
        let mut md5_request = 5i32.to_be_bytes().to_vec();
        md5_request.extend_from_slice(&[1, 2, 3, 4]);
        let (md5_stream, md5_client_bytes) =
            scripted_password_auth_server(md5_request, false).await;
        let mut config = scram_config();
        config.require_auth(RequireAuth::Reject(AuthMethods::new(AuthMethod::Md5)));

        let error = match config.connect_raw(md5_stream, NoTls).await {
            Ok(_) => panic!("require_auth=!md5 accepted MD5 authentication"),
            Err(error) => error,
        };
        let md5_client_bytes = compio::time::timeout(Duration::from_secs(5), md5_client_bytes)
            .await
            .expect("scripted MD5 server did not finish")
            .expect("scripted MD5 server dropped its observation")
            .expect("scripted MD5 server failed");
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("!md5") && chain.contains("hashed password"),
            "the refusal must name the requirement and server request: {chain}"
        );
        assert!(
            md5_client_bytes.is_empty(),
            "an MD5 response reached the rejected server: {md5_client_bytes:?}"
        );

        let cleartext_request = 3i32.to_be_bytes().to_vec();
        let (cleartext_stream, cleartext_client_bytes) =
            scripted_password_auth_server(cleartext_request, true).await;
        let (client, connection) = config
            .connect_raw(cleartext_stream, NoTls)
            .await
            .expect("require_auth=!md5 must permit cleartext password authentication");
        let cleartext_client_bytes =
            compio::time::timeout(Duration::from_secs(5), cleartext_client_bytes)
                .await
                .expect("scripted cleartext server did not finish")
                .expect("scripted cleartext server dropped its observation")
                .expect("scripted cleartext server failed");
        assert_eq!(cleartext_client_bytes.first(), Some(&b'p'));
        assert!(
            cleartext_client_bytes
                .windows(b"scripted-password".len())
                .any(|window| window == b"scripted-password"),
            "the permitted cleartext exchange did not carry the configured password"
        );
        drop(client);
        drop(connection);
    }

    /// Each unimplemented authentication method is refused BY NAME.
    ///
    /// All four returned the identical string "unsupported authentication
    /// method", so an operator whose server asked for GSSAPI learned only that
    /// something was unsupported -- not which of the four, and so not what to
    /// reconfigure. That is the failure `libpq_parameter_parity` already rules
    /// out for connection parameters, whose refusals must name the key through
    /// the error's source chain; the same standard belongs here.
    ///
    /// The table is the test: a shared message passes any single-method
    /// assertion, so each name must be checked against a refusal that should
    /// NOT produce it. That is what the second loop does.
    #[compio::test]
    async fn every_unimplemented_authentication_method_is_refused_by_name() {
        // (AuthenticationRequest code, the name its refusal must carry)
        const UNIMPLEMENTED: &[(i32, &str)] = &[
            (2, "Kerberos V5"),
            (6, "SCM credential"),
            (7, "GSSAPI"),
            (9, "SSPI"),
        ];

        let mut chains = Vec::new();
        for (code, name) in UNIMPLEMENTED {
            let (stream, _) =
                scripted_password_auth_server(code.to_be_bytes().to_vec(), false).await;
            let error = match scram_config().connect_raw(stream, NoTls).await {
                Ok(_) => panic!("the driver accepted authentication code {code}"),
                Err(error) => error,
            };
            let chain = authentication_error_chain(error);
            assert!(
                chain.contains(name),
                "authentication code {code} must be refused by name, got: {chain}"
            );
            chains.push((code, name, chain));
        }

        // No refusal may carry another method's name, which is what a single
        // shared message would do.
        for (code, _, chain) in &chains {
            for (other_code, other_name) in UNIMPLEMENTED {
                if code != &other_code {
                    assert!(
                        !chain.contains(other_name),
                        "the refusal for code {code} also names {other_name}: {chain}"
                    );
                }
            }
        }
    }

    /// The MD5 response the driver actually puts on the wire.
    ///
    /// MD5 is reached today only through `require_auth=!md5`, which REFUSES
    /// before any hash is computed -- the assertion there is
    /// `md5_client_bytes.is_empty()`. So the SUCCESS path, where the driver
    /// derives the response and sends it, has never been checked. It is three
    /// arguments in an order nothing else would catch: PostgreSQL specifies
    /// `"md5" + md5(md5(password + username) + salt)`, so swapping user and
    /// password, or hashing the raw digest rather than its hex text, yields a
    /// well-formed frame that every md5-configured server rejects. Nothing in
    /// this suite talks to such a server, so the failure would first appear in
    /// a deployment.
    ///
    /// THE EXPECTED VALUE IS A LITERAL COMPUTED OUTSIDE THIS CRATE (`md5sum`
    /// over the two concatenations), not by calling the helper the driver
    /// calls. An oracle that shares the implementation under test cannot fail.
    /// A server that announces an encoding this driver cannot decode is
    /// refused DURING THE HANDSHAKE, before any row can be mis-decoded.
    ///
    /// `Config::param` already refused one named in the connection string and
    /// `route_async` retires a session that changes it later; the startup
    /// announcement was believed. Where the foreign bytes are themselves valid
    /// UTF-8 the mis-decode is SILENT, which is why this is caught at the
    /// protocol layer rather than left to the string decoder.
    #[compio::test]
    async fn a_startup_encoding_this_driver_cannot_decode_is_refused() {
        let script = successful_handshake([frame(b'S', b"client_encoding\0LATIN1\0")]);
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let error = match scram_config().connect_raw(stream, NoTls).await {
            Ok(_) => panic!("a server announcing LATIN1 was accepted"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("client_encoding"),
            "the refusal must name the setting that caused it: {chain}"
        );
    }

    /// THE CONTROL, and it is not optional: PostgreSQL reports client_encoding
    /// at startup on EVERY healthy connection, so "refuse when the server
    /// announces client_encoding" would be satisfied by refusing everything.
    /// Both accepted spellings are exercised -- `UNICODE` is libpq's alias for
    /// UTF8, so dropping it would narrow the predicate silently.
    #[compio::test]
    async fn a_startup_encoding_the_driver_can_decode_is_accepted() {
        for spelling in ["UTF8", "utf-8", "UNICODE"] {
            let announcement = format!("client_encoding\0{spelling}\0");
            let script = successful_handshake([frame(b'S', announcement.as_bytes())]);
            let (stream, _) = scripted_server_after_startup(Some(script)).await;
            let (client, connection) = scram_config()
                .connect_raw(stream, NoTls)
                .await
                .unwrap_or_else(|error| panic!("{spelling} must be accepted: {error}"));
            drop(client);
            drop(connection);
        }
    }

    #[compio::test]
    async fn the_md5_response_is_the_digest_postgresql_specifies() {
        let mut md5_request = 5i32.to_be_bytes().to_vec();
        md5_request.extend_from_slice(&[1, 2, 3, 4]);
        let (stream, client_bytes) = scripted_password_auth_server(md5_request, true).await;

        // `scram_config` sets user `scripted-user`, password `scripted-password`.
        let (client, connection) = scram_config()
            .connect_raw(stream, NoTls)
            .await
            .expect("the scripted MD5 exchange must complete");

        let client_bytes = compio::time::timeout(Duration::from_secs(5), client_bytes)
            .await
            .expect("scripted MD5 server did not finish")
            .expect("scripted MD5 server dropped its observation")
            .expect("scripted MD5 server failed");

        assert_eq!(
            client_bytes.first(),
            Some(&b'p'),
            "an MD5 challenge is answered with a PasswordMessage"
        );
        // md5sum("scripted-passwordscripted-user")           -> 68b72f20...
        // md5sum(that hex text || 0x01 0x02 0x03 0x04)       -> d2ba4564...
        const EXPECTED: &[u8] = b"md5d2ba45640380329c74e4c87fb4a1bdfa";
        assert!(
            client_bytes
                .windows(EXPECTED.len())
                .any(|window| window == EXPECTED),
            "the MD5 response is not the digest PostgreSQL specifies: {}",
            String::from_utf8_lossy(&client_bytes)
        );
        drop(client);
        drop(connection);
    }

    #[compio::test]
    async fn none_and_negated_none_control_whether_authentication_may_be_skipped() {
        let mut none = scram_config();
        none.require_auth(RequireAuth::Require(AuthMethods::new(AuthMethod::None)));
        let (no_auth_stream, _) =
            scripted_server_after_startup(Some(successful_handshake(std::iter::empty()))).await;
        let (client, connection) = none
            .connect_raw(no_auth_stream, NoTls)
            .await
            .expect("require_auth=none must permit an authentication-free connection");
        drop(client);
        drop(connection);

        let (cleartext_stream, cleartext_client_bytes) =
            scripted_password_auth_server(3i32.to_be_bytes().to_vec(), false).await;
        let error = match none.connect_raw(cleartext_stream, NoTls).await {
            Ok(_) => panic!("require_auth=none accepted a password challenge"),
            Err(error) => error,
        };
        let cleartext_client_bytes =
            compio::time::timeout(Duration::from_secs(5), cleartext_client_bytes)
                .await
                .expect("scripted cleartext server did not finish")
                .expect("scripted cleartext server dropped its observation")
                .expect("scripted cleartext server failed");
        assert!(
            cleartext_client_bytes.is_empty(),
            "require_auth=none answered a password challenge: {cleartext_client_bytes:?}"
        );
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("none") && chain.contains("cleartext password"),
            "require_auth=none produced an unclear refusal: {chain}"
        );

        let mut not_none = scram_config();
        not_none.require_auth(RequireAuth::Reject(AuthMethods::new(AuthMethod::None)));
        let (no_auth_stream, _) =
            scripted_server_after_startup(Some(successful_handshake(std::iter::empty()))).await;
        let error = match not_none.connect_raw(no_auth_stream, NoTls).await {
            Ok(_) => panic!("require_auth=!none accepted an authentication-free connection"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("!none") && chain.contains("did not complete authentication"),
            "require_auth=!none produced an unclear refusal: {chain}"
        );

        let (cleartext_stream, _) =
            scripted_password_auth_server(3i32.to_be_bytes().to_vec(), true).await;
        let (client, connection) = not_none
            .connect_raw(cleartext_stream, NoTls)
            .await
            .expect("require_auth=!none must permit a completed password exchange");
        drop(client);
        drop(connection);
    }

    /// A server that offers SCRAM and then names `iteration_count` in its
    /// server-first message.
    ///
    /// The count drives PBKDF2 on the CLIENT, so a server that names an absurd
    /// one spends our CPU during authentication, before any query runs.
    async fn scram_server_naming_iteration_count(iteration_count: u32) -> crate::Socket {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Startup packet: a bare length prefix, no tag.
            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.unwrap();
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
            result.unwrap();

            // AuthenticationSASL: offer SCRAM-SHA-256, then the list terminator.
            let mut body = 10i32.to_be_bytes().to_vec();
            body.extend_from_slice(b"SCRAM-SHA-256\0\0");
            let compio::BufResult(result, _) = socket.write_all(frame(b'R', &body)).await;
            result.unwrap();
            socket.flush().await.unwrap();

            // The client's SASLInitialResponse, so its nonce can be echoed back:
            // the client rejects a server-first whose nonce is not its own.
            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; 1]).await;
            result.unwrap();
            let compio::BufResult(result, len) = socket.read_exact(vec![0u8; 4]).await;
            result.unwrap();
            let len = u32::from_be_bytes(len.try_into().unwrap()) as usize;
            let compio::BufResult(result, body) = socket.read_exact(vec![0u8; len - 4]).await;
            result.unwrap();
            let marker = body
                .windows(2)
                .position(|w| w == b"r=")
                .expect("the client-first message carries a nonce");
            let client_nonce = String::from_utf8(body[marker + 2..].to_vec())
                .expect("the SCRAM nonce is printable ASCII");

            // AuthenticationSASLContinue. The salt is a valid base64 literal;
            // what is under test is the iteration count beside it.
            let server_first =
                format!("r={client_nonce}cpgserver,s=QSXCR+Q6sek8bf92,i={iteration_count}");
            let mut body = 11i32.to_be_bytes().to_vec();
            body.extend_from_slice(server_first.as_bytes());
            let compio::BufResult(result, _) = socket.write_all(frame(b'R', &body)).await;
            result.unwrap();
            socket.flush().await.unwrap();

            let compio::BufResult(_, _) = socket.read(vec![0u8; 1]).await;
        })
        .detach();

        crate::Socket::new_tcp(TcpStream::connect(addr).await.unwrap())
    }

    /// An absurd SCRAM iteration count must be REFUSED, not computed.
    ///
    /// `postgres-protocol` 0.6.11 fed the server's count straight into PBKDF2
    /// with no ceiling; 0.6.12 rejects anything above 100000 inside
    /// `ScramSha256::update`, before the key derivation runs. The cap lives in
    /// the dependency, so this test exists to fail if the lockfile is ever
    /// moved back.
    ///
    /// The assertion is the ERROR MESSAGE, not the timeout: PBKDF2 is a
    /// synchronous loop, so `compio::time::timeout` cannot interrupt it. The
    /// timeout here only stops an uncapped build from wedging the suite
    /// forever; 5000000 iterations is absurd but still bounded, so a build
    /// without the cap fails on the message rather than hanging for hours.
    #[compio::test]
    async fn an_absurd_scram_iteration_count_is_refused_rather_than_computed() {
        let stream = scram_server_naming_iteration_count(5_000_000).await;

        let result = compio::time::timeout(
            Duration::from_secs(10),
            scram_config().connect_raw(stream, NoTls),
        )
        .await
        .expect("the client burned CPU on the server's iteration count instead of refusing it");

        let error = match result {
            Ok(_) => panic!("a 900000000-iteration server-first message was accepted"),
            Err(error) => error,
        };
        let text = error.to_string();
        let cause = error
            .into_source()
            .map(|c| c.to_string())
            .unwrap_or_default();
        let chain = format!("{text}: {cause}");
        assert!(
            chain.contains("iteration"),
            "expected the iteration cap to name itself, got: {chain}"
        );
    }

    #[compio::test]
    async fn handshake_rejects_excess_delayed_messages() {
        let notices = (0..=EXPECTED_DELAYED_MESSAGE_LIMIT)
            .map(|index| notice(&format!("notice {index}")));
        let (stream, _) =
            scripted_server_after_startup(Some(successful_handshake(notices))).await;

        let error = match plaintext_config().connect_raw(stream, NoTls).await {
            Ok(_) => panic!(
                "handshake retained more than {EXPECTED_DELAYED_MESSAGE_LIMIT} delayed messages"
            ),
            Err(error) => error,
        };
        let cause = error
            .into_source()
            .expect("the delayed-message error must explain the limit");
        let io = cause
            .downcast_ref::<std::io::Error>()
            .expect("the delayed-message limit is a protocol I/O error");
        assert_eq!(io.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            io.to_string()
                .contains(&EXPECTED_DELAYED_MESSAGE_LIMIT.to_string()),
            "the error must name the delayed-message limit: {io}"
        );
    }

    /// The byte budget must charge what a message RETAINS, not what its
    /// parsed fields sum to.
    ///
    /// `Message::parse` splits the whole frame off the read buffer and the
    /// message owns all of it, so a notice whose fields are followed by
    /// padding holds the padding too. The first version of this guard walked
    /// the parsed fields instead: the frame below measured a handful of bytes
    /// and retained a megabyte, so a peer could stay under the budget while
    /// holding whatever it liked. Sixteen of these is past 1 MiB but nowhere
    /// near the 256-message count cap, so only the byte budget can reject it.
    #[compio::test]
    async fn handshake_charges_a_padded_notice_its_whole_frame() {
        let padded = (0..16).map(|index| {
            let mut body = Vec::new();
            body.extend_from_slice(b"SNOTICE\0");
            body.extend_from_slice(b"VNOTICE\0");
            body.extend_from_slice(b"C00000\0");
            body.push(b'M');
            body.extend_from_slice(format!("padded {index}").as_bytes());
            body.extend_from_slice(b"\0\0");
            // Trailing bytes the field walk never reaches and the message
            // still owns.
            body.extend_from_slice(&vec![0x41u8; 128 * 1024]);
            frame(b'N', &body)
        });
        let (stream, _) = scripted_server_after_startup(Some(successful_handshake(padded))).await;

        let error = match plaintext_config().connect_raw(stream, NoTls).await {
            Ok(_) => panic!("the handshake retained 2 MiB of padded notices"),
            Err(error) => error,
        };
        let cause = error
            .into_source()
            .expect("the byte-budget error must explain the limit");
        let io = cause
            .downcast_ref::<std::io::Error>()
            .expect("the byte budget is a protocol I/O error");
        assert_eq!(io.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            io.to_string().contains("bytes"),
            "the error must name the byte budget, not the message count: {io}"
        );
    }

    #[compio::test]
    async fn handshake_replays_a_few_delayed_notices() {
        const NOTICE_TEXTS: [&str; 3] = ["first warning", "second warning", "third warning"];
        let notices = NOTICE_TEXTS.into_iter().map(notice);
        let (stream, _) =
            scripted_server_after_startup(Some(successful_handshake(notices))).await;

        let (client, mut connection) = plaintext_config()
            .connect_raw(stream, NoTls)
            .await
            .expect("a normal handshake with a few notices must succeed");
        let mut messages = connection.notifications();
        let run = compio::runtime::spawn(async move { connection.run().await });

        for expected in NOTICE_TEXTS {
            let message = compio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .expect("connection task did not replay the delayed notice")
                .expect("the asynchronous message stream ended before replay");
            match message {
                AsyncMessage::Notice(notice) => assert_eq!(notice.message(), expected),
                other => panic!("expected a delayed notice, got {other:?}"),
            }
        }

        drop(client);
        let _ = compio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("scripted connection task did not stop after its peer closed");
    }

    #[compio::test]
    async fn connect_raw_timeout_covers_a_stalled_handshake() {
        let (stream, startup_observed) = scripted_server_after_startup(None).await;
        let mut config = plaintext_config();
        config.connect_timeout(Duration::from_secs(1));
        let connect =
            compio::runtime::spawn(async move { config.connect_raw(stream, NoTls).await });

        compio::time::timeout(Duration::from_secs(5), startup_observed)
            .await
            .expect("client did not send startup before the test watchdog")
            .expect("server closed before observing the complete startup packet");

        let result = compio::time::timeout(Duration::from_secs(5), connect)
            .await
            .expect("outer watchdog expired because connect_raw ignored connect_timeout")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let error = match result {
            Ok(_) => panic!("a silent server completed the PostgreSQL handshake"),
            Err(error) => error,
        };

        let cause = error
            .into_source()
            .expect("connection timeout must retain its I/O cause");
        let io = cause
            .downcast_ref::<std::io::Error>()
            .expect("connection timeout cause must be an I/O error");
        assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(io.to_string(), "connection timed out");
    }

    /// The shape every third-party TLS backend has before it opts into a
    /// policy: a working connector (`can_connect` is true) that overrides none
    /// of the `can_honor_*` attestations.
    struct UnattestedTls;

    impl<S> TlsConnect<S> for UnattestedTls {
        type Stream = crate::tls::NoTlsStream;
        type Error = std::io::Error;
        type Future = std::future::Ready<Result<crate::tls::NoTlsStream, std::io::Error>>;

        fn connect(self, _: S) -> Self::Future {
            std::future::ready(Err(std::io::Error::other("not a real handshake")))
        }
    }

    const VERIFIER_CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/verifier_ca.pem");

    fn tls_config(extra: &str) -> Config {
        format!("host=db.example.com {extra}")
            .parse::<Config>()
            .expect("parse a TLS connection string")
    }

    /// A connector that never read `sslrootcert` cannot have checked the
    /// server certificate against it, so the modes that promise verification
    /// must refuse it rather than report an authenticated session.
    #[test]
    fn a_connector_that_cannot_attest_to_verification_is_refused() {
        for extra in [
            format!("sslmode=verify-full sslrootcert={VERIFIER_CA}"),
            format!("sslmode=verify-ca sslrootcert={VERIFIER_CA}"),
            format!("sslmode=require sslrootcert={VERIFIER_CA}"),
        ] {
            let config = tls_config(&extra);
            let error = validate_tls_connector_parameters::<crate::Socket, _>(
                &UnattestedTls,
                Encryption::Tls,
                &config,
            )
            .expect_err(&format!("`{extra}` accepted an unverifying TLS connector"));
            let cause = std::error::Error::source(&error)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                cause.contains("sslmode="),
                "the refusal must name the setting it could not honour: {cause}"
            );
        }
    }

    /// The same hole reached without any `sslrootcert` at all. The rustls
    /// connector refuses this configuration when it is built; a connector this
    /// crate did not build was never asked, so the refusal has to live on the
    /// connection path as well.
    #[test]
    fn a_verifying_mode_with_no_trust_anchors_is_refused_on_the_connection_path() {
        for mode in ["verify-ca", "verify-full"] {
            let config = tls_config(&format!("sslmode={mode}"));
            let error = validate_tls_connector_parameters::<crate::Socket, _>(
                &UnattestedTls,
                Encryption::Tls,
                &config,
            )
            .unwrap_err();
            let cause = std::error::Error::source(&error)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                cause.contains("needs trust anchors"),
                "sslmode={mode} with no sslrootcert must say what is missing: {cause}"
            );
        }
    }

    /// The one-variable partner. The SAME connector, under the modes that
    /// promise no server authentication at all, must still be accepted -
    /// otherwise the check above would be satisfied by refusing everything.
    #[test]
    fn a_connector_that_cannot_attest_still_serves_the_unverified_modes() {
        for mode in ["require", "prefer", "allow"] {
            let config = tls_config(&format!("sslmode={mode}"));
            validate_tls_connector_parameters::<crate::Socket, _>(
                &UnattestedTls,
                Encryption::Tls,
                &config,
            )
            .unwrap_or_else(|e| {
                panic!("sslmode={mode} promises no verification and must not be refused: {e}")
            });
        }
    }

    /// The other one-variable partner, and the one that keeps the refusal from
    /// being "nothing may ever use verify-full": the connector this crate ships
    /// reads the same `sslmode` and `sslrootcert`, so it attests and is
    /// accepted for exactly the configurations the unattested connector above
    /// was refused.
    #[cfg(feature = "tls")]
    #[test]
    fn the_connector_built_from_the_same_config_is_accepted() {
        use crate::tls::MakeTlsConnect;
        use crate::tls_rustls::MakeRustlsConnect;

        for extra in [
            format!("sslmode=verify-full sslrootcert={VERIFIER_CA}"),
            format!("sslmode=verify-ca sslrootcert={VERIFIER_CA}"),
            format!("sslmode=require sslrootcert={VERIFIER_CA}"),
            "sslmode=require".to_string(),
        ] {
            let config = tls_config(&extra);
            let mut make = MakeRustlsConnect::from_config(&config)
                .unwrap_or_else(|e| panic!("`{extra}` did not build a connector: {e}"));
            let connector = <MakeRustlsConnect as MakeTlsConnect<crate::Socket>>::make_tls_connect(
                &mut make,
                "db.example.com",
            )
            .expect("make the per-connection rustls connector");
            validate_tls_connector_parameters::<crate::Socket, _>(
                &connector,
                Encryption::Tls,
                &config,
            )
            .unwrap_or_else(|e| panic!("`{extra}` refused its own rustls connector: {e}"));
        }
    }

    /// A connector built from a *different* `sslmode` is refused, so the
    /// attestation is about the configuration in hand rather than a constant
    /// the rustls connector always returns.
    #[cfg(feature = "tls")]
    #[test]
    fn a_rustls_connector_built_for_a_weaker_mode_is_refused() {
        use crate::tls::MakeTlsConnect;
        use crate::tls_rustls::MakeRustlsConnect;

        let weak = tls_config("sslmode=require");
        let mut make = MakeRustlsConnect::from_config(&weak).expect("build the weak connector");
        let connector = <MakeRustlsConnect as MakeTlsConnect<crate::Socket>>::make_tls_connect(
            &mut make,
            "db.example.com",
        )
        .expect("make the per-connection rustls connector");

        let strong = tls_config(&format!("sslmode=verify-full sslrootcert={VERIFIER_CA}"));
        validate_tls_connector_parameters::<crate::Socket, _>(&connector, Encryption::Tls, &strong)
            .expect_err("a connector built for sslmode=require must not serve verify-full");
    }
    /// `channel_binding=require` refuses a server that never offers
    /// SCRAM-SHA-256-PLUS, and says SO.
    ///
    /// Two separate guards reject a channel-binding downgrade: the server not
    /// advertising the PLUS mechanism, and the TLS backend being unable to
    /// export `tls-server-end-point`. `tests/tls_live.rs`'s
    /// `channel_binding_require_fails_without_tls` reaches this code, but its
    /// assertion is `contains("channel binding")` -- which BOTH refusals
    /// satisfy. Over plaintext the second guard fires too, so deleting the
    /// first one leaves that test green and the server-side downgrade
    /// unguarded.
    ///
    /// This peer advertises ONLY `SCRAM-SHA-256`, which is what a server
    /// performing the downgrade looks like, and the assertion names the
    /// mechanism so the two arms cannot be confused.
    #[compio::test]
    async fn channel_binding_require_refuses_a_server_that_omits_scram_plus() {
        // AuthenticationSASL: int32(10) then NUL-terminated mechanism names,
        // the list itself terminated by a final NUL. Only the non-PLUS
        // mechanism is offered.
        let mut body = Vec::new();
        body.extend_from_slice(&10i32.to_be_bytes());
        body.extend_from_slice(b"SCRAM-SHA-256\0");
        body.push(0);

        let (stream, _) = scripted_server_after_startup(Some(frame(b'R', &body))).await;
        let mut config = scram_config();
        config.channel_binding(crate::config::ChannelBinding::Require);

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("channel_binding=require accepted a server without SCRAM-SHA-256-PLUS"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("SCRAM-SHA-256-PLUS"),
            "the refusal must name the mechanism the server failed to offer, \
             so it cannot be confused with the backend-support refusal: {chain}"
        );
    }

    /// One variable away: the same peer is ACCEPTED when channel binding is
    /// only preferred, so the refusal above belongs to the setting and not to
    /// the mechanism list.
    ///
    /// `prefer` proceeds to the SCRAM exchange, which this stub does not
    /// continue, so the connection still fails -- but it must fail SOMEWHERE
    /// ELSE, and never by naming SCRAM-SHA-256-PLUS.
    #[compio::test]
    async fn channel_binding_prefer_does_not_refuse_a_server_without_scram_plus() {
        let mut body = Vec::new();
        body.extend_from_slice(&10i32.to_be_bytes());
        body.extend_from_slice(b"SCRAM-SHA-256\0");
        body.push(0);

        let (stream, _) = scripted_server_after_startup(Some(frame(b'R', &body))).await;
        let mut config = scram_config();
        config.channel_binding(crate::config::ChannelBinding::Prefer);

        if let Err(error) = config.connect_raw(stream, NoTls).await {
            let chain = authentication_error_chain(error);
            assert!(
                !chain.contains("SCRAM-SHA-256-PLUS"),
                "prefer must not raise the channel-binding downgrade refusal: {chain}"
            );
        }
    }
}
