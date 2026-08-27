// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// COPY TO STDOUT. Self-contained: uses the normal request/response flow; no
// `Connection` changes needed. The resulting `CopyOutStream` just drains
// the `Responses` channel until `CopyDone`.

use crate::client::{CopyMode, CopyModeGuard, InnerClient, Responses};
use crate::copy_format::CopyResponse;
use crate::query::ExecutionError;
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
    copy_out_inner(client, statement, unnamed_sql)
        .await
        .map_err(ExecutionError::into_error)
}

pub(crate) async fn copy_out_cached(
    client: &InnerClient,
    statement: Statement,
    unnamed_sql: Option<&str>,
) -> Result<CopyOutStream, ExecutionError> {
    copy_out_inner(client, statement, unnamed_sql).await
}

async fn copy_out_inner(
    client: &InnerClient,
    statement: Statement,
    unnamed_sql: Option<&str>,
) -> Result<CopyOutStream, ExecutionError> {
    debug!("executing copy out statement {}", statement.name());

    let buf = match unnamed_sql {
        Some(sql) => query::encode_unnamed(client, sql, &statement, slice_iter(&[]))
            .map_err(ExecutionError::before_bind_complete)?,
        None => query::encode(client, &statement, slice_iter(&[]))
            .map_err(ExecutionError::before_bind_complete)?,
    };
    let (responses, response, copy_mode) =
        match start(client, buf, &statement, unnamed_sql.is_some()).await {
            Ok(result) => result,
            Err(error) => {
                if matches!(&error, ExecutionError::BeforeBindComplete(_)) {
                    statement.invalidate_cache_on_error(error.error());
                }
                return Err(error);
            }
        };
    Ok(CopyOutStream {
        responses,
        response,
        copy_done: false,
        command_complete: false,
        copy_mode: Some(copy_mode),
    })
}

async fn start(
    client: &InnerClient,
    buf: Bytes,
    statement: &Statement,
    reparsed: bool,
) -> Result<(Responses, CopyResponse, CopyModeGuard), ExecutionError> {
    let (mut responses, copy_mode) = client
        .send_copy_statement(
            query::producerless_request(buf, statement.may_enter_copy_in()),
            statement,
            CopyMode::Out,
        )
        .map_err(ExecutionError::before_bind_complete)?;

    if reparsed {
        match responses
            .next()
            .await
            .map_err(ExecutionError::before_bind_complete)?
        {
            Message::ParseComplete => {}
            _ => {
                return Err(ExecutionError::before_bind_complete(
                    Error::unexpected_message(),
                ));
            }
        }
    }

    match responses
        .next()
        .await
        .map_err(ExecutionError::before_bind_complete)?
    {
        Message::BindComplete => {}
        _ => {
            return Err(ExecutionError::before_bind_complete(
                Error::unexpected_message(),
            ));
        }
    }

    let response = loop {
        match responses
            .next()
            .await
            .map_err(ExecutionError::after_bind_complete)?
        {
            Message::CopyOutResponse(body) => {
                break CopyResponse::from_backend(body.format(), body.column_formats())
                    .map_err(ExecutionError::after_bind_complete)?;
            }
            // The connection-owned producer is already sending CopyFail.
            Message::CopyInResponse(_) => {}
            _ => {
                return Err(ExecutionError::after_bind_complete(
                    Error::unexpected_message(),
                ));
            }
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
        copy_done: bool,
        command_complete: bool,
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

        loop {
            match ready!(this.responses.poll_next(cx)) {
                Ok(Message::CopyData(body)) if !*this.copy_done => {
                    return Poll::Ready(Some(Ok(body.into_bytes())));
                }
                Ok(Message::CopyDone) if !*this.copy_done => {
                    // CopyDone terminates the data stream, but PostgreSQL has
                    // not completed the command yet. ExecutorFinish runs after
                    // CopyDone and can still produce an ErrorResponse, so do
                    // not report EOF until CommandComplete arrives.
                    *this.copy_done = true;
                }
                Ok(Message::CommandComplete(_)) if *this.copy_done => {
                    // The following Sync closes the implicit transaction. A
                    // deferred constraint can still fail there, so command
                    // completion is not yet stream success.
                    *this.command_complete = true;
                }
                Ok(Message::ReadyForQuery(_)) if *this.command_complete => {
                    this.copy_mode.take();
                    return Poll::Ready(None);
                }
                Ok(_) => {
                    this.copy_mode.take();
                    return Poll::Ready(Some(Err(Error::unexpected_message())));
                }
                Err(error) => {
                    this.copy_mode.take();
                    return Poll::Ready(Some(Err(error)));
                }
            }
        }
    }
}
