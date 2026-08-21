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
use crate::client::Client;
use crate::codec::{BackendMessage, BackendMessages, FrontendMessage, read_backend, write_frontend};
use crate::config::{self, Config, ReplicationMode, TargetSessionAttrs};
use crate::connect_tls::{Encryption, negotiate_tls};
use crate::connection::Connection;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::{TlsConnect, TlsStream};
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
    connect_raw_with_target_session_attrs(
        stream,
        tls,
        encryption,
        has_hostname,
        config,
        TargetSessionAttrs::Any,
        release,
    )
    .await
}

/// The normal host-routing connection path, including its session-property
/// check before the raw stream is packaged into a `Connection`.
pub(crate) async fn connect_raw_with_target_session_attrs<S, T>(
    stream: S,
    tls: T,
    encryption: Encryption,
    has_hostname: bool,
    config: &Config,
    target_session_attrs: TargetSessionAttrs,
    release: Option<crate::release::ConnectionRelease>,
) -> Result<(Client, Connection<S, T::Stream>), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    let stream = negotiate_tls(
        stream,
        encryption,
        config.get_ssl_mode(),
        config.get_ssl_negotiation(),
        tls,
        has_hostname,
    )
    .await?;

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
    let (process_id, secret_key, mut parameters) = read_info(&mut handshake).await?;
    probe_target_session_attrs(
        &mut handshake,
        target_session_attrs,
        &mut parameters,
    )
    .await
    .map_err(Error::target_session_attrs)?;

    let (sender, receiver) = mpsc::unbounded();
    let client = Client::new(
        sender,
        config.get_ssl_mode(),
        config.get_ssl_negotiation(),
        process_id,
        secret_key,
        release,
    );
    let connection = Connection::new(
        handshake.stream,
        handshake.delayed,
        parameters,
        receiver,
        client.tx_status_handle(),
        client.in_flight_requests_handle(),
    );

    Ok((client, connection))
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
    if target == TargetSessionAttrs::Any {
        return Ok(());
    }

    let mut buf = BytesMut::new();
    frontend::query("SHOW transaction_read_only", &mut buf).map_err(Error::encode)?;
    handshake.send(FrontendMessage::Raw(buf.freeze())).await?;

    let mut saw_row_description = false;
    let mut read_only = None;
    let mut saw_command_complete = false;

    loop {
        match handshake.next().await? {
            Some(Message::RowDescription(_))
                if !saw_row_description && read_only.is_none() && !saw_command_complete =>
            {
                saw_row_description = true;
            }
            Some(Message::DataRow(row))
                if saw_row_description && read_only.is_none() && !saw_command_complete =>
            {
                read_only = Some(parse_transaction_read_only(&row)?);
            }
            Some(Message::CommandComplete(_))
                if saw_row_description && read_only.is_some() && !saw_command_complete =>
            {
                saw_command_complete = true;
            }
            Some(Message::ParameterStatus(body)) => {
                parameters.insert(
                    body.name().map_err(Error::parse)?.to_string(),
                    body.value().map_err(Error::parse)?.to_string(),
                );
            }
            Some(Message::ReadyForQuery(_))
                if saw_row_description && saw_command_complete =>
            {
                let read_only = read_only.ok_or_else(Error::unexpected_message)?;
                return require_target_session_attrs(target, read_only);
            }
            Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
            Some(_) => return Err(Error::unexpected_message()),
            None => return Err(Error::closed()),
        }
    }
}

fn parse_transaction_read_only(row: &DataRowBody) -> Result<bool, Error> {
    let mut ranges = row.ranges();
    let Some(Some(range)) = ranges.next().map_err(Error::parse)? else {
        return Err(Error::unexpected_message());
    };
    if ranges.next().map_err(Error::parse)?.is_some() {
        return Err(Error::unexpected_message());
    }

    match row.buffer().get(range) {
        Some(b"on") => Ok(true),
        Some(b"off") => Ok(false),
        _ => Err(Error::connect(io::Error::new(
            io::ErrorKind::InvalidData,
            "server returned an invalid transaction_read_only value",
        ))),
    }
}

fn require_target_session_attrs(
    target: TargetSessionAttrs,
    read_only: bool,
) -> Result<(), Error> {
    match (target, read_only) {
        (TargetSessionAttrs::ReadWrite, true) => Err(Error::connect(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "database does not allow writes",
        ))),
        (TargetSessionAttrs::ReadOnly, false) => Err(Error::connect(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "database is not read only",
        ))),
        (TargetSessionAttrs::Any, _)
        | (TargetSessionAttrs::ReadWrite, false)
        | (TargetSessionAttrs::ReadOnly, true) => Ok(()),
    }
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
    let (_pid, _key, parameters) = read_info(&mut handshake).await?;

    Ok((handshake.stream.into_inner(), parameters))
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
    if let Some(application_name) = config.get_application_name() {
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
            can_skip_channel_binding(config)?;
            return Ok(());
        }
        Some(Message::AuthenticationCleartextPassword) => {
            can_skip_channel_binding(config)?;

            let pass = config
                .get_password()
                .ok_or_else(|| Error::config("password missing".into()))?;

            authenticate_password(handshake, pass).await?;
        }
        Some(Message::AuthenticationMd5Password(body)) => {
            can_skip_channel_binding(config)?;

            let pass = config
                .get_password()
                .ok_or_else(|| Error::config("password missing".into()))?;

            let output = authentication::md5_hash(user.as_bytes(), pass, body.salt());
            authenticate_password(handshake, output.as_bytes()).await?;
        }
        Some(Message::AuthenticationSasl(body)) => {
            authenticate_sasl(handshake, body, config).await?;
        }
        Some(Message::AuthenticationKerberosV5)
        | Some(Message::AuthenticationScmCredential)
        | Some(Message::AuthenticationGss)
        | Some(Message::AuthenticationSspi) => {
            return Err(Error::authentication(
                "unsupported authentication method".into(),
            ));
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
        Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
        Some(_) => return Err(Error::unexpected_message()),
        None => return Err(Error::closed()),
    };

    scram
        .finish(body.data())
        .map_err(|e| Error::authentication(e.into()))?;

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
                parameters.insert(
                    body.name().map_err(Error::parse)?.to_string(),
                    body.value().map_err(Error::parse)?.to_string(),
                );
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
    use crate::config::SslMode;
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

    fn plaintext_config() -> Config {
        let mut config = Config::new();
        config
            .user("scripted-user")
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_secs(5));
        config
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
}
