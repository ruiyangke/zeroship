// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Near-verbatim from tokio-postgres. The Bind/Execute state machine,
// parameter encoding, and `RowStream` implementation are purely
// protocol-level, so only the imports change. `pin_project_lite` is used
// identically, and `InnerClient::send` is the real request path.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
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
use postgres_protocol::message::backend::{CommandCompleteBody, Message};
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
        Err(error) => {
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
    let mut rows = 0;
    let mut copy_out_refused = false;
    loop {
        match responses.next().await? {
            Message::ParseComplete
            | Message::BindComplete
            | Message::ParameterDescription(_)
            | Message::RowDescription(_) => {}
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

#[allow(dead_code)]
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

/// Extract the number of rows affected from [`CommandCompleteBody`].
pub fn extract_row_affected(body: &CommandCompleteBody) -> Result<u64, Error> {
    let rows = body
        .tag()
        .map_err(Error::parse)?
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap_or(0);
    Ok(rows)
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
        Err(error) => {
            statement.invalidate_cache_on_error(&error);
            return Err(ExecutionError::before_bind_complete(error));
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
) -> Result<Responses, Error> {
    let mut responses = client.send_statement(
        producerless_request(buf, statement.may_enter_copy_in()),
        statement,
    )?;

    match responses.next().await? {
        Message::BindComplete => {}
        _ => return Err(Error::unexpected_message()),
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
/// deleting it leaves `tests/domain_parameters.rs` green. `to_sql_checked`
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

#[allow(dead_code)]
pub async fn sync(client: &InnerClient) -> Result<(), Error> {
    let buf = Bytes::from_static(b"S\0\0\0\x04");
    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    match responses.next().await? {
        Message::ReadyForQuery(_) => Ok(()),
        _ => Err(Error::unexpected_message()),
    }
}
