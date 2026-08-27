// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// COPY TO STDOUT. Self-contained: uses the normal request/response flow; no
// `Connection` changes needed. The resulting `CopyOutStream` just drains
// the `Responses` channel until `CopyDone`.

use crate::client::{CopyMode, CopyModeGuard, InnerClient, Responses};
use crate::copy_format::CopyResponse;
use crate::{CopyFormat, Error, Statement, query, slice_iter};
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
    let (responses, response, copy_mode) =
        match start(client, buf, &statement, unnamed_sql.is_some()).await {
            Ok(result) => result,
            Err(error) => {
                statement.invalidate_cache_on_error(&error);
                return Err(error);
            }
        };
    Ok(CopyOutStream {
        responses,
        response,
        copy_mode: Some(copy_mode),
    })
}

async fn start(
    client: &InnerClient,
    buf: Bytes,
    statement: &Statement,
    reparsed: bool,
) -> Result<(Responses, CopyResponse, CopyModeGuard), Error> {
    let (mut responses, copy_mode) = client.send_copy_statement(
        query::producerless_request(buf, statement.may_enter_copy_in()),
        statement,
        CopyMode::Out,
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

    let response = loop {
        match responses.next().await? {
            Message::CopyOutResponse(body) => {
                break CopyResponse::from_backend(body.format(), body.column_formats())?;
            }
            // The connection-owned producer is already sending CopyFail.
            Message::CopyInResponse(_) => {}
            _ => return Err(Error::unexpected_message()),
        }
    };

    Ok((responses, response, copy_mode))
}

pin_project! {
    /// A stream of `COPY ... TO STDOUT` query data.
    #[project(!Unpin)]
    pub struct CopyOutStream {
        responses: Responses,
        response: CopyResponse,
        // Last so Drop disconnects the consumer, arming connection-owned
        // draining, before ordinary requests may queue behind it.
        copy_mode: Option<CopyModeGuard>,
    }
}

impl CopyOutStream {
    /// The overall format selected by PostgreSQL for this copy.
    pub fn format(&self) -> CopyFormat {
        self.response.format()
    }

    /// The format PostgreSQL selected for each copied column.
    pub fn column_formats(&self) -> &[CopyFormat] {
        self.response.column_formats()
    }
}

impl Stream for CopyOutStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match ready!(this.responses.poll_next(cx)) {
            Ok(Message::CopyData(body)) => Poll::Ready(Some(Ok(body.into_bytes()))),
            Ok(Message::CopyDone) => {
                this.copy_mode.take();
                Poll::Ready(None)
            }
            Ok(_) => {
                this.copy_mode.take();
                Poll::Ready(Some(Err(Error::unexpected_message())))
            }
            Err(error) => {
                this.copy_mode.take();
                Poll::Ready(Some(Err(error)))
            }
        }
    }
}
