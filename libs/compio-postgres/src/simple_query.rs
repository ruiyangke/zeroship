// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Verbatim translation. The simple query protocol is a single `Query`
// frontend message producing a mixed stream of RowDescription / DataRow /
// CommandComplete terminated by ReadyForQuery.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::query::extract_row_affected;
use crate::{Error, SimpleQueryMessage, SimpleQueryRow};
use bytes::Bytes;
use fallible_iterator::FallibleIterator;
use futures_util::Stream;
use log::debug;
use pin_project_lite::pin_project;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

/// Information about a column of a single query row.
#[derive(Debug)]
pub struct SimpleColumn {
    name: String,
}

impl SimpleColumn {
    pub(crate) fn new(name: String) -> SimpleColumn {
        SimpleColumn { name }
    }

    /// Returns the name of the column.
    pub fn name(&self) -> &str {
        &self.name
    }
}

pub async fn simple_query(client: &InnerClient, query: &str) -> Result<SimpleQueryStream, Error> {
    debug!("executing simple query: {query}");

    let buf = encode(client, query)?;
    let responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    Ok(SimpleQueryStream {
        responses,
        columns: None,
    })
}

pub async fn batch_execute(client: &InnerClient, query: &str) -> Result<(), Error> {
    let responses = start_batch_execute(client, query)?;
    finish_batch_execute(responses).await
}

/// Enqueue the batch and hand back its response stream, without awaiting it.
///
/// Split out of `batch_execute` so a caller can arm a cleanup guard around
/// exactly the window where one is owed: after the request reaches the
/// connection, and before its response has been consumed. Everything this
/// function does before `send` - logging, encoding - can fail or unwind while
/// the session is still untouched, and a guard armed across it would undo work
/// the caller never did.
pub(crate) fn start_batch_execute(
    client: &InnerClient,
    query: &str,
) -> Result<Responses, Error> {
    debug!("executing statement batch: {query}");

    let buf = encode(client, query)?;
    client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))
}

pub(crate) fn start_batch_execute_with_error_cleanup(
    client: &InnerClient,
    query: &str,
    cleanup: &str,
) -> Result<Responses, Error> {
    debug!("executing statement batch: {query}");

    let query = encode(client, query)?;
    let cleanup = encode(client, cleanup)?;
    client.send_with_error_cleanup(FrontendMessage::Raw(query), FrontendMessage::Raw(cleanup))
}

/// Drain the response stream `start_batch_execute` returned.
pub(crate) async fn finish_batch_execute(mut responses: Responses) -> Result<(), Error> {
    loop {
        match responses.next().await? {
            Message::ReadyForQuery(_) => return Ok(()),
            Message::CommandComplete(_)
            | Message::EmptyQueryResponse
            | Message::RowDescription(_)
            | Message::DataRow(_) => {}
            _ => return Err(Error::unexpected_message()),
        }
    }
}

/// Drain the stream and report the command tag of the LAST statement in the
/// batch that completed.
///
/// The tag is the only place PostgreSQL says WHICH command it decided it had
/// run, and for one statement that answer is not the one that was sent:
/// `COMMIT` inside an aborted transaction block is executed, discards every
/// change, and completes with the tag `ROLLBACK` and no `ErrorResponse`.
/// [`finish_batch_execute`] drops tags, so a caller draining through it cannot
/// tell that outcome from a successful commit. libpq exposes it as
/// `PQcmdStatus`; this is the equivalent for the one caller that needs it.
///
/// `None` means the batch completed without any `CommandComplete` - an empty
/// query.
pub(crate) async fn finish_batch_execute_reporting_tag(
    mut responses: Responses,
) -> Result<Option<String>, Error> {
    let mut tag = None;
    loop {
        match responses.next().await? {
            Message::ReadyForQuery(_) => return Ok(tag),
            Message::CommandComplete(body) => {
                tag = Some(body.tag().map_err(Error::parse)?.to_string());
            }
            Message::EmptyQueryResponse | Message::RowDescription(_) | Message::DataRow(_) => {}
            _ => return Err(Error::unexpected_message()),
        }
    }
}

fn encode(client: &InnerClient, query: &str) -> Result<Bytes, Error> {
    client.with_buf(|buf| {
        frontend::query(query, buf).map_err(Error::encode)?;
        Ok(buf.split().freeze())
    })
}

pin_project! {
    /// A stream of simple query results.
    #[project(!Unpin)]
    pub struct SimpleQueryStream {
        responses: Responses,
        columns: Option<Arc<[SimpleColumn]>>,
    }
}

impl Stream for SimpleQueryStream {
    type Item = Result<SimpleQueryMessage, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match ready!(this.responses.poll_next(cx)?) {
            Message::CommandComplete(body) => {
                let rows = extract_row_affected(&body)?;
                Poll::Ready(Some(Ok(SimpleQueryMessage::CommandComplete(rows))))
            }
            Message::EmptyQueryResponse => {
                Poll::Ready(Some(Ok(SimpleQueryMessage::CommandComplete(0))))
            }
            Message::RowDescription(body) => {
                let columns: Arc<[SimpleColumn]> = body
                    .fields()
                    .map(|f| Ok(SimpleColumn::new(f.name().to_string())))
                    .collect::<Vec<_>>()
                    .map_err(Error::parse)?
                    .into();

                *this.columns = Some(columns.clone());
                Poll::Ready(Some(Ok(SimpleQueryMessage::RowDescription(columns))))
            }
            Message::DataRow(body) => {
                let row = match &this.columns {
                    Some(columns) => SimpleQueryRow::new(columns.clone(), body)?,
                    None => return Poll::Ready(Some(Err(Error::unexpected_message()))),
                };
                Poll::Ready(Some(Ok(SimpleQueryMessage::Row(row))))
            }
            Message::ReadyForQuery(_) => Poll::Ready(None),
            _ => Poll::Ready(Some(Err(Error::unexpected_message()))),
        }
    }
}
