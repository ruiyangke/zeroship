// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! Errors.

use fallible_iterator::FallibleIterator;
use postgres_protocol::message::backend::{ErrorFields, ErrorResponseBody};
use std::error::{self, Error as _Error};
use std::fmt;
use std::io;
use std::time::Duration;

pub use self::sqlstate::*;

#[allow(clippy::unreadable_literal)]
mod sqlstate;

/// The severity of a Postgres error or notice.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Severity {
    /// PANIC
    Panic,
    /// FATAL
    Fatal,
    /// ERROR
    Error,
    /// WARNING
    Warning,
    /// NOTICE
    Notice,
    /// DEBUG
    Debug,
    /// INFO
    Info,
    /// LOG
    Log,
}

impl fmt::Display for Severity {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            Severity::Panic => "PANIC",
            Severity::Fatal => "FATAL",
            Severity::Error => "ERROR",
            Severity::Warning => "WARNING",
            Severity::Notice => "NOTICE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Log => "LOG",
        };
        fmt.write_str(s)
    }
}

impl Severity {
    fn from_str(s: &str) -> Option<Severity> {
        match s {
            "PANIC" => Some(Severity::Panic),
            "FATAL" => Some(Severity::Fatal),
            "ERROR" => Some(Severity::Error),
            "WARNING" => Some(Severity::Warning),
            "NOTICE" => Some(Severity::Notice),
            "DEBUG" => Some(Severity::Debug),
            "INFO" => Some(Severity::Info),
            "LOG" => Some(Severity::Log),
            _ => None,
        }
    }
}

/// A Postgres error or notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    severity: String,
    parsed_severity: Option<Severity>,
    code: SqlState,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    position: Option<ErrorPosition>,
    where_: Option<String>,
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    datatype: Option<String>,
    constraint: Option<String>,
    file: Option<String>,
    line: Option<u32>,
    routine: Option<String>,
}

impl DbError {
    pub(crate) fn parse(fields: &mut ErrorFields<'_>) -> io::Result<DbError> {
        let mut severity = None;
        let mut parsed_severity = None;
        let mut code = None;
        let mut message = None;
        let mut detail = None;
        let mut hint = None;
        let mut normal_position = None;
        let mut internal_position = None;
        let mut internal_query = None;
        let mut where_ = None;
        let mut schema = None;
        let mut table = None;
        let mut column = None;
        let mut datatype = None;
        let mut constraint = None;
        let mut file = None;
        let mut line = None;
        let mut routine = None;

        while let Some(field) = fields.next()? {
            let value = String::from_utf8_lossy(field.value_bytes());
            match field.type_() {
                b'S' => severity = Some(value.into_owned()),
                b'C' => code = Some(SqlState::from_code(&value)),
                b'M' => message = Some(value.into_owned()),
                b'D' => detail = Some(value.into_owned()),
                b'H' => hint = Some(value.into_owned()),
                // An unreadable position leaves this `None` rather than failing
                // the whole message, for the same reason as `V` below: these
                // are OPTIONAL informational fields whose absence is already a
                // supported state, so a value this parser cannot read is
                // "unknown", not "the message is malformed". Refusing here
                // discarded a well-formed error's SQLSTATE and text and
                // reported a parse failure in their place.
                b'P' => normal_position = value.parse::<u32>().ok(),
                // See `P` above.
                b'p' => internal_position = value.parse::<u32>().ok(),
                b'q' => internal_query = Some(value.into_owned()),
                b'W' => where_ = Some(value.into_owned()),
                b's' => schema = Some(value.into_owned()),
                b't' => table = Some(value.into_owned()),
                b'c' => column = Some(value.into_owned()),
                b'd' => datatype = Some(value.into_owned()),
                b'n' => constraint = Some(value.into_owned()),
                b'F' => file = Some(value.into_owned()),
                // See `P` above.
                b'L' => line = value.parse::<u32>().ok(),
                b'R' => routine = Some(value.into_owned()),
                // An unrecognised level leaves this `None` rather than failing
                // the whole message. `V` is a non-localized copy of `S` and its
                // absence is already a supported state - it is `None` for every
                // pre-9.6 server - so a level this table does not know is
                // "unknown", not "malformed". Refusing here instead discarded a
                // well-formed error's SQLSTATE and text and reported a parse
                // failure in their place, while `S` carries the raw string
                // either way.
                b'V' => parsed_severity = Severity::from_str(&value),
                _ => {}
            }
        }

        Ok(DbError {
            severity: severity
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "`S` field missing"))?,
            parsed_severity,
            code: code
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "`C` field missing"))?,
            message: message
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "`M` field missing"))?,
            detail,
            hint,
            position: match normal_position {
                Some(position) => Some(ErrorPosition::Original(position)),
                None => match internal_position {
                    Some(position) => Some(ErrorPosition::Internal {
                        position,
                        query: internal_query.ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "`q` field missing but `p` field present",
                            )
                        })?,
                    }),
                    None => None,
                },
            },
            where_,
            schema,
            table,
            column,
            datatype,
            constraint,
            file,
            line,
            routine,
        })
    }

    /// The field contents are ERROR, FATAL, or PANIC (in an error message),
    /// or WARNING, NOTICE, DEBUG, INFO, or LOG (in a notice message), or a
    /// localized translation of one of these.
    pub fn severity(&self) -> &str {
        &self.severity
    }

    /// A parsed, nonlocalized version of `severity`. (PostgreSQL 9.6+)
    pub fn parsed_severity(&self) -> Option<Severity> {
        self.parsed_severity
    }

    /// The SQLSTATE code for the error.
    pub fn code(&self) -> &SqlState {
        &self.code
    }

    /// The primary human-readable error message.
    ///
    /// This should be accurate but terse (typically one line).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// An optional secondary error message carrying more detail about the
    /// problem.
    ///
    /// Might run to multiple lines.
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// An optional suggestion what to do about the problem.
    ///
    /// This is intended to differ from `detail` in that it offers advice
    /// (potentially inappropriate) rather than hard facts. Might run to
    /// multiple lines.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// An optional error cursor position into either the original query string
    /// or an internally generated query.
    pub fn position(&self) -> Option<&ErrorPosition> {
        self.position.as_ref()
    }

    /// An indication of the context in which the error occurred.
    ///
    /// Presently this includes a call stack traceback of active procedural
    /// language functions and internally-generated queries. The trace is one
    /// entry per line, most recent first.
    pub fn where_(&self) -> Option<&str> {
        self.where_.as_deref()
    }

    /// If the error was associated with a specific database object, the name
    /// of the schema containing that object, if any. (PostgreSQL 9.3+)
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// If the error was associated with a specific table, the name of the
    /// table. (Refer to the schema name field for the name of the table's
    /// schema.) (PostgreSQL 9.3+)
    pub fn table(&self) -> Option<&str> {
        self.table.as_deref()
    }

    /// If the error was associated with a specific table column, the name of
    /// the column.
    ///
    /// (Refer to the schema and table name fields to identify the table.)
    /// (PostgreSQL 9.3+)
    pub fn column(&self) -> Option<&str> {
        self.column.as_deref()
    }

    /// If the error was associated with a specific data type, the name of the
    /// data type. (Refer to the schema name field for the name of the data
    /// type's schema.) (PostgreSQL 9.3+)
    pub fn datatype(&self) -> Option<&str> {
        self.datatype.as_deref()
    }

    /// If the error was associated with a specific constraint, the name of the
    /// constraint.
    ///
    /// Refer to fields listed above for the associated table or domain.
    /// (For this purpose, indexes are treated as constraints, even if they
    /// weren't created with constraint syntax.) (PostgreSQL 9.3+)
    pub fn constraint(&self) -> Option<&str> {
        self.constraint.as_deref()
    }

    /// The file name of the source-code location where the error was reported.
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    /// The line number of the source-code location where the error was
    /// reported.
    pub fn line(&self) -> Option<u32> {
        self.line
    }

    /// The name of the source-code routine reporting the error.
    pub fn routine(&self) -> Option<&str> {
        self.routine.as_deref()
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{}: {}", self.severity, self.message)?;
        if let Some(detail) = &self.detail {
            write!(fmt, "\nDETAIL: {detail}")?;
        }
        if let Some(hint) = &self.hint {
            write!(fmt, "\nHINT: {hint}")?;
        }
        Ok(())
    }
}

impl error::Error for DbError {}

/// Represents the position of an error in a query.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ErrorPosition {
    /// A position in the original query.
    Original(u32),
    /// A position in an internally generated query.
    Internal {
        /// The byte position.
        position: u32,
        /// A query generated by the Postgres server.
        query: String,
    },
}

#[derive(Debug, PartialEq)]
enum Kind {
    Io,
    UnexpectedMessage,
    CopyOutUnsupported,
    CopyOutAnsweredCopyIn,
    CopyInProgress,
    CopyOutProgress,
    CopyInFinished,
    /// A TLS problem that is settled before any handshake bytes are exchanged:
    /// an impossible `sslmode` combination, a server that refuses `SSLRequest`
    /// under a mode that requires TLS, an unreadable `sslrootcert`.
    Tls,
    /// The TLS handshake itself failed - certificate rejected, protocol
    /// mismatch, connection reset mid-handshake.
    ///
    /// Separate from [`Kind::Tls`] because `sslmode=prefer` retries exactly
    /// this failure in plaintext (libpq's `CONNECTION_FAILED()` after
    /// `pqsecure_open_client`) and must retry nothing else.
    TlsHandshake,
    /// The supplied [`TlsConnect`](crate::tls::TlsConnect) does not attest to a
    /// TLS parameter the configuration asks for - `sslsni`, `sslcertmode`, or
    /// the server verification `sslmode` plus `sslrootcert` demand.
    ///
    /// Separate from [`Kind::Tls`] for the same reason [`Kind::TlsHandshake`]
    /// is: it decides a retry. TLS-as-configured is unavailable through this
    /// connector, which is the condition `sslmode=prefer` exists to handle, so
    /// `prefer` falls back to plaintext here exactly as it does for a failed
    /// handshake. The modes that actually promise encryption or verification -
    /// `require`, `verify-ca`, `verify-full` - have no plaintext leg and so
    /// still fail, which is what stops the fallback being a silent downgrade.
    ///
    /// libpq has no analogue: its TLS implementation IS the attestation, so
    /// this category only exists for a driver that takes a pluggable connector.
    TlsUnattested,
    ToSql(usize),
    FromSql(usize),
    Column(String),
    ColumnCount,
    Parameters(usize, usize),
    /// A prepared statement with a live owning connection was passed to a
    /// different connection. `PostgreSQL` prepared statements are session-local,
    /// so sending it can only produce `26000` for the unknown generated name.
    ///
    /// libpq has no analogue: `PQexecPrepared` accepts a caller-owned statement
    /// name and a `PGconn`, not a statement handle carrying its provenance.
    StatementOwnerMismatch,
    /// A prepared statement outlived the connection that created it. Its
    /// server-side name disappeared with that session and cannot be used on any
    /// later connection.
    ///
    /// libpq has no analogue: it does not expose an owner-bearing prepared
    /// statement handle whose connection lifetime can be checked locally.
    StatementOwnerDropped,
    Closed,
    /// An earlier operation on this connection was dropped before it
    /// completed, so the connection is out of step with the server.
    ///
    /// Distinct from [`Kind::Closed`]: the socket is open and the server is
    /// fine. What is broken is the agreement about where in the byte stream
    /// each side is - a cancelled read discards bytes it took off the socket,
    /// and a cancelled write can leave a fraction of a message on the wire.
    /// Neither is repairable by anything the driver can send, so the only
    /// correct answer to a later call is to refuse it.
    Cancelled,
    Db,
    Parse,
    Encode,
    Authentication,
    ConfigParse,
    Config,
    RowCount,
    Connect,
    PoolClosed,
    PoolTimeout,
    /// A post-startup target-session probe produced a valid rejection, an SQL
    /// error, or an unusable result. This rejects the current configured host,
    /// skipping its other transports and addresses, but permits the next host.
    TargetSessionAttrs,
    /// Sending or reading the post-startup target-session probe failed.
    /// libpq treats this as a broken connection request, so no transport,
    /// address, configured host, or `prefer-standby` pass may be retried.
    TargetSessionAttrsFatal,
    /// A deadline configured by the caller around a pooled client command
    /// expired. This is local policy, not `PostgreSQL`'s `57014` response.
    CommandTimeout,
    /// A post-startup socket read made no progress before the connection's
    /// configured inactivity deadline. The protocol session is unrecoverable.
    ReadTimeout,
    /// `COMMIT` reached the server inside an aborted transaction block, so
    /// PostgreSQL discarded every change and answered with the `ROLLBACK`
    /// command tag instead of `COMMIT`.
    ///
    /// This is NOT an `ErrorResponse`: the server considers the statement to
    /// have succeeded, and libpq reports it only through `PQcmdStatus`. The
    /// driver's `commit()` returns `Result<(), Error>` and has nowhere to put a
    /// command tag, so without this kind the one signal that a write was thrown
    /// away has no way to reach the caller.
    TransactionRolledBack,
}

struct ErrorInner {
    kind: Kind,
    cause: Option<Box<dyn error::Error + Sync + Send>>,
    cancel_delivery: CancelDelivery,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum CancelDelivery {
    Unsent,
    PossiblySent,
}

/// An error communicating with the Postgres server.
pub struct Error(Box<ErrorInner>);

impl fmt::Debug for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Error")
            .field("kind", &self.0.kind)
            .field("cause", &self.0.cause)
            .finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0.kind {
            Kind::Io => fmt.write_str("error communicating with the server"),
            Kind::UnexpectedMessage => fmt.write_str("unexpected message from server"),
            Kind::CopyOutUnsupported => {
                fmt.write_str("COPY TO STDOUT is not supported by this API; use Client::copy_out")
            }
            Kind::CopyOutAnsweredCopyIn => fmt.write_str(
                "the server answered a COPY IN request with COPY OUT; if the statement is \
                 COPY ... TO STDOUT use Client::copy_out, otherwise the server violated the \
                 protocol",
            ),
            Kind::CopyInProgress => fmt.write_str("cannot queue commands during COPY IN"),
            Kind::CopyOutProgress => fmt.write_str("cannot queue commands during COPY OUT"),
            Kind::CopyInFinished => fmt.write_str("COPY IN sink is already finished"),
            Kind::Tls => fmt.write_str("TLS could not be negotiated"),
            Kind::TlsHandshake => fmt.write_str("error performing TLS handshake"),
            Kind::TlsUnattested => fmt.write_str(
                "the supplied TLS connector does not attest to the configured TLS parameters",
            ),
            Kind::ToSql(idx) => write!(fmt, "error serializing parameter {idx}"),
            Kind::FromSql(idx) => write!(fmt, "error deserializing column {idx}"),
            Kind::Column(column) => write!(fmt, "invalid column `{column}`"),
            Kind::ColumnCount => write!(fmt, "query returned an unexpected number of columns"),
            Kind::Parameters(real, expected) => {
                write!(fmt, "expected {expected} parameters but got {real}")
            }
            Kind::StatementOwnerMismatch => {
                fmt.write_str("prepared statement belongs to a different connection")
            }
            Kind::StatementOwnerDropped => {
                fmt.write_str("prepared statement's owning connection has been dropped")
            }
            Kind::Closed => fmt.write_str("connection closed"),
            Kind::Cancelled => fmt.write_str(
                "an operation was dropped before it completed, leaving this connection out of \
                 step with the server",
            ),
            Kind::Db => fmt.write_str("db error"),
            Kind::Parse => fmt.write_str("error parsing response from server"),
            Kind::Encode => fmt.write_str("error encoding message to server"),
            Kind::Authentication => fmt.write_str("authentication error"),
            Kind::ConfigParse => fmt.write_str("invalid connection string"),
            Kind::Config => fmt.write_str("invalid configuration"),
            Kind::RowCount => fmt.write_str("query returned an unexpected number of rows"),
            Kind::Connect => fmt.write_str("error connecting to server"),
            Kind::PoolClosed => fmt.write_str("pool is closed"),
            Kind::PoolTimeout => fmt.write_str("pool acquisition timed out"),
            Kind::TargetSessionAttrs => fmt.write_str("error checking target session attributes"),
            Kind::TargetSessionAttrsFatal => {
                fmt.write_str("error communicating during target session attribute check")
            }
            Kind::CommandTimeout => fmt.write_str("client command timeout expired"),
            Kind::ReadTimeout => fmt.write_str("socket read timeout expired"),
            Kind::TransactionRolledBack => fmt.write_str(
                "the server rolled the transaction back instead of committing it: a statement \
                 in it had already failed",
            ),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.0.cause.as_ref().map(|e| &**e as _)
    }
}

impl Error {
    /// Consumes the error, returning its cause.
    pub fn into_source(self) -> Option<Box<dyn error::Error + Sync + Send>> {
        self.0.cause
    }

    /// Returns the source of this error if it was a `DbError`.
    ///
    /// This is a simple convenience method.
    pub fn as_db_error(&self) -> Option<&DbError> {
        self.source().and_then(|e| e.downcast_ref::<DbError>())
    }

    /// Determines if the error was associated with closed connection.
    pub fn is_closed(&self) -> bool {
        self.0.kind == Kind::Closed
    }

    /// Whether acquisition was rejected because pool shutdown has begun.
    #[must_use]
    pub fn is_pool_closed(&self) -> bool {
        self.0.kind == Kind::PoolClosed
    }

    /// Whether the pool's acquisition budget expired during startup or checkout.
    ///
    /// This includes connection setup, validation, and lifecycle hooks. It does
    /// not imply that the pool was full or that the database was unreachable.
    /// Command and socket deadlines have their own error predicates.
    #[must_use]
    pub fn is_pool_timeout(&self) -> bool {
        self.0.kind == Kind::PoolTimeout
    }

    /// Whether the connection was refused because an earlier operation on it
    /// was dropped before completing.
    ///
    /// A connection reporting this cannot be recovered or retried on - see
    /// [`Kind::Cancelled`]. Discard it and open another.
    pub fn is_cancelled(&self) -> bool {
        self.0.kind == Kind::Cancelled
    }

    /// Whether a pooled client's configured command deadline expired.
    ///
    /// This reports only the client-side deadline from
    /// [`crate::PoolConfig::command_timeout`]. A server-side cancellation,
    /// including `PostgreSQL`'s `statement_timeout`, remains a database error
    /// with SQLSTATE `57014` and returns `false` here.
    #[must_use]
    pub fn is_command_timeout(&self) -> bool {
        self.0.kind == Kind::CommandTimeout
    }

    /// Whether the configured post-startup socket-read inactivity deadline
    /// expired.
    ///
    /// This is distinct from a pooled command timeout and from a PostgreSQL
    /// `57014` cancellation. The connection that reports it has been retired;
    /// a possibly partial protocol read cannot be resumed safely.
    #[must_use]
    pub fn is_read_timeout(&self) -> bool {
        self.0.kind == Kind::ReadTimeout
    }

    /// Whether `commit` failed because the server rolled the transaction back.
    ///
    /// True only for the outcome PostgreSQL reports as a SUCCESSFUL statement:
    /// a `COMMIT` inside an aborted transaction block, answered with the
    /// `ROLLBACK` command tag. There is no `ErrorResponse` and no SQLSTATE, so
    /// [`Error::code`] returns `None` here - the failure a retry loop wants to
    /// key on is the one that came earlier, from the statement that aborted the
    /// block.
    #[must_use]
    pub fn is_transaction_rolled_back(&self) -> bool {
        self.0.kind == Kind::TransactionRolledBack
    }

    /// Whether this is a failure of the TLS handshake itself.
    ///
    /// `sslmode=prefer` keys its plaintext retry on exactly this, and on
    /// nothing else - see [`Kind::TlsHandshake`].
    pub(crate) fn is_tls_handshake(&self) -> bool {
        self.0.kind == Kind::TlsHandshake
    }

    /// Whether the supplied connector could not attest to a configured TLS
    /// parameter. See [`Kind::TlsUnattested`]: `prefer` treats this as "TLS is
    /// not available here" and falls back.
    pub(crate) fn is_tls_unattested(&self) -> bool {
        self.0.kind == Kind::TlsUnattested
    }

    /// Whether authentication failed locally while processing the server's
    /// challenge, before the server could report an SQLSTATE.
    pub(crate) fn is_authentication(&self) -> bool {
        self.0.kind == Kind::Authentication
    }

    /// Whether a locally enforced connection configuration rejected a valid
    /// server response.
    pub(crate) fn is_config(&self) -> bool {
        self.0.kind == Kind::Config
    }

    /// Whether the post-startup session-property check rejected the current
    /// configured host while permitting the next configured host.
    pub(crate) fn is_target_session_attrs(&self) -> bool {
        self.0.kind == Kind::TargetSessionAttrs
    }

    /// Whether sending or reading the post-startup session-property check
    /// failed and therefore ends the entire connection request.
    pub(crate) fn is_target_session_attrs_fatal(&self) -> bool {
        self.0.kind == Kind::TargetSessionAttrsFatal
    }

    /// Returns the SQLSTATE error code associated with the error.
    ///
    /// This is a convenience method that downcasts the cause to a `DbError` and returns its code.
    pub fn code(&self) -> Option<&SqlState> {
        self.as_db_error().map(DbError::code)
    }

    fn new(kind: Kind, cause: Option<Box<dyn error::Error + Sync + Send>>) -> Error {
        Error(Box::new(ErrorInner {
            kind,
            cause,
            cancel_delivery: CancelDelivery::Unsent,
        }))
    }

    pub(crate) fn with_cancel_delivery(mut self, delivery: CancelDelivery) -> Error {
        self.0.cancel_delivery = delivery;
        self
    }

    pub(crate) fn cancel_delivery(&self) -> CancelDelivery {
        self.0.cancel_delivery
    }

    pub(crate) fn closed() -> Error {
        Error::new(Kind::Closed, None)
    }

    pub(crate) fn cancelled() -> Error {
        Error::new(Kind::Cancelled, None)
    }

    pub(crate) fn transaction_rolled_back() -> Error {
        Error::new(Kind::TransactionRolledBack, None)
    }

    pub(crate) fn unexpected_message() -> Error {
        Error::new(Kind::UnexpectedMessage, None)
    }

    pub(crate) fn copy_out_unsupported() -> Error {
        Error::new(Kind::CopyOutUnsupported, None)
    }

    pub(crate) fn copy_out_answered_copy_in() -> Error {
        Error::new(Kind::CopyOutAnsweredCopyIn, None)
    }

    pub(crate) fn copy_in_progress() -> Error {
        Error::new(Kind::CopyInProgress, None)
    }

    pub(crate) fn copy_out_progress() -> Error {
        Error::new(Kind::CopyOutProgress, None)
    }

    pub(crate) fn copy_in_finished() -> Error {
        Error::new(Kind::CopyInFinished, None)
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn db(error: ErrorResponseBody) -> Error {
        match DbError::parse(&mut error.fields()) {
            Ok(e) => Error::from_db_error(e),
            Err(e) => Error::new(Kind::Parse, Some(Box::new(e))),
        }
    }

    pub(crate) fn from_db_error(error: DbError) -> Error {
        Error::new(Kind::Db, Some(Box::new(error)))
    }

    pub(crate) fn parse(e: io::Error) -> Error {
        Error::new(Kind::Parse, Some(Box::new(e)))
    }

    pub(crate) fn encode(e: io::Error) -> Error {
        Error::new(Kind::Encode, Some(Box::new(e)))
    }

    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_sql(e: Box<dyn error::Error + Sync + Send>, idx: usize) -> Error {
        Error::new(Kind::ToSql(idx), Some(e))
    }

    pub(crate) fn from_sql(e: Box<dyn error::Error + Sync + Send>, idx: usize) -> Error {
        Error::new(Kind::FromSql(idx), Some(e))
    }

    pub(crate) fn column(column: String) -> Error {
        Error::new(Kind::Column(column), None)
    }

    pub(crate) fn column_count() -> Error {
        Error::new(Kind::ColumnCount, None)
    }

    pub(crate) fn parameters(real: usize, expected: usize) -> Error {
        Error::new(Kind::Parameters(real, expected), None)
    }

    pub(crate) fn statement_owner_mismatch() -> Error {
        Error::new(Kind::StatementOwnerMismatch, None)
    }

    pub(crate) fn statement_owner_dropped() -> Error {
        Error::new(Kind::StatementOwnerDropped, None)
    }

    pub(crate) fn tls(e: Box<dyn error::Error + Sync + Send>) -> Error {
        Error::new(Kind::Tls, Some(e))
    }

    pub(crate) fn tls_handshake(e: Box<dyn error::Error + Sync + Send>) -> Error {
        Error::new(Kind::TlsHandshake, Some(e))
    }

    pub(crate) fn tls_unattested(e: Box<dyn error::Error + Sync + Send>) -> Error {
        Error::new(Kind::TlsUnattested, Some(e))
    }

    pub(crate) fn io(e: io::Error) -> Error {
        Error::new(Kind::Io, Some(Box::new(e)))
    }

    pub(crate) fn authentication(e: Box<dyn error::Error + Sync + Send>) -> Error {
        Error::new(Kind::Authentication, Some(e))
    }

    pub(crate) fn config_parse(e: Box<dyn error::Error + Sync + Send>) -> Error {
        Error::new(Kind::ConfigParse, Some(e))
    }

    pub(crate) fn config(e: Box<dyn error::Error + Sync + Send>) -> Error {
        Error::new(Kind::Config, Some(e))
    }

    pub(crate) fn row_count() -> Error {
        Error::new(Kind::RowCount, None)
    }

    pub(crate) fn connect(e: io::Error) -> Error {
        Error::new(Kind::Connect, Some(Box::new(e)))
    }

    pub(crate) fn pool_closed() -> Error {
        Error::new(Kind::PoolClosed, None)
    }

    pub(crate) fn pool_timeout(detail: String) -> Error {
        Error::new(
            Kind::PoolTimeout,
            Some(Box::new(io::Error::new(io::ErrorKind::TimedOut, detail))),
        )
    }

    pub(crate) fn target_session_attrs(e: Error) -> Error {
        Error::new(Kind::TargetSessionAttrs, e.into_source())
    }

    pub(crate) fn target_session_attrs_fatal(e: Error) -> Error {
        Error::new(Kind::TargetSessionAttrsFatal, e.into_source())
    }

    pub(crate) fn command_timeout(cause: Option<Box<dyn error::Error + Sync + Send>>) -> Error {
        Error::new(Kind::CommandTimeout, cause)
    }

    pub(crate) fn read_timeout(timeout: Duration) -> Error {
        Error::new(
            Kind::ReadTimeout,
            Some(Box::new(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("socket read made no progress for {timeout:?}; the connection was retired"),
            ))),
        )
    }

    /// Build the same public classification for a response channel while the
    /// original error remains owned by `Connection::run`.
    /// A copy of a terminal connection error, for handing to a request that
    /// was waiting on the connection when it died.
    ///
    /// `Error` is not `Clone` - it owns a boxed cause - so the copy carries the
    /// RENDERED text of this error and its cause chain rather than the cause
    /// itself. That loses the concrete type and keeps the diagnosis, which is
    /// the part the caller needs: the alternative is `Kind::Closed` with an
    /// empty cause chain, which says only that the connection is gone and
    /// never why.
    pub(crate) fn duplicate_terminal(&self) -> Error {
        let mut detail = self.to_string();
        let mut source = self.source();
        while let Some(cause) = source {
            detail.push_str(": ");
            detail.push_str(&cause.to_string());
            source = cause.source();
        }
        Error::io(io::Error::other(detail))
    }

    pub(crate) fn duplicate_read_timeout(&self) -> Option<Error> {
        if !self.is_read_timeout() {
            return None;
        }
        let detail = self.source().map_or_else(
            || "the connection was retired".to_string(),
            ToString::to_string,
        );
        Some(Error::new(
            Kind::ReadTimeout,
            Some(Box::new(io::Error::new(io::ErrorKind::TimedOut, detail))),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use postgres_protocol::message::backend::Message;

    #[test]
    fn pool_errors_are_typed_independently_of_their_messages() {
        let closed = Error::pool_closed();
        let timeout = Error::pool_timeout("diagnostic context".into());
        assert!(closed.is_pool_closed());
        assert!(!closed.is_closed());
        assert!(!closed.is_pool_timeout());
        assert_eq!(closed.to_string(), "pool is closed");
        assert!(timeout.is_pool_timeout());
        assert!(!timeout.is_pool_closed());
        assert!(!timeout.is_command_timeout());
        assert!(!timeout.is_read_timeout());
        assert_eq!(timeout.to_string(), "pool acquisition timed out");
        let source = timeout
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(source.kind(), io::ErrorKind::TimedOut);
        assert_eq!(source.to_string(), "diagnostic context");
        for error in [
            Error::connect(io::Error::other("pool is closed")),
            Error::connect(io::Error::other("pool acquisition timed out")),
            Error::closed(),
            Error::command_timeout(None),
            Error::read_timeout(Duration::from_secs(1)),
        ] {
            assert!(!error.is_pool_closed(), "misclassified {error:?}");
            assert!(!error.is_pool_timeout(), "misclassified {error:?}");
        }
    }

    /// Build a real `ErrorResponseBody` by framing `fields` and running the
    /// protocol crate's own parser over it, rather than reaching into its
    /// private storage. Each field is a type byte then a NUL-terminated value,
    /// and a lone NUL ends the list.
    fn error_response(fields: &[(u8, &str)]) -> ErrorResponseBody {
        let mut body = BytesMut::new();
        for (tag, value) in fields {
            body.extend_from_slice(&[*tag]);
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\0");
        }
        body.extend_from_slice(b"\0");

        let mut frame = BytesMut::new();
        frame.extend_from_slice(b"E");
        frame.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(&body);

        match Message::parse(&mut frame)
            .expect("frame a valid ErrorResponse")
            .expect("a whole message")
        {
            Message::ErrorResponse(body) => body,
            _ => panic!("expected an ErrorResponse"),
        }
    }

    /// An unrecognised `V` must cost the SEVERITY, not the whole error.
    ///
    /// `V` is a non-localized copy of `S`, added in PostgreSQL 9.6, and this
    /// type already models its absence: `parsed_severity` is an `Option` and is
    /// `None` for every pre-9.6 server. Refusing the message outright on an
    /// unknown value throws away the SQLSTATE and the text the caller needs and
    /// hands them a parse error instead, while the raw severity string is right
    /// there in `S` and is kept regardless.
    ///
    /// Reaching this needs a server that is not stock PostgreSQL -- a fork, a
    /// proxy, or a future release that adds a level. Low severity; the point is
    /// that the failure mode is losing a good error, which is worse than the
    /// thing it guards against.
    #[test]
    fn an_unrecognised_severity_costs_the_severity_not_the_whole_error() {
        let body = error_response(&[
            (b'S', "ERROR"),
            (b'C', "42P01"),
            (b'M', "relation \"t\" does not exist"),
            (b'V', "SOMETHING_NEW"),
        ]);

        let error = DbError::parse(&mut body.fields())
            .expect("an unknown V must not discard a well-formed error");

        assert_eq!(error.code().code(), "42P01");
        assert_eq!(error.message(), "relation \"t\" does not exist");
        assert_eq!(
            error.severity(),
            "ERROR",
            "the raw S field is still carried"
        );
        assert_eq!(
            error.parsed_severity(),
            None,
            "an unrecognised level is unknown, not fatal"
        );
    }

    /// One variable away: a level the table DOES know must still parse, or the
    /// arm above could be satisfied by never parsing severity at all.
    #[test]
    fn a_recognised_severity_is_still_parsed() {
        let body = error_response(&[
            (b'S', "FATAL"),
            (b'C', "57P01"),
            (b'M', "terminating connection"),
            (b'V', "FATAL"),
        ]);

        let error = DbError::parse(&mut body.fields()).expect("a well-formed error");
        assert_eq!(error.parsed_severity(), Some(Severity::Fatal));
    }

    /// The fields the protocol makes mandatory are still mandatory: dropping
    /// the guard on `V` must not read as "accept anything".
    #[test]
    fn a_missing_mandatory_field_is_still_refused() {
        // No `C`.
        let body = error_response(&[(b'S', "ERROR"), (b'M', "no sqlstate here")]);
        DbError::parse(&mut body.fields()).expect_err("`C` is mandatory");
    }

    /// A malformed AUXILIARY field must not discard the diagnostic.
    ///
    /// `P`, `p` and `L` are optional informational fields - a character
    /// position, an internal position, and a source line. Each was parsed with
    /// `?`, so a non-integer in any of them failed the WHOLE `ErrorResponse`
    /// and the caller got a parse error in place of the server's SQLSTATE and
    /// message.
    ///
    /// This is the defect already fixed for `V`, whose comment in the parser
    /// says it outright: refusing there "discarded a well-formed error's
    /// SQLSTATE and text and reported a parse failure in their place". The
    /// reasoning transfers unchanged - these fields are optional, their absence
    /// is a supported state, so a value this parser cannot read is "unknown",
    /// not "the message is malformed".
    #[test]
    fn a_malformed_position_field_keeps_the_sqlstate_and_message() {
        for tag in [b'P', b'p', b'L'] {
            let body = error_response(&[
                (b'S', "ERROR"),
                (b'C', "42P01"),
                (b'M', "relation \"t\" does not exist"),
                (tag, "not-an-integer"),
            ]);
            let error = DbError::parse(&mut body.fields()).unwrap_or_else(|e| {
                panic!(
                    "a non-integer in the optional `{}` field discarded the whole error: {e}",
                    tag as char
                )
            });
            assert_eq!(error.code(), &SqlState::UNDEFINED_TABLE);
            assert_eq!(error.message(), "relation \"t\" does not exist");
        }
    }

    /// THE CONTROL, one variable: the same fields carrying VALID integers must
    /// still be parsed and exposed. Dropping them outright would also satisfy
    /// the test above, so this is what stops the fix being "ignore P/p/L".
    #[test]
    fn a_well_formed_position_field_is_still_reported() {
        let body = error_response(&[
            (b'S', "ERROR"),
            (b'C', "42601"),
            (b'M', "syntax error"),
            (b'P', "42"),
            (b'L', "1234"),
        ]);
        let error = DbError::parse(&mut body.fields()).expect("a well-formed error parses");
        assert_eq!(error.position(), Some(&ErrorPosition::Original(42)));
        assert_eq!(error.line(), Some(1234));
    }
}
