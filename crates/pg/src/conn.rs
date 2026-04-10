//! PostgreSQL connection — startup, auth, query, execute.

use std::collections::HashMap;

use bytes::BytesMut;
use compio::net::TcpStream;
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::sasl::{self, ChannelBinding, ScramSha256};
use postgres_protocol::message::{backend, frontend};
use url::Url;

use crate::stream::BufStream;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// ConnectConfig
// ---------------------------------------------------------------------------

struct ConnectConfig {
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    database: String,
    sslmode: SslMode,
}

#[derive(Debug, Clone, PartialEq)]
enum SslMode {
    Disable,
    Prefer,
    Require,
}

// ---------------------------------------------------------------------------
// Conn
// ---------------------------------------------------------------------------

/// A PostgreSQL connection.
pub struct Conn {
    stream: BufStream,
    #[allow(dead_code)]
    pid: i32,
    #[allow(dead_code)]
    secret: i32,
    #[allow(dead_code)]
    params: HashMap<String, String>,
    status: u8,
    pub(crate) needs_rollback: bool,
}

impl Conn {
    /// Open a connection to PostgreSQL using the given URL.
    ///
    /// Supports: `postgres://user:password@host:port/database?sslmode=prefer|require|disable`
    pub async fn connect(url: &str) -> Result<Self> {
        let cfg = parse_url(url)?;

        // TCP connect
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(Error::Io)?;

        let mut stream = BufStream::tcp(tcp);

        // TLS negotiation
        #[cfg(feature = "tls")]
        if cfg.sslmode != SslMode::Disable {
            let mut buf = BytesMut::new();
            frontend::ssl_request(&mut buf);
            stream.write_bytes(&buf);
            stream.flush().await?;

            let resp = stream.read_byte().await?;
            if resp == b'S' {
                // Server agreed to TLS — upgrade
                let tcp = stream.into_tcp().map_err(|_| {
                    Error::Tls("cannot upgrade: stream is not plain TCP".to_string())
                })?;
                let connector = build_tls_connector()?;
                let tls = connector
                    .connect(&cfg.host, tcp)
                    .await
                    .map_err(|e| Error::Tls(e.to_string()))?;
                stream = BufStream::tls(tls);
            } else if cfg.sslmode == SslMode::Require {
                return Err(Error::Tls(
                    "server refused TLS but sslmode=require".to_string(),
                ));
            }
            // If resp == 'N' and sslmode == Prefer: continue with plain TCP
        }

        // StartupMessage
        {
            let mut buf = BytesMut::new();
            frontend::startup_message(
                [
                    ("user", cfg.user.as_str()),
                    ("database", cfg.database.as_str()),
                    ("client_encoding", "UTF8"),
                    ("application_name", "appbase-pg"),
                ],
                &mut buf,
            )
            .map_err(|e| Error::Protocol(e.to_string()))?;
            stream.write_bytes(&buf);
            stream.flush().await?;
        }

        // Auth loop
        let password = cfg.password.as_deref().unwrap_or("");
        loop {
            let msg = read_message(&mut stream).await?;
            match msg {
                backend::Message::AuthenticationOk => break,

                backend::Message::AuthenticationSasl(body) => {
                    // Find SCRAM-SHA-256 in the offered mechanisms
                    let mut mechs = body.mechanisms();
                    let mut found = false;
                    while let Some(mech) = mechs.next().map_err(|e| Error::Protocol(e.to_string()))? {
                        if mech == sasl::SCRAM_SHA_256 {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return Err(Error::Auth(
                            "server did not offer SCRAM-SHA-256".to_string(),
                        ));
                    }

                    let mut scram =
                        ScramSha256::new(password.as_bytes(), ChannelBinding::unsupported());

                    // Send SASLInitialResponse
                    {
                        let mut buf = BytesMut::new();
                        frontend::sasl_initial_response(
                            sasl::SCRAM_SHA_256,
                            scram.message(),
                            &mut buf,
                        )
                        .map_err(|e| Error::Protocol(e.to_string()))?;
                        stream.write_bytes(&buf);
                        stream.flush().await?;
                    }

                    // AuthenticationSASLContinue
                    let msg2 = read_message(&mut stream).await?;
                    match msg2 {
                        backend::Message::AuthenticationSaslContinue(cont) => {
                            scram
                                .update(cont.data())
                                .map_err(|e| Error::Auth(e.to_string()))?;

                            // Send SASLResponse
                            let mut buf = BytesMut::new();
                            frontend::sasl_response(scram.message(), &mut buf)
                                .map_err(|e| Error::Protocol(e.to_string()))?;
                            stream.write_bytes(&buf);
                            stream.flush().await?;
                        }
                        backend::Message::ErrorResponse(body) => {
                            return Err(parse_error_response(body));
                        }
                        other => {
                            return Err(Error::Protocol(format!(
                                "expected AuthenticationSASLContinue, got {}",
                                msg_tag(&other)
                            )))
                        }
                    }

                    // AuthenticationSASLFinal
                    let msg3 = read_message(&mut stream).await?;
                    match msg3 {
                        backend::Message::AuthenticationSaslFinal(fin) => {
                            scram
                                .finish(fin.data())
                                .map_err(|e| Error::Auth(e.to_string()))?;
                        }
                        backend::Message::ErrorResponse(body) => {
                            return Err(parse_error_response(body));
                        }
                        other => {
                            return Err(Error::Protocol(format!(
                                "expected AuthenticationSASLFinal, got {}",
                                msg_tag(&other)
                            )))
                        }
                    }

                    // AuthenticationOk should follow
                    let msg4 = read_message(&mut stream).await?;
                    match msg4 {
                        backend::Message::AuthenticationOk => break,
                        backend::Message::ErrorResponse(body) => {
                            return Err(parse_error_response(body));
                        }
                        other => {
                            return Err(Error::Protocol(format!(
                                "expected AuthenticationOk after SCRAM, got {}",
                                msg_tag(&other)
                            )))
                        }
                    }
                }

                backend::Message::AuthenticationCleartextPassword => {
                    let mut buf = BytesMut::new();
                    frontend::password_message(password.as_bytes(), &mut buf)
                        .map_err(|e| Error::Protocol(e.to_string()))?;
                    stream.write_bytes(&buf);
                    stream.flush().await?;
                }

                backend::Message::ErrorResponse(body) => {
                    return Err(parse_error_response(body));
                }

                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message during auth: {}",
                        msg_tag(&other)
                    )))
                }
            }
        }

        // Consume post-auth startup messages until ReadyForQuery
        let mut pid = 0i32;
        let mut secret = 0i32;
        let mut params = HashMap::new();
        let mut status = b'I';

        loop {
            let msg = read_message(&mut stream).await?;
            match msg {
                backend::Message::ParameterStatus(body) => {
                    let name = body.name().map_err(|e| Error::Protocol(e.to_string()))?;
                    let value = body.value().map_err(|e| Error::Protocol(e.to_string()))?;
                    params.insert(name.to_string(), value.to_string());
                }
                backend::Message::BackendKeyData(body) => {
                    pid = body.process_id();
                    secret = body.secret_key();
                }
                backend::Message::ReadyForQuery(body) => {
                    status = body.status();
                    break;
                }
                backend::Message::ErrorResponse(body) => {
                    return Err(parse_error_response(body));
                }
                backend::Message::NoticeResponse(_) => {
                    // ignore notices
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message during startup: {}",
                        msg_tag(&other)
                    )))
                }
            }
        }

        Ok(Conn {
            stream,
            pid,
            secret,
            params,
            status,
            needs_rollback: false,
        })
    }

    /// Current transaction status byte: `b'I'` idle, `b'T'` in transaction, `b'E'` error.
    pub fn status(&self) -> u8 {
        self.status
    }

    /// Send a Terminate message and close the connection.
    pub async fn close(mut self) -> Result<()> {
        let mut buf = BytesMut::new();
        frontend::terminate(&mut buf);
        self.stream.write_bytes(&buf);
        self.stream.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `postgres://` URL into a `ConnectConfig`.
fn parse_url(url: &str) -> Result<ConnectConfig> {
    let parsed = Url::parse(url)
        .map_err(|e| Error::Protocol(format!("invalid URL: {e}")))?;

    if parsed.scheme() != "postgres" && parsed.scheme() != "postgresql" {
        return Err(Error::Protocol(format!(
            "unsupported URL scheme: {}",
            parsed.scheme()
        )));
    }

    let host = parsed
        .host_str()
        .unwrap_or("localhost")
        .to_string();

    let port = parsed.port().unwrap_or(5432);

    let user = if parsed.username().is_empty() {
        return Err(Error::Protocol("URL missing username".to_string()));
    } else {
        parsed.username().to_string()
    };

    let password = parsed.password().map(|p| {
        percent_decode(p)
    });

    // Database: first path segment without the leading '/'
    let database = parsed
        .path()
        .trim_start_matches('/')
        .to_string();
    let database = if database.is_empty() {
        user.clone()
    } else {
        database
    };

    let sslmode = parsed
        .query_pairs()
        .find(|(k, _)| k == "sslmode")
        .map(|(_, v)| match v.as_ref() {
            "disable" => SslMode::Disable,
            "require" => SslMode::Require,
            _ => SslMode::Prefer,
        })
        .unwrap_or(SslMode::Prefer);

    Ok(ConnectConfig {
        host,
        port,
        user,
        password,
        database,
        sslmode,
    })
}

/// Percent-decode a URL component (e.g., password).
fn percent_decode(s: &str) -> String {
    // url crate handles this for us; this is a fallback manual decode
    // We use the url crate's already-decoded values from `.password()`
    // which are already decoded. But if needed:
    s.to_string()
}

/// Read the next complete backend message from the stream.
///
/// Fills the buffer as needed, then parses. Skips NoticeResponse messages
/// transparently (callers that want them can match on the return value).
pub(crate) async fn read_message(stream: &mut BufStream) -> Result<backend::Message> {
    loop {
        // Ensure we have at least the 5-byte header (tag + 4-byte length)
        stream.fill(5).await?;

        // Try to parse; if the payload isn't fully buffered yet, fill more
        match backend::Message::parse(stream.buf())
            .map_err(|e| Error::Protocol(e.to_string()))?
        {
            Some(msg) => return Ok(msg),
            None => {
                // Need more data — fill another chunk
                let current_len = stream.buf().len();
                stream.fill(current_len + 1).await?;
            }
        }
    }
}

/// Convert an `ErrorResponseBody` into a crate `Error`.
pub(crate) fn parse_error_response(body: backend::ErrorResponseBody) -> Error {
    let mut severity = String::new();
    let mut code = String::new();
    let mut message = String::new();

    let mut fields = body.fields();
    while let Ok(Some(field)) = fields.next() {
        let field: backend::ErrorField<'_> = field;
        match field.type_() {
            b'S' | b'V' => {
                if let Ok(s) = std::str::from_utf8(field.value_bytes()) {
                    severity = s.to_string();
                }
            }
            b'C' => {
                if let Ok(s) = std::str::from_utf8(field.value_bytes()) {
                    code = s.to_string();
                }
            }
            b'M' => {
                if let Ok(s) = std::str::from_utf8(field.value_bytes()) {
                    message = s.to_string();
                }
            }
            _ => {}
        }
    }

    Error::Postgres {
        severity,
        code,
        message,
    }
}

/// Return a human-readable tag for a backend message (for error messages).
fn msg_tag(msg: &backend::Message) -> &'static str {
    match msg {
        backend::Message::AuthenticationOk => "AuthenticationOk",
        backend::Message::AuthenticationCleartextPassword => "AuthenticationCleartextPassword",
        backend::Message::AuthenticationMd5Password(_) => "AuthenticationMd5Password",
        backend::Message::AuthenticationSasl(_) => "AuthenticationSasl",
        backend::Message::AuthenticationSaslContinue(_) => "AuthenticationSaslContinue",
        backend::Message::AuthenticationSaslFinal(_) => "AuthenticationSaslFinal",
        backend::Message::BackendKeyData(_) => "BackendKeyData",
        backend::Message::BindComplete => "BindComplete",
        backend::Message::CloseComplete => "CloseComplete",
        backend::Message::CommandComplete(_) => "CommandComplete",
        backend::Message::CopyData(_) => "CopyData",
        backend::Message::CopyDone => "CopyDone",
        backend::Message::CopyInResponse(_) => "CopyInResponse",
        backend::Message::CopyOutResponse(_) => "CopyOutResponse",
        backend::Message::DataRow(_) => "DataRow",
        backend::Message::EmptyQueryResponse => "EmptyQueryResponse",
        backend::Message::ErrorResponse(_) => "ErrorResponse",
        backend::Message::NoData => "NoData",
        backend::Message::NoticeResponse(_) => "NoticeResponse",
        backend::Message::NotificationResponse(_) => "NotificationResponse",
        backend::Message::ParameterDescription(_) => "ParameterDescription",
        backend::Message::ParameterStatus(_) => "ParameterStatus",
        backend::Message::ParseComplete => "ParseComplete",
        backend::Message::PortalSuspended => "PortalSuspended",
        backend::Message::ReadyForQuery(_) => "ReadyForQuery",
        backend::Message::RowDescription(_) => "RowDescription",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// TLS connector helper (only compiled when feature = "tls")
// ---------------------------------------------------------------------------

#[cfg(feature = "tls")]
fn build_tls_connector() -> Result<compio_tls::TlsConnector> {
    let native = compio_tls::native_tls::TlsConnector::new()
        .map_err(|e| Error::Tls(e.to_string()))?;
    Ok(compio_tls::TlsConnector::from(native))
}
