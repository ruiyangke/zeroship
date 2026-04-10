//! PostgreSQL connection — startup, auth, query, execute.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::BytesMut;
use compio::net::TcpStream;
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::sasl::{self, ChannelBinding, ScramSha256};
use postgres_protocol::message::{backend, frontend};
use postgres_protocol::IsNull as ProtoIsNull;
use postgres_types::Type;
use url::Url;

use crate::stream::BufStream;
use crate::{Column, Error, Result, Row, ToSql};

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
    /// Whether this connection has a dropped transaction that needs rollback.
    pub needs_rollback: bool,
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

    /// Execute a query using the Extended Query Protocol, returning rows.
    ///
    /// Pipelines Parse → Bind → Describe(Portal) → Execute → Sync in one flush.
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>> {
        let mut buf = BytesMut::new();

        // Parse (unnamed statement)
        frontend::parse("", sql, std::iter::empty::<u32>(), &mut buf)
            .map_err(|e| Error::Protocol(e.to_string()))?;

        // Encode params as text-format bytes
        let param_values = encode_params(params)?;

        // Bind (unnamed portal ← unnamed statement, text params, binary results)
        let param_refs: Vec<Option<&[u8]>> = param_values
            .iter()
            .map(|v| v.as_ref().map(|b| b.as_ref()))
            .collect();
        frontend::bind(
            "",
            "",
            std::iter::once(1i16),    // param format codes: 1 = binary for all
            param_refs,
            |val: Option<&[u8]>, buf: &mut BytesMut| match val {
                Some(bytes) => {
                    buf.extend_from_slice(bytes);
                    Ok(ProtoIsNull::No)
                }
                None => Ok(ProtoIsNull::Yes),
            },
            std::iter::once(1i16),    // result format codes: 1 = binary for all columns
            &mut buf,
        )
        .map_err(bind_error)?;

        // Describe portal
        frontend::describe(b'P', "", &mut buf)
            .map_err(|e| Error::Protocol(e.to_string()))?;

        // Execute (fetch all)
        frontend::execute("", 0, &mut buf)
            .map_err(|e| Error::Protocol(e.to_string()))?;

        // Sync
        frontend::sync(&mut buf);

        self.stream.write_bytes(&buf);
        self.stream.flush().await?;

        // Read responses: ParseComplete → BindComplete → RowDescription → DataRow* → CommandComplete → ReadyForQuery
        // On ErrorResponse, drain until ReadyForQuery.
        // NoticeResponse can appear anywhere and is silently skipped.

        // ParseComplete
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::ParseComplete => break,
                backend::Message::NoticeResponse(_) => continue,
                backend::Message::ErrorResponse(body) => {
                    let err = parse_error_response(body);
                    drain_until_ready(&mut self.stream, &mut self.status).await?;
                    return Err(err);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected ParseComplete, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        // BindComplete
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::BindComplete => break,
                backend::Message::NoticeResponse(_) => continue,
                backend::Message::ErrorResponse(body) => {
                    let err = parse_error_response(body);
                    drain_until_ready(&mut self.stream, &mut self.status).await?;
                    return Err(err);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected BindComplete, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        // RowDescription (or NoData for non-SELECT)
        let columns: Arc<Vec<Column>> = loop {
            match read_message(&mut self.stream).await? {
                backend::Message::RowDescription(body) => {
                    break Arc::new(parse_row_description(body)?);
                }
                backend::Message::NoData => break Arc::new(Vec::new()),
                backend::Message::NoticeResponse(_) => continue,
                backend::Message::ErrorResponse(body) => {
                    let err = parse_error_response(body);
                    drain_until_ready(&mut self.stream, &mut self.status).await?;
                    return Err(err);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected RowDescription or NoData, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        };

        // DataRow* → CommandComplete
        let mut rows = Vec::new();
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::DataRow(body) => {
                    rows.push(parse_data_row(body, &columns)?);
                }
                backend::Message::CommandComplete(_) => break,
                backend::Message::EmptyQueryResponse => break,
                backend::Message::NoticeResponse(_) => continue,
                backend::Message::ErrorResponse(body) => {
                    let err = parse_error_response(body);
                    drain_until_ready(&mut self.stream, &mut self.status).await?;
                    return Err(err);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected DataRow or CommandComplete, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        // ReadyForQuery
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::ReadyForQuery(body) => {
                    self.status = body.status();
                    break;
                }
                backend::Message::NoticeResponse(_) => continue,
                other => {
                    return Err(Error::Protocol(format!(
                        "expected ReadyForQuery, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        Ok(rows)
    }

    /// Execute a statement, returning the number of affected rows.
    ///
    /// Like `query()` but skips Describe and does not collect rows.
    pub async fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64> {
        let mut buf = BytesMut::new();

        // Parse
        frontend::parse("", sql, std::iter::empty::<u32>(), &mut buf)
            .map_err(|e| Error::Protocol(e.to_string()))?;

        // Encode params
        let param_values = encode_params(params)?;
        let param_refs: Vec<Option<&[u8]>> = param_values
            .iter()
            .map(|v| v.as_ref().map(|b| b.as_ref()))
            .collect();

        // Bind (binary params, no result format needed for execute)
        frontend::bind(
            "",
            "",
            std::iter::once(1i16),    // param format codes: 1 = binary for all
            param_refs,
            |val: Option<&[u8]>, buf: &mut BytesMut| match val {
                Some(bytes) => {
                    buf.extend_from_slice(bytes);
                    Ok(ProtoIsNull::No)
                }
                None => Ok(ProtoIsNull::Yes),
            },
            std::iter::empty(),
            &mut buf,
        )
        .map_err(bind_error)?;

        // Execute
        frontend::execute("", 0, &mut buf)
            .map_err(|e| Error::Protocol(e.to_string()))?;

        // Sync
        frontend::sync(&mut buf);

        self.stream.write_bytes(&buf);
        self.stream.flush().await?;

        // ParseComplete
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::ParseComplete => break,
                backend::Message::NoticeResponse(_) => continue,
                backend::Message::ErrorResponse(body) => {
                    let err = parse_error_response(body);
                    drain_until_ready(&mut self.stream, &mut self.status).await?;
                    return Err(err);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected ParseComplete, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        // BindComplete
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::BindComplete => break,
                backend::Message::NoticeResponse(_) => continue,
                backend::Message::ErrorResponse(body) => {
                    let err = parse_error_response(body);
                    drain_until_ready(&mut self.stream, &mut self.status).await?;
                    return Err(err);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected BindComplete, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        // Skip DataRows (shouldn't be any for execute, but drain them)
        let mut affected = 0u64;
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::DataRow(_) => {
                    // discard
                }
                backend::Message::CommandComplete(body) => {
                    let tag = body
                        .tag()
                        .map_err(|e| Error::Protocol(e.to_string()))?;
                    affected = parse_command_tag(tag);
                    break;
                }
                backend::Message::EmptyQueryResponse => break,
                backend::Message::NoticeResponse(_) => continue,
                backend::Message::ErrorResponse(body) => {
                    let err = parse_error_response(body);
                    drain_until_ready(&mut self.stream, &mut self.status).await?;
                    return Err(err);
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected CommandComplete, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        // ReadyForQuery
        loop {
            match read_message(&mut self.stream).await? {
                backend::Message::ReadyForQuery(body) => {
                    self.status = body.status();
                    break;
                }
                backend::Message::NoticeResponse(_) => continue,
                other => {
                    return Err(Error::Protocol(format!(
                        "expected ReadyForQuery, got {}",
                        msg_tag(&other)
                    )));
                }
            }
        }

        Ok(affected)
    }

    /// Begin a transaction. Returns a `Transaction` that must be committed or
    /// rolled back. If dropped without commit, it sets `needs_rollback` on the
    /// connection (the pool will issue ROLLBACK before reuse).
    pub async fn begin(&mut self) -> Result<Transaction<'_>> {
        self.execute("BEGIN", &[]).await?;
        Ok(Transaction { conn: self })
    }
}

// ---------------------------------------------------------------------------
// Transaction
// ---------------------------------------------------------------------------

/// An in-progress transaction. Borrows the connection exclusively.
///
/// If dropped without calling `commit()` or `rollback()`, the connection is
/// marked as needing rollback (the pool handles cleanup).
pub struct Transaction<'a> {
    conn: &'a mut Conn,
}

impl<'a> Transaction<'a> {
    /// Execute a query within this transaction, returning rows.
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>> {
        self.conn.query(sql, params).await
    }

    /// Execute a statement within this transaction, returning affected row count.
    pub async fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64> {
        self.conn.execute(sql, params).await
    }

    /// Commit the transaction.
    pub async fn commit(self) -> Result<()> {
        self.conn.execute("COMMIT", &[]).await?;
        // Consume self without Drop firing
        std::mem::forget(self);
        Ok(())
    }

    /// Roll back the transaction.
    pub async fn rollback(self) -> Result<()> {
        self.conn.execute("ROLLBACK", &[]).await?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        self.conn.needs_rollback = true;
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

/// Convert a `BindError` (which has no Display/Debug) into our Error type.
fn bind_error(e: frontend::BindError) -> Error {
    match e {
        frontend::BindError::Conversion(e) => Error::Protocol(format!("bind conversion: {e}")),
        frontend::BindError::Serialization(e) => Error::Protocol(format!("bind serialization: {e}")),
    }
}

/// Encode query parameters as binary-format byte buffers.
///
/// Tries common PostgreSQL types via `to_sql_checked()` in order of popularity.
/// The first type that succeeds is used. This approach works because
/// `to_sql_checked()` does runtime type checking and returns an error if the
/// Rust type doesn't match the requested PostgreSQL type.
fn encode_params(params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Option<Vec<u8>>>> {
    static TRY_TYPES: &[Type] = &[
        Type::INT4,
        Type::INT8,
        Type::FLOAT4,
        Type::FLOAT8,
        Type::BOOL,
        Type::UUID,
        Type::TEXT,
        Type::VARCHAR,
        Type::BYTEA,
        Type::OID,
        Type::INT2,
    ];

    let mut out = Vec::with_capacity(params.len());
    for param in params {
        let mut encoded = None;
        for pg_type in TRY_TYPES {
            let mut buf = BytesMut::new();
            match param.to_sql_checked(pg_type, &mut buf) {
                Ok(postgres_types::IsNull::Yes) => {
                    encoded = Some(None);
                    break;
                }
                Ok(postgres_types::IsNull::No) => {
                    encoded = Some(Some(buf.to_vec()));
                    break;
                }
                Err(_) => continue,
            }
        }
        match encoded {
            Some(v) => out.push(v),
            None => {
                return Err(Error::Protocol(
                    "param encode: no supported type found".to_string(),
                ));
            }
        }
    }
    Ok(out)
}

/// Parse a RowDescription body into a Vec<Column>.
fn parse_row_description(body: backend::RowDescriptionBody) -> Result<Vec<Column>> {
    let mut cols = Vec::new();
    let mut fields = body.fields();
    while let Some(field) = fields.next().map_err(|e| Error::Protocol(e.to_string()))? {
        cols.push(Column {
            name: field.name().to_string(),
            oid: field.type_oid(),
        });
    }
    Ok(cols)
}

/// Parse a DataRow body into a Row, using the shared column metadata.
fn parse_data_row(body: backend::DataRowBody, columns: &Arc<Vec<Column>>) -> Result<Row> {
    let buf = body.buffer();
    let mut values = Vec::with_capacity(columns.len());
    let mut ranges = body.ranges();
    while let Some(range) = ranges.next().map_err(|e| Error::Protocol(e.to_string()))? {
        match range {
            Some(r) => values.push(Some(buf[r].to_vec())),
            None => values.push(None),
        }
    }
    Ok(Row {
        columns: Arc::clone(columns),
        values,
    })
}

/// Parse the affected-row count from a CommandComplete tag.
///
/// Tags: "INSERT 0 5", "DELETE 3", "UPDATE 2", "SELECT 5", "CREATE TABLE", etc.
fn parse_command_tag(tag: &str) -> u64 {
    tag.rsplit(' ')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Drain messages until ReadyForQuery, updating the connection status.
/// Used for error recovery after an ErrorResponse in the middle of a pipeline.
async fn drain_until_ready(stream: &mut BufStream, status: &mut u8) -> Result<()> {
    loop {
        let msg = read_message(stream).await?;
        if let backend::Message::ReadyForQuery(body) = msg {
            *status = body.status();
            return Ok(());
        }
        // Discard everything else (DataRow, CommandComplete, etc.)
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
