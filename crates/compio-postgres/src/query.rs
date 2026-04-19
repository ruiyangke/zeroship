// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Phase 4 port: near-verbatim from tokio-postgres. The Bind/Execute state
// machine, parameter encoding, and `RowStream` implementation are purely
// protocol-level, so only the imports change. `pin_project_lite` is used
// identically; the `InnerClient::send` primitive is Phase 3's real impl.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::prepare::get_type;
use crate::types::{BorrowToSql, IsNull};
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
    client: &InnerClient,
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
    let responses = start(client, buf).await?;
    Ok(RowStream {
        statement,
        responses,
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
        frontend::parse("", query, std::iter::empty::<u32>(), buf).map_err(Error::parse)?;

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
                    statement: Statement::unnamed(vec![], vec![]),
                    responses,
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
                    statement: Statement::unnamed(vec![], columns),
                    responses,
                    rows_affected: None,
                });
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
            frontend::parse("", query, param_oids, buf).map_err(Error::parse)?;
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
                    statement: Statement::unnamed(vec![], vec![]),
                    responses,
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
                    statement: Statement::unnamed(vec![], columns),
                    responses,
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
            frontend::parse("", query, param_oids, buf).map_err(Error::parse)?;
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
            Message::ReadyForQuery(_) => return Ok(rows),
            _ => {
                return Err(Error::unexpected_message());
            }
        }
    }
}

#[allow(dead_code)]
pub async fn query_portal(
    client: &InnerClient,
    portal: &Portal,
    max_rows: i32,
) -> Result<RowStream, Error> {
    let buf = client.with_buf(|buf| {
        frontend::execute(portal.name(), max_rows, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    Ok(RowStream {
        statement: portal.statement().clone(),
        responses,
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
    client: &InnerClient,
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
    let mut responses = start(client, buf).await?;

    let mut rows = 0;
    loop {
        match responses.next().await? {
            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                rows = extract_row_affected(&body)?;
            }
            Message::EmptyQueryResponse => rows = 0,
            Message::ReadyForQuery(_) => return Ok(rows),
            _ => return Err(Error::unexpected_message()),
        }
    }
}

async fn start(client: &InnerClient, buf: Bytes) -> Result<Responses, Error> {
    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

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
        |(idx, (param, ty)), buf| match param.borrow_to_sql().to_sql_checked(&ty, buf) {
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
    }
}

impl Stream for RowStream {
    type Item = Result<Row, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        loop {
            match ready!(this.responses.poll_next(cx)?) {
                Message::DataRow(body) => {
                    return Poll::Ready(Some(Ok(Row::new(this.statement.clone(), body)?)));
                }
                Message::CommandComplete(body) => {
                    *this.rows_affected = Some(extract_row_affected(&body)?);
                }
                Message::EmptyQueryResponse | Message::PortalSuspended => {}
                Message::ReadyForQuery(_) => return Poll::Ready(None),
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
