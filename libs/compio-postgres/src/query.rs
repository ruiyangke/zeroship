// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Near-verbatim from tokio-postgres. The Bind/Execute state machine,
// parameter encoding, and `RowStream` implementation are purely
// protocol-level, so only the imports change. `pin_project_lite` is used
// identically, and `InnerClient::send` is the real request path.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::prepare::get_type;
use crate::simple_query::{CopyAbortProtocol, abort_copy_in};
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
use std::sync::{Arc, Weak};
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
    query_inner(client, statement, params, false).await
}

/// Start a cache-hit query, retaining its first execution message until the
/// returned RowStream is polled. Waiting for this one message keeps a 26000
/// raised during Execute inside the retryable call without allowing any row
/// to escape before the decision.
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
    query_inner(client, statement, params, true).await
}

async fn query_inner<P, I>(
    client: &Arc<InnerClient>,
    statement: Statement,
    params: I,
    prefetch_first: bool,
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
    let mut responses = match start(client, buf, &statement).await {
        Ok(responses) => responses,
        Err(error) => {
            statement.invalidate_cache_on_error(&error);
            return Err(error);
        }
    };
    let pending = if prefetch_first {
        match responses.next().await {
            Ok(message) => Some(message),
            Err(error) => {
                statement.invalidate_cache_on_error(&error);
                return Err(error);
            }
        }
    } else {
        None
    };
    Ok(RowStream {
        client: Arc::downgrade(client),
        statement,
        responses,
        pending,
        rows_affected: None,
    })
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
        // Parse with no type hints — server infers.
        frontend::parse("", query, std::iter::empty::<u32>(), buf).map_err(Error::encode)?;

        // Bind: text format for parameters (code 0), binary format for results (code 1).
        let param_refs: Vec<Option<&[u8]>> =
            params.iter().map(|s| Some(s.as_bytes())).collect();
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

    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    loop {
        match responses.next().await? {
            Message::ParseComplete | Message::BindComplete | Message::ParameterDescription(_) => {}
            Message::NoData => {
                return Ok(RowStream {
                    client: Arc::downgrade(client),
                    statement: Statement::unnamed(vec![], vec![]),
                    responses,
                    pending: None,
                    rows_affected: None,
                });
            }
            Message::RowDescription(row_description) => {
                let mut columns: Vec<Column> = vec![];
                let mut it = row_description.fields();
                while let Some(field) = it.next().map_err(Error::parse)? {
                    let type_ = get_type(client, field.type_oid()).await?;
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
                    client: Arc::downgrade(client),
                    statement: Statement::unnamed(vec![], columns),
                    responses,
                    pending: None,
                    rows_affected: None,
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
/// JSON-query-builder coercion model — a value passed as text is implicit-cast to
/// the target column type (`'2026-01-01'` → `timestamptz`, `'1.5'` → `numeric`),
/// which the typed-binary `execute_typed` path cannot do for a cross-type bind.
/// A `None` param is a SQL NULL (sent with no bytes). The op.* DML executor uses
/// this so a creator `insert`/`update` value coerces to the column type without
/// the assembler knowing the schema (names-are-strings, §3.3).
pub async fn execute_text_params(
    client: &Arc<InnerClient>,
    query: &str,
    params: &[Option<String>],
) -> Result<u64, Error> {
    let buf = client.with_buf(|buf| {
        frontend::parse("", query, std::iter::empty::<u32>(), buf).map_err(Error::encode)?;
        let param_refs: Vec<Option<&[u8]>> =
            params.iter().map(|s| s.as_ref().map(|v| v.as_bytes())).collect();
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

    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;
    let mut rows = 0;
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
            // Not `unexpected_message`: walking away here leaves the SESSION
            // in copy mode, and the `Sync` this request already sent was
            // ignored while it is. See `simple_query::abort_copy_in` - this is
            // the extended-protocol peer of the arm it documents.
            Message::CopyInResponse(_) => abort_copy_in(client, CopyAbortProtocol::Extended)?,
            Message::ReadyForQuery(_) => return Ok(rows),
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

    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    loop {
        match responses.next().await? {
            Message::ParseComplete | Message::BindComplete | Message::ParameterDescription(_) => {}
            Message::NoData => {
                return Ok(RowStream {
                    client: Arc::downgrade(client),
                    statement: Statement::unnamed(vec![], vec![]),
                    responses,
                    pending: None,
                    rows_affected: None,
                });
            }
            Message::RowDescription(row_description) => {
                let mut columns: Vec<Column> = vec![];
                let mut it = row_description.fields();
                while let Some(field) = it.next().map_err(Error::parse)? {
                    let type_ = get_type(client, field.type_oid()).await?;
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
                    client: Arc::downgrade(client),
                    statement: Statement::unnamed(vec![], columns),
                    responses,
                    pending: None,
                    rows_affected: None,
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

    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    let mut rows = 0;

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
            // Not `unexpected_message`: walking away here leaves the SESSION
            // in copy mode, and the `Sync` this request already sent was
            // ignored while it is. See `simple_query::abort_copy_in` - this is
            // the extended-protocol peer of the arm it documents.
            Message::CopyInResponse(_) => abort_copy_in(client, CopyAbortProtocol::Extended)?,
            Message::ReadyForQuery(_) => return Ok(rows),
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
    let buf = client.with_buf(|buf| {
        frontend::execute(portal.name(), max_rows, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let responses = client.send_statement(
        RequestMessages::Single(FrontendMessage::Raw(buf)),
        portal.statement(),
    )?;

    Ok(RowStream {
        client: Arc::downgrade(client),
        statement: portal.statement().clone(),
        responses,
        pending: None,
        rows_affected: None,
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
    let mut responses = match start(client, buf, &statement).await {
        Ok(responses) => responses,
        Err(error) => {
            statement.invalidate_cache_on_error(&error);
            return Err(error);
        }
    };

    let mut rows = 0;
    loop {
        match responses.next().await? {
            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                rows = extract_row_affected(&body)?;
            }
            Message::EmptyQueryResponse => rows = 0,
            // Not `unexpected_message`: walking away here leaves the SESSION
            // in copy mode, and the `Sync` this request already sent was
            // ignored while it is. See `simple_query::abort_copy_in` - this is
            // the extended-protocol peer of the arm it documents.
            Message::CopyInResponse(_) => abort_copy_in(client, CopyAbortProtocol::Extended)?,
            Message::ReadyForQuery(_) => return Ok(rows),
            _ => return Err(Error::unexpected_message()),
        }
    }
}

async fn start(
    client: &InnerClient,
    buf: Bytes,
    statement: &Statement,
) -> Result<Responses, Error> {
    let mut responses = client.send_statement(
        RequestMessages::Single(FrontendMessage::Raw(buf)),
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

    // Walk the whole chain: a domain may be defined over another domain.
    let mut base = ty;
    while let Kind::Domain(inner) = base.kind() {
        base = inner;
    }
    if std::ptr::eq(base, ty) {
        return first;
    }

    buf.truncate(checkpoint);
    param.to_sql_checked(base, buf)
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
        // Held so the stream can end a copy this path cannot feed
        // (`simple_query::abort_copy_in`). WEAK for the same reason
        // `SimpleQueryStream` holds a weak handle: `InnerClient` owns the
        // request channel and `Connection::run` finishes only once every
        // sender is gone, so a strong handle would keep a connection alive
        // for as long as a caller held the stream.
        client: Weak<InnerClient>,
        statement: Statement,
        responses: Responses,
        pending: Option<Message>,
        rows_affected: Option<u64>,
    }
}

impl Stream for RowStream {
    type Item = Result<Row, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        loop {
            let message = match this.pending.take() {
                Some(message) => message,
                None => match ready!(this.responses.poll_next(cx)) {
                    Ok(message) => message,
                    Err(error) => {
                        this.statement.invalidate_cache_on_error(&error);
                        return Poll::Ready(Some(Err(error)));
                    }
                },
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
                Message::ReadyForQuery(_) => return Poll::Ready(None),
                // This is where a `COPY ... FROM STDIN` sent through `query`
                // lands: `start` has already consumed `BindComplete` and the
                // `Describe` answered before the `Execute` that entered copy
                // mode. Walking away leaves the SESSION there, and the `Sync`
                // this request already sent was ignored while it is. See
                // `simple_query::abort_copy_in`.
                Message::CopyInResponse(_) => match this.client.upgrade() {
                    Some(client) => {
                        if let Err(error) = abort_copy_in(&client, CopyAbortProtocol::Extended) {
                            return Poll::Ready(Some(Err(error)));
                        }
                    }
                    None => return Poll::Ready(Some(Err(Error::closed()))),
                },
                _ => return Poll::Ready(Some(Err(Error::unexpected_message()))),
            }
        }
    }
}

impl RowStream {
    /// Returns the number of rows affected by the query.
    ///
    /// This function will return `None` until the stream has been exhausted.
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
