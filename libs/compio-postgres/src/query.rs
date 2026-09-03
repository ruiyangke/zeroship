// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Derived from tokio-postgres's extended-query framing, but local execution
// adds text-parameter and statement-cache paths, portal liveness,
// pre-/post-BindComplete error tracking, and connection-owned COPY recovery.
// Bind conversion falls back through domain base types; `RowStream` adds COPY
// draining, empty-query counts, and described-column access.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::command_tag::extract_row_affected;
use crate::connection::RequestMessages;
use crate::copy_in::CopyInReceiver;
use crate::prepare::get_type;
use crate::simple_query::{COPY_IN_UNSUPPORTED_EXTENDED, may_enter_copy_in};
use crate::types::{BorrowToSql, IsNull, Kind, ToSql};
use crate::{Column, Error, Portal, Row, Statement};
use bytes::{Bytes, BytesMut};
use fallible_iterator::FallibleIterator;
use futures_util::Stream;
use log::{Level, debug, log_enabled};
use pin_project_lite::pin_project;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use postgres_types::Type;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

struct BorrowToSqlParamsDebug<'a, T>(&'a [T]);

impl<T> fmt::Debug for BorrowToSqlParamsDebug<'_, T>
where
    T: BorrowToSql,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|x| x.borrow_to_sql()))
            .finish()
    }
}

pub async fn query<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
) -> Result<RowStream, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    query_inner(client, statement, params).await
}

/// Start a cache-hit query and return only after PostgreSQL has accepted Bind.
///
/// A genuine stale statement fails before `BindComplete`. Once that message
/// arrives, Execute may have run application code, so every later error belongs
/// to the returned stream and is never eligible for transparent replay.
pub(crate) async fn query_cached<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
) -> Result<RowStream, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    query_inner(client, statement, params).await
}

async fn query_inner<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
) -> Result<RowStream, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let buf = if log_enabled!(Level::Debug) {
        let params = params.into_iter().collect::<Vec<_>>();
        debug!(
            "executing statement {} with parameters: {:?}",
            statement.name(),
            BorrowToSqlParamsDebug(params.as_slice()),
        );
        encode(client, &statement, params)?
    } else {
        encode(client, &statement, params)?
    };
    let responses = match start(client, buf, &statement).await {
        Ok(responses) => responses,
        Err(failure) => {
            let error = failure.into_error();
            // A pre-BindComplete ErrorResponse has already crossed the
            // connection dispatcher, which invalidates this same statement
            // before waking the response consumer. This idempotent call is a
            // defensive second check.
            //
            // The last sentence here used to add that local send/read errors
            // "cannot make it observable alone", which read as: no test can
            // see this. Not so - a scripted 26000 in the start slot reaches it
            // directly, and `a_stale_start_error_invalidates_the_cached_
            // statement` binds this line and the `execute` twin at :573.
            // Deleting either reddens that test and nothing else.
            statement.invalidate_cache_on_error(&error);
            return Err(error);
        }
    };
    Ok(RowStream {
        statement,
        responses,
        rows_affected: None,
        copy_out_refused: false,
    })
}

/// The protocol phase in which a cached execution failed.
///
/// Only the first variant can describe PostgreSQL rejecting the named
/// statement itself. Ordinary explicit statements report the same public
/// [`Error`] in either phase.
pub(crate) enum ExecutionError {
    BeforeBindComplete(Error),
    AfterBindComplete(Error),
}

impl ExecutionError {
    pub(crate) fn before_bind_complete(error: Error) -> Self {
        Self::BeforeBindComplete(error)
    }

    pub(crate) fn after_bind_complete(error: Error) -> Self {
        Self::AfterBindComplete(error)
    }

    /// Classify a message that stood in for an expected `ParseComplete` or
    /// `BindComplete`.
    ///
    /// A copy response means the server is already in copy mode, so it is past
    /// Bind however the exchange reached this point, and `BeforeBindComplete`
    /// is documented as the phase that can describe PostgreSQL rejecting the
    /// named statement.
    ///
    /// This is defence in depth rather than a reachable replay. The phase is
    /// only the FIRST of two gates: `reprepare_cached_statement_once` also
    /// requires `cached_statement_error_is_stale`, and the
    /// `Error::unexpected_message` raised here carries no `DbError`, so its
    /// `code()` is `None` and that predicate is already false. Classifying by
    /// the documented meaning keeps the phase honest instead of leaving it
    /// resting on the second gate.
    pub(crate) fn pre_bind_mismatch(message: &Message) -> Self {
        match message {
            Message::CopyInResponse(_) | Message::CopyOutResponse(_) => {
                Self::after_bind_complete(Error::unexpected_message())
            }
            _ => Self::before_bind_complete(Error::unexpected_message()),
        }
    }

    pub(crate) fn error(&self) -> &Error {
        match self {
            Self::BeforeBindComplete(error) | Self::AfterBindComplete(error) => error,
        }
    }

    pub(crate) fn into_parts(self) -> (Error, bool) {
        match self {
            Self::BeforeBindComplete(error) => (error, true),
            Self::AfterBindComplete(error) => (error, false),
        }
    }

    pub(crate) fn into_error(self) -> Error {
        self.into_parts().0
    }
}

/// Execute a statement with text-format string parameters.
///
/// Parse is sent with an empty OID list so the server infers each
/// parameter's type from its position in the SQL. This matches the legacy
/// `zeroship-pg` behaviour and is what JSON-driven query builders rely on
/// (they pass everything as strings and expect `UPDATE t SET c = $1` to
/// implicit-cast regardless of `c`'s column type).
///
/// Results come back in binary format (formats_code=1) so the normal
/// `FromSql` machinery decodes rows correctly.
pub async fn query_text_params(
    client: &Arc<InnerClient>,
    query: &str,
    params: &[&str],
) -> Result<RowStream, Error> {
    let buf = client.with_buf(|buf| {
        // Parse with no type hints - server infers.
        frontend::parse("", query, std::iter::empty::<u32>(), buf).map_err(Error::encode)?;

        // Bind: text format for parameters (code 0), binary format for results (code 1).
        let param_refs: Vec<Option<&[u8]>> = params.iter().map(|s| Some(s.as_bytes())).collect();
        frontend::bind(
            "",
            "",
            std::iter::once(0i16), // all params: text format
            param_refs,
            |val: Option<&[u8]>, out: &mut BytesMut| match val {
                Some(bytes) => {
                    out.extend_from_slice(bytes);
                    Ok(postgres_protocol::IsNull::No)
                }
                None => Ok(postgres_protocol::IsNull::Yes),
            },
            std::iter::once(1i16), // all results: binary format
            buf,
        )
        .map_err(|e| match e {
            frontend::BindError::Serialization(io_err) => Error::encode(io_err),
            frontend::BindError::Conversion(boxed) => {
                Error::encode(std::io::Error::other(format!("bind: {boxed}")))
            }
        })?;
        frontend::describe(b'P', "", buf).map_err(Error::encode)?;
        frontend::execute("", 0, buf).map_err(Error::encode)?;
        frontend::sync(buf);

        Ok(buf.split().freeze())
    })?;

    let mut responses = client.send(producerless_request(buf, may_enter_copy_in(query)))?;

    loop {
        match responses.next().await? {
            // Same portal rule as `execute_text_params`: this sends
            // `describe(b'P', ...)`, and a portal Describe answers with
            // RowDescription or NoData. ParameterDescription belongs to a
            // STATEMENT Describe, which only the `b'S'` paths below issue.
            Message::ParseComplete | Message::BindComplete => {}
            Message::NoData => {
                return Ok(RowStream {
                    statement: Statement::unnamed(vec![], vec![]),
                    responses,
                    rows_affected: None,
                    copy_out_refused: false,
                });
            }
            Message::RowDescription(row_description) => {
                let mut columns: Vec<Column> = vec![];
                let mut it = row_description.fields();
                while let Some(field) = it.next().map_err(Error::parse)? {
                    let type_ = get_type(client, field.type_oid())
                        .await
                        .map_err(|error| responses.take_request_server_error().unwrap_or(error))?;
                    let column = Column {
                        name: field.name().to_string(),
                        table_oid: Some(field.table_oid()).filter(|n| *n != 0),
                        column_id: Some(field.column_id()).filter(|n| *n != 0),
                        type_modifier: field.type_modifier(),
                        r#type: type_,
                    };
                    columns.push(column);
                }
                return Ok(RowStream {
                    statement: Statement::unnamed(vec![], columns),
                    responses,
                    rows_affected: None,
                    copy_out_refused: false,
                });
            }
            _ => return Err(Error::unexpected_message()),
        }
    }
}

fn map_execute_text_bind_error(error: frontend::BindError) -> Error {
    match error {
        frontend::BindError::Serialization(io_err) => {
            // The names and format counts are fixed and bounded. The one
            // remaining route is a Bind body above `i32::MAX`, which needs
            // more than 2 GiB of caller-owned text to construct. Keep the
            // resource-bound refusal without a suite-sized allocation.
            Error::encode(io_err)
        }
        frontend::BindError::Conversion(boxed) => {
            // `bind` shares the serializer's boxed error type with its
            // values-count encoder. More than `u16::MAX` values therefore
            // reaches this arm even though both serializer arms return
            // `Ok`; the bind-count test pins that upstream classification.
            Error::encode(std::io::Error::other(format!("bind: {boxed}")))
        }
    }
}

/// Execute a non-row statement with **NULL-aware text-format** parameters,
/// returning the affected-row count.
///
/// The `execute` peer of [`query_text_params`]: Parse is sent with an empty OID
/// list so the server infers each parameter's type FROM ITS POSITION in the SQL,
/// and every param is encoded in TEXT format (code 0). This is the
/// JSON-query-builder coercion model - a value passed as text is implicit-cast to
/// the target column type (`'2026-01-01'` -> `timestamptz`, `'1.5'` -> `numeric`),
/// which the typed-binary `execute_typed` path cannot do for a cross-type bind.
/// A `None` param is a SQL NULL (sent with no bytes). The op.* DML executor uses
/// this so a creator `insert`/`update` value coerces to the column type without
/// the assembler knowing the schema (names-are-strings, section 3.3).
pub async fn execute_text_params(
    client: &Arc<InnerClient>,
    query: &str,
    params: &[Option<String>],
) -> Result<u64, Error> {
    let buf = client.with_buf(|buf| {
        frontend::parse("", query, std::iter::empty::<u32>(), buf).map_err(Error::encode)?;
        let param_refs: Vec<Option<&[u8]>> = params
            .iter()
            .map(|s| s.as_ref().map(|v| v.as_bytes()))
            .collect();
        frontend::bind(
            "",
            "",
            std::iter::once(0i16), // all params: text format
            param_refs,
            |val: Option<&[u8]>, out: &mut BytesMut| match val {
                Some(bytes) => {
                    out.extend_from_slice(bytes);
                    Ok(postgres_protocol::IsNull::No)
                }
                None => Ok(postgres_protocol::IsNull::Yes),
            },
            std::iter::once(1i16), // results: binary (no rows for a DML, but keep uniform)
            buf,
        )
        .map_err(map_execute_text_bind_error)?;
        // These fixed-size messages use the literal empty portal name. Their
        // encoders can only fail for an interior NUL or an overflowing message
        // body, neither of which those inputs can contain.
        frontend::describe(b'P', "", buf).map_err(Error::encode)?;
        frontend::execute("", 0, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let mut responses = client.send(producerless_request(buf, may_enter_copy_in(query)))?;
    let mut rows = 0;
    let mut copy_out_refused = false;
    loop {
        match responses.next().await? {
            Message::ParseComplete | Message::BindComplete | Message::RowDescription(_) => {}
            Message::NoData => rows = 0,
            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                rows = extract_row_affected(&body)?;
            }
            Message::EmptyQueryResponse => rows = 0,
            // The connection-owned producer is already committed to
            // `CopyFail + Sync`; keep draining to its server error.
            Message::CopyInResponse(_) => {}
            Message::CopyOutResponse(_) => copy_out_refused = true,
            Message::CopyData(_) | Message::CopyDone if copy_out_refused => {}
            Message::ReadyForQuery(_) => {
                return if copy_out_refused {
                    Err(Error::copy_out_unsupported())
                } else {
                    Ok(rows)
                };
            }
            _ => return Err(Error::unexpected_message()),
        }
    }
}

pub async fn query_typed<P, I>(
    client: &Arc<InnerClient>,
    query: &str,
    params: I,
) -> Result<RowStream, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = (P, Type)>,
{
    let buf = {
        let params = params.into_iter().collect::<Vec<_>>();
        let param_oids = params.iter().map(|(_, t)| t.oid()).collect::<Vec<_>>();

        client.with_buf(|buf| {
            frontend::parse("", query, param_oids, buf).map_err(Error::encode)?;
            encode_bind_raw("", params, "", buf)?;
            frontend::describe(b'S', "", buf).map_err(Error::encode)?;
            frontend::execute("", 0, buf).map_err(Error::encode)?;
            frontend::sync(buf);

            Ok(buf.split().freeze())
        })?
    };

    let mut responses = client.send(producerless_request(buf, may_enter_copy_in(query)))?;

    loop {
        match responses.next().await? {
            Message::ParseComplete | Message::BindComplete | Message::ParameterDescription(_) => {}
            Message::NoData => {
                return Ok(RowStream {
                    statement: Statement::unnamed(vec![], vec![]),
                    responses,
                    rows_affected: None,
                    copy_out_refused: false,
                });
            }
            Message::RowDescription(row_description) => {
                let mut columns: Vec<Column> = vec![];
                let mut it = row_description.fields();
                while let Some(field) = it.next().map_err(Error::parse)? {
                    let type_ = get_type(client, field.type_oid())
                        .await
                        .map_err(|error| responses.take_request_server_error().unwrap_or(error))?;
                    let column = Column {
                        name: field.name().to_string(),
                        table_oid: Some(field.table_oid()).filter(|n| *n != 0),
                        column_id: Some(field.column_id()).filter(|n| *n != 0),
                        type_modifier: field.type_modifier(),
                        r#type: type_,
                    };
                    columns.push(column);
                }
                return Ok(RowStream {
                    statement: Statement::unnamed(vec![], columns),
                    responses,
                    rows_affected: None,
                    copy_out_refused: false,
                });
            }
            _ => return Err(Error::unexpected_message()),
        }
    }
}

pub async fn execute_typed<P, I>(
    client: &Arc<InnerClient>,
    query: &str,
    params: I,
) -> Result<u64, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = (P, Type)>,
{
    let buf = {
        let params = params.into_iter().collect::<Vec<_>>();
        let param_oids = params.iter().map(|(_, t)| t.oid()).collect::<Vec<_>>();

        client.with_buf(|buf| {
            frontend::parse("", query, param_oids, buf).map_err(Error::encode)?;
            encode_bind_raw("", params, "", buf)?;
            frontend::describe(b'S', "", buf).map_err(Error::encode)?;
            frontend::execute("", 0, buf).map_err(Error::encode)?;
            frontend::sync(buf);

            Ok(buf.split().freeze())
        })?
    };

    let mut responses = client.send(producerless_request(buf, may_enter_copy_in(query)))?;

    let mut rows = 0;
    let mut copy_out_refused = false;

    loop {
        match responses.next().await? {
            Message::ParseComplete
            | Message::BindComplete
            | Message::ParameterDescription(_)
            | Message::RowDescription(_) => {}
            Message::NoData => {
                rows = 0;
            }

            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                rows = extract_row_affected(&body)?;
            }

            Message::EmptyQueryResponse => rows = 0,
            Message::CopyInResponse(_) => {}
            Message::CopyOutResponse(_) => copy_out_refused = true,
            Message::CopyData(_) | Message::CopyDone if copy_out_refused => {}
            Message::ReadyForQuery(_) => {
                return if copy_out_refused {
                    Err(Error::copy_out_unsupported())
                } else {
                    Ok(rows)
                };
            }
            _ => {
                return Err(Error::unexpected_message());
            }
        }
    }
}

pub async fn query_portal(
    client: &Arc<InnerClient>,
    portal: &Portal,
    max_rows: i32,
) -> Result<RowStream, Error> {
    portal.with_live_on(client, || {
        let buf = client.with_buf(|buf| {
            frontend::execute(portal.name(), max_rows, buf).map_err(Error::encode)?;
            frontend::sync(buf);
            Ok(buf.split().freeze())
        })?;

        let responses = client.send_statement(
            producerless_request(buf, portal.statement().may_enter_copy_in()),
            portal.statement(),
        )?;

        Ok(RowStream {
            statement: portal.statement().clone(),
            responses,
            rows_affected: None,
            copy_out_refused: false,
        })
    })
}

pub async fn execute<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
) -> Result<u64, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    execute_inner(client, statement, params)
        .await
        .map_err(ExecutionError::into_error)
}

pub(crate) async fn execute_cached<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
) -> Result<u64, ExecutionError>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    execute_inner(client, statement, params).await
}

async fn execute_inner<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
) -> Result<u64, ExecutionError>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let buf = if log_enabled!(Level::Debug) {
        let params = params.into_iter().collect::<Vec<_>>();
        debug!(
            "executing statement {} with parameters: {:?}",
            statement.name(),
            BorrowToSqlParamsDebug(params.as_slice()),
        );
        encode(client, &statement, params).map_err(ExecutionError::before_bind_complete)?
    } else {
        encode(client, &statement, params).map_err(ExecutionError::before_bind_complete)?
    };
    let mut responses = match start(client, buf, &statement).await {
        Ok(responses) => responses,
        Err(failure) => {
            statement.invalidate_cache_on_error(failure.error());
            return Err(failure);
        }
    };

    let mut rows = 0;
    let mut copy_out_refused = false;
    loop {
        match responses
            .next()
            .await
            .map_err(ExecutionError::after_bind_complete)?
        {
            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                rows = extract_row_affected(&body).map_err(ExecutionError::after_bind_complete)?;
            }
            Message::EmptyQueryResponse => rows = 0,
            Message::CopyInResponse(_) => {}
            Message::CopyOutResponse(_) => copy_out_refused = true,
            Message::CopyData(_) | Message::CopyDone if copy_out_refused => {}
            Message::ReadyForQuery(_) => {
                return if copy_out_refused {
                    Err(ExecutionError::after_bind_complete(
                        Error::copy_out_unsupported(),
                    ))
                } else {
                    Ok(rows)
                };
            }
            _ => {
                return Err(ExecutionError::after_bind_complete(
                    Error::unexpected_message(),
                ));
            }
        }
    }
}

async fn start(
    client: &InnerClient,
    buf: Bytes,
    statement: &Statement,
) -> Result<Responses, ExecutionError> {
    let mut responses = client
        .send_statement(
            producerless_request(buf, statement.may_enter_copy_in()),
            statement,
        )
        .map_err(ExecutionError::before_bind_complete)?;

    match responses.next().await {
        Ok(Message::BindComplete) => {}
        Ok(other) => return Err(ExecutionError::pre_bind_mismatch(&other)),
        Err(error) => return Err(ExecutionError::before_bind_complete(error)),
    }

    Ok(responses)
}

pub fn encode<P, I>(client: &InnerClient, statement: &Statement, params: I) -> Result<Bytes, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    client.with_buf(|buf| {
        encode_bind(statement, params, "", buf)?;
        frontend::execute("", 0, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })
}

/// Route a known `COPY FROM STDIN` through the real COPY state machine even
/// when this API has no data producer. The connection owns the abort, so a
/// dropped response consumer cannot suppress it and a later request cannot be
/// written ahead of it. Startup rejection remains balanced because the COPY
/// loop suppresses its terminal frame after the initial `ReadyForQuery`.
pub(crate) fn producerless_request(buf: Bytes, may_enter_copy_in: bool) -> RequestMessages {
    let initial = FrontendMessage::Raw(buf);
    if may_enter_copy_in {
        RequestMessages::CopyIn(CopyInReceiver::aborting(
            initial,
            COPY_IN_UNSUPPORTED_EXTENDED,
        ))
    } else {
        RequestMessages::Single(initial)
    }
}

/// Recreate a described statement in PostgreSQL's unnamed slot and bind it in
/// the same frontend batch. Keeping Parse adjacent to Bind is what makes the
/// threshold path safe when multiple callers share one connection.
pub(crate) fn encode_unnamed<P, I>(
    client: &InnerClient,
    sql: &str,
    statement: &Statement,
    params: I,
) -> Result<Bytes, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    client.with_buf(|buf| {
        frontend::parse("", sql, statement.params().iter().map(Type::oid), buf)
            .map_err(Error::encode)?;
        encode_bind(statement, params, "", buf)?;
        frontend::execute("", 0, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })
}

pub fn encode_bind<P, I>(
    statement: &Statement,
    params: I,
    portal: &str,
    buf: &mut BytesMut,
) -> Result<(), Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let params = params.into_iter();
    if params.len() != statement.params().len() {
        return Err(Error::parameters(params.len(), statement.params().len()));
    }

    encode_bind_raw(
        statement.name(),
        params.zip(statement.params().iter().cloned()),
        portal,
        buf,
    )
}

/// Encode one bind parameter, falling back to a domain's BASE type.
///
/// PostgreSQL reports the DECLARED parameter type in `Describe`, so binding to
/// a domain column hands us the domain's own oid. Every `ToSql` impl in
/// `postgres-types` gates on the base oid (`ToSql for i32` accepts `INT4` and
/// nothing else), so `INSERT INTO t (v) VALUES ($1)` against a domain column
/// was refused outright with "error serializing parameter" -- a statement psql
/// executes without complaint, because libpq type-checks nothing client-side.
/// The result direction never had this problem: `RowDescription` reports the
/// BASE type, which is why decoding a domain column always worked.
///
/// The domain type is tried FIRST and the base only as a fallback, rather than
/// unwrapping up front. A caller may legitimately implement `ToSql` for a
/// domain by name -- that is the natural way to model `CREATE DOMAIN email` --
/// and unwrapping unconditionally would hand such an impl the base type it
/// does not accept, breaking code that works today.
///
/// The buffer is REWOUND between attempts. `to_sql` may write bytes before it
/// fails, and leaving them would prepend garbage to the retry's encoding: the
/// same defect that let a refused row reach the server in `binary_copy`'s
/// `write_raw`, where `[count][len=0]` was a legal empty value PostgreSQL
/// happily inserted.
///
/// THAT REWIND IS DEFENSIVE AND UNEXERCISED, measured rather than assumed:
/// deleting it leaves `tests/suite/domain_parameters.rs` green. `to_sql_checked`
/// consults `accepts` BEFORE calling `to_sql`, so the rejection this function
/// exists to recover from writes no bytes at all. The rewind covers the other
/// shape -- an impl whose `accepts` admits the domain and whose `to_sql` then
/// fails partway -- which no test here reaches. Keep it: it costs one `len()`
/// on a path that already failed once, and the failure it prevents is a
/// silently malformed parameter rather than an error.
///
/// This only chooses an ENCODING. The server still applies the domain's
/// constraints, which is what `a_domain_check_constraint_still_rejects_a_bad_value`
/// pins.
pub(crate) fn encode_parameter(
    param: &dyn ToSql,
    ty: &Type,
    buf: &mut BytesMut,
) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
    let checkpoint = buf.len();
    let first = param.to_sql_checked(ty, buf);
    if first.is_ok() {
        return first;
    }

    match underlying_base_type(ty) {
        Some(base) => {
            buf.truncate(checkpoint);
            param.to_sql_checked(&base, buf)
        }
        None => first,
    }
}

/// The type a domain reduces to, if `ty` is one or contains one.
///
/// ONE DEFINITION, called from both directions, because the two disagreed:
/// the bind path unwrapped a domain and the decode path did not, so the same
/// `d[]` column that could be written could not be read back.
///
/// Two shapes reduce:
///
/// * `d` itself - `Kind::Domain(base)`, walked to the end since a domain may be
///   defined over another domain.
/// * `d[]` - reported as `Kind::Array(Domain(base))`. The walk above never
///   reaches that domain because the OUTER kind is `Array`, which is why
///   `Vec<i32>` was refused against a column it can perfectly well fill.
///
/// Only the ELEMENT is substituted, never the array's own oid: the value still
/// names the domain array on the wire, so PostgreSQL still applies the domain's
/// constraints to every element. MEASURED against a live server - binding
/// `[1, -5]` through the substituted type is refused with SQLSTATE 23514, so
/// this relaxes what `accepts` will look at and nothing else.
pub(crate) fn underlying_base_type(ty: &Type) -> Option<Type> {
    let mut base = ty;
    while let Kind::Domain(inner) = base.kind() {
        base = inner;
    }
    if !std::ptr::eq(base, ty) {
        return Some(base.clone());
    }

    if let Kind::Array(element) = ty.kind() {
        let mut base_element = element;
        while let Kind::Domain(inner) = base_element.kind() {
            base_element = inner;
        }
        if !std::ptr::eq(base_element, element) {
            return Some(Type::new(
                ty.name().to_string(),
                ty.oid(),
                Kind::Array(base_element.clone()),
                ty.schema().to_string(),
            ));
        }
    }

    None
}

fn encode_bind_raw<P, I>(
    statement_name: &str,
    params: I,
    portal: &str,
    buf: &mut BytesMut,
) -> Result<(), Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = (P, Type)>,
    I::IntoIter: ExactSizeIterator,
{
    let (param_formats, params): (Vec<_>, Vec<_>) = params
        .into_iter()
        .map(|(p, ty)| (p.borrow_to_sql().encode_format(&ty) as i16, (p, ty)))
        .unzip();

    let mut error_idx = 0;
    let r = frontend::bind(
        portal,
        statement_name,
        param_formats,
        params.into_iter().enumerate(),
        |(idx, (param, ty)), buf| match encode_parameter(param.borrow_to_sql(), &ty, buf) {
            Ok(IsNull::No) => Ok(postgres_protocol::IsNull::No),
            Ok(IsNull::Yes) => Ok(postgres_protocol::IsNull::Yes),
            Err(e) => {
                error_idx = idx;
                Err(e)
            }
        },
        Some(1),
        buf,
    );
    match r {
        Ok(()) => Ok(()),
        Err(frontend::BindError::Conversion(e)) => Err(Error::to_sql(e, error_idx)),
        Err(frontend::BindError::Serialization(e)) => Err(Error::encode(e)),
    }
}

pin_project! {
    /// A stream of table rows.
    #[project(!Unpin)]
    pub struct RowStream {
        statement: Statement,
        responses: Responses,
        rows_affected: Option<u64>,
        copy_out_refused: bool,
    }
}

impl Stream for RowStream {
    type Item = Result<Row, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        loop {
            let message = match ready!(this.responses.poll_next(cx)) {
                Ok(message) => message,
                Err(error) => return Poll::Ready(Some(Err(error))),
            };
            match message {
                Message::DataRow(body) => {
                    return Poll::Ready(Some(Ok(Row::new(this.statement.clone(), body)?)));
                }
                Message::CommandComplete(body) => {
                    *this.rows_affected = Some(extract_row_affected(&body)?);
                }
                // An empty query completes with NO `CommandComplete`, so
                // nothing else here would ever set the count and
                // `rows_affected` stayed `None` on a stream that was fully
                // exhausted. `None` then meant both "not finished yet" and "no
                // count was sent", which the accessor's own documentation says
                // it does not -- and a caller cannot tell those apart. Zero is
                // also what `execute("")` already reports for the same query.
                Message::EmptyQueryResponse => *this.rows_affected = Some(0),
                // NOT the same case: a suspended portal has more rows to come,
                // so the stream genuinely is not exhausted and `None` is the
                // honest answer.
                Message::PortalSuspended => {}
                Message::ReadyForQuery(_) => {
                    if *this.copy_out_refused {
                        *this.copy_out_refused = false;
                        return Poll::Ready(Some(Err(Error::copy_out_unsupported())));
                    }
                    return Poll::Ready(None);
                }
                // The connection owns the producerless COPY abort. Returning
                // no item here keeps polling until PostgreSQL reports it.
                Message::CopyInResponse(_) => {}
                Message::CopyOutResponse(_) => *this.copy_out_refused = true,
                Message::CopyData(_) | Message::CopyDone if *this.copy_out_refused => {}
                _ => return Poll::Ready(Some(Err(Error::unexpected_message()))),
            }
        }
    }
}

impl RowStream {
    /// Returns the number of rows affected by the query.
    ///
    /// `None` until the stream has been exhausted - and, for a PORTAL, `None`
    /// even then, which is load-bearing rather than a gap. A page that ends on
    /// `PortalSuspended` has more rows waiting, so `None` is how a caller
    /// learns to fetch again; only the page that ends on `CommandComplete`
    /// reports a count.
    ///
    /// THAT COUNT IS THE EXECUTE'S, NOT THE PORTAL'S. Measured over a 5-row
    /// portal paged 2 at a time: the three pages report `None`, `None`,
    /// `Some(1)` - one, not five. Summing it across pages does not give the
    /// row total either, since the suspended pages contribute nothing.
    pub fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }

    /// The columns the statement will produce.
    ///
    /// Available BEFORE any row arrives, because it comes from the
    /// `RowDescription` the Describe already returned. That is what lets a
    /// caller rule on the shape of a result set that turns out to be empty --
    /// see `Client::query_scalar`, which reported an arity error only when rows
    /// happened to come back until this existed.
    pub fn columns(&self) -> &[Column] {
        self.statement.columns()
    }
}

pub async fn sync(client: &InnerClient) -> Result<(), Error> {
    let buf = Bytes::from_static(b"S\0\0\0\x04");
    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    match responses.next().await? {
        Message::ReadyForQuery(_) => Ok(()),
        _ => Err(Error::unexpected_message()),
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecutionError, encode_bind, map_execute_text_bind_error, query_text_params};
    use crate::Statement;
    use crate::client::Client;
    use crate::codec::FrontendMessage;
    use crate::config::{SslMode, SslNegotiation};
    use crate::connection::RequestMessages;
    use crate::types::{IsNull, ToSql, Type, to_sql_checked};
    use bytes::BytesMut;
    use futures_channel::mpsc;
    use futures_util::task::noop_waker;
    use std::error::Error as _;
    use std::future::Future;
    use std::task::{Context, Poll};

    /// A stale cached statement must leave the cache when `query` fails before
    /// Bind, and again when `execute` does.
    ///
    /// Measured 2026-09-02: deleting the `query` call left every lib and suite
    /// test green, and deleting the `execute` one reddened only
    /// `frontend_sequence::a_completed_copy_in_ends_with_copy_done_and_sync` -
    /// a COPY framing test that says nothing about the statement cache, so a
    /// regression there would have reported the wrong defect. `bind.rs` and
    /// the two COPY paths assert the same call by name; these are the last two
    /// that did not.
    #[compio::test]
    async fn a_stale_start_error_invalidates_the_cached_statement() {
        use crate::codec::BackendMessages;
        use crate::config::ProtocolVersion;
        use futures_util::StreamExt as _;
        use std::num::NonZeroUsize;
        use std::sync::Arc;

        // (label, drive the operation and return only whether it failed)
        let mut ruled_on = 0usize;
        for label in ["query", "execute"] {
            let (sender, mut receiver) = mpsc::unbounded();
            let client = Client::new_with_statement_cache(
                sender,
                SslMode::Disable,
                SslNegotiation::Postgres,
                0,
                Some(0.into()),
                None,
                ProtocolVersion::V3_0,
                crate::client::StatementCacheSettings::new(1, NonZeroUsize::MIN),
            );
            let inner = Arc::clone(client.inner());
            let sql = format!("SELECT {label}");
            let statement =
                Statement::new(&inner, format!("s_stale_{label}"), vec![], vec![], false);
            let statement = inner.cache_statement(&sql, statement, inner.type_cache_generation());
            assert!(inner.cached_statement(&sql).is_some(), "{label} fixture");

            let respond = async {
                let mut request = receiver
                    .next()
                    .await
                    .unwrap_or_else(|| panic!("{label} enqueued no request"));
                let body =
                    b"SERROR\0C26000\0Mscripted stale statement\0RFetchPreparedStatement\0\0";
                let mut frame = vec![b'E'];
                frame.extend_from_slice(&(u32::try_from(body.len()).unwrap() + 4).to_be_bytes());
                frame.extend_from_slice(body);
                request
                    .sender
                    .try_send(crate::client::ResponseMessages::Raw(
                        BackendMessages::from_test_bytes(BytesMut::from(frame.as_slice())),
                    ))
                    .unwrap_or_else(|_| panic!("{label} could not receive its stale error"));
            };

            if label == "query" {
                let run = super::query_inner::<&(dyn crate::types::ToSql + Sync), _>(
                    &inner,
                    statement,
                    crate::slice_iter(&[]),
                );
                let (result, ()) = futures_util::future::join(run, respond).await;
                assert!(result.is_err(), "{label} accepted a stale cached statement");
            } else {
                let run = super::execute_inner::<&(dyn crate::types::ToSql + Sync), _>(
                    &inner,
                    statement,
                    crate::slice_iter(&[]),
                );
                let (result, ()) = futures_util::future::join(run, respond).await;
                assert!(result.is_err(), "{label} accepted a stale cached statement");
            }

            assert!(
                inner.cached_statement(&sql).is_none(),
                "{label} left its stale statement cached"
            );
            ruled_on += 1;
        }
        assert_eq!(ruled_on, 2, "both start paths must be ruled on");
    }

    fn test_client() -> (Client, mpsc::UnboundedReceiver<crate::connection::Request>) {
        let (sender, receiver) = mpsc::unbounded();
        let client = Client::new(
            sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        (client, receiver)
    }

    fn scripted_backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(body.len() + 5);
        frame.push(tag);
        frame.extend_from_slice(&(u32::try_from(body.len()).unwrap() + 4).to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    /// `execute` classified every `start` failure as `BeforeBindComplete` - the
    /// phase documented as PostgreSQL rejecting the named statement, and the
    /// one that gates the stale-cache replay. A copy response standing where
    /// `BindComplete` was due means the server is already in copy mode, so it
    /// is past Bind and cannot be a rejected statement.
    ///
    /// Unreachable today: the replay also requires
    /// `cached_statement_error_is_stale`, which `unexpected_message` fails.
    /// This pins the phase so the contract does not rest on that second gate.
    #[compio::test]
    async fn a_copy_response_in_the_bind_slot_is_not_a_rejected_statement() {
        use crate::client::ResponseMessages;
        use futures_util::StreamExt;
        use postgres_protocol::message::backend::Message;
        use std::collections::VecDeque;

        let (client, mut requests) = test_client();
        let statement = Statement::unnamed(Vec::new(), Vec::new());
        let run = super::execute_inner(client.inner(), statement, crate::slice_iter(&[]));
        let connection = async move {
            let crate::connection::Request {
                sender: mut response_sender,
                ..
            } = requests.next().await.expect("execute enqueued no request");
            // 'G' CopyInResponse: Int8 overall format, Int16 column count.
            let mut frame = BytesMut::from(&scripted_backend_frame(b'G', &[0, 0, 0])[..]);
            let message = Message::parse(&mut frame)
                .expect("parse the scripted CopyInResponse")
                .expect("the scripted CopyInResponse was incomplete");
            response_sender
                .try_send(ResponseMessages::Observed(VecDeque::from([Ok(message)])))
                .expect("deliver the scripted CopyInResponse");
        };

        let (result, ()) = futures_util::future::join(run, connection).await;
        let failure = match result {
            Ok(_) => panic!("a copy response in the Bind slot completed execute"),
            Err(failure) => failure,
        };
        let (_, before_bind_complete) = failure.into_parts();
        assert!(
            !before_bind_complete,
            "a copy response in the Bind slot was classified as a rejected statement"
        );
    }

    /// The row stream refuses a frame its own poll does not name.
    ///
    /// `RowStream::poll_next` names nine message kinds and errors on anything
    /// else, and that arm had never run. It is a different guard from the one
    /// in `query_typed`: this one runs AFTER the caller already holds a
    /// stream, so accepting a stray frame would surface it as rows rather
    /// than as an error, or silently swallow it and keep polling.
    ///
    /// `NoData` gets `query_typed` to hand back the stream; `BindComplete` is
    /// then the stray - legal, well framed, and not one of the nine.
    #[compio::test]
    async fn the_row_stream_refuses_a_frame_its_poll_does_not_name() {
        use futures_util::TryStreamExt;

        let mut frames = scripted_backend_frame(b'n', b"");
        frames.extend_from_slice(&scripted_backend_frame(b'2', b""));

        let error = typed_against(frames, |inner| async move {
            let stream =
                super::query_typed(&inner, "SELECT 1", std::iter::empty::<(i32, Type)>()).await?;
            let mut stream = Box::pin(stream);
            while stream.as_mut().try_next().await?.is_some() {}
            Ok(())
        })
        .await
        .expect_err("the row stream accepted a frame its poll does not name");
        assert!(
            format!("{error}").contains("unexpected message from server"),
            "the row stream reported {error} rather than an out-of-order message"
        );
    }

    /// The one-variable control: the same stream ended by a frame it does name.
    #[compio::test]
    async fn the_row_stream_accepts_ready_for_query_as_its_end() {
        use futures_util::TryStreamExt;

        let mut frames = scripted_backend_frame(b'n', b"");
        frames.extend_from_slice(&scripted_backend_frame(b'Z', b"I"));

        typed_against(frames, |inner| async move {
            let stream =
                super::query_typed(&inner, "SELECT 1", std::iter::empty::<(i32, Type)>()).await?;
            let mut stream = Box::pin(stream);
            let mut rows = 0;
            while stream.as_mut().try_next().await?.is_some() {
                rows += 1;
            }
            assert_eq!(rows, 0, "no DataRow was sent, so no rows arrive");
            Ok(())
        })
        .await
        .expect("the row stream rejected a well-formed ReadyForQuery");
    }

    /// Answer one typed-query call with a scripted frame.
    ///
    /// Both entry points send their whole Parse/Bind/Describe/Execute/Sync
    /// batch and then read, so a single frame lands on the first arm of the
    /// loop with nothing consumed before it.
    async fn typed_against<T, F, Fut>(frame: Vec<u8>, call: F) -> Result<T, crate::Error>
    where
        F: FnOnce(std::sync::Arc<crate::client::InnerClient>) -> Fut,
        Fut: std::future::Future<Output = Result<T, crate::Error>>,
    {
        use crate::client::ResponseMessages;
        use crate::codec::BackendMessages;
        use futures_util::StreamExt;
        use std::sync::Arc;

        let (client, mut receiver) = test_client();
        let inner = Arc::clone(client.inner());
        let call = call(Arc::clone(&inner));
        let respond = async {
            let mut request = receiver
                .next()
                .await
                .expect("the typed query did not enqueue its request");
            request
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    BytesMut::from(&frame[..]),
                )))
                .expect("deliver the scripted typed-query response");
        };
        let (result, ()) = futures_util::join!(call, respond);
        result
    }

    /// `query_typed` and `execute_typed` both refuse a frame their loop does
    /// not name.
    ///
    /// Both are public, both drive the extended protocol themselves, and both
    /// refusal arms were uncovered. `PortalSuspended` is the payload: a real
    /// extended-query message, correctly framed, that neither loop expects -
    /// so accepting it would mean continuing to read a stream whose position
    /// the driver no longer understands.
    #[compio::test]
    async fn typed_queries_refuse_a_frame_their_loop_does_not_name() {
        let suspended = scripted_backend_frame(b's', b"");

        let error = typed_against(suspended.clone(), |inner| async move {
            super::query_typed(&inner, "SELECT 1", std::iter::empty::<(i32, Type)>())
                .await
                .map(|_| ())
        })
        .await
        .expect_err("query_typed accepted a frame its loop does not name");
        assert!(
            format!("{error}").contains("unexpected message from server"),
            "query_typed reported {error} rather than an out-of-order message"
        );

        let error = typed_against(suspended, |inner| async move {
            super::execute_typed(&inner, "SELECT 1", std::iter::empty::<(i32, Type)>())
                .await
                .map(|_| ())
        })
        .await
        .expect_err("execute_typed accepted a frame its loop does not name");
        assert!(
            format!("{error}").contains("unexpected message from server"),
            "execute_typed reported {error} rather than an out-of-order message"
        );
    }

    /// The one-variable control: each entry point on a frame it does name.
    #[compio::test]
    async fn typed_queries_accept_the_frames_they_name() {
        typed_against(scripted_backend_frame(b'n', b""), |inner| async move {
            super::query_typed(&inner, "SELECT 1", std::iter::empty::<(i32, Type)>())
                .await
                .map(|_| ())
        })
        .await
        .expect("query_typed rejected a well-formed NoData");

        let rows = typed_against(scripted_backend_frame(b'Z', b"I"), |inner| async move {
            super::execute_typed(&inner, "SELECT 1", std::iter::empty::<(i32, Type)>()).await
        })
        .await
        .expect("execute_typed rejected a well-formed ReadyForQuery");
        assert_eq!(
            rows, 0,
            "no CommandComplete was sent, so no rows are counted"
        );
    }

    /// Drive `start` against one scripted backend frame.
    async fn start_against(frame: Vec<u8>) -> Result<(), crate::Error> {
        use crate::client::ResponseMessages;
        use crate::codec::BackendMessages;
        use futures_util::StreamExt;
        use std::sync::Arc;

        let (client, mut receiver) = test_client();
        let inner = Arc::clone(client.inner());
        let statement = Statement::new(&inner, "s_start".to_string(), vec![], vec![], false);
        let start = super::start(
            &inner,
            bytes::Bytes::from_static(b"B\0\0\0\x04"),
            &statement,
        );
        let respond = async {
            let mut request = receiver
                .next()
                .await
                .expect("start did not enqueue its request");
            request
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    BytesMut::from(&frame[..]),
                )))
                .expect("deliver the scripted start response");
        };
        let (result, ()) = futures_util::join!(start, respond);
        result.map(|_| ()).map_err(ExecutionError::into_error)
    }

    /// `start` refuses anything but `BindComplete` as the first reply.
    ///
    /// It is the shared prelude of every prepared-statement query path, and
    /// its refusal arm had never run. Accepting a different frame would hand
    /// the caller a `Responses` positioned mid-exchange, so the rows that
    /// follow belong to a step the caller never asked for.
    ///
    /// `ParseComplete` is the payload: legal, well framed, and owed earlier in
    /// a different exchange rather than here.
    #[compio::test]
    async fn start_refuses_a_first_reply_that_is_not_bind_complete() {
        let error = start_against(scripted_backend_frame(b'1', b""))
            .await
            .expect_err("start accepted a frame that was not BindComplete");
        let rendered = format!("{error}");
        assert!(
            rendered.contains("unexpected message from server"),
            "start reported {rendered:?} rather than an out-of-order message"
        );
        assert!(
            error.code().is_none(),
            "start reported a server SQLSTATE, so the refusal was not local"
        );
    }

    /// The one-variable control: the same path with the frame it is owed.
    #[compio::test]
    async fn start_accepts_bind_complete() {
        start_against(scripted_backend_frame(b'2', b""))
            .await
            .expect("start rejected a well-formed BindComplete");
    }

    /// Drive `sync` against one scripted backend frame.
    async fn sync_against(frame: Vec<u8>) -> Result<(), crate::Error> {
        use crate::client::ResponseMessages;
        use crate::codec::BackendMessages;
        use futures_util::StreamExt;
        use std::sync::Arc;

        let (client, mut receiver) = test_client();
        let inner = Arc::clone(client.inner());
        let sync = super::sync(&inner);
        let respond = async {
            let mut request = receiver
                .next()
                .await
                .expect("sync did not enqueue its Sync request");
            request
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    BytesMut::from(&frame[..]),
                )))
                .expect("deliver the scripted sync response");
        };
        let (result, ()) = futures_util::join!(sync, respond);
        result
    }

    /// `Client::check_connection` must not call a desynchronised session
    /// healthy.
    ///
    /// `sync` is the whole body of that public method: it sends a bare Sync
    /// and accepts only `ReadyForQuery`. The refusal arm had never run, and it
    /// is the one that matters - a health check reporting `Ok` after the peer
    /// answered with something else tells the caller the connection is usable
    /// when the next request will find the stream out of step.
    ///
    /// `BindComplete` is the payload because it is well framed and legal in
    /// its own right; only its position is wrong.
    #[compio::test]
    async fn check_connection_refuses_a_reply_that_is_not_ready_for_query() {
        let error = sync_against(scripted_backend_frame(b'2', b""))
            .await
            .expect_err("sync accepted a frame that was not ReadyForQuery");
        let rendered = format!("{error}");
        assert!(
            rendered.contains("unexpected message from server"),
            "sync reported {rendered:?} rather than an out-of-order message"
        );
        assert!(
            error.code().is_none(),
            "sync reported a server SQLSTATE, so the refusal was not local"
        );
    }

    /// The one-variable control: the same path with the frame it is owed.
    /// Without it the refusal above also passes for a `sync` that never
    /// succeeds at all.
    #[compio::test]
    async fn check_connection_accepts_ready_for_query() {
        sync_against(scripted_backend_frame(b'Z', b"I"))
            .await
            .expect("sync rejected a well-formed ReadyForQuery");
    }

    fn frontend_frames(mut batch: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut frames = Vec::new();
        while !batch.is_empty() {
            assert!(batch.len() >= 5, "frontend frame is missing its header");
            let body_len = u32::from_be_bytes(batch[1..5].try_into().unwrap()) as usize;
            assert!(
                body_len >= 4,
                "frontend frame length excludes its own header"
            );
            let frame_len = body_len + 1;
            assert!(
                frame_len <= batch.len(),
                "frontend frame length exceeds the captured batch"
            );
            frames.push((batch[0], batch[5..frame_len].to_vec()));
            batch = &batch[frame_len..];
        }
        frames
    }

    #[test]
    fn encode_bind_rejects_too_many_parameters_before_writing_bind() {
        let statement = Statement::unnamed(vec![Type::INT4], vec![]);
        let mut buf = BytesMut::from(&b"sentinel"[..]);

        let error = encode_bind(&statement, [1_i32, 2_i32], "", &mut buf).unwrap_err();

        assert_eq!(error.to_string(), "expected 1 parameters but got 2");
        assert_eq!(&buf[..], b"sentinel");
    }

    #[test]
    fn encode_bind_rejects_too_few_parameters_before_writing_bind() {
        let statement = Statement::unnamed(vec![Type::INT4], vec![]);
        let mut buf = BytesMut::from(&b"sentinel"[..]);

        let error = encode_bind(&statement, std::iter::empty::<i32>(), "", &mut buf).unwrap_err();

        assert_eq!(error.to_string(), "expected 1 parameters but got 0");
        assert_eq!(&buf[..], b"sentinel");
    }

    #[test]
    fn text_query_batches_parse_bind_portal_describe_unlimited_execute_and_sync() {
        let (client, mut requests) = test_client();
        let params = ["42"];
        let mut query = Box::pin(query_text_params(
            client.inner(),
            "SELECT $1::int4",
            &params,
        ));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(matches!(query.as_mut().poll(&mut context), Poll::Pending));
        let request = requests
            .try_recv()
            .expect("text query did not enqueue its frontend batch");
        let RequestMessages::Single(FrontendMessage::Raw(bytes)) = request.messages else {
            panic!("text query was not encoded as one raw frontend batch");
        };

        let frames = frontend_frames(&bytes);
        assert_eq!(
            frames.iter().map(|(tag, _)| *tag).collect::<Vec<_>>(),
            b"PBDES"
        );
        assert_eq!(frames[2].1, b"P\0", "Describe must target the portal");
        assert_eq!(
            frames[3].1, b"\0\0\0\0\0",
            "Execute must name the unnamed portal and request unlimited rows"
        );
    }

    #[derive(Debug)]
    struct RefusesConversion;

    impl ToSql for RefusesConversion {
        fn to_sql(
            &self,
            _: &Type,
            _: &mut BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            Err(std::io::Error::other("sentinel conversion").into())
        }

        fn accepts(_: &Type) -> bool {
            true
        }

        to_sql_checked!();
    }

    #[test]
    fn bind_conversion_names_the_failing_parameter_and_preserves_its_source() {
        let statement = Statement::unnamed(vec![Type::INT4, Type::TEXT], vec![]);
        let first = 1_i32;
        let second = RefusesConversion;
        let params: [&(dyn ToSql + Sync); 2] = [&first, &second];
        let mut buf = BytesMut::new();

        let error = encode_bind(&statement, params, "", &mut buf).unwrap_err();

        assert_eq!(error.to_string(), "error serializing parameter 1");
        assert_eq!(
            error
                .source()
                .expect("conversion error lost its source")
                .to_string(),
            "sentinel conversion"
        );
    }

    #[test]
    fn bind_protocol_serialization_is_an_encode_error() {
        const TOO_MANY: usize = u16::MAX as usize + 1;
        let statement = Statement::unnamed(vec![Type::INT4; TOO_MANY], vec![]);
        let params = vec![None::<i32>; TOO_MANY];
        let mut buf = BytesMut::new();

        let error = encode_bind(&statement, params, "", &mut buf).unwrap_err();

        assert_eq!(error.to_string(), "error encoding message to server");
    }

    #[test]
    fn execute_text_bind_serialization_preserves_its_encode_error_source() {
        let error = map_execute_text_bind_error(
            postgres_protocol::message::frontend::BindError::Serialization(std::io::Error::other(
                "sentinel serialization",
            )),
        );

        assert_eq!(error.to_string(), "error encoding message to server");
        assert_eq!(
            error
                .source()
                .expect("serialization error lost its source")
                .to_string(),
            "sentinel serialization"
        );
    }
}
