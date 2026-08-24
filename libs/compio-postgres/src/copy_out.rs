// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// COPY TO STDOUT. Self-contained: uses the normal request/response flow; no
// `Connection` changes needed. The resulting `CopyOutStream` just drains
// the `Responses` channel until `CopyDone`.

use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::simple_query::{CopyAbortProtocol, abort_copy_in};
use crate::{Error, Statement, query, slice_iter};
use bytes::Bytes;
use futures_util::Stream;
use log::debug;
use pin_project_lite::pin_project;
use postgres_protocol::message::backend::Message;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

pub async fn copy_out(
    client: &InnerClient,
    statement: Statement,
    unnamed_sql: Option<&str>,
) -> Result<CopyOutStream, Error> {
    debug!("executing copy out statement {}", statement.name());

    let buf = match unnamed_sql {
        Some(sql) => query::encode_unnamed(client, sql, &statement, slice_iter(&[]))?,
        None => query::encode(client, &statement, slice_iter(&[]))?,
    };
    let responses = match start(client, buf, &statement, unnamed_sql.is_some()).await {
        Ok(responses) => responses,
        Err(error) => {
            statement.invalidate_cache_on_error(&error);
            return Err(error);
        }
    };
    Ok(CopyOutStream { responses })
}

async fn start(
    client: &InnerClient,
    buf: Bytes,
    statement: &Statement,
    reparsed: bool,
) -> Result<Responses, Error> {
    let mut responses = client.send_statement(
        RequestMessages::Single(FrontendMessage::Raw(buf)),
        statement,
    )?;

    if reparsed {
        match responses.next().await? {
            Message::ParseComplete => {}
            _ => return Err(Error::unexpected_message()),
        }
    }

    match responses.next().await? {
        Message::BindComplete => {}
        _ => return Err(Error::unexpected_message()),
    }

    loop {
        match responses.next().await? {
            Message::CopyOutResponse(_) => break,
            // A caller can hand `copy_out` a COPY in the opposite direction.
            // PostgreSQL is now waiting for data this API cannot supply, and
            // the Sync already in the extended-protocol batch was ignored.
            // Keep reading after the abort so its ErrorResponse is returned to
            // this caller while the second Sync pays the housekeeping slot.
            Message::CopyInResponse(_) => {
                abort_copy_in(client, CopyAbortProtocol::Extended)?;
            }
            _ => return Err(Error::unexpected_message()),
        }
    }

    Ok(responses)
}

pin_project! {
    /// A stream of `COPY ... TO STDOUT` query data.
    #[project(!Unpin)]
    pub struct CopyOutStream {
        responses: Responses,
    }
}

impl Stream for CopyOutStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match ready!(this.responses.poll_next(cx)?) {
            Message::CopyData(body) => Poll::Ready(Some(Ok(body.into_bytes()))),
            Message::CopyDone => Poll::Ready(None),
            _ => Poll::Ready(Some(Err(Error::unexpected_message()))),
        }
    }
}
