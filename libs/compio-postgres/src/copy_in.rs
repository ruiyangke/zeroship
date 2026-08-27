// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// COPY FROM STDIN. The frontend pipes frames through an mpsc channel; the
// connection task drains that channel directly, writing CopyData / CopyDone /
// CopyFail frames onto the wire. Logic mirrors tokio-postgres exactly - the
// only compio-specific bit is in `connection.rs`, where the request-handler
// branch reads the `CopyInReceiver` with `next().await` instead of
// `poll_next_unpin(cx)`.

use crate::client::{CopyMode, CopyModeGuard, InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::copy_format::CopyResponse;
use crate::query::ExecutionError;
use crate::{CopyFormat, Error, Statement, query, slice_iter};
use bytes::{Buf, BufMut, BytesMut};
use futures_channel::mpsc;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use log::debug;
use pin_project_lite::pin_project;
use postgres_protocol::message::backend::{CommandCompleteBody, Message};
use postgres_protocol::message::frontend;
use postgres_protocol::message::frontend::CopyData;
use std::future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

/// Internal message type flowing from user code to the connection task.
enum CopyInMessage {
    Message(FrontendMessage),
    Done,
    /// The COPY never entered copy mode, so end the request writing NOTHING
    /// further.
    ///
    /// `CopyFail` is only legal while the server is in COPY mode. If the
    /// server refused the `Bind`/`Execute` -- a missing table, a permissions
    /// failure -- it answered the `Sync` that `query::encode` already appended
    /// and is back at `ReadyForQuery`. Sending `CopyFail` there earns a second
    /// `ErrorResponse` ("no COPY in progress") and, after our second `Sync`, a
    /// second `ReadyForQuery`. Both are unowned: the request that would have
    /// consumed them has already returned its error. The next request on the
    /// connection reads them instead of its own reply and fails with
    /// `UnexpectedMessage`, which in a pool means every later borrower of that
    /// connection fails too.
    Abort,
}

/// Stream of frontend messages fed to the connection task for a `COPY FROM
/// STDIN` request. The connection's request-handler branch polls `next()`
/// until the stream terminates. Extended protocol uses `CopyDone+Sync` or
/// `CopyFail+Sync`; a producerless simple query uses `CopyFail` alone because
/// that protocol emits `ReadyForQuery` without a Sync barrier.
pub struct CopyInReceiver {
    receiver: mpsc::Receiver<CopyInMessage>,
    done: bool,
    abort_reason: Option<&'static str>,
    sync_after_terminal: bool,
}

impl CopyInReceiver {
    fn new(receiver: mpsc::Receiver<CopyInMessage>) -> CopyInReceiver {
        CopyInReceiver {
            receiver,
            done: false,
            abort_reason: None,
            sync_after_terminal: true,
        }
    }

    /// Build a connection-owned COPY producer for an API which can execute a
    /// `COPY FROM STDIN` statement but has no caller data channel.
    ///
    /// The initial extended-protocol batch is sent first. If `PostgreSQL` accepts
    /// it, the closed channel below deterministically yields `CopyFail + Sync`
    /// with the caller-visible reason. If startup is rejected, the connection
    /// observes the initial batch's `ReadyForQuery` and never polls this
    /// terminal frame. Reusing the real COPY state machine is what makes both
    /// paths cost exactly one response slot.
    pub(crate) fn aborting(initial: FrontendMessage, reason: &'static str) -> Self {
        Self::aborting_with_terminal(initial, reason, true)
    }

    /// Build the same producerless COPY for a simple-protocol `Query`.
    ///
    /// Simple-query `CopyFail` produces its own `ReadyForQuery`; appending a
    /// `Sync` would produce a second one. A transaction pooler may release the
    /// backend after the first, so the driver must neither send that redundant
    /// barrier nor account for a response to it.
    pub(crate) fn aborting_simple(initial: FrontendMessage, reason: &'static str) -> Self {
        Self::aborting_with_terminal(initial, reason, false)
    }

    fn aborting_with_terminal(
        initial: FrontendMessage,
        reason: &'static str,
        sync_after_terminal: bool,
    ) -> Self {
        let (mut sender, receiver) = mpsc::channel(1);
        sender
            .try_send(CopyInMessage::Message(initial))
            .expect("a new producerless COPY channel accepts its initial frame");
        drop(sender);
        Self {
            receiver,
            done: false,
            abort_reason: Some(reason),
            sync_after_terminal,
        }
    }

    /// True after this stream emitted its terminal CopyDone/CopyFail frame,
    /// including the extended-protocol Sync when one is required. The
    /// connection uses this to start the final server-response read clock only
    /// after that frame finishes flushing.
    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// Whether this producer's terminal COPY frame includes an extended-query
    /// Sync. PostgreSQL can answer both the opening Sync and this terminal Sync
    /// after an error that occurs immediately after CopyInResponse.
    pub(crate) fn terminal_includes_sync(&self) -> bool {
        self.sync_after_terminal
    }
}

impl Stream for CopyInReceiver {
    type Item = FrontendMessage;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<FrontendMessage>> {
        if self.done {
            return Poll::Ready(None);
        }

        match ready!(self.receiver.poll_next_unpin(cx)) {
            Some(CopyInMessage::Message(message)) => Poll::Ready(Some(message)),
            Some(CopyInMessage::Abort) => {
                self.done = true;
                Poll::Ready(None)
            }
            Some(CopyInMessage::Done) => {
                self.done = true;
                let mut buf = BytesMut::new();
                frontend::copy_done(&mut buf);
                frontend::sync(&mut buf);
                Poll::Ready(Some(FrontendMessage::Raw(buf.freeze())))
            }
            None => {
                self.done = true;
                let mut buf = BytesMut::new();
                frontend::copy_fail(self.abort_reason.unwrap_or(""), &mut buf).unwrap();
                if self.sync_after_terminal {
                    frontend::sync(&mut buf);
                }
                Poll::Ready(Some(FrontendMessage::Raw(buf.freeze())))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SinkState {
    Active,
    Closing,
    Reading,
    Finished(u64),
}

fn copy_row_count(body: &CommandCompleteBody) -> Result<u64, Error> {
    let tag = body.tag().map_err(Error::parse)?;
    let rows = tag
        .strip_prefix("COPY ")
        .filter(|rows| !rows.is_empty() && rows.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(Error::unexpected_message)?;
    rows.parse().map_err(|_| Error::unexpected_message())
}

struct BufferedCopyAppend<'a> {
    buf: &'a mut BytesMut,
    checkpoint: usize,
    committed: bool,
}

impl<'a> BufferedCopyAppend<'a> {
    fn new(buf: &'a mut BytesMut) -> Self {
        Self {
            checkpoint: buf.len(),
            buf,
            committed: false,
        }
    }

    fn put<T: Buf>(&mut self, item: T) {
        self.buf.put(item);
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for BufferedCopyAppend<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.buf.truncate(self.checkpoint);
        }
    }
}

pin_project! {
    /// A sink for `COPY ... FROM STDIN` query data.
    ///
    /// The copy *must* be explicitly completed via the `Sink::close` or
    /// `finish` methods. If it is not, the copy will be aborted.
    #[project(!Unpin)]
    pub struct CopyInSink<T> {
        #[pin]
        sender: mpsc::Sender<CopyInMessage>,
        responses: Responses,
        response: CopyResponse,
        buf: BytesMut,
        state: SinkState,
        completion: Option<Result<u64, Error>>,
        _p2: PhantomData<T>,
        // Last so Drop closes the producer and response consumer before it
        // publishes that ordinary requests may queue behind their recovery.
        copy_mode: Option<CopyModeGuard>,
    }
}

impl<T> CopyInSink<T>
where
    T: Buf + 'static + Send,
{
    /// The overall format selected by PostgreSQL for this copy.
    pub fn format(&self) -> CopyFormat {
        self.response.format()
    }

    /// The format PostgreSQL selected for each copied column.
    pub fn column_formats(&self) -> &[CopyFormat] {
        self.response.column_formats()
    }

    fn clear_copy_mode(self: Pin<&mut Self>) {
        self.project().copy_mode.take();
    }

    /// Once the connection task drops the producer it has already published
    /// every backend message decoded before that drop. Poll that response FIFO
    /// before turning the local channel symptom into `connection closed`.
    fn poll_disconnected_diagnosis(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Error>> {
        *self.as_mut().project().state = SinkState::Reading;
        loop {
            match self.as_mut().project().responses.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    self.as_mut().clear_copy_mode();
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(_)) => {}
            }
        }
    }

    fn disconnected_diagnosis_now(mut self: Pin<&mut Self>) -> Error {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match self.as_mut().poll_disconnected_diagnosis(&mut cx) {
            Poll::Ready(Err(error)) => error,
            Poll::Pending | Poll::Ready(Ok(())) => Error::closed(),
        }
    }

    /// A poll-based version of `finish`.
    pub fn poll_finish(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<u64, Error>> {
        loop {
            match self.state {
                SinkState::Active => {
                    match self.as_mut().poll_flush(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(error)) if error.is_closed() => {
                            // The connection can close its COPY producer only
                            // after it has published any decoded backend
                            // messages. Prefer that queued ErrorResponse to the
                            // local sender-disconnected symptom.
                            *self.as_mut().project().state = SinkState::Reading;
                            continue;
                        }
                        Poll::Ready(Err(error)) => {
                            self.as_mut().clear_copy_mode();
                            return Poll::Ready(Err(error));
                        }
                    }
                    let sender_ready = {
                        let mut this = self.as_mut().project();
                        this.sender
                            .as_mut()
                            .poll_ready(cx)
                            .map_err(|_| Error::closed())
                    };
                    match sender_ready {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(_)) => {
                            *self.as_mut().project().state = SinkState::Reading;
                            continue;
                        }
                    }
                    let send_result = {
                        let mut this = self.as_mut().project();
                        this.sender
                            .as_mut()
                            .start_send(CopyInMessage::Done)
                            .map_err(|_| Error::closed())
                    };
                    if send_result.is_err() {
                        *self.as_mut().project().state = SinkState::Reading;
                        continue;
                    }
                    *self.as_mut().project().state = SinkState::Closing;
                }
                SinkState::Closing => {
                    let sender_closed = {
                        let this = self.as_mut().project();
                        this.sender.poll_close(cx).map_err(|_| Error::closed())
                    };
                    match sender_closed {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(())) => {
                            *self.as_mut().project().state = SinkState::Reading;
                        }
                        Poll::Ready(Err(_)) => {
                            *self.as_mut().project().state = SinkState::Reading;
                        }
                    }
                }
                SinkState::Reading => {
                    let response = {
                        let this = self.as_mut().project();
                        this.responses.poll_next(cx)
                    };
                    match response {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(Message::CommandComplete(body))) => {
                            let this = self.as_mut().project();
                            if this.completion.is_some() {
                                *this.completion = Some(Err(Error::unexpected_message()));
                            } else {
                                *this.completion = Some(copy_row_count(&body));
                            }
                        }
                        Poll::Ready(Ok(Message::ReadyForQuery(_))) => {
                            let result = self
                                .as_mut()
                                .project()
                                .completion
                                .take()
                                .unwrap_or_else(|| Err(Error::unexpected_message()));
                            self.as_mut().clear_copy_mode();
                            match result {
                                Ok(rows) => {
                                    *self.as_mut().project().state = SinkState::Finished(rows);
                                    return Poll::Ready(Ok(rows));
                                }
                                Err(error) => return Poll::Ready(Err(error)),
                            }
                        }
                        Poll::Ready(Ok(_)) => {
                            let this = self.as_mut().project();
                            if !matches!(this.completion.as_ref(), Some(Err(_))) {
                                *this.completion = Some(Err(Error::unexpected_message()));
                            }
                        }
                        Poll::Ready(Err(error)) => {
                            self.as_mut().clear_copy_mode();
                            return Poll::Ready(Err(error));
                        }
                    }
                }
                SinkState::Finished(rows) => return Poll::Ready(Ok(rows)),
            }
        }
    }

    /// Completes the copy, returning the number of rows inserted.
    ///
    /// The `Sink::close` method is equivalent to `finish`, except that it
    /// does not return the number of rows. After successful completion,
    /// repeated calls return the same row count without touching the session.
    pub async fn finish(mut self: Pin<&mut Self>) -> Result<u64, Error> {
        future::poll_fn(|cx| self.as_mut().poll_finish(cx)).await
    }
}

impl<T> Sink<T> for CopyInSink<T>
where
    T: Buf + 'static + Send,
{
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if matches!(self.state, SinkState::Finished(_)) {
            return Poll::Ready(Err(Error::copy_in_finished()));
        }
        let ready = self.as_mut().project().sender.poll_ready(cx);
        match ready {
            Poll::Ready(Err(_)) => self.poll_disconnected_diagnosis(cx),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Error> {
        if matches!(self.state, SinkState::Finished(_)) {
            return Err(Error::copy_in_finished());
        }
        let this = self.as_mut().project();

        let large_item = item.remaining() > 4096;
        let staged_buffered_prefix = large_item && !this.buf.is_empty();
        let data: Box<dyn Buf + Send> = if large_item {
            if this.buf.is_empty() {
                Box::new(item)
            } else {
                Box::new(this.buf.clone().freeze().chain(item))
            }
        } else {
            let mut append = BufferedCopyAppend::new(this.buf);
            append.put(item);
            append.commit();
            if this.buf.len() >= 4096 {
                Box::new(this.buf.split().freeze())
            } else {
                return Ok(());
            }
        };

        let data = CopyData::new(data).map_err(Error::encode)?;
        if this
            .sender
            .start_send(CopyInMessage::Message(FrontendMessage::CopyData(data)))
            .is_err()
        {
            return Err(self.disconnected_diagnosis_now());
        }
        if staged_buffered_prefix {
            this.buf.clear();
        }
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if matches!(self.state, SinkState::Finished(_)) {
            return Poll::Ready(Ok(()));
        }
        let buffered = !self.as_mut().project().buf.is_empty();
        if buffered {
            let ready = self.as_mut().project().sender.as_mut().poll_ready(cx);
            match ready {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(_)) => return self.poll_disconnected_diagnosis(cx),
                Poll::Ready(Ok(())) => {}
            }

            let send_result = {
                let mut this = self.as_mut().project();
                let data: Box<dyn Buf + Send> = Box::new(this.buf.split().freeze());
                let data = CopyData::new(data).map_err(Error::encode)?;
                this.sender
                    .as_mut()
                    .start_send(CopyInMessage::Message(FrontendMessage::CopyData(data)))
            };
            if send_result.is_err() {
                return self.poll_disconnected_diagnosis(cx);
            }
        }

        let (flushed, disconnected) = {
            let mut this = self.as_mut().project();
            let flushed = this.sender.as_mut().poll_flush(cx);
            let disconnected = this.sender.is_closed();
            (flushed, disconnected)
        };
        match flushed {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => self.poll_disconnected_diagnosis(cx),
            Poll::Ready(Ok(())) if disconnected => self.poll_disconnected_diagnosis(cx),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.poll_finish(cx).map_ok(|_| ())
    }
}

pub async fn copy_in<T>(
    client: &InnerClient,
    statement: Statement,
    unnamed_sql: Option<&str>,
) -> Result<CopyInSink<T>, Error>
where
    T: Buf + 'static + Send,
{
    copy_in_inner(client, statement, unnamed_sql)
        .await
        .map_err(ExecutionError::into_error)
}

pub(crate) async fn copy_in_cached<T>(
    client: &InnerClient,
    statement: Statement,
    unnamed_sql: Option<&str>,
) -> Result<CopyInSink<T>, ExecutionError>
where
    T: Buf + 'static + Send,
{
    copy_in_inner(client, statement, unnamed_sql).await
}

async fn copy_in_inner<T>(
    client: &InnerClient,
    statement: Statement,
    unnamed_sql: Option<&str>,
) -> Result<CopyInSink<T>, ExecutionError>
where
    T: Buf + 'static + Send,
{
    debug!("executing copy in statement {}", statement.name());

    let buf = match unnamed_sql {
        Some(sql) => query::encode_unnamed(client, sql, &statement, slice_iter(&[]))
            .map_err(ExecutionError::before_bind_complete)?,
        None => query::encode(client, &statement, slice_iter(&[]))
            .map_err(ExecutionError::before_bind_complete)?,
    };

    let (mut sender, receiver) = mpsc::channel(1);
    let receiver = CopyInReceiver::new(receiver);
    let (mut responses, copy_mode) = client
        .send_copy_statement(RequestMessages::CopyIn(receiver), &statement, CopyMode::In)
        .map_err(ExecutionError::before_bind_complete)?;

    send_initial_copy_message(&mut sender, &mut responses, FrontendMessage::Raw(buf))
        .await
        .map_err(ExecutionError::before_bind_complete)?;

    // Until `CopyInResponse` arrives the server is NOT in copy mode, so every
    // failure below must leave the request silent rather than let the sender's
    // drop synthesize a `CopyFail`. See [`CopyInMessage::Abort`].
    async fn abort(sender: &mut mpsc::Sender<CopyInMessage>) {
        let _ = sender.send(CopyInMessage::Abort).await;
    }

    // A local API/protocol mismatch can precede an ErrorResponse generated
    // while Execute or the following Sync finishes. Keep the mismatch only as
    // a fallback at ReadyForQuery; a server diagnosis always wins.
    async fn drain_refusal(responses: &mut Responses, fallback: Error) -> Error {
        loop {
            match responses.next().await {
                Ok(Message::ReadyForQuery(_)) => return fallback,
                Ok(_) => {}
                Err(error) => return error,
            }
        }
    }

    if unnamed_sql.is_some() {
        match responses.next().await {
            Ok(Message::ParseComplete) => {}
            Ok(_) => {
                abort(&mut sender).await;
                return Err(ExecutionError::before_bind_complete(
                    Error::unexpected_message(),
                ));
            }
            Err(e) => {
                abort(&mut sender).await;
                return Err(ExecutionError::before_bind_complete(e));
            }
        }
    }

    match responses.next().await {
        Ok(Message::BindComplete) => {}
        Ok(_) => {
            abort(&mut sender).await;
            return Err(ExecutionError::before_bind_complete(
                Error::unexpected_message(),
            ));
        }
        Err(e) => {
            statement.invalidate_cache_on_error(&e);
            abort(&mut sender).await;
            return Err(ExecutionError::before_bind_complete(e));
        }
    }

    let response = match responses.next().await {
        Ok(Message::CopyInResponse(body)) => {
            CopyResponse::from_backend(body.format(), body.column_formats())
                .map_err(ExecutionError::after_bind_complete)?
        }
        Ok(Message::CopyOutResponse(_)) => {
            abort(&mut sender).await;
            return Err(ExecutionError::after_bind_complete(
                drain_refusal(&mut responses, Error::copy_out_answered_copy_in()).await,
            ));
        }
        Ok(_) => {
            abort(&mut sender).await;
            return Err(ExecutionError::after_bind_complete(
                drain_refusal(&mut responses, Error::unexpected_message()).await,
            ));
        }
        Err(e) => {
            abort(&mut sender).await;
            return Err(ExecutionError::after_bind_complete(e));
        }
    };

    Ok(CopyInSink {
        sender,
        responses,
        response,
        buf: BytesMut::new(),
        state: SinkState::Active,
        completion: None,
        _p2: PhantomData,
        copy_mode: Some(copy_mode),
    })
}

async fn send_initial_copy_message(
    sender: &mut mpsc::Sender<CopyInMessage>,
    responses: &mut Responses,
    message: FrontendMessage,
) -> Result<(), Error> {
    if sender.send(CopyInMessage::Message(message)).await.is_ok() {
        return Ok(());
    }

    // The request owns both the COPY receiver and its response producer. If
    // the former has gone away, the latter must either carry the backend
    // diagnosis which ended the request or terminate too. Drain that FIFO so
    // a decoded ErrorResponse wins over the local producer-channel symptom.
    loop {
        match responses.next().await {
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CopyInReceiver, CopyInSink, SinkState, send_initial_copy_message};
    use crate::client::{Client, CopyMode, ResponseMessages};
    use crate::codec::FrontendMessage;
    use crate::config::{SslMode, SslNegotiation};
    use crate::connection::RequestMessages;
    use crate::copy_format::CopyResponse;
    use crate::error::{DbError, SqlState};
    use crate::{Error, Statement};
    use bytes::Bytes;
    use bytes::{BufMut, BytesMut};
    use futures_channel::mpsc;
    use futures_util::StreamExt;
    use postgres_protocol::message::backend::Message;
    use std::collections::VecDeque;
    use std::marker::PhantomData;

    fn admin_shutdown() -> DbError {
        let payload = b"SFATAL\0VFATAL\0C57P01\0Mscripted shutdown\0\0";
        let mut frame = BytesMut::new();
        frame.put_u8(b'E');
        frame.put_u32(u32::try_from(payload.len() + 4).unwrap());
        frame.extend_from_slice(payload);

        match Message::parse(&mut frame).expect("parse scripted ErrorResponse") {
            Some(Message::ErrorResponse(body)) => {
                DbError::parse(&mut body.fields()).expect("parse scripted DbError")
            }
            _ => panic!("scripted 57P01 did not decode as ErrorResponse"),
        }
    }

    #[compio::test]
    async fn initial_producer_failure_preserves_terminal_server_diagnosis() {
        let (request_sender, mut requests) = mpsc::unbounded();
        let client = Client::new(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        let (mut producer, receiver) = mpsc::channel(1);
        let statement = Statement::unnamed(Vec::new(), Vec::new());
        let (mut responses, _copy_mode) = client
            .inner()
            .send_copy_statement(
                RequestMessages::CopyIn(CopyInReceiver::new(receiver)),
                &statement,
                CopyMode::In,
            )
            .expect("enqueue the scripted COPY request");
        let request = requests.try_recv().expect("receive the COPY request");
        *client.terminal_server_error_handle().lock() = Some(admin_shutdown());
        drop(request);

        let error = send_initial_copy_message(
            &mut producer,
            &mut responses,
            FrontendMessage::Raw(Bytes::from_static(b"initial COPY batch")),
        )
        .await
        .expect_err("the disconnected COPY producer accepted its initial batch");

        assert_eq!(
            error.code(),
            Some(&SqlState::ADMIN_SHUTDOWN),
            "initial COPY producer failure discarded SQLSTATE 57P01: {error}"
        );
        assert!(!error.is_closed());
    }

    async fn terminal_bytes(mut receiver: CopyInReceiver) -> Bytes {
        receiver
            .next()
            .await
            .expect("producerless COPY omitted its initial query");
        match receiver
            .next()
            .await
            .expect("producerless COPY omitted its abort terminal")
        {
            FrontendMessage::Raw(bytes) => bytes,
            FrontendMessage::CopyData(_) => panic!("COPY abort was encoded as data"),
        }
    }

    #[compio::test]
    async fn simple_query_abort_ends_at_copy_fail_without_sync() {
        let bytes = terminal_bytes(CopyInReceiver::aborting_simple(
            FrontendMessage::Raw(Bytes::from_static(b"Q")),
            "simple abort",
        ))
        .await;

        assert_eq!(bytes[0], b'f');
        let frame_len = 1 + u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        assert_eq!(
            bytes.len(),
            frame_len,
            "simple CopyFail carried an extra frontend frame"
        );
    }

    #[compio::test]
    async fn extended_query_abort_keeps_its_sync_barrier() {
        let bytes = terminal_bytes(CopyInReceiver::aborting(
            FrontendMessage::Raw(Bytes::from_static(b"PBE")),
            "extended abort",
        ))
        .await;

        assert_eq!(bytes[0], b'f');
        let copy_fail_len = 1 + u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        assert_eq!(&bytes[copy_fail_len..], &[b'S', 0, 0, 0, 4]);
    }

    #[compio::test]
    async fn non_numeric_copy_command_tag_cannot_confirm_a_row_count() {
        let (request_sender, mut requests) = mpsc::unbounded();
        let client = Client::new(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        let (sender, receiver) = mpsc::channel(1);
        let statement = Statement::unnamed(Vec::new(), Vec::new());
        let (responses, copy_mode) = client
            .inner()
            .send_copy_statement(
                RequestMessages::CopyIn(CopyInReceiver::new(receiver)),
                &statement,
                CopyMode::In,
            )
            .expect("enqueue the scripted COPY request");
        let mut response_sender = requests
            .try_recv()
            .expect("receive the scripted COPY request")
            .sender;

        let mut command_frame = BytesMut::new();
        command_frame.put_u8(b'C');
        command_frame.put_u32(14);
        command_frame.extend_from_slice(b"COPY nope\0");
        let command = Message::parse(&mut command_frame)
            .expect("parse malformed-count CommandComplete")
            .expect("malformed-count CommandComplete is complete");

        let mut ready_frame = BytesMut::from(&b"Z\0\0\0\x05I"[..]);
        let ready = Message::parse(&mut ready_frame)
            .expect("parse ReadyForQuery")
            .expect("ReadyForQuery is complete");
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([
                Ok(command),
                Ok(ready),
            ])))
            .expect("deliver scripted COPY completion");

        let sink = CopyInSink::<Bytes> {
            sender,
            responses,
            response: CopyResponse::default(),
            buf: BytesMut::new(),
            state: SinkState::Reading,
            completion: None,
            _p2: PhantomData,
            copy_mode: Some(copy_mode),
        };
        let mut sink = Box::pin(sink);
        match sink.as_mut().finish().await {
            Ok(rows) => {
                panic!("COPY IN invented row count {rows} from malformed command tag")
            }
            Err(error) => assert_eq!(
                error.to_string(),
                "unexpected message from server",
                "malformed COPY count reported the wrong protocol failure"
            ),
        }
    }

    #[compio::test]
    async fn poll_ready_error_clears_copy_mode_guard() {
        use futures_util::Sink;

        let (request_sender, mut requests) = mpsc::unbounded();
        let client = Client::new(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        let (sender, receiver) = mpsc::channel(1);
        let statement = Statement::unnamed(Vec::new(), Vec::new());
        let (responses, copy_mode) = client
            .inner()
            .send_copy_statement(
                RequestMessages::CopyIn(CopyInReceiver::new(receiver)),
                &statement,
                CopyMode::In,
            )
            .expect("enqueue the scripted COPY request");
        let request = requests
            .try_recv()
            .expect("receive the scripted COPY request");
        let crate::connection::Request {
            messages,
            sender: mut response_sender,
            ..
        } = request;
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([Err(
                Error::from_db_error(admin_shutdown()),
            )])))
            .expect("deliver terminal COPY error");
        drop(messages);

        let sink = CopyInSink::<Bytes> {
            sender,
            responses,
            response: CopyResponse::default(),
            buf: BytesMut::new(),
            state: SinkState::Active,
            completion: None,
            _p2: PhantomData,
            copy_mode: Some(copy_mode),
        };
        let mut sink = Box::pin(sink);
        let error = std::future::poll_fn(|cx| sink.as_mut().poll_ready(cx))
            .await
            .expect_err("the disconnected COPY producer hid its server error");
        assert_eq!(error.code(), Some(&SqlState::ADMIN_SHUTDOWN));

        client
            .inner()
            .send(RequestMessages::Single(FrontendMessage::Raw(
                Bytes::from_static(b"next request"),
            )))
            .unwrap_or_else(|error| panic!("the recovered COPY kept rejecting commands: {error}"));
    }

    #[compio::test]
    async fn a_message_after_copy_confirmation_refuses_success() {
        let (request_sender, mut requests) = mpsc::unbounded();
        let client = Client::new(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        let (sender, receiver) = mpsc::channel(1);
        let statement = Statement::unnamed(Vec::new(), Vec::new());
        let (responses, copy_mode) = client
            .inner()
            .send_copy_statement(
                RequestMessages::CopyIn(CopyInReceiver::new(receiver)),
                &statement,
                CopyMode::In,
            )
            .expect("enqueue the scripted COPY request");
        let mut response_sender = requests
            .try_recv()
            .expect("receive the scripted COPY request")
            .sender;

        let mut command_frame = BytesMut::new();
        command_frame.put_u8(b'C');
        command_frame.put_u32(11);
        command_frame.extend_from_slice(b"COPY 1\0");
        let command = Message::parse(&mut command_frame)
            .expect("parse COPY CommandComplete")
            .expect("COPY CommandComplete is complete");

        let mut done_frame = BytesMut::from(&b"c\0\0\0\x04"[..]);
        let done = Message::parse(&mut done_frame)
            .expect("parse backend CopyDone")
            .expect("backend CopyDone is complete");
        let mut ready_frame = BytesMut::from(&b"Z\0\0\0\x05I"[..]);
        let ready = Message::parse(&mut ready_frame)
            .expect("parse ReadyForQuery")
            .expect("ReadyForQuery is complete");
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([
                Ok(command),
                Ok(done),
                Ok(ready),
            ])))
            .expect("deliver scripted COPY completion");

        let sink = CopyInSink::<Bytes> {
            sender,
            responses,
            response: CopyResponse::default(),
            buf: BytesMut::new(),
            state: SinkState::Reading,
            completion: None,
            _p2: PhantomData,
            copy_mode: Some(copy_mode),
        };
        let mut sink = Box::pin(sink);
        match sink.as_mut().finish().await {
            Ok(rows) => {
                panic!("COPY IN reported {rows} rows after an illegal post-confirmation message")
            }
            Err(error) => assert_eq!(
                error.to_string(),
                "unexpected message from server",
                "post-confirmation message reported the wrong protocol failure"
            ),
        }
    }
}
