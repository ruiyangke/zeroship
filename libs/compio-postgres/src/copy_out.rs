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
                // DEFENSIVE SECOND CHECK, not the primary invalidation. A
                // pre-BindComplete ErrorResponse has already crossed the connection
                // dispatcher, which invalidates this same statement at
                // `connection.rs:1356` before waking this consumer, and
                // `invalidate_cached_statement_on_error` returns early unless the
                // error is a genuine stale-statement one. `query.rs` carries the
                // same call with the same reasoning spelled out; the sites in
                // `copy_in.rs` and `bind.rs` carry no such comment, though both
                // are bound by name.
                //
                // Nothing bound this line until 2026-09-02: replacing the condition
                // with `if false` left all 1694 tests green across every target.
                // The first version of this comment concluded that no test SHOULD
                // exist, on the grounds that one would assert a redundant call
                // rather than behaviour. That was wrong. `bind.rs` asserts the same
                // call on both of its paths, so the effect is observable from the
                // statement cache and this crate already treats it as worth
                // asserting; being a second check does not make it unassertable.
                // `a_stale_bind_error_invalidates_the_cached_copy_out_statement`
                // now binds it, and `copy_in.rs` has the twin.
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
            // A malformed or rewritten peer can answer this COPY OUT request
            // with CopyInResponse. Arm the same connection-owned CopyFail used
            // by producerless queries so that wrong-way COPY IN is actively
            // terminated instead of waiting forever for frontend input.
            query::producerless_request(buf, true),
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
            other => return Err(ExecutionError::pre_bind_mismatch(&other)),
        }
    }

    match responses
        .next()
        .await
        .map_err(ExecutionError::before_bind_complete)?
    {
        Message::BindComplete => {}
        other => return Err(ExecutionError::pre_bind_mismatch(&other)),
    }

    // A non-COPY command can complete Execute and then fail while Sync closes
    // its implicit transaction. Keep the local mismatch only as a fallback at
    // ReadyForQuery so the later ErrorResponse can win.
    async fn drain_refusal(responses: &mut Responses, fallback: Error) -> Error {
        loop {
            match responses.next().await {
                Ok(Message::ReadyForQuery(_)) => return fallback,
                Ok(_) => {}
                Err(error) => return error,
            }
        }
    }

    let response = loop {
        match responses
            .next()
            .await
            .map_err(ExecutionError::after_bind_complete)?
        {
            Message::CopyOutResponse(body) => {
                // `codec::read_backend` validates every COPY response body
                // before dispatch. This conversion is defense in depth; a
                // deterministic connection test cannot reach its error arm.
                break CopyResponse::from_backend(body.format(), body.column_formats())
                    .map_err(ExecutionError::after_bind_complete)?;
            }
            // The connection-owned producer is already sending CopyFail. The
            // server entered COPY IN when it sent this response, so no later
            // response can turn the exchange into COPY OUT.
            Message::CopyInResponse(_) => {
                return Err(ExecutionError::after_bind_complete(
                    drain_refusal(&mut responses, Error::unexpected_message()).await,
                ));
            }
            _ => {
                return Err(ExecutionError::after_bind_complete(
                    drain_refusal(&mut responses, Error::unexpected_message()).await,
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
        // draining, before ordinary requests may queue behind it. Safe code
        // has no hook between synchronous field drops, so after-Drop probes
        // cannot deterministically observe the opposite ordering.
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

        if this.copy_mode.is_none() {
            return Poll::Ready(None);
        }

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
                Ok(Message::CommandComplete(_)) if *this.copy_done && !*this.command_complete => {
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

#[cfg(test)]
mod tests {
    use super::start;
    use crate::Statement;
    use crate::client::{Client, ResponseMessages};
    use crate::config::{SslMode, SslNegotiation};
    use crate::connection::Request;
    use bytes::{BufMut, Bytes, BytesMut};
    use futures_channel::mpsc;
    use futures_util::StreamExt;
    use postgres_protocol::message::backend::Message;
    use std::collections::VecDeque;

    /// The COPY OUT twin of `bind.rs`'s two
    /// `..._invalidates_cached_statement` tests and `copy_in.rs`'s.
    ///
    /// The comment on the call used to say a test here would only assert that
    /// a redundant call happened. That was wrong: `bind.rs` asserts exactly
    /// this on both of its paths, so the behaviour is observable and this
    /// crate already treats it as worth asserting. Being a second check after
    /// the connection dispatcher does not make it unassertable.
    #[compio::test]
    async fn a_stale_bind_error_invalidates_the_cached_copy_out_statement() {
        use crate::codec::BackendMessages;
        use std::num::NonZeroUsize;
        use std::sync::Arc;

        const SQL: &str = "COPY cached_copy_out TO STDOUT";
        let (request_sender, mut requests) = mpsc::unbounded();
        let client = Client::new_with_statement_cache(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
            crate::config::ProtocolVersion::V3_0,
            crate::client::StatementCacheSettings::new(1, NonZeroUsize::MIN),
        );
        let inner = Arc::clone(client.inner());
        let statement = Statement::new(
            &inner,
            "s_stale_copy_out".to_string(),
            vec![],
            vec![],
            false,
        );
        let statement = inner.cache_statement(SQL, statement, inner.type_cache_generation());
        assert!(
            inner.cached_statement(SQL).is_some(),
            "fixture did not cache"
        );

        let copy = super::copy_out_inner(&inner, statement, None);
        let respond = async {
            let mut request = requests
                .next()
                .await
                .expect("COPY OUT did not enqueue its request");
            let mut frame = Vec::with_capacity(64);
            frame.push(b'E');
            let body = b"SERROR\0C26000\0Mscripted stale statement\0RFetchPreparedStatement\0\0";
            frame.extend_from_slice(&(u32::try_from(body.len()).unwrap() + 4).to_be_bytes());
            frame.extend_from_slice(body);
            request
                .sender
                .try_send(ResponseMessages::Raw(BackendMessages::from_test_bytes(
                    BytesMut::from(frame.as_slice()),
                )))
                .expect("deliver the stale-statement COPY OUT error");
        };

        let (result, ()) = futures_util::future::join(copy, respond).await;
        let error = match result {
            Err(failure) => failure.into_error(),
            Ok(_) => panic!("a stale cached COPY OUT unexpectedly started"),
        };
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("26000"),
            "the COPY OUT failure lost the server's stale-statement diagnosis"
        );
        assert!(
            inner.cached_statement(SQL).is_none(),
            "the pre-BindComplete error left its stale statement cached"
        );
    }

    /// `G` (CopyInResponse) and `H` (CopyOutResponse) share a body: Int8
    /// overall format, Int16 column count.
    fn copy_response_message(tag: u8) -> Message {
        let mut frame = BytesMut::new();
        frame.put_u8(tag);
        frame.put_u32(4 + 3);
        frame.put_u8(0);
        frame.put_i16(0);
        Message::parse(&mut frame)
            .expect("parse the scripted copy response")
            .expect("the scripted copy response was incomplete")
    }

    /// Either copy response standing where `BindComplete` was due means the
    /// server is already in copy mode. `ExecutionError::BeforeBindComplete` is
    /// documented as the phase that can describe PostgreSQL rejecting the named
    /// statement, and it licenses the stale-cache replay in `Client::copy_out`;
    /// a server past Bind is not a rejected statement.
    ///
    /// The `CopyInResponse` arm one exchange later already reasons that "the
    /// server entered COPY IN when it sent this response". This asserts the
    /// Bind slot draws the same conclusion.
    #[compio::test]
    async fn a_copy_response_standing_in_for_bind_complete_forbids_replay() {
        for (tag, reparsed) in [
            (b'G', false),
            (b'H', false),
            // `reparsed` puts the scripted response in the ParseComplete slot
            // instead, one exchange earlier - still past Bind.
            (b'G', true),
            (b'H', true),
        ] {
            let (request_sender, mut requests) = mpsc::unbounded();
            let client = Client::new(
                request_sender,
                SslMode::Disable,
                SslNegotiation::Postgres,
                0,
                Some(0.into()),
                None,
            );
            let statement = Statement::unnamed(Vec::new(), Vec::new());
            let run = start(
                client.inner(),
                Bytes::from_static(b""),
                &statement,
                reparsed,
            );
            let connection = async move {
                let Request {
                    sender: mut response_sender,
                    ..
                } = requests.next().await.expect("COPY OUT enqueued no request");
                response_sender
                    .try_send(ResponseMessages::Observed(VecDeque::from([Ok(
                        copy_response_message(tag),
                    )])))
                    .expect("deliver the scripted copy response");
            };

            let (result, ()) = futures_util::future::join(run, connection).await;
            let failure = match result {
                Ok(_) => panic!("a copy response in the Bind slot started COPY OUT"),
                Err(failure) => failure,
            };
            let (_, before_bind_complete) = failure.into_parts();
            assert!(
                !before_bind_complete,
                "a '{}' (reparsed={}) in a pre-Bind slot was classified as replay-safe",
                char::from(tag),
                reparsed
            );
        }
    }
}
