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
// The source's `StartupStream` has three fields: framed transport, the current
// backend batch, and delayed async messages. `Handshake` keeps those roles as
// `stream`, `pending`, and `delayed`, but has ten fields: direct buffered I/O
// and parsed delayed messages are joined by delayed-byte accounting, four
// protocol-negotiation fields, the backend key, and an explicit phase.
// `pending` preserves unread messages from each fresh `read_backend` batch.

use crate::Error;
use crate::buf_stream::BufStream;
use crate::cancel_token::CancelKey;
use crate::client::{Client, StatementCacheSettings};
use crate::codec::{
    BackendMessage, BackendMessages, FrontendMessage, read_backend_detached_async_frames,
    write_frontend,
};
use crate::config::{
    self, AuthMethod, Config, ProtocolVersion, ReplicationMode, TargetSessionAttrs,
};
use crate::connect_tls::negotiate_tls;
use crate::connection::Connection;
use crate::encryption::Encryption;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::{ServerVerification, TlsConnect, TlsStream};
use bytes::{Bytes, BytesMut};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use futures_channel::mpsc;
use postgres_protocol::authentication;
use postgres_protocol::authentication::sasl;
use postgres_protocol::authentication::sasl::ScramSha256;
use postgres_protocol::message::backend::{
    AuthenticationSaslBody, DataRowBody, ErrorResponseBody, Message,
};
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
/// [`DEFAULT_MAX_MESSAGE_SIZE`](crate::buf_stream::DEFAULT_MAX_MESSAGE_SIZE), 64 MiB, so 255 of
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandshakePhase {
    AwaitingAuthentication,
    Authenticating,
    ReadingStartupInfo,
    Complete,
}

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
    requested_protocol: ProtocolVersion,
    protocol: ProtocolVersion,
    min_protocol: ProtocolVersion,
    negotiation_seen: bool,
    backend_key: Option<(i32, CancelKey)>,
    phase: HandshakePhase,
}

impl<S, T> Handshake<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn new(stream: MaybeTlsStream<S, T>, config: &Config) -> Self {
        let requested_protocol = config.get_max_protocol_version();
        Self {
            stream: BufStream::new(stream),
            pending: BackendMessages::empty(),
            delayed: VecDeque::new(),
            delayed_bytes: 0,
            requested_protocol,
            protocol: requested_protocol,
            min_protocol: config.get_min_protocol_version(),
            negotiation_seen: false,
            backend_key: None,
            phase: HandshakePhase::AwaitingAuthentication,
        }
    }

    async fn send(&mut self, msg: FrontendMessage) -> Result<(), Error> {
        write_frontend(&mut self.stream, msg)?;
        if let Err(error) = self.stream.flush().await {
            return Err(self.take_available_server_error().unwrap_or(error));
        }
        Ok(())
    }

    /// Prefer an ErrorResponse which a previous handshake read already put in
    /// memory over the local write symptom which made us look for it.
    ///
    /// This never submits a socket read: a one-way transport failure must not
    /// turn a failed write into an unbounded wait. `pending` owns an already-
    /// decoded batch; the stream buffer may contain complete frames over-read
    /// behind the message which triggered this frontend write.
    fn take_available_server_error(&mut self) -> Option<Error> {
        self.take_available_server_error_if(|_| true)
    }

    fn take_available_ascii_server_error(&mut self) -> Option<Error> {
        self.take_available_server_error_if(error_response_is_ascii)
    }

    fn take_available_server_error_if(
        &mut self,
        accept: impl Fn(&ErrorResponseBody) -> bool,
    ) -> Option<Error> {
        if let Ok(Some(body)) = self.pending.first_error_response() {
            return accept(&body).then(|| Error::db(body));
        }

        loop {
            let length = self.stream.peek_u32_be(1)?;
            if length < 4 || self.stream.validate_length(length).is_err() {
                return None;
            }
            let total_len = usize::try_from(length).ok()?.checked_add(1)?;
            if self.stream.buf().len() < total_len {
                return None;
            }

            match self.stream.buf()[0] {
                b'E' => {
                    let mut frame = self.stream.buf().split_to(total_len);
                    let message = Message::parse(&mut frame).ok()??;
                    return match message {
                        Message::ErrorResponse(body) if accept(&body) => Some(Error::db(body)),
                        _ => None,
                    };
                }
                // These messages may arrive without a frontend request. Skip
                // only complete async frames; an ordinary response is a
                // protocol boundary and an error beyond it cannot safely be
                // attributed to the write which just failed.
                b'N' | b'A' | b'S' => {
                    let _ = self.stream.buf().split_to(total_len);
                }
                _ => return None,
            }
        }
    }

    /// Prefer a complete server diagnosis already decoded or over-read over a
    /// local policy, configuration, or capability refusal.
    fn prefer_available_server_error<R>(&mut self, result: Result<R, Error>) -> Result<R, Error> {
        match result {
            Ok(value) => Ok(value),
            Err(local) => Err(self.take_available_server_error().unwrap_or(local)),
        }
    }

    /// After the server announces an encoding this driver cannot decode, only
    /// an ASCII ErrorResponse can safely replace the local encoding refusal.
    fn prefer_available_ascii_server_error<R>(
        &mut self,
        result: Result<R, Error>,
    ) -> Result<R, Error> {
        match result {
            Ok(value) => Ok(value),
            Err(local) => Err(self.take_available_ascii_server_error().unwrap_or(local)),
        }
    }

    /// Read one post-handshake message. Returns `None` on clean EOF.
    ///
    /// Classification of async messages (those `read_backend` returns as
    /// `BackendMessage::Async` because they arrive at the head of a
    /// batch - ParameterStatus, NoticeResponse, NotificationResponse):
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
    ///
    /// Returns a `Message`, never an `Option<Message>`.
    ///
    /// It used to return `Result<Option<Message>, Error>`, and the `None` was
    /// unreachable: the body is a `loop` with no `break`, its only success
    /// returns are the two `Ok(..)` below, and every other exit is an `Err`. A
    /// peer that hangs up does not produce `None` either - the read fails
    /// first, with `buf_stream.rs`'s `UnexpectedEof` ("connection closed by
    /// server"), which `a_peer_that_hangs_up_during_authentication_is
    /// _reported_as_closed` pins at two cut points.
    ///
    /// That `Option` cost six `None => Err(Error::closed())` arms across this
    /// function's callers, all dead, all reported by coverage as untested.
    /// They were measured dead on 2026-09-01 - mutating two of them left the
    /// hang-up test green - and removed with the `Option` itself.
    ///
    /// Do not reintroduce it: a caller that needs "the peer went away" already
    /// gets it as an `Err` from the `?` on this call.
    async fn next(&mut self) -> Result<Message, Error> {
        loop {
            if let Some(body) = self.pending.take_raw_frame(b'v').map_err(Error::parse)? {
                match self.phase {
                    HandshakePhase::AwaitingAuthentication
                    | HandshakePhase::Authenticating
                    | HandshakePhase::ReadingStartupInfo => {}
                    HandshakePhase::Complete => {
                        return Err(protocol_error(
                            "PostgreSQL sent NegotiateProtocolVersion after startup completed",
                        ));
                    }
                }
                match self.negotiate_protocol(body) {
                    Ok(()) => continue,
                    Err(local) if local.is_config() => {
                        return Err(self.take_available_server_error().unwrap_or(local));
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(body) = self.pending.take_raw_frame(b'K').map_err(Error::parse)? {
                if self.phase != HandshakePhase::ReadingStartupInfo {
                    let timing = match self.phase {
                        HandshakePhase::AwaitingAuthentication | HandshakePhase::Authenticating => {
                            "before authentication completed"
                        }
                        HandshakePhase::Complete => "after startup completed",
                        HandshakePhase::ReadingStartupInfo => {
                            unreachable!("the phase guard excludes ReadingStartupInfo")
                        }
                    };
                    return Err(protocol_error(format!(
                        "PostgreSQL sent BackendKeyData {timing}"
                    )));
                }
                self.record_backend_key(body)?;
                continue;
            }

            // First, drain any unread messages from the previous batch -
            // only pull a fresh batch off the wire when the pending
            // iterator is empty.
            if let Some(m) = self.pending.next().map_err(Error::parse)? {
                self.authentication_started();
                return Ok(m);
            }

            // DETACHED, not shared. A delayed async frame outlives this batch:
            // it is queued in `self.delayed` and replayed to the connection
            // task after the handshake. Parsing it straight out of the read
            // buffer leaves it sharing that allocation, so a 13-byte notice
            // keeps a grown read buffer alive in full - measured at 13 bytes
            // pinning 1 MiB. `MAX_DELAYED_HANDSHAKE_BYTES` charges the frame,
            // so what is charged and what is held must be the same bytes.
            let batch = read_backend_detached_async_frames(&mut self.stream).await?;
            match batch {
                BackendMessage::Async {
                    message: msg,
                    frame_len,
                } => match msg {
                    Message::NoticeResponse(_) | Message::NotificationResponse(_) => {
                        // Preserve ordering - the connection task
                        // will replay these in front of its first
                        // real read.
                        self.delay(msg, frame_len)?;
                    }
                    // ParameterStatus must be surfaced to the handshake
                    // caller so `read_info` updates the parameter map
                    // directly. Every other async-tagged message is
                    // unexpected here; return it and let the caller
                    // produce an `unexpected_message` error.
                    _ => {
                        self.authentication_started();
                        return Ok(msg);
                    }
                },
                BackendMessage::Normal { messages, .. } => {
                    // `deferred_error` is gated by `saw_error_response`, so this
                    // batch already makes startup fail before `ReadyForQuery`;
                    // the server's ErrorResponse is the better diagnostic.
                    // Stash the iterator; the top of the loop will drain
                    // it one call at a time.
                    self.pending = messages;
                }
            }
        }
    }

    fn authentication_started(&mut self) {
        if self.phase == HandshakePhase::AwaitingAuthentication {
            self.phase = HandshakePhase::Authenticating;
        }
    }

    fn begin_startup_info(&mut self) -> Result<(), Error> {
        if self.phase != HandshakePhase::Authenticating {
            return Err(protocol_error(
                "PostgreSQL authentication completed in an invalid startup phase",
            ));
        }
        self.phase = HandshakePhase::ReadingStartupInfo;
        Ok(())
    }

    fn finish_startup(&mut self) {
        self.phase = HandshakePhase::Complete;
    }

    fn negotiate_protocol(&mut self, body: Bytes) -> Result<(), Error> {
        if self.negotiation_seen {
            return Err(protocol_error(
                "PostgreSQL sent NegotiateProtocolVersion more than once",
            ));
        }
        if body.len() < 8 {
            return Err(protocol_error(
                "PostgreSQL sent a truncated NegotiateProtocolVersion message",
            ));
        }

        let version = u32::from_be_bytes(
            body[..4]
                .try_into()
                .expect("the eight-byte body minimum guarantees a complete protocol version"),
        );
        let option_count = i32::from_be_bytes(
            body[4..8]
                .try_into()
                .expect("the eight-byte body minimum guarantees a complete option count"),
        );
        let Some(protocol) = ProtocolVersion::from_wire(version) else {
            return Err(protocol_error(format!(
                "PostgreSQL negotiated unsupported protocol version {}.{}",
                version >> 16,
                version & 0xffff
            )));
        };
        if protocol > self.requested_protocol {
            return Err(protocol_error(format!(
                "PostgreSQL negotiated protocol {} above the requested max_protocol_version={}",
                protocol.as_str(),
                self.requested_protocol.as_str()
            )));
        }
        if option_count < 0 {
            return Err(protocol_error(
                "PostgreSQL sent a negative unsupported-option count in \
                 NegotiateProtocolVersion",
            ));
        }
        if protocol == self.requested_protocol && option_count == 0 {
            return Err(protocol_error(format!(
                "PostgreSQL sent NegotiateProtocolVersion without lowering protocol {}",
                protocol.as_str()
            )));
        }
        if protocol < self.min_protocol {
            return Err(Error::config(
                format!(
                    "PostgreSQL negotiated protocol {}, below min_protocol_version={}",
                    protocol.as_str(),
                    self.min_protocol.as_str()
                )
                .into(),
            ));
        }

        let mut cursor = 8;
        let mut first_option = None;
        for option_index in 0..option_count {
            let remaining = &body[cursor..];
            let Some(name_len) = remaining.iter().position(|byte| *byte == 0) else {
                return Err(protocol_error(format!(
                    "PostgreSQL ended NegotiateProtocolVersion before protocol option {} of \
                     {option_count}",
                    option_index + 1
                )));
            };
            let raw_name = &remaining[..name_len];
            let name = String::from_utf8_lossy(raw_name);
            if !raw_name.starts_with(b"_pq_.") {
                return Err(protocol_error(format!(
                    "PostgreSQL reported protocol option `{name}` without the required `_pq_.` \
                     prefix"
                )));
            }
            if first_option.is_none() {
                first_option = Some(name.into_owned());
            }
            cursor += name_len + 1;
        }
        if cursor != body.len() {
            return Err(protocol_error(
                "PostgreSQL sent trailing bytes in NegotiateProtocolVersion",
            ));
        }
        if let Some(option) = first_option {
            return Err(protocol_error(format!(
                "PostgreSQL reported unrequested protocol option `{option}`"
            )));
        }
        if let Some((_, secret_key)) = &self.backend_key {
            Self::validate_cancel_key(protocol, secret_key)?;
        }

        self.negotiation_seen = true;
        self.protocol = protocol;
        Ok(())
    }

    fn record_backend_key(&mut self, body: Bytes) -> Result<(), Error> {
        if self.backend_key.is_some() {
            return Err(protocol_error(
                "PostgreSQL sent BackendKeyData more than once during startup",
            ));
        }
        if body.len() < 4 {
            return Err(protocol_error(
                "PostgreSQL sent BackendKeyData without a complete process ID",
            ));
        }

        let process_id = i32::from_be_bytes(
            body[..4]
                .try_into()
                .expect("the four-byte body minimum guarantees a complete process ID"),
        );
        let secret_key = CancelKey::new(body.slice(4..)).map_err(Error::parse)?;
        Self::validate_cancel_key(self.protocol, &secret_key)?;

        self.backend_key = Some((process_id, secret_key));
        Ok(())
    }

    fn validate_cancel_key(protocol: ProtocolVersion, secret_key: &CancelKey) -> Result<(), Error> {
        if protocol == ProtocolVersion::V3_0 && secret_key.as_bytes().len() != 4 {
            return Err(protocol_error(format!(
                "PostgreSQL sent a {}-byte cancel key for protocol 3.0; expected 4 bytes",
                secret_key.as_bytes().len()
            )));
        }

        Ok(())
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

fn error_response_is_ascii(body: &ErrorResponseBody) -> bool {
    let mut fields = body.fields();
    loop {
        match fields.next() {
            Ok(Some(field)) => {
                if !field.type_().is_ascii() || !field.value_bytes().is_ascii() {
                    return false;
                }
            }
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

fn protocol_error(message: impl Into<String>) -> Error {
    Error::connect(io::Error::new(io::ErrorKind::InvalidData, message.into()))
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
    // A caller-owned stream has no `SocketConfig` address, but the Client still
    // records the negotiated transport so `CancelToken::cancel_query_raw` can
    // reproduce it on another caller-owned stream.
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
    mut release: Option<crate::release::ConnectionRelease>,
) -> Result<(Client, Connection<S, T::Stream>, Encryption), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    validate_tls_connector_parameters(&tls, encryption, config)?;
    let cancel_tls_policy_identity = tls.cancel_policy_identity().cloned();
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
    if let Some(release) = release.as_mut() {
        stream.configure_release(
            crate::tls::private::ForcePrivateApi,
            crate::tls::private::ReleaseConfig::new(release),
        );
    }

    let mut handshake = Handshake::new(stream, config);

    let user = match config.get_user() {
        Some(user) => Cow::Borrowed(user),
        None => Cow::Owned(whoami::username().map_err(|err| Error::io(err.into()))?),
    };

    startup(&mut handshake, config, &user).await?;
    authenticate(&mut handshake, config, &user).await?;
    let client_cert_status = handshake.stream.get_mut().client_cert_status();
    let ssl_cert_check = check_ssl_cert_mode(config, client_cert_status);
    handshake.prefer_available_server_error(ssl_cert_check)?;
    let (process_id, secret_key, mut parameters) = read_info(&mut handshake).await?;
    probe_target_session_attrs(&mut handshake, target_session_attrs, &mut parameters).await?;

    // `connect_timeout` owns negotiation, startup, authentication, and the
    // target-session probe above. The socket-read inactivity clock is a
    // different, post-startup clock and begins only when Connection::run owes
    // an application response.
    handshake
        .stream
        .set_read_timeout(config.get_read_timeout().copied());

    // Applied after the handshake, like the read deadline above: startup
    // frames are small and fixed, so a caller's limit governs the data phase
    // and cannot make authentication unreachable.
    if let Some(max) = config.get_max_message_size() {
        handshake.stream.set_max_message_size(max);
    }

    let (sender, receiver) = mpsc::unbounded();
    let drop_release = release.as_ref().map(|release| release.connection_guard());
    let mut client = Client::new_with_statement_cache(
        sender,
        config.get_ssl_mode(),
        config.get_ssl_negotiation(),
        process_id,
        secret_key,
        release,
        handshake.protocol,
        StatementCacheSettings::new(
            config.get_statement_cache_capacity(),
            config.get_statement_cache_execution_threshold(),
        ),
    );
    client.set_cancel_tls_policy(
        negotiated,
        config.get_ssl_sni(),
        config.get_ssl_cert_mode(),
        if negotiated == Encryption::Plaintext {
            ServerVerification::None
        } else {
            ServerVerification::demanded_by(config.get_ssl_mode(), config.get_ssl_root_cert())?
        },
        if negotiated == Encryption::Tls {
            cancel_tls_policy_identity
        } else {
            None
        },
    );
    let connection = Connection::new(
        handshake.stream,
        handshake.delayed,
        parameters,
        client.parameters_handle(),
        receiver,
        client.tx_status_handle(),
        client.in_flight_requests_handle(),
        client.terminal_server_error_handle(),
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
        return Err(Error::tls_unattested(
            format!(
                "sslsni={} cannot be honoured by the supplied TLS connector",
                u8::from(ssl_sni)
            )
            .into(),
        ));
    }

    let ssl_cert_mode = config.get_ssl_cert_mode();
    if !tls.can_honor_sslcertmode(ssl_cert_mode) {
        return Err(Error::tls_unattested(
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
        return Err(Error::tls_unattested(
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
    if let Some(state) = target_session_state_from_parameters(target, parameters) {
        return require_target_session_attrs(target, state);
    }

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
    frontend::query(probe.query(), &mut buf)
        .map_err(Error::encode)
        .map_err(Error::target_session_attrs_fatal)?;
    handshake
        .send(FrontendMessage::Raw(buf.freeze()))
        .await
        .map_err(Error::target_session_attrs_fatal)?;

    let mut saw_row_description = false;
    let mut state = None;
    let mut saw_command_complete = false;

    loop {
        let message = handshake
            .next()
            .await
            .map_err(Error::target_session_attrs_fatal)?;
        match message {
            Message::RowDescription(_)
                if !saw_row_description && state.is_none() && !saw_command_complete =>
            {
                saw_row_description = true;
            }
            Message::DataRow(row)
                if saw_row_description && state.is_none() && !saw_command_complete =>
            {
                state = Some(probe.parse(&row).map_err(Error::target_session_attrs)?);
            }
            Message::CommandComplete(_)
                if saw_row_description && state.is_some() && !saw_command_complete =>
            {
                saw_command_complete = true;
            }
            Message::ParameterStatus(body) => {
                let name = body
                    .name()
                    .map_err(Error::parse)
                    .map_err(Error::target_session_attrs_fatal)?
                    .to_string();
                let value = body
                    .value()
                    .map_err(Error::parse)
                    .map_err(Error::target_session_attrs_fatal)?
                    .to_string();
                let result = record_parameter_status(parameters, name, value);
                handshake
                    .prefer_available_ascii_server_error(result)
                    .map_err(Error::target_session_attrs_fatal)?;
            }
            Message::ReadyForQuery(_) if saw_row_description && saw_command_complete => {
                let state = state
                    .ok_or_else(Error::unexpected_message)
                    .map_err(Error::target_session_attrs)?;
                return require_target_session_attrs(target, state);
            }
            Message::ErrorResponse(body) => {
                return Err(Error::target_session_attrs(Error::db(body)));
            }
            _ => return Err(Error::target_session_attrs(Error::unexpected_message())),
        }
    }
}

/// Use the same startup status fast path as libpq before falling back to SQL.
///
/// For a read-only decision, both values must be known: hot standby OR a
/// read-only transaction default makes the session read-only. For a recovery
/// decision, `in_hot_standby` alone is the property being tested.
fn target_session_state_from_parameters(
    target: TargetSessionAttrs,
    parameters: &HashMap<String, String>,
) -> Option<TargetSessionState> {
    let in_hot_standby = parameter_status_bool(parameters, "in_hot_standby");

    match target {
        TargetSessionAttrs::Any => None,
        TargetSessionAttrs::ReadWrite | TargetSessionAttrs::ReadOnly => {
            let default_read_only =
                parameter_status_bool(parameters, "default_transaction_read_only")?;
            let in_hot_standby = in_hot_standby?;
            Some(TargetSessionState::TransactionReadOnly(
                default_read_only || in_hot_standby,
            ))
        }
        TargetSessionAttrs::Primary
        | TargetSessionAttrs::Standby
        | TargetSessionAttrs::PreferStandby => {
            // Servers before 9.0 predate hot standby and cannot be in
            // recovery as a queryable standby. libpq therefore treats them as
            // primary without sending pg_is_in_recovery(), a function those
            // servers do not have.
            let in_hot_standby = in_hot_standby.or_else(|| {
                server_major_version(parameters)
                    .is_some_and(|major| major < 9)
                    .then_some(false)
            })?;
            Some(TargetSessionState::InRecovery(in_hot_standby))
        }
    }
}

fn server_major_version(parameters: &HashMap<String, String>) -> Option<u32> {
    parameters
        .get("server_version")?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn parameter_status_bool(parameters: &HashMap<String, String>, name: &str) -> Option<bool> {
    match parameters.get(name).map(String::as_str) {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
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
            (Self::TransactionReadOnly, b"on") => Ok(TargetSessionState::TransactionReadOnly(true)),
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
        (TargetSessionAttrs::ReadWrite, TargetSessionState::TransactionReadOnly(true)) => Err(
            target_session_attrs_mismatch("database does not allow writes"),
        ),
        (TargetSessionAttrs::ReadOnly, TargetSessionState::TransactionReadOnly(false)) => {
            Err(target_session_attrs_mismatch("database is not read only"))
        }
        (TargetSessionAttrs::Primary, TargetSessionState::InRecovery(true)) => Err(
            target_session_attrs_mismatch("database server is in recovery"),
        ),
        (
            TargetSessionAttrs::Standby | TargetSessionAttrs::PreferStandby,
            TargetSessionState::InRecovery(false),
        ) => Err(target_session_attrs_mismatch(
            "database server is not in recovery",
        )),
        (TargetSessionAttrs::Any, _)
        | (TargetSessionAttrs::ReadWrite, TargetSessionState::TransactionReadOnly(false))
        | (TargetSessionAttrs::ReadOnly, TargetSessionState::TransactionReadOnly(true))
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
/// Used by [`crate::replication::connect_replication`] - the
/// replication-mode connection does NOT spawn a `Connection::run` task
/// because the wire protocol after `START_REPLICATION` is bespoke
/// (`CopyBothResponse` is not in postgres-protocol's tag list).
///
/// This is exported `pub(crate)` so the replication module can reuse
/// the handshake state machine without duplicating ~250 LOC of auth/SASL
/// code. The returned [`BufStream`] is the SAME one that decoded startup:
/// bytes following `ReadyForQuery` may already be in its read buffer.
pub(crate) async fn handshake_for_replication<S, T>(
    stream: MaybeTlsStream<S, T>,
    config: &Config,
) -> Result<
    (
        BufStream<MaybeTlsStream<S, T>>,
        i32,
        Option<CancelKey>,
        std::collections::HashMap<String, String>,
    ),
    Error,
>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: crate::tls::TlsStream + Unpin,
{
    let mut handshake = Handshake::new(stream, config);

    let user = match config.get_user() {
        Some(user) => Cow::Borrowed(user),
        None => Cow::Owned(whoami::username().map_err(|err| Error::io(err.into()))?),
    };

    startup(&mut handshake, config, &user).await?;
    authenticate(&mut handshake, config, &user).await?;
    let client_cert_status = handshake.stream.get_mut().client_cert_status();
    let ssl_cert_check = check_ssl_cert_mode(config, client_cert_status);
    handshake.prefer_available_server_error(ssl_cert_check)?;
    let (process_id, secret_key, parameters) = read_info(&mut handshake).await?;

    Ok((handshake.stream, process_id, secret_key, parameters))
}

/// Enforce `sslcertmode=require` only after PostgreSQL authentication succeeds.
///
/// That timing matches libpq: an earlier server authentication failure remains
/// the primary error, while a server that would otherwise accept the session
/// cannot quietly omit the requested TLS client-certificate exchange.
fn check_ssl_cert_mode(config: &Config, status: crate::tls::ClientCertStatus) -> Result<(), Error> {
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
    buf[4..8].copy_from_slice(&config.get_max_protocol_version().as_wire().to_be_bytes());

    handshake.send(FrontendMessage::Raw(buf.freeze())).await
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
        Message::AuthenticationOk => {
            handshake
                .prefer_available_server_error(check_require_auth(config, AuthMethod::None))?;
            handshake.prefer_available_server_error(can_skip_channel_binding(config))?;
            return Ok(());
        }
        Message::AuthenticationCleartextPassword => {
            handshake
                .prefer_available_server_error(check_require_auth(config, AuthMethod::Password))?;
            handshake.prefer_available_server_error(can_skip_channel_binding(config))?;

            let pass = handshake.prefer_available_server_error(
                config
                    .get_password()
                    .ok_or_else(|| Error::authentication("password missing".into())),
            )?;

            authenticate_password(handshake, pass).await?;
        }
        Message::AuthenticationMd5Password(body) => {
            handshake.prefer_available_server_error(check_require_auth(config, AuthMethod::Md5))?;
            handshake.prefer_available_server_error(can_skip_channel_binding(config))?;

            let pass = handshake.prefer_available_server_error(
                config
                    .get_password()
                    .ok_or_else(|| Error::authentication("password missing".into())),
            )?;

            let output = authentication::md5_hash(user.as_bytes(), pass, body.salt());
            authenticate_password(handshake, output.as_bytes()).await?;
        }
        Message::AuthenticationSasl(body) => {
            // PostgreSQL 16's only SASL authentication family is SCRAM; both
            // SCRAM-SHA-256 and SCRAM-SHA-256-PLUS map to this policy name.
            // Check before constructing or writing the client-first message.
            handshake.prefer_available_server_error(check_require_auth(
                config,
                AuthMethod::ScramSha256,
            ))?;
            authenticate_sasl(handshake, body, config).await?;
        }
        // The four methods this driver does not implement. Each names ITSELF:
        // all four used to return the same "unsupported authentication method",
        // which tells an operator nothing about what their server asked for or
        // what to reconfigure. This crate already holds refusals to that
        // standard elsewhere -- `libpq_parameter_parity` requires a refused
        // connection parameter to be named through the error's source chain,
        // for exactly this reason.
        Message::AuthenticationGss => {
            handshake.prefer_available_server_error(check_require_auth(config, AuthMethod::Gss))?;
            return handshake
                .prefer_available_server_error(Err(unsupported_authentication("GSSAPI")));
        }
        Message::AuthenticationSspi => {
            handshake
                .prefer_available_server_error(check_require_auth(config, AuthMethod::Sspi))?;
            return handshake
                .prefer_available_server_error(Err(unsupported_authentication("SSPI")));
        }
        // Neither of these has an `AuthMethod` variant, so neither consults
        // `require_auth`: there is no policy that could permit a method the
        // driver cannot perform.
        Message::AuthenticationKerberosV5 => {
            return handshake
                .prefer_available_server_error(Err(unsupported_authentication("Kerberos V5")));
        }
        Message::AuthenticationScmCredential => {
            return handshake
                .prefer_available_server_error(Err(unsupported_authentication("SCM credential")));
        }
        Message::ErrorResponse(body) => return Err(Error::db(body)),
        _ => return Err(Error::unexpected_message()),
    }

    // After sending our credentials, expect an AuthenticationOk.
    match handshake.next().await? {
        Message::AuthenticationOk => Ok(()),
        Message::ErrorResponse(body) => Err(Error::db(body)),
        _ => Err(Error::unexpected_message()),
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
        format!("authentication method requirement \"{policy}\" failed: {reason}").into(),
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
    let encoded = frontend::password_message(password, &mut buf).map_err(Error::encode);
    handshake.prefer_available_server_error(encoded)?;
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
    let password = handshake.prefer_available_server_error(
        config
            .get_password()
            .ok_or_else(|| Error::authentication("password missing".into())),
    )?;

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

    let negotiated_encryption = handshake.stream.get_mut().negotiated_encryption();
    let tls_server_end_point = handshake
        .stream
        .get_mut()
        .channel_binding()
        .tls_server_end_point;

    let channel_binding_cfg = config.get_channel_binding();

    // A plaintext PLUS offer can signal that a proxy stripped TLS. Refuse it
    // even when channel binding was explicitly disabled, as libpq does.
    if has_scram_plus && negotiated_encryption == Encryption::Plaintext {
        return handshake.prefer_available_server_error(Err(Error::authentication(
            "server offered SCRAM-SHA-256-PLUS authentication over a non-TLS connection".into(),
        )));
    }

    // Give `require` precise errors before the general selector below.
    if channel_binding_cfg == config::ChannelBinding::Require {
        if !has_scram_plus {
            return handshake.prefer_available_server_error(Err(Error::authentication(
                "server did not offer SCRAM-SHA-256-PLUS but channel binding was required".into(),
            )));
        }
        if tls_server_end_point.is_none() {
            return handshake.prefer_available_server_error(Err(Error::tls(
                "channel binding requested but backend does not support it".into(),
            )));
        }
    }

    let channel_binding = tls_server_end_point
        .filter(|_| channel_binding_cfg != config::ChannelBinding::Disable)
        .map(sasl::ChannelBinding::tls_server_end_point);

    let (channel_binding, mechanism) =
        if has_scram_plus && channel_binding_cfg != config::ChannelBinding::Disable {
            match channel_binding {
                Some(channel_binding) => (channel_binding, sasl::SCRAM_SHA_256_PLUS),
                None => {
                    return handshake.prefer_available_server_error(Err(Error::tls(
                        "tls-server-end-point channel binding is unavailable for SCRAM-SHA-256-PLUS"
                            .into(),
                    )));
                }
            }
        } else if has_scram {
            let channel_binding = if negotiated_encryption == Encryption::Tls
                && channel_binding_cfg != config::ChannelBinding::Disable
            {
                // `y` lets a capable server detect a stripped PLUS advertisement.
                sasl::ChannelBinding::unrequested()
            } else {
                sasl::ChannelBinding::unsupported()
            };
            (channel_binding, sasl::SCRAM_SHA_256)
        } else {
            return handshake.prefer_available_server_error(Err(Error::authentication(
                "unsupported SASL mechanism".into(),
            )));
        };

    if mechanism != sasl::SCRAM_SHA_256_PLUS {
        handshake.prefer_available_server_error(can_skip_channel_binding(config))?;
    }

    let mut scram = ScramSha256::new(password, channel_binding);

    let mut buf = BytesMut::new();
    frontend::sasl_initial_response(mechanism, scram.message(), &mut buf).map_err(Error::encode)?;
    handshake.send(FrontendMessage::Raw(buf.freeze())).await?;

    let body = match handshake.next().await? {
        Message::AuthenticationSaslContinue(body) => body,
        Message::AuthenticationOk => {
            return Err(incomplete_authentication_exchange(config));
        }
        Message::ErrorResponse(body) => return Err(Error::db(body)),
        _ => return Err(Error::unexpected_message()),
    };

    scram
        .update(body.data())
        .map_err(|e| Error::authentication(e.into()))?;

    let mut buf = BytesMut::new();
    frontend::sasl_response(scram.message(), &mut buf).map_err(Error::encode)?;
    handshake.send(FrontendMessage::Raw(buf.freeze())).await?;

    let body = match handshake.next().await? {
        Message::AuthenticationSaslFinal(body) => body,
        Message::AuthenticationOk => {
            return Err(incomplete_authentication_exchange(config));
        }
        Message::ErrorResponse(body) => return Err(Error::db(body)),
        _ => return Err(Error::unexpected_message()),
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
) -> Result<(i32, Option<CancelKey>, HashMap<String, String>), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    handshake.begin_startup_info()?;
    let mut parameters = HashMap::new();

    loop {
        match handshake.next().await? {
            Message::ParameterStatus(body) => {
                let result = record_parameter_status(
                    &mut parameters,
                    body.name().map_err(Error::parse)?.to_string(),
                    body.value().map_err(Error::parse)?.to_string(),
                );
                handshake.prefer_available_ascii_server_error(result)?;
            }
            // NO `NoticeResponse` ARM HERE, and its absence is deliberate.
            // `Handshake::next` only yields what `read_backend` did not classify
            // as async, and `read_backend` never leaves a notice inside a
            // `Normal` batch: at offset zero it returns `Async`, and further in
            // it ends the batch before the frame. So a notice cannot reach this
            // match, and the arm that used to sit here was unreachable - it
            // queued into `delayed` a second time, which is why the byte budget
            // has exactly one enforcement point rather than two.
            Message::ReadyForQuery(_) => {
                handshake.finish_startup();
                let (process_id, secret_key) = match handshake.backend_key.take() {
                    Some((process_id, secret_key)) => (process_id, Some(secret_key)),
                    None => (0, None),
                };
                return Ok((process_id, secret_key, parameters));
            }
            Message::ErrorResponse(body) => return Err(Error::db(body)),
            _ => return Err(Error::unexpected_message()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncMessage;
    use crate::config::{AuthMethod, AuthMethods, RequireAuth, SslMode};
    use crate::tls::NoTls;
    use compio::buf::{BufResult, IoBuf, IoBufMut};
    use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use compio::net::{TcpListener, TcpStream};
    use futures_channel::oneshot;
    use futures_util::StreamExt;
    use std::time::Duration;

    /// `expect_err` cannot interpolate the loop variable, so name the failing
    /// status explicitly when the refusal does not happen.
    trait UnwrapErrForStatus {
        fn unwrap_err_or_else_msg(self, status: crate::tls::ClientCertStatus) -> Error;
    }

    impl UnwrapErrForStatus for Result<(), Error> {
        fn unwrap_err_or_else_msg(self, status: crate::tls::ClientCertStatus) -> Error {
            match self {
                Ok(()) => panic!("sslcertmode=require was not honoured for {status:?}"),
                Err(error) => error,
            }
        }
    }

    const EXPECTED_DELAYED_MESSAGE_LIMIT: usize = 256;

    /// The full truth table for `require_target_session_attrs`: every one of the
    /// six targets against all four probe answers.
    ///
    /// The catch-all arm is the point. `ReadWrite`/`ReadOnly` are settled by
    /// `transaction_read_only` and `Primary`/`Standby`/`PreferStandby` by
    /// `pg_is_in_recovery`, so a probe answering the OTHER property cannot
    /// settle the requirement at all - ten of these twenty-four pairs are that
    /// case, and none of them had ever run.
    ///
    /// The two failure classes must not be conflated, which is why each case
    /// asserts the rendered error rather than merely `is_err`: a requirement
    /// that was checked and failed is `error checking target session
    /// attributes`, while a probe that answered the wrong question is `error
    /// connecting to server`. Asserting only "an error came back" would let the
    /// covered mismatch arms stand in for the uncovered catch-all.
    #[test]
    fn every_target_session_attrs_pairing_is_classified() {
        use TargetSessionAttrs as A;
        use TargetSessionState as S;

        #[derive(Debug)]
        enum Expect {
            Allowed,
            Mismatch,
            WrongProperty,
        }

        let cases = [
            (A::Any, S::TransactionReadOnly(true), Expect::Allowed),
            (A::Any, S::TransactionReadOnly(false), Expect::Allowed),
            (A::Any, S::InRecovery(true), Expect::Allowed),
            (A::Any, S::InRecovery(false), Expect::Allowed),
            (A::ReadWrite, S::TransactionReadOnly(false), Expect::Allowed),
            (A::ReadWrite, S::TransactionReadOnly(true), Expect::Mismatch),
            (A::ReadWrite, S::InRecovery(true), Expect::WrongProperty),
            (A::ReadWrite, S::InRecovery(false), Expect::WrongProperty),
            (A::ReadOnly, S::TransactionReadOnly(true), Expect::Allowed),
            (A::ReadOnly, S::TransactionReadOnly(false), Expect::Mismatch),
            (A::ReadOnly, S::InRecovery(true), Expect::WrongProperty),
            (A::ReadOnly, S::InRecovery(false), Expect::WrongProperty),
            (A::Primary, S::InRecovery(false), Expect::Allowed),
            (A::Primary, S::InRecovery(true), Expect::Mismatch),
            (
                A::Primary,
                S::TransactionReadOnly(true),
                Expect::WrongProperty,
            ),
            (
                A::Primary,
                S::TransactionReadOnly(false),
                Expect::WrongProperty,
            ),
            (A::Standby, S::InRecovery(true), Expect::Allowed),
            (A::Standby, S::InRecovery(false), Expect::Mismatch),
            (
                A::Standby,
                S::TransactionReadOnly(true),
                Expect::WrongProperty,
            ),
            (
                A::Standby,
                S::TransactionReadOnly(false),
                Expect::WrongProperty,
            ),
            (A::PreferStandby, S::InRecovery(true), Expect::Allowed),
            (A::PreferStandby, S::InRecovery(false), Expect::Mismatch),
            (
                A::PreferStandby,
                S::TransactionReadOnly(true),
                Expect::WrongProperty,
            ),
            (
                A::PreferStandby,
                S::TransactionReadOnly(false),
                Expect::WrongProperty,
            ),
        ];
        assert_eq!(cases.len(), 24, "the truth table stopped being exhaustive");

        let mut wrong_property_seen = 0;
        for (target, state, expect) in cases {
            let outcome = require_target_session_attrs(target, state);
            match expect {
                Expect::Allowed => {
                    outcome.unwrap_or_else(|error| {
                        panic!("{target:?} against {state:?} was refused: {error}")
                    });
                }
                Expect::Mismatch => {
                    let Err(error) = outcome else {
                        panic!("{target:?} against {state:?} was accepted, not refused")
                    };
                    assert_eq!(
                        error.to_string(),
                        "error checking target session attributes",
                        "{target:?} against {state:?} was not a requirement mismatch"
                    );
                }
                Expect::WrongProperty => {
                    wrong_property_seen += 1;
                    let Err(error) = outcome else {
                        panic!("{target:?} against {state:?} was accepted, not refused")
                    };
                    assert_eq!(
                        error.to_string(),
                        "error connecting to server",
                        "{target:?} against {state:?} was not refused as a bad probe"
                    );
                }
            }
        }
        assert_eq!(
            wrong_property_seen, 10,
            "the wrong-property arm stopped being exercised"
        );
    }

    /// `sslcertmode=require` is a demand that the connection be authenticated
    /// by a client certificate, so every way of NOT having sent one must be a
    /// refusal. Two of the five arms had never run - `NotSent` and `Unknown` -
    /// and both are the dangerous direction: had either returned `Ok`, a caller
    /// demanding certificate authentication would have proceeded without one.
    ///
    /// The `Unknown` arm is the subtler of the two. It fires when the TLS
    /// backend cannot report what it did, and refusing there is a choice: the
    /// alternative is to assume success, which is exactly the assumption this
    /// setting exists to forbid.
    #[test]
    fn sslcertmode_require_refuses_every_status_but_sent() {
        use crate::tls::ClientCertStatus;

        let required = "host=h sslcertmode=require"
            .parse::<Config>()
            .expect("parse a sslcertmode=require config");

        check_ssl_cert_mode(&required, ClientCertStatus::Sent)
            .expect("a sent certificate satisfies the demand");

        for (status, expected) in [
            (ClientCertStatus::NotApplicable, "did not request"),
            (ClientCertStatus::NotRequested, "did not request"),
            (ClientCertStatus::NotSent, "without a valid SSL certificate"),
            (ClientCertStatus::Unknown, "did not report"),
        ] {
            let error = check_ssl_cert_mode(&required, status).unwrap_err_or_else_msg(status);
            // The setting name lives in the SOURCE, not in the top-level
            // Display, which renders only "authentication error". Walk the
            // chain or the assertion below would be checking the wrong string.
            let rendered = std::iter::successors(std::error::Error::source(&error), |error| {
                std::error::Error::source(*error)
            })
            .fold(format!("{error}"), |chain, error| {
                format!("{chain}: {error}")
            });
            assert!(
                rendered.contains("sslcertmode=require"),
                "the refusal must name the setting: {rendered}"
            );
            assert!(
                rendered.contains(expected),
                "{status:?} must say why it failed, wanted {expected:?}: {rendered}"
            );
        }

        // The whole check is scoped to `require`; any other mode returns early
        // regardless of status, which is what keeps a plaintext connection from
        // being refused for lacking a certificate nobody asked for.
        let default = "host=h".parse::<Config>().expect("parse a default config");
        for status in [
            ClientCertStatus::NotApplicable,
            ClientCertStatus::NotRequested,
            ClientCertStatus::NotSent,
            ClientCertStatus::Sent,
            ClientCertStatus::Unknown,
        ] {
            check_ssl_cert_mode(&default, status)
                .expect("only sslcertmode=require inspects the status");
        }
    }

    /// Replays one coalesced backend read, then fails every frontend write.
    /// This isolates the handshake's already-buffered diagnosis choice from
    /// kernel timing and from whether a real TCP reset wins a race.
    struct HandshakeWriteFailure {
        input: Vec<u8>,
        offset: usize,
    }

    impl AsyncRead for HandshakeWriteFailure {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            let mut remaining = &self.input[self.offset..];
            let before = remaining.len();
            let BufResult(result, buf) = AsyncRead::read(&mut remaining, buf).await;
            self.offset += before - remaining.len();
            BufResult(result, buf)
        }
    }

    impl AsyncWrite for HandshakeWriteFailure {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "scripted handshake write failure",
                )),
                buf,
            )
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct HandshakeWriteSuccess {
        input: Vec<u8>,
        offset: usize,
        output: Vec<u8>,
    }

    impl AsyncRead for HandshakeWriteSuccess {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            let mut remaining = &self.input[self.offset..];
            let before = remaining.len();
            let BufResult(result, buf) = AsyncRead::read(&mut remaining, buf).await;
            self.offset += before - remaining.len();
            BufResult(result, buf)
        }
    }

    impl AsyncWrite for HandshakeWriteSuccess {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.output.extend_from_slice(buf.as_init());
            BufResult(Ok(buf.buf_len()), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl TlsStream for HandshakeWriteSuccess {
        fn channel_binding(&self) -> crate::tls::ChannelBinding {
            crate::tls::ChannelBinding::none()
        }
    }

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + body.len());
        frame.push(tag);
        frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    const SCRAM: &[u8] = b"SCRAM-SHA-256\0\0";
    const SCRAM_PLUS: &[u8] = b"SCRAM-SHA-256-PLUS\0\0";
    const SCRAM_BOTH: &[u8] = b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0";

    #[allow(clippy::future_not_send)] // compio test futures are thread-local.
    async fn sasl_attempt(
        config: &Config,
        encryption: Encryption,
        mechanisms: &[u8],
    ) -> (String, String) {
        let mut body = 10i32.to_be_bytes().to_vec();
        body.extend_from_slice(mechanisms);

        let stream = HandshakeWriteSuccess {
            input: frame(b'R', &body),
            offset: 0,
            output: vec![],
        };
        let mut handshake = Handshake::new(
            match encryption {
                Encryption::Plaintext => MaybeTlsStream::Raw(stream),
                Encryption::Tls => MaybeTlsStream::Tls(stream),
            },
            config,
        );
        let error = authenticate(&mut handshake, config, "scripted-user")
            .await
            .expect_err("the scripted peer never completes SCRAM");
        let output = match handshake.stream.get_mut() {
            MaybeTlsStream::Raw(stream) | MaybeTlsStream::Tls(stream) => {
                std::mem::take(&mut stream.output)
            }
        };
        (
            authentication_error_chain(error),
            String::from_utf8_lossy(&output).into_owned(),
        )
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

    fn error_response(code: &str, message: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"SERROR\0");
        body.extend_from_slice(b"VERROR\0");
        body.push(b'C');
        body.extend_from_slice(code.as_bytes());
        body.push(0);
        body.push(b'M');
        body.extend_from_slice(message.as_bytes());
        body.extend_from_slice(b"\0\0");
        frame(b'E', &body)
    }

    fn write_failing_handshake(
        input: Vec<u8>,
        config: &Config,
    ) -> Handshake<HandshakeWriteFailure, crate::tls::NoTlsStream> {
        Handshake::new(
            MaybeTlsStream::Raw(HandshakeWriteFailure { input, offset: 0 }),
            config,
        )
    }

    async fn assert_auth_refusal_prefers_server_error(
        config: &Config,
        authentication_body: &[u8],
        label: &str,
    ) {
        let mut script = frame(b'R', authentication_body);
        script.extend_from_slice(&error_response(
            "57P01",
            "terminating during authentication",
        ));
        let mut handshake = write_failing_handshake(script, config);
        let error = authenticate(&mut handshake, config, "scripted-user")
            .await
            .unwrap_err();
        assert_eq!(
            error.code().map(|code| code.code()),
            Some("57P01"),
            "{label} discarded queued SQLSTATE 57P01: {error}"
        );
    }

    #[compio::test]
    async fn password_write_failure_preserves_a_pending_server_error() {
        let mut script = frame(b'R', &3i32.to_be_bytes());
        script.extend_from_slice(&error_response("28P01", "password rejected"));
        let config = plaintext_config();
        let mut handshake = write_failing_handshake(script, &config);

        assert!(matches!(
            handshake.next().await.unwrap(),
            Message::AuthenticationCleartextPassword
        ));
        let error = authenticate_password(&mut handshake, b"wrong")
            .await
            .expect_err("the scripted transport must fail the password write");

        assert_eq!(
            error.code().map(|code| code.code()),
            Some("28P01"),
            "password write failure discarded SQLSTATE 28P01: {error}"
        );
    }

    #[compio::test]
    async fn authentication_policy_preserves_a_pending_server_error() {
        let mut script = frame(b'R', &0i32.to_be_bytes());
        script.extend_from_slice(&error_response(
            "57P01",
            "terminating during authentication",
        ));
        let mut config = plaintext_config();
        config.require_auth(RequireAuth::Require(AuthMethods::new(
            AuthMethod::ScramSha256,
        )));
        let mut handshake = write_failing_handshake(script, &config);

        let error = authenticate(&mut handshake, &config, "scripted-user")
            .await
            .expect_err("require_auth=scram-sha-256 accepted AuthenticationOk");
        assert_eq!(
            error.code().map(|code| code.code()),
            Some("57P01"),
            "require_auth after AuthenticationOk discarded queued SQLSTATE 57P01: {error}"
        );

        let mut local_only = write_failing_handshake(frame(b'R', &0i32.to_be_bytes()), &config);
        let error = authenticate(&mut local_only, &config, "scripted-user")
            .await
            .expect_err("require_auth=scram-sha-256 accepted AuthenticationOk");
        assert!(
            error.code().is_none(),
            "require_auth invented a server diagnosis when none was queued"
        );
    }

    #[compio::test]
    async fn authentication_local_refusals_preserve_pending_server_errors() {
        let cleartext = 3i32.to_be_bytes();
        assert_auth_refusal_prefers_server_error(
            &plaintext_config(),
            &cleartext,
            "missing cleartext password",
        )
        .await;

        let mut binding = plaintext_config();
        binding
            .password("secret")
            .channel_binding(crate::config::ChannelBinding::Require);
        assert_auth_refusal_prefers_server_error(
            &binding,
            &cleartext,
            "cleartext channel-binding refusal",
        )
        .await;

        let mut nul_password = plaintext_config();
        nul_password.password(b"secret\0suffix");
        assert_auth_refusal_prefers_server_error(
            &nul_password,
            &cleartext,
            "cleartext password encoding",
        )
        .await;

        let mut md5 = 5i32.to_be_bytes().to_vec();
        md5.extend_from_slice(b"salt");
        let mut md5_policy = plaintext_config();
        md5_policy
            .password("secret")
            .require_auth(RequireAuth::Require(AuthMethods::new(AuthMethod::Password)));
        assert_auth_refusal_prefers_server_error(&md5_policy, &md5, "MD5 authentication policy")
            .await;

        let mut scram = 10i32.to_be_bytes().to_vec();
        scram.extend_from_slice(b"SCRAM-SHA-256\0\0");
        let mut scram_policy = scram_config();
        scram_policy.require_auth(RequireAuth::Require(AuthMethods::new(AuthMethod::Md5)));
        assert_auth_refusal_prefers_server_error(
            &scram_policy,
            &scram,
            "SASL authentication policy",
        )
        .await;
        assert_auth_refusal_prefers_server_error(
            &plaintext_config(),
            &scram,
            "missing SASL password",
        )
        .await;

        let mut require_binding = scram_config();
        require_binding.channel_binding(crate::config::ChannelBinding::Require);
        assert_auth_refusal_prefers_server_error(
            &require_binding,
            &scram,
            "missing SCRAM-SHA-256-PLUS",
        )
        .await;

        let mut scram_plus = 10i32.to_be_bytes().to_vec();
        scram_plus.extend_from_slice(b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0");
        assert_auth_refusal_prefers_server_error(
            &require_binding,
            &scram_plus,
            "missing TLS channel-binding endpoint",
        )
        .await;

        let mut unsupported_sasl = 10i32.to_be_bytes().to_vec();
        unsupported_sasl.extend_from_slice(b"SCRAM-SHA-999\0\0");
        assert_auth_refusal_prefers_server_error(
            &scram_config(),
            &unsupported_sasl,
            "unsupported valid SASL mechanism",
        )
        .await;

        for (code, label) in [
            (7i32, "unsupported GSSAPI authentication"),
            (9, "unsupported SSPI authentication"),
            (2, "unsupported Kerberos V5 authentication"),
            (6, "unsupported SCM credential authentication"),
        ] {
            assert_auth_refusal_prefers_server_error(
                &plaintext_config(),
                &code.to_be_bytes(),
                label,
            )
            .await;
        }
    }

    #[compio::test]
    async fn sslcertmode_refusal_preserves_a_pending_server_error() {
        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&error_response("57P01", "terminating after authentication"));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let mut config = plaintext_config();
        config.ssl_cert_mode(config::SslCertMode::Require);

        let error = match connect_raw_with_target_session_attrs(
            stream,
            NoTls,
            Encryption::Plaintext,
            false,
            &config,
            TargetSessionAttrs::Any,
            None,
        )
        .await
        {
            Ok(_) => panic!("sslcertmode=require accepted a plaintext session"),
            Err(error) => error,
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "sslcertmode refusal discarded queued SQLSTATE 57P01: {error}"
        );
    }

    #[compio::test]
    async fn replication_sslcertmode_refusal_preserves_a_pending_server_error() {
        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&error_response("57P01", "terminating after authentication"));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let mut config = plaintext_config();
        config.ssl_cert_mode(config::SslCertMode::Require);

        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(stream);
        let error = match handshake_for_replication(stream, &config).await {
            Ok(_) => panic!("replication sslcertmode=require accepted a plaintext session"),
            Err(error) => error,
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "replication sslcertmode refusal discarded queued SQLSTATE 57P01: {error}"
        );
    }

    /// An idle PostgreSQL backend can send a FATAL ErrorResponse immediately
    /// after ReadyForQuery, for example when `pg_terminate_backend` reaches it.
    /// One socket read can therefore over-read the start of that frame while
    /// decoding startup. The replication handoff must carry those bytes into
    /// its data-phase framer; the ordinary handoff already keeps its BufStream.
    #[compio::test]
    async fn replication_handshake_preserves_coalesced_post_ready_bytes() {
        let post_ready = error_response("57P01", "terminating connection after startup");
        let mut script = successful_handshake(std::iter::empty());
        script.extend_from_slice(&post_ready);

        let config = plaintext_config();
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(HandshakeWriteSuccess {
            input: script,
            offset: 0,
            output: vec![],
        });
        let (stream, _, _, _) = handshake_for_replication(stream, &config)
            .await
            .expect("scripted replication startup must succeed");
        let mut stream = stream;

        assert_eq!(
            stream.buf().len(),
            post_ready.len(),
            "replication handshake discarded coalesced post-ReadyForQuery bytes"
        );

        let BackendMessage::Normal { mut messages, .. } =
            read_backend_detached_async_frames(&mut stream)
                .await
                .expect("decode the preserved post-ReadyForQuery frame")
        else {
            panic!("a FATAL ErrorResponse is not an asynchronous frame");
        };
        let Some(Message::ErrorResponse(error)) =
            messages.next().expect("parse the preserved ErrorResponse")
        else {
            panic!("the preserved frame was not the scripted ErrorResponse");
        };
        let error = Error::db(error);
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01")
        );
    }

    /// `Handshake::new` reads the config but does not hold it, so a local one
    /// is enough and the returned value borrows nothing.
    fn empty_handshake() -> Handshake<HandshakeWriteSuccess, crate::tls::NoTlsStream> {
        let config = plaintext_config();
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(HandshakeWriteSuccess {
            input: Vec::new(),
            offset: 0,
            output: vec![],
        });
        Handshake::new(stream, &config)
    }

    /// `record_backend_key` reads the process ID with `body[..4]` and an
    /// `expect` whose message names this very check as its justification. The
    /// body is server-controlled, so without the length guard a short
    /// BackendKeyData panics the connection task instead of failing it. The
    /// guard had never run.
    #[compio::test]
    async fn a_short_backend_key_is_refused_rather_than_panicking() {
        let mut handshake = empty_handshake();
        let error = handshake
            .record_backend_key(Bytes::from_static(&[0, 0, 0]))
            .expect_err("a three-byte BackendKeyData has no complete process ID");
        assert!(
            probe_error_chain(&error).contains("without a complete process ID"),
            "the refusal named the wrong thing: {error}"
        );
    }

    /// The same "password missing" refusal exists on three auth methods -
    /// cleartext, MD5 and SASL. Two are covered; the MD5 one was not, which is
    /// the shape where a rule gets applied to some call sites and not others.
    /// A server may still request MD5, and without a password the driver must
    /// say which thing is absent rather than hash an empty one.
    #[compio::test]
    async fn md5_authentication_without_a_password_is_refused() {
        let config = plaintext_config();
        let mut body = 5i32.to_be_bytes().to_vec(); // AuthenticationMD5Password
        body.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]); // salt
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(HandshakeWriteSuccess {
            input: frame(b'R', &body),
            offset: 0,
            output: vec![],
        });
        let mut handshake = Handshake::new(stream, &config);
        handshake.phase = HandshakePhase::Authenticating;

        let error = authenticate(&mut handshake, &config, "scripted-user")
            .await
            .expect_err("MD5 authentication cannot proceed without a password");
        assert!(
            probe_error_chain(&error).contains("password missing"),
            "the refusal named the wrong thing: {error}"
        );
    }

    /// The eight-byte minimum guards peer-controlled input, and it had never
    /// run. Dropping it panics the connection task instead of failing the
    /// handshake - MEASURED, because the mechanism is not the one the code's
    /// own comments suggest. Both `expect` messages cite this minimum, but
    /// neither fires: on a seven-byte body `body[..4]` succeeds and
    /// `body[4..8]` panics first with "range end index 8 out of range for
    /// slice of length 7". The slice bound is the real protection; the
    /// `expect`s only document it.
    #[compio::test]
    async fn a_truncated_negotiate_protocol_version_is_refused() {
        let mut handshake = empty_handshake();
        let error = handshake
            .negotiate_protocol(Bytes::from_static(&[0, 3, 0, 0, 0, 0, 0]))
            .expect_err("a seven-byte NegotiateProtocolVersion is not a complete message");
        assert!(
            probe_error_chain(&error).contains("truncated NegotiateProtocolVersion"),
            "the refusal named the wrong thing: {error}"
        );
    }

    /// PostgreSQL may only report protocol options it was asked about, and the
    /// protocol reserves the `_pq_.` prefix for them. An option without it is a
    /// peer inventing a name, which must be refused rather than recorded.
    #[compio::test]
    async fn a_protocol_option_without_the_pq_prefix_is_refused() {
        let mut handshake = empty_handshake();
        let mut body = 196608i32.to_be_bytes().to_vec(); // protocol 3.0
        body.extend_from_slice(&1i32.to_be_bytes()); // one option
        body.extend_from_slice(b"invented\0");

        let error = handshake
            .negotiate_protocol(Bytes::from(body))
            .expect_err("an option without the _pq_. prefix must be refused");
        assert!(
            probe_error_chain(&error).contains("without the required `_pq_.` prefix"),
            "the refusal named the wrong thing: {error}"
        );
    }

    /// One text column, which is the shape both target-session probes read.
    fn single_column_row_description(name: &str) -> Vec<u8> {
        let mut body = 1i16.to_be_bytes().to_vec();
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table oid
        body.extend_from_slice(&0i16.to_be_bytes()); // column id
        body.extend_from_slice(&25i32.to_be_bytes()); // text
        body.extend_from_slice(&(-1i16).to_be_bytes()); // type size
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        body.extend_from_slice(&0i16.to_be_bytes()); // text format
        frame(b'T', &body)
    }

    fn single_column_data_row(value: &[u8]) -> Vec<u8> {
        let mut body = 1i16.to_be_bytes().to_vec();
        body.extend_from_slice(&i32::try_from(value.len()).unwrap().to_be_bytes());
        body.extend_from_slice(value);
        frame(b'D', &body)
    }

    fn target_probe_script(column: &str, value: &[u8]) -> Vec<u8> {
        let mut script = single_column_row_description(column);
        script.extend_from_slice(&single_column_data_row(value));
        script.extend_from_slice(&frame(b'C', b"SHOW\0"));
        script.extend_from_slice(&frame(b'Z', b"I"));
        script
    }

    /// The message lives in the SOURCE chain; the top-level Display renders
    /// only "error connecting to server".
    fn probe_error_chain(error: &Error) -> String {
        std::iter::successors(std::error::Error::source(error), |error| {
            std::error::Error::source(*error)
        })
        .fold(format!("{error}"), |chain, error| {
            format!("{chain}: {error}")
        })
    }

    async fn target_probe_rejects(attrs: TargetSessionAttrs, column: &str, value: &[u8]) -> Error {
        let config = plaintext_config();
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(HandshakeWriteSuccess {
            input: target_probe_script(column, value),
            offset: 0,
            output: vec![],
        });
        let mut handshake = Handshake::new(stream, &config);
        handshake.phase = HandshakePhase::Complete;
        probe_target_session_attrs(&mut handshake, attrs, &mut HashMap::new())
            .await
            .expect_err("the probe accepted a value outside its documented set")
    }

    /// Both probes answer a fixed question with a fixed vocabulary - `on`/`off`
    /// for `transaction_read_only`, `t`/`f` for `pg_is_in_recovery`. Anything
    /// else must be refused rather than guessed at: the probe decides whether
    /// this session may take writes, so reading an unknown value as "false"
    /// would route writes at a standby. Neither refusal had ever run.
    #[compio::test]
    async fn a_target_probe_refuses_a_value_outside_its_vocabulary() {
        let read_only = target_probe_rejects(
            TargetSessionAttrs::ReadWrite,
            "transaction_read_only",
            b"maybe",
        )
        .await;
        assert!(
            probe_error_chain(&read_only).contains("invalid transaction_read_only"),
            "transaction_read_only refusal named the wrong thing: {read_only}"
        );

        let recovery =
            target_probe_rejects(TargetSessionAttrs::Standby, "pg_is_in_recovery", b"yes").await;
        assert!(
            probe_error_chain(&recovery).contains("invalid pg_is_in_recovery"),
            "pg_is_in_recovery refusal named the wrong thing: {recovery}"
        );
    }

    #[compio::test]
    async fn target_probe_write_failure_preserves_an_error_buffered_after_ready() {
        let mut script = frame(b'Z', b"I");
        script.extend_from_slice(&notice("retiring after startup"));
        script.extend_from_slice(&error_response("57P01", "terminating connection"));
        let config = plaintext_config();
        let mut handshake = write_failing_handshake(script, &config);
        handshake.phase = HandshakePhase::ReadingStartupInfo;

        assert!(matches!(
            handshake.next().await.unwrap(),
            Message::ReadyForQuery(_)
        ));
        handshake.finish_startup();
        let error = probe_target_session_attrs(
            &mut handshake,
            TargetSessionAttrs::ReadWrite,
            &mut HashMap::new(),
        )
        .await
        .expect_err("the scripted transport must fail the target probe write");

        assert_eq!(
            error.code().map(|code| code.code()),
            Some("57P01"),
            "target probe write failure discarded buffered SQLSTATE 57P01: {error}"
        );
    }

    #[compio::test]
    async fn target_probe_encoding_refusal_preserves_a_pending_server_error() {
        let mut script = parameter_status("client_encoding", "LATIN1");
        script.extend_from_slice(&error_response("57P01", "terminating during target probe"));
        let config = plaintext_config();
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(HandshakeWriteSuccess {
            input: script,
            offset: 0,
            output: vec![],
        });
        let mut handshake = Handshake::new(stream, &config);
        handshake.phase = HandshakePhase::Complete;

        let error = probe_target_session_attrs(
            &mut handshake,
            TargetSessionAttrs::ReadWrite,
            &mut HashMap::new(),
        )
        .await
        .expect_err("the scripted target probe accepted client_encoding=LATIN1");
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "target probe client_encoding refusal discarded queued SQLSTATE 57P01: {error}"
        );
    }

    fn parameter_status(name: &str, value: &str) -> Vec<u8> {
        let mut body = Vec::with_capacity(name.len() + value.len() + 2);
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
        frame(b'S', &body)
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
    ) -> (crate::Socket, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (startup_seen, startup_observed) = oneshot::channel();

        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.unwrap();
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            assert!(length >= 4, "startup packet length must include its header");

            let compio::BufResult(result, startup) = socket.read_exact(vec![0u8; length - 4]).await;
            result.unwrap();
            let _ = startup_seen.send(startup);

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

    async fn connect_with_startup_parameters(
        target: TargetSessionAttrs,
        parameters: &[(&str, &str)],
    ) -> Result<(), Error> {
        let mut script = frame(b'R', &0u32.to_be_bytes());
        for &(name, value) in parameters {
            script.extend_from_slice(&parameter_status(name, value));
        }
        script.extend_from_slice(&frame(b'K', &[0; 8]));
        script.extend_from_slice(&frame(b'Z', b"I"));

        // The fixture closes immediately after ReadyForQuery. A SQL probe
        // therefore sees EOF, while a decision made from startup status can
        // return the connected pair before the regular connection task starts.
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");
        connect_raw_with_target_session_attrs(
            stream,
            NoTls,
            Encryption::Plaintext,
            false,
            &config,
            target,
            None,
        )
        .await
        .map(drop)
    }

    #[compio::test]
    async fn read_only_targets_use_startup_parameter_status_without_a_query() {
        for (target, default_read_only, in_hot_standby, accepted) in [
            (TargetSessionAttrs::ReadWrite, "off", "off", true),
            (TargetSessionAttrs::ReadOnly, "off", "on", true),
            (TargetSessionAttrs::ReadWrite, "on", "off", false),
            (TargetSessionAttrs::ReadOnly, "off", "off", false),
        ] {
            let result = connect_with_startup_parameters(
                target,
                &[
                    ("default_transaction_read_only", default_read_only),
                    ("in_hot_standby", in_hot_standby),
                ],
            )
            .await;

            if accepted {
                result.unwrap_or_else(|error| {
                    panic!("startup status did not satisfy {target:?}: {error:?}")
                });
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.is_target_session_attrs(),
                    "startup status mismatch for {target:?} was not classified: {error:?}"
                );
            }
        }
    }

    #[compio::test]
    async fn recovery_targets_use_startup_parameter_status_without_a_query() {
        for (target, in_hot_standby, accepted) in [
            (TargetSessionAttrs::Primary, "off", true),
            (TargetSessionAttrs::Standby, "on", true),
            (TargetSessionAttrs::PreferStandby, "on", true),
            (TargetSessionAttrs::Primary, "on", false),
            (TargetSessionAttrs::Standby, "off", false),
        ] {
            let result =
                connect_with_startup_parameters(target, &[("in_hot_standby", in_hot_standby)])
                    .await;

            if accepted {
                result.unwrap_or_else(|error| {
                    panic!("startup status did not satisfy {target:?}: {error:?}")
                });
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.is_target_session_attrs(),
                    "startup status mismatch for {target:?} was not classified: {error:?}"
                );
            }
        }
    }

    #[compio::test]
    async fn pre_nine_servers_are_primary_without_an_unsupported_probe() {
        connect_with_startup_parameters(
            TargetSessionAttrs::Primary,
            &[("server_version", "8.4.22")],
        )
        .await
        .expect("a server predating hot standby is necessarily primary");

        let error = connect_with_startup_parameters(
            TargetSessionAttrs::Standby,
            &[("server_version", "8.4.22")],
        )
        .await
        .expect_err("a server predating hot standby cannot be a standby");
        assert!(
            error.is_target_session_attrs(),
            "the pre-9 standby mismatch was not classified: {error:?}"
        );
    }

    /// The default has to exercise both halves of protocol negotiation: ask
    /// for 3.2, then keep the same startup exchange alive when an older server
    /// selects 3.0. Checking only the requested bytes misses a decoder that
    /// cannot consume `NegotiateProtocolVersion`; checking only that the
    /// connection succeeded passes on a client that quietly kept requesting
    /// 3.0.
    #[compio::test]
    async fn the_default_requests_3_2_and_accepts_a_3_0_negotiation() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'v', &negotiation);
        script.extend_from_slice(&successful_handshake(std::iter::empty()));
        let (stream, startup) = scripted_server_after_startup(Some(script)).await;

        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");
        let connected = config.connect_raw(stream, NoTls).await;
        let startup = startup.await.expect("scripted server did not see startup");

        assert_eq!(
            startup.get(..4),
            Some(&0x0003_0002u32.to_be_bytes()[..]),
            "the default startup packet did not request protocol 3.2"
        );
        let (client, connection) =
            connected.expect("protocol 3.0 negotiation did not continue startup");
        drop((client, connection));
    }

    /// `BackendKeyData` belongs after authentication. In particular, a peer
    /// must not be able to install a 3.2 key and then downgrade the connection
    /// to 3.0, where only the fixed four-byte key is valid.
    #[compio::test]
    async fn backend_key_data_before_authentication_is_rejected() {
        let mut key_data = Vec::with_capacity(36);
        key_data.extend_from_slice(&1234i32.to_be_bytes());
        key_data.extend_from_slice(&[0x5a; 32]);

        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'K', &key_data);
        script.extend_from_slice(&frame(b'v', &negotiation));
        script.extend_from_slice(&frame(b'R', &0u32.to_be_bytes()));
        script.extend_from_slice(&frame(b'Z', b"I"));

        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");
        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("BackendKeyData before authentication was accepted"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("BackendKeyData") && chain.contains("before authentication"),
            "the out-of-phase BackendKeyData was not identified: {chain}"
        );
    }

    /// `AuthenticationOk` does not end startup. The wire contract explicitly
    /// allows the server to decline the requested minor version afterward, so
    /// a valid downgrade before version-shaped `BackendKeyData` must continue on
    /// the same connection.
    #[compio::test]
    async fn post_authentication_negotiation_continues_before_backend_key_data() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&frame(b'v', &negotiation));
        script.extend_from_slice(&frame(b'K', &[0; 8]));
        script.extend_from_slice(&frame(b'Z', b"I"));

        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");
        let (client, connection) = config
            .connect_raw(stream, NoTls)
            .await
            .expect("post-authentication protocol negotiation did not continue startup");

        assert_eq!(
            client.protocol_version(),
            ProtocolVersion::V3_0,
            "the post-authentication downgrade was not retained"
        );
        drop((client, connection));
    }

    /// Protocol negotiation remains legal between authentication exchanges.
    /// The downgrade must be transparent to the password response and the
    /// following `AuthenticationOk`.
    #[compio::test]
    async fn negotiation_during_password_authentication_continues() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'R', &3u32.to_be_bytes());
        script.extend_from_slice(&frame(b'v', &negotiation));
        script.extend_from_slice(&frame(b'R', &0u32.to_be_bytes()));
        script.extend_from_slice(&frame(b'K', &[0; 8]));
        script.extend_from_slice(&frame(b'Z', b"I"));

        let mut config = plaintext_config();
        config.password("scripted-password");
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(HandshakeWriteSuccess {
            input: script,
            offset: 0,
            output: Vec::new(),
        });
        let mut handshake = Handshake::new(stream, &config);

        authenticate(&mut handshake, &config, "scripted-user")
            .await
            .expect("protocol negotiation interrupted password authentication");
        read_info(&mut handshake)
            .await
            .expect("startup did not continue after the authentication downgrade");
        assert_eq!(handshake.protocol, ProtocolVersion::V3_0);
    }

    /// A four-byte cancel key is valid under both 3.2 and 3.0, so a later
    /// downgrade can retain it unchanged.
    #[compio::test]
    async fn negotiation_after_four_byte_backend_key_data_continues() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&frame(b'K', &[0; 8]));
        script.extend_from_slice(&frame(b'v', &negotiation));
        script.extend_from_slice(&frame(b'Z', b"I"));

        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");
        let (client, connection) = config
            .connect_raw(stream, NoTls)
            .await
            .expect("a valid four-byte cancel key prevented protocol downgrade");

        assert_eq!(client.protocol_version(), ProtocolVersion::V3_0);
        drop((client, connection));
    }

    /// `BackendKeyData` is shaped by the protocol version: 3.0 fixes the key at
    /// four bytes while 3.2 permits a variable length. A peer cannot first have
    /// a variable key accepted under requested 3.2 and then change the version
    /// governing that already-consumed key.
    #[compio::test]
    async fn downgrade_after_variable_backend_key_data_is_rejected() {
        let mut key_data = 1234i32.to_be_bytes().to_vec();
        key_data.extend_from_slice(&[0x5a; 32]);

        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&frame(b'K', &key_data));
        script.extend_from_slice(&frame(b'v', &negotiation));
        script.extend_from_slice(&frame(b'Z', b"I"));

        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");
        let Err(error) = config.connect_raw(stream, NoTls).await else {
            panic!("protocol downgrade retained a variable-length cancel key");
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("32-byte cancel key") && chain.contains("protocol 3.0"),
            "the cancel key was not revalidated against the negotiated version: {chain}"
        );
    }

    /// `max_protocol_version` is the version put on the wire, not merely a
    /// value the connection-string parser accepts.
    #[compio::test]
    async fn max_protocol_version_bounds_the_startup_request() {
        let (stream, startup) =
            scripted_server_after_startup(Some(successful_handshake(std::iter::empty()))).await;
        let config: Config = "user=scripted-user sslmode=disable max_protocol_version=3.0"
            .parse()
            .expect("parse scripted protocol maximum");

        let connected = config.connect_raw(stream, NoTls).await;
        let startup = startup.await.expect("scripted server did not see startup");
        assert_eq!(
            startup.get(..4),
            Some(&0x0003_0000u32.to_be_bytes()[..]),
            "max_protocol_version=3.0 did not bound the startup request"
        );

        let (client, connection) = connected.expect("protocol 3.0 startup failed");
        drop((client, connection));
    }

    /// A fallback below the configured floor must stop on the same socket
    /// before authentication continues, and the error must name the bound the
    /// server could not satisfy.
    #[compio::test]
    async fn min_protocol_version_rejects_a_lower_server_negotiation() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'v', &negotiation);
        script.extend_from_slice(&successful_handshake(std::iter::empty()));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable min_protocol_version=3.2"
            .parse()
            .expect("parse scripted protocol minimum");

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("protocol 3.0 satisfied min_protocol_version=3.2"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("min_protocol_version")
                && chain.contains("3.0")
                && chain.contains("3.2"),
            "the negotiation refusal did not name the server version and configured floor: \
             {chain}"
        );
    }

    #[compio::test]
    async fn min_protocol_refusal_preserves_a_pending_server_error() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'v', &negotiation);
        script.extend_from_slice(&error_response(
            "57P01",
            "terminating after protocol negotiation",
        ));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable min_protocol_version=3.2"
            .parse()
            .expect("parse scripted protocol minimum");

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("protocol 3.0 satisfied min_protocol_version=3.2"),
            Err(error) => error,
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "minimum protocol refusal discarded queued SQLSTATE 57P01: {error}"
        );
    }

    /// A version negotiation can report protocol options the server did not
    /// recognize without changing the version. This client sends no `_pq_.`
    /// options, so such a response is invalid, but the refusal must parse and
    /// name the option instead of collapsing the variable-length message into
    /// a framing error.
    #[compio::test]
    async fn an_unrequested_protocol_option_is_refused_by_name() {
        const OPTION: &str = "_pq_.future_feature";

        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0002u32.to_be_bytes());
        negotiation.extend_from_slice(&1u32.to_be_bytes());
        negotiation.extend_from_slice(OPTION.as_bytes());
        negotiation.push(0);

        let mut script = frame(b'v', &negotiation);
        script.extend_from_slice(&successful_handshake(std::iter::empty()));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("an unrequested protocol option was accepted"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains(OPTION),
            "the protocol-option refusal did not name {OPTION}: {chain}"
        );
    }

    /// The configured version floor takes precedence over the option list,
    /// matching libpq: the caller needs to know that no protocol in its range
    /// can be spoken, even if the same negotiation also names an option.
    #[compio::test]
    async fn a_below_minimum_negotiation_with_options_names_both_versions() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0000u32.to_be_bytes());
        negotiation.extend_from_slice(&1u32.to_be_bytes());
        negotiation.extend_from_slice(b"_pq_.future_feature\0");

        let mut script = frame(b'v', &negotiation);
        script.extend_from_slice(&successful_handshake(std::iter::empty()));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable min_protocol_version=3.2"
            .parse()
            .expect("parse scripted protocol minimum");

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("protocol 3.0 with options satisfied a 3.2 minimum"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("min_protocol_version")
                && chain.contains("3.0")
                && chain.contains("3.2"),
            "the minimum-version refusal did not name both versions: {chain}"
        );
    }

    /// With no `_pq_.` startup options, the server sends negotiation only to
    /// lower the requested version. An equal version asks the client to make
    /// no change and is a malformed startup response.
    #[compio::test]
    async fn negotiation_without_a_protocol_downgrade_is_rejected() {
        let mut negotiation = Vec::new();
        negotiation.extend_from_slice(&0x0003_0002u32.to_be_bytes());
        negotiation.extend_from_slice(&0u32.to_be_bytes());

        let mut script = frame(b'v', &negotiation);
        script.extend_from_slice(&successful_handshake(std::iter::empty()));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");

        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("a no-op NegotiateProtocolVersion continued startup"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("without lowering"),
            "the invalid negotiation was not identified: {chain}"
        );
    }

    /// A 3.2 `BackendKeyData` key is retained byte-for-byte and determines the
    /// variable length of the later `CancelRequest`.
    #[compio::test]
    async fn a_variable_backend_key_is_echoed_in_the_cancel_request() {
        const PROCESS_ID: i32 = 1234;
        const KEY: [u8; 32] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];

        let mut key_data = Vec::with_capacity(4 + KEY.len());
        key_data.extend_from_slice(&PROCESS_ID.to_be_bytes());
        key_data.extend_from_slice(&KEY);
        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&frame(b'K', &key_data));
        script.extend_from_slice(&frame(b'Z', b"I"));

        let (stream, startup) = scripted_server_after_startup(Some(script)).await;
        let config: Config = "user=scripted-user sslmode=disable"
            .parse()
            .expect("parse scripted config");
        let (client, connection) = config
            .connect_raw(stream, NoTls)
            .await
            .expect("accept a variable BackendKeyData key");
        let startup = startup.await.expect("scripted server did not see startup");
        assert_eq!(startup.get(..4), Some(&0x0003_0002u32.to_be_bytes()[..]));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted cancel peer");
        let address = listener.local_addr().expect("scripted cancel peer address");
        let (packet_tx, packet_rx) = oneshot::channel();
        compio::runtime::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept cancel connection");
            let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
            result.expect("read cancel packet length");
            let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
            let compio::BufResult(result, body) = socket.read_exact(vec![0u8; length - 4]).await;
            result.expect("read cancel packet body");
            packet_tx
                .send((length, body))
                .expect("report scripted cancel packet");
        })
        .detach();

        let mut cancel_stream = TcpStream::connect(address)
            .await
            .expect("connect scripted cancel peer");
        client
            .cancel_token()
            .cancel_query_raw(&mut cancel_stream, NoTls)
            .await
            .expect("send variable-length CancelRequest");
        let (length, body) = packet_rx
            .await
            .expect("scripted cancel peer did not report");

        assert_eq!(length, 12 + KEY.len());
        assert_eq!(&body[..4], &80_877_102i32.to_be_bytes());
        assert_eq!(&body[4..8], &PROCESS_ID.to_be_bytes());
        assert_eq!(&body[8..], &KEY);
        drop((client, connection));
    }

    async fn scripted_password_auth_server(
        auth_request: Vec<u8>,
        complete_after_response: bool,
    ) -> (crate::Socket, oneshot::Receiver<Result<Vec<u8>, String>>) {
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
            let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
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
                    let _ = client_bytes_tx
                        .send(Err(format!("invalid frontend frame length {frame_length}")));
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
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
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

            for (current, compressed) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
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
                encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
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

    async fn scripted_successful_scram_server()
    -> (crate::Socket, oneshot::Receiver<Result<(), String>>) {
        // PBKDF2-HMAC-SHA-256("scripted-password", "salt", 1), followed by
        // HMAC-SHA-256(salted_password, "Server Key"). Keeping the derived
        // verifier here lets the scripted peer compute its nonce-dependent
        // server signature without adding a test-only crypto dependency.
        const SERVER_KEY: [u8; 32] = [
            0xf1, 0x83, 0x92, 0xfc, 0x94, 0x37, 0xe6, 0x33, 0x43, 0x42, 0x26, 0x63, 0x9a, 0xba,
            0x84, 0x91, 0x32, 0xa6, 0x3e, 0x12, 0x47, 0xbf, 0x0f, 0x07, 0x58, 0x71, 0x43, 0xa4,
            0x57, 0x01, 0x2d, 0xc7,
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
                let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
                result.map_err(|e| e.to_string())?;

                let mut auth_sasl = 10i32.to_be_bytes().to_vec();
                auth_sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
                let compio::BufResult(result, _) = socket.write_all(frame(b'R', &auth_sasl)).await;
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
                if response_length < 0 || response_length as usize != initial.len() - response_start
                {
                    return Err("SASL initial response length was invalid".to_string());
                }
                let client_first =
                    std::str::from_utf8(&initial[response_start..]).map_err(|e| e.to_string())?;
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
                let client_final = std::str::from_utf8(&client_final).map_err(|e| e.to_string())?;
                let proof_start = client_final
                    .rfind(",p=")
                    .ok_or_else(|| format!("client-final message had no proof: {client_final}"))?;
                let client_final_without_proof = &client_final[..proof_start];
                let auth_message =
                    format!("{client_first_bare},{server_first},{client_final_without_proof}");
                let server_signature = hmac_sha256(&SERVER_KEY, auth_message.as_bytes());
                let mut auth_final = 12i32.to_be_bytes().to_vec();
                auth_final.extend_from_slice(format!("v={}", base64(&server_signature)).as_bytes());
                let compio::BufResult(result, _) = socket.write_all(frame(b'R', &auth_final)).await;
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
                let compio::BufResult(result, _) = socket.read_exact(vec![0u8; length - 4]).await;
                result.map_err(|e| e.to_string())?;

                let mut auth_sasl = 10i32.to_be_bytes().to_vec();
                auth_sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
                let compio::BufResult(result, _) = socket.write_all(frame(b'R', &auth_sasl)).await;
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
        let (stream, client_bytes) = scripted_password_auth_server(auth_request, false).await;
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
            chain.contains("scram-sha-256") && chain.contains("did not complete authentication"),
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
            chain.contains("scram-sha-256") && chain.contains("did not complete authentication"),
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

    #[compio::test]
    async fn startup_client_encoding_refusal_preserves_a_pending_server_error() {
        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&parameter_status("client_encoding", "LATIN1"));
        script.extend_from_slice(&error_response("57P01", "terminating during startup"));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;

        let error = match plaintext_config().connect_raw(stream, NoTls).await {
            Ok(_) => panic!("a server announcing LATIN1 was accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("57P01"),
            "startup client_encoding refusal discarded queued SQLSTATE 57P01: {error}"
        );
    }

    #[compio::test]
    async fn startup_encoding_refusal_outranks_a_non_ascii_error_response() {
        let mut non_ascii_error = b"SFATAL\0VFATAL\0C57P01\0Mlatin1 bytes ".to_vec();
        non_ascii_error.extend_from_slice(&[0xc3, 0xa9]);
        non_ascii_error.extend_from_slice(b"\0\0");

        let mut script = frame(b'R', &0u32.to_be_bytes());
        script.extend_from_slice(&parameter_status("client_encoding", "LATIN1"));
        script.extend_from_slice(&frame(b'E', &non_ascii_error));
        let (stream, _) = scripted_server_after_startup(Some(script)).await;

        let error = match plaintext_config().connect_raw(stream, NoTls).await {
            Ok(_) => panic!("a server announcing LATIN1 was accepted"),
            Err(error) => error,
        };
        assert!(
            error.code().is_none(),
            "a non-ASCII response from a LATIN1 session replaced the safe local refusal: {error}"
        );
        let chain = authentication_error_chain(error);
        assert!(
            chain.contains("client_encoding"),
            "the non-ASCII server response hid the encoding refusal: {chain}"
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
    async fn replication_handshake_uses_the_configured_user_in_an_md5_response() {
        const EXPECTED: &[u8] = b"md538b05cef655ccdf8f933eb51f8cd2ef6";

        let mut md5_request = 5i32.to_be_bytes().to_vec();
        md5_request.extend_from_slice(&[5, 6, 7, 8]);
        let (stream, client_bytes) = scripted_password_auth_server(md5_request, true).await;

        let mut config = Config::new();
        config
            .user("replication-md5-user")
            .password("replication-md5-password")
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_secs(5));
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(stream);
        let (stream, _, _, _) = handshake_for_replication(stream, &config)
            .await
            .expect("the scripted replication MD5 exchange must complete");

        let client_bytes = compio::time::timeout(Duration::from_secs(5), client_bytes)
            .await
            .expect("scripted replication MD5 server did not finish")
            .expect("scripted replication MD5 server dropped its observation")
            .expect("scripted replication MD5 server failed");
        assert!(
            client_bytes
                .windows(EXPECTED.len())
                .any(|window| window == EXPECTED),
            "the replication handshake used the wrong MD5 user: {}",
            String::from_utf8_lossy(&client_bytes)
        );
        drop(stream);
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
        let notices =
            (0..=EXPECTED_DELAYED_MESSAGE_LIMIT).map(|index| notice(&format!("notice {index}")));
        let (stream, _) = scripted_server_after_startup(Some(successful_handshake(notices))).await;

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

    /// A delayed async frame must not keep the handshake read buffer's whole
    /// allocation alive after that buffer grows. `bytes 1.11.1` implements
    /// `BytesMut::split_to` by sharing the allocation, so parsing a tiny frame
    /// directly from an over-allocated read buffer can pin all of it.
    #[compio::test]
    async fn delayed_message_does_not_pin_handshake_read_allocation() {
        const NOTIFICATION_FRAME_LEN: usize = 13;

        let mut script = frame(b'A', &[0, 0, 0, 1, b'c', 0, b'p', 0]);
        assert_eq!(script.len(), NOTIFICATION_FRAME_LEN);
        script.extend_from_slice(&frame(b'R', &0u32.to_be_bytes()));

        let config = plaintext_config();
        let stream = MaybeTlsStream::<_, crate::tls::NoTlsStream>::Raw(HandshakeWriteSuccess {
            input: script,
            offset: 0,
            output: vec![],
        });
        let mut handshake = Handshake::new(stream, &config);
        handshake.stream.buf().reserve(1024 * 1024);
        let allocation_capacity = handshake.stream.buf().capacity();
        let allocation_base = handshake.stream.buf().as_ptr();

        assert!(matches!(
            handshake.next().await.expect("read scripted handshake"),
            Message::AuthenticationOk
        ));
        assert_eq!(handshake.delayed.len(), 1);
        assert_eq!(handshake.delayed_bytes, NOTIFICATION_FRAME_LEN);

        // The exhausted normal batch is another shared view of the read
        // allocation. Remove it so only the delayed message can prevent the
        // stream from reclaiming its consumed prefix.
        handshake.pending = BackendMessages::empty();
        assert_eq!(handshake.stream.buf().len(), 0);
        handshake.stream.buf().reserve(allocation_capacity);

        assert_eq!(
            handshake.stream.buf().as_ptr(),
            allocation_base,
            "a {NOTIFICATION_FRAME_LEN}-byte delayed message pinned the handshake's \
             {allocation_capacity}-byte read allocation"
        );
    }

    #[compio::test]
    async fn handshake_replays_a_few_delayed_notices() {
        const NOTICE_TEXTS: [&str; 3] = ["first warning", "second warning", "third warning"];
        let notices = NOTICE_TEXTS.into_iter().map(notice);
        let (stream, _) = scripted_server_after_startup(Some(successful_handshake(notices))).await;

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

    #[compio::test]
    async fn scram_plus_advertised_over_plaintext_is_refused() {
        let mut config = scram_config();
        config.channel_binding(crate::config::ChannelBinding::Disable);
        let (error, output) = sasl_attempt(&config, Encryption::Plaintext, SCRAM_BOTH).await;
        assert!(
            output.is_empty() && error.contains("non-TLS"),
            "{error}: {output:?}"
        );
    }

    #[compio::test]
    async fn scram_plus_without_tls_endpoint_does_not_fallback() {
        let config = scram_config();
        let (error, output) = sasl_attempt(&config, Encryption::Tls, SCRAM_BOTH).await;
        assert!(
            output.is_empty() && error.contains("tls-server-end-point"),
            "{error}: {output:?}"
        );
    }

    #[compio::test]
    async fn tls_without_endpoint_uses_y_for_bare_scram() {
        let config = scram_config();
        let (_, output) = sasl_attempt(&config, Encryption::Tls, SCRAM).await;
        assert!(
            output.contains("SCRAM-SHA-256") && output.contains("y,,n="),
            "TLS without endpoint material suppressed the y downgrade sentinel: {output:?}"
        );
    }

    #[compio::test]
    async fn scram_plus_only_never_selects_unadvertised_bare_mechanism() {
        let mut config = scram_config();
        config.channel_binding(crate::config::ChannelBinding::Disable);
        let (error, output) = sasl_attempt(&config, Encryption::Tls, SCRAM_PLUS).await;
        assert!(
            output.is_empty() && error.contains("unsupported SASL mechanism"),
            "{error}: {output:?}"
        );

        let (_, output) = sasl_attempt(&config, Encryption::Tls, SCRAM_BOTH).await;
        assert!(
            output.contains("SCRAM-SHA-256") && output.contains("n,,n="),
            "disable did not select the advertised bare mechanism with n: {output:?}"
        );
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

        // `expect_err`, not `if let Err`: the stub never continues the SCRAM
        // exchange, so failing is this test's PREMISE. Written as a conditional,
        // a future change that let this connect would skip the assertion and
        // the test would keep passing while checking nothing.
        // A match, not `expect_err`: `Connection` is not `Debug`, which is very
        // likely why this began life as `if let Err`.
        let error = match config.connect_raw(stream, NoTls).await {
            Ok(_) => panic!("the stub never completes SCRAM, so the connection must fail"),
            Err(error) => error,
        };
        let chain = authentication_error_chain(error);
        assert!(
            !chain.contains("SCRAM-SHA-256-PLUS"),
            "prefer must not raise the channel-binding downgrade refusal: {chain}"
        );
    }
}
