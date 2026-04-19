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
use crate::config::{self, Config};
use crate::connect_tls::connect_tls;
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
use postgres_protocol::message::backend::{AuthenticationSaslBody, Message};
use postgres_protocol::message::frontend;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};

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
    ///   iteration. This matches tokio-postgres's behavior.
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
                BackendMessage::Async(msg) => match msg {
                    Message::NoticeResponse(_) | Message::NotificationResponse(_) => {
                        // Preserve ordering — the connection task
                        // will replay these in front of its first
                        // real read.
                        self.delayed.push_back(msg);
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
}

/// Negotiate TLS if configured, drive the startup + auth exchange,
/// capture `ParameterStatus` + `BackendKeyData` up to `ReadyForQuery`,
/// and return a wired-up `(Client, Connection)` pair.
pub async fn connect_raw<S, T>(
    stream: S,
    tls: T,
    has_hostname: bool,
    config: &Config,
) -> Result<(Client, Connection<S, T::Stream>), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    let stream = connect_tls(
        stream,
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
    };

    let user = match config.get_user() {
        Some(user) => Cow::Borrowed(user),
        None => Cow::Owned(whoami::username().map_err(|err| Error::io(err.into()))?),
    };

    startup(&mut handshake, config, &user).await?;
    authenticate(&mut handshake, config, &user).await?;
    let (process_id, secret_key, parameters) = read_info(&mut handshake).await?;

    let (sender, receiver) = mpsc::unbounded();
    let client = Client::new(
        sender,
        config.get_ssl_mode(),
        config.get_ssl_negotiation(),
        process_id,
        secret_key,
    );
    let connection = Connection::new(handshake.stream, handshake.delayed, parameters, receiver);

    Ok((client, connection))
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
            Some(Message::NoticeResponse(body)) => {
                // Preserve ordering by pushing back into the handshake
                // delayed queue; the connection task will replay these
                // on startup.
                handshake
                    .delayed
                    .push_back(Message::NoticeResponse(body));
            }
            Some(Message::ReadyForQuery(_)) => return Ok((process_id, secret_key, parameters)),
            Some(Message::ErrorResponse(body)) => return Err(Error::db(body)),
            Some(_) => return Err(Error::unexpected_message()),
            None => return Err(Error::closed()),
        }
    }
}
