// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Near-verbatim translation. The simple query protocol is a single `Query`
// frontend message producing a mixed stream of RowDescription / DataRow /
// CommandComplete terminated by ReadyForQuery.
//
// ONE DELIBERATE DIVERGENCE from upstream: a `CopyInResponse` is answered with
// `CopyFail` rather than reported as an unexpected message and abandoned.
// Upstream abandons it, and abandoning it leaves the SESSION in copy mode,
// which costs the connection. See `abort_copy_in`.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::{RequestDisposition, RequestMessages, TransactionEffect};
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
use std::sync::{Arc, Weak};
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

pub async fn simple_query(
    client: &Arc<InnerClient>,
    query: &str,
) -> Result<SimpleQueryStream, Error> {
    debug!("executing simple query: {query}");

    let buf = encode(client, query)?;
    let responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    Ok(SimpleQueryStream {
        client: Arc::downgrade(client),
        responses,
        columns: None,
    })
}

pub async fn batch_execute(client: &InnerClient, query: &str) -> Result<(), Error> {
    let responses = start_batch_execute(client, query)?;
    finish_batch_execute(client, responses).await
}

/// The reason this driver gives PostgreSQL for aborting a copy it cannot feed.
///
/// It travels to the caller inside the server's own `COPY from stdin failed:`
/// error, which is what makes the failure attributable: an abandoned
/// [`crate::CopyInSink`] aborts the identical statement with an EMPTY reason,
/// so the server prefix alone cannot tell the two apart.
const COPY_IN_UNSUPPORTED: &str = "simple query execution cannot supply COPY data; use copy_in";

/// The reason the EXTENDED-protocol drain gives for the same abort.
///
/// Spelled differently on purpose: it is the only thing that tells a caller
/// WHICH entry point could not feed the copy, and both paths reach the same
/// `COPY from stdin failed:` prefix.
pub(crate) const COPY_IN_UNSUPPORTED_EXTENDED: &str =
    "extended query execution cannot supply COPY data; use copy_in";

/// Which protocol started the copy being aborted.
///
/// It decides how many `ReadyForQuery` frames the abort earns, which is why
/// the two paths cannot share one frame list. In SIMPLE query mode PostgreSQL
/// answers `CopyFail` with `ErrorResponse` AND its own `ReadyForQuery`. Under
/// an EXTENDED-protocol copy the same error sets `ignore_till_sync` instead
/// and no terminator is released until a `Sync`. The caller's response is
/// still at the head of the queue and consumes the first terminator either
/// way, so the extended form has to trail a SECOND `Sync` to pay for the
/// housekeeping slot this send registers.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum CopyAbortProtocol {
    Simple,
    Extended,
}

/// Take the session back out of COPY-IN mode.
///
/// A `COPY ... FROM STDIN` sent as a simple query is answered with
/// `CopyInResponse` and then PostgreSQL WAITS for copy data. The request was
/// encoded as one pre-built buffer, so this path has no channel to push
/// `CopyData` through and cannot finish the copy. Reporting
/// `Error::unexpected_message` and walking away is not enough: the session
/// stays in copy mode, the next frontend message is read as copy data, and
/// PostgreSQL answers `unexpected message type 0x50 during COPY from stdin`
/// followed by `FATAL: terminating connection because protocol synchronization
/// was lost`. The failure then lands on whatever ran NEXT - in a pool, the next
/// borrower, because nothing about the failing call marks the connection closed
/// or dirty.
///
/// `CopyFail` is the message that ends copy mode. It costs one extra request
/// slot and that is deliberate, not an oversight: in SIMPLE query mode
/// PostgreSQL answers the `CopyFail` with `ErrorResponse` AND its own
/// `ReadyForQuery` - it does not suppress the terminator the way it does for an
/// extended-protocol copy, where the error sets `ignore_till_sync` and only the
/// `Sync` releases a `ReadyForQuery`. That first group belongs to the request
/// still at the head of the queue - the caller's, which is what turns its drain
/// into the server's real diagnostic. The trailing `Sync` then produces a
/// SECOND, bare `ReadyForQuery` for the slot this send registers, so the
/// connection task's response accounting balances. MEASURED: delete that one
/// line and every assertion in `tests/simple_query_copy_resync.rs` still holds,
/// because the caller's error is unchanged - the follow-up query simply never
/// returns. The orphaned slot stays at the head of the queue and is handed the
/// NEXT request's reply, so a regression here is a HANG, not a wrong answer.
pub(crate) fn abort_copy_in(
    client: &InnerClient,
    protocol: CopyAbortProtocol,
) -> Result<(), Error> {
    let reason = match protocol {
        CopyAbortProtocol::Simple => COPY_IN_UNSUPPORTED,
        CopyAbortProtocol::Extended => COPY_IN_UNSUPPORTED_EXTENDED,
    };
    let buf = client.with_buf(|buf| {
        frontend::copy_fail(reason, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        if protocol == CopyAbortProtocol::Extended {
            // The second terminator. See `CopyAbortProtocol`: under an
            // extended-protocol copy the `CopyFail` error releases no
            // `ReadyForQuery` of its own, so one `Sync` pays the caller and
            // this one pays the slot below.
            frontend::sync(buf);
        }
        Ok(buf.split().freeze())
    })?;
    // Housekeeping: the bare `ReadyForQuery` this earns is bookkeeping, not an
    // answer anybody reads, and the caller's own response is still awaited so a
    // failed write is still reported rather than swallowed.
    drop(client.send_with(
        RequestMessages::Single(FrontendMessage::Raw(buf)),
        RequestDisposition::Housekeeping,
        TransactionEffect::MayChange,
    )?);
    Ok(())
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
pub(crate) async fn finish_batch_execute(
    client: &InnerClient,
    mut responses: Responses,
) -> Result<(), Error> {
    loop {
        match responses.next().await? {
            Message::ReadyForQuery(_) => return Ok(()),
            Message::CommandComplete(_)
            | Message::EmptyQueryResponse
            | Message::RowDescription(_)
            | Message::DataRow(_) => {}
            // Not `unexpected_message`: walking away here leaves the session in
            // copy mode. See `abort_copy_in`. The abort makes PostgreSQL answer
            // this very stream with the copy's own error, so the loop keeps
            // draining and the caller gets that instead.
            Message::CopyInResponse(_) => abort_copy_in(client, CopyAbortProtocol::Simple)?,
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
    client: &InnerClient,
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
            Message::CopyInResponse(_) => abort_copy_in(client, CopyAbortProtocol::Simple)?,
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
        // Held so the stream can end a copy this path cannot feed
        // (`abort_copy_in`). WEAK, and that is load-bearing: `InnerClient` owns
        // the request channel, and `Connection::run` finishes only once every
        // sender is gone. A strong handle here would keep a connection alive for
        // as long as the caller held the stream - measured, it hangs
        // `integration::an_awaited_query_queued_before_client_drop_still_reports_its_write_error`
        // outright. If the client has already gone there is no session left to
        // rescue, so failing to upgrade is not an error.
        client: Weak<InnerClient>,
        responses: Responses,
        columns: Option<Arc<[SimpleColumn]>>,
    }
}

impl Stream for SimpleQueryStream {
    type Item = Result<SimpleQueryMessage, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        // A loop, not a single match: the copy-mode arm produces no item of its
        // own, and returning `Pending` after it would park a stream nobody will
        // wake.
        loop {
            match ready!(this.responses.poll_next(cx)?) {
                Message::CommandComplete(body) => {
                    let rows = extract_row_affected(&body)?;
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::CommandComplete(rows))));
                }
                Message::EmptyQueryResponse => {
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::CommandComplete(0))));
                }
                Message::RowDescription(body) => {
                    let columns: Arc<[SimpleColumn]> = body
                        .fields()
                        .map(|f| Ok(SimpleColumn::new(f.name().to_string())))
                        .collect::<Vec<_>>()
                        .map_err(Error::parse)?
                        .into();

                    *this.columns = Some(columns.clone());
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::RowDescription(columns))));
                }
                Message::DataRow(body) => {
                    let row = match &this.columns {
                        Some(columns) => SimpleQueryRow::new(columns.clone(), body)?,
                        None => return Poll::Ready(Some(Err(Error::unexpected_message()))),
                    };
                    return Poll::Ready(Some(Ok(SimpleQueryMessage::Row(row))));
                }
                Message::ReadyForQuery(_) => return Poll::Ready(None),
                Message::CopyInResponse(_) => match this.client.upgrade() {
                    Some(client) => {
                        if let Err(error) = abort_copy_in(&client, CopyAbortProtocol::Simple) {
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
