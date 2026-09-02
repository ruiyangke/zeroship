// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// COPY FROM STDIN retains tokio-postgres's channel-fed framing and buffered
// sink, but startup, abort, and completion have diverged.
// `CopyInMessage::Abort` can suppress a pre-COPY terminal; explicit receiver
// states distinguish simple from extended CopyFail framing; format/copy-mode
// tracking and ReadyForQuery draining preserve diagnostics and synchronization.

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
#[cfg(test)]
use std::cell::RefCell;
use std::future;
use std::marker::PhantomData;
use std::pin::Pin;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
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

#[derive(Clone, Copy)]
enum CopyInState {
    /// Caller-fed extended-protocol COPY input is still streaming.
    Streaming,
    /// A producerless extended query must terminate with `CopyFail + Sync`.
    ExtendedFailing { reason: &'static str },
    /// A producerless simple query must terminate with `CopyFail` alone.
    SimpleFailing { reason: &'static str },
    /// The extended-protocol stream has emitted or suppressed its terminal.
    ExtendedFinished,
    /// The simple-protocol stream has emitted its terminal without `Sync`.
    SimpleFinished,
}

/// Stream of frontend messages fed to the connection task for a `COPY FROM
/// STDIN` request. The connection's request-handler branch polls `next()`
/// until the stream terminates. Extended protocol uses `CopyDone+Sync` or
/// `CopyFail+Sync`; a producerless simple query uses `CopyFail` alone because
/// that protocol emits `ReadyForQuery` without a Sync barrier.
pub struct CopyInReceiver {
    receiver: mpsc::Receiver<CopyInMessage>,
    state: CopyInState,
    #[cfg(test)]
    poll_observed: Option<Arc<AtomicBool>>,
}

#[cfg(test)]
pub(crate) struct CopyInTestProducer {
    _sender: mpsc::Sender<CopyInMessage>,
}

impl CopyInReceiver {
    fn new(receiver: mpsc::Receiver<CopyInMessage>) -> CopyInReceiver {
        CopyInReceiver {
            receiver,
            state: CopyInState::Streaming,
            #[cfg(test)]
            poll_observed: None,
        }
    }

    /// Build an open COPY producer channel for connection-state fixtures.
    ///
    /// `initial` distinguishes a request whose first frontend batch is ready
    /// from the real publication-before-send interval in `copy_in_inner`. The
    /// optional observer lets a scripted reader wait until the connection has
    /// registered the request and polled that still-empty producer.
    #[cfg(test)]
    pub(crate) fn for_connection_test(
        initial: Option<FrontendMessage>,
        poll_observed: Option<Arc<AtomicBool>>,
    ) -> (Self, CopyInTestProducer) {
        let (mut sender, receiver) = mpsc::channel(1);
        if let Some(initial) = initial {
            sender
                .try_send(CopyInMessage::Message(initial))
                .expect("a new test COPY channel accepts its initial frame");
        }
        (
            Self {
                receiver,
                state: CopyInState::Streaming,
                poll_observed,
            },
            CopyInTestProducer { _sender: sender },
        )
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
        Self {
            receiver: Self::producerless_receiver(initial),
            state: CopyInState::ExtendedFailing { reason },
            #[cfg(test)]
            poll_observed: None,
        }
    }

    /// Build the same producerless COPY for a simple-protocol `Query`.
    ///
    /// Simple-query `CopyFail` produces its own `ReadyForQuery`; appending a
    /// `Sync` would produce a second one. A transaction pooler may release the
    /// backend after the first, so the driver must neither send that redundant
    /// barrier nor account for a response to it.
    pub(crate) fn aborting_simple(initial: FrontendMessage, reason: &'static str) -> Self {
        Self {
            receiver: Self::producerless_receiver(initial),
            state: CopyInState::SimpleFailing { reason },
            #[cfg(test)]
            poll_observed: None,
        }
    }

    fn producerless_receiver(initial: FrontendMessage) -> mpsc::Receiver<CopyInMessage> {
        let (mut sender, receiver) = mpsc::channel(1);
        sender
            .try_send(CopyInMessage::Message(initial))
            .expect("a new producerless COPY channel accepts its initial frame");
        drop(sender);
        receiver
    }

    /// True after this stream emitted or suppressed its terminal
    /// CopyDone/CopyFail frame, including the extended-protocol Sync when one
    /// is required. The connection uses this to start the final server-response
    /// read clock only after that frame finishes flushing.
    pub(crate) fn is_done(&self) -> bool {
        matches!(
            self.state,
            CopyInState::ExtendedFinished | CopyInState::SimpleFinished
        )
    }

    /// Whether this producer's terminal COPY frame includes an extended-query
    /// Sync. PostgreSQL can answer both the opening Sync and this terminal Sync
    /// after an error that occurs immediately after CopyInResponse.
    pub(crate) fn terminal_includes_sync(&self) -> bool {
        !matches!(
            self.state,
            CopyInState::SimpleFailing { .. } | CopyInState::SimpleFinished
        )
    }
}

impl Stream for CopyInReceiver {
    type Item = FrontendMessage;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<FrontendMessage>> {
        #[cfg(test)]
        if let Some(observer) = &self.poll_observed {
            observer.store(true, Ordering::Release);
        }
        if self.is_done() {
            return Poll::Ready(None);
        }

        match ready!(self.receiver.poll_next_unpin(cx)) {
            Some(CopyInMessage::Message(message)) => Poll::Ready(Some(message)),
            Some(CopyInMessage::Abort) => {
                self.state = CopyInState::ExtendedFinished;
                Poll::Ready(None)
            }
            Some(CopyInMessage::Done) => {
                self.state = CopyInState::ExtendedFinished;
                let mut buf = BytesMut::new();
                frontend::copy_done(&mut buf);
                frontend::sync(&mut buf);
                Poll::Ready(Some(FrontendMessage::Raw(buf.freeze())))
            }
            None => {
                let (reason, include_sync, finished) = match self.state {
                    CopyInState::Streaming => ("", true, CopyInState::ExtendedFinished),
                    CopyInState::ExtendedFailing { reason } => {
                        (reason, true, CopyInState::ExtendedFinished)
                    }
                    CopyInState::SimpleFailing { reason } => {
                        (reason, false, CopyInState::SimpleFinished)
                    }
                    CopyInState::ExtendedFinished | CopyInState::SimpleFinished => {
                        unreachable!("finished COPY streams return before polling their channel")
                    }
                };
                self.state = finished;
                let mut buf = BytesMut::new();
                frontend::copy_fail(reason, &mut buf).expect(
                    "COPY failure reasons are fixed NUL-free driver strings that fit the protocol frame",
                );
                if include_sync {
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
    Reading,
    ReadingAfterClose,
    Finished(u64),
}

#[cfg(test)]
thread_local! {
    static BEFORE_DONE_TEST_HOOK: RefCell<Option<Box<dyn FnOnce() + Send>>> =
        RefCell::new(None);
}

#[cfg(test)]
fn set_before_done_test_hook(hook: Box<dyn FnOnce() + Send>) {
    BEFORE_DONE_TEST_HOOK.with(|slot| {
        assert!(slot.borrow_mut().replace(hook).is_none());
    });
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
                        Poll::Ready(Err(error)) => {
                            self.as_mut().clear_copy_mode();
                            return Poll::Ready(Err(error));
                        }
                    }

                    #[cfg(test)]
                    if let Some(hook) = BEFORE_DONE_TEST_HOOK.with(|slot| slot.borrow_mut().take())
                    {
                        hook();
                    }

                    // On this active sink, a successful `poll_flush` just
                    // established readiness for this same sender handle. No
                    // task runs between that poll and this send. A receiver on
                    // another thread can still close concurrently, and
                    // `start_send` reports that race below.
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
                    {
                        let mut this = self.as_mut().project();
                        // `futures_channel::mpsc::Sender::poll_close` does
                        // exactly this and returns `Ready(Ok(()))`; it has no
                        // pending or error state for us to preserve.
                        this.sender.as_mut().get_mut().disconnect();
                    }
                    *self.as_mut().project().state = SinkState::ReadingAfterClose;
                }
                SinkState::Reading | SinkState::ReadingAfterClose => {
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
        let closed_by_sink = matches!(self.state, SinkState::ReadingAfterClose);
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
            Poll::Ready(Ok(())) if disconnected && !closed_by_sink => {
                self.poll_disconnected_diagnosis(cx)
            }
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
            Ok(other) => {
                abort(&mut sender).await;
                return Err(ExecutionError::pre_bind_mismatch(&other));
            }
            Err(e) => {
                abort(&mut sender).await;
                return Err(ExecutionError::before_bind_complete(e));
            }
        }
    }

    match responses.next().await {
        Ok(Message::BindComplete) => {}
        Ok(other) => {
            abort(&mut sender).await;
            return Err(ExecutionError::pre_bind_mismatch(&other));
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
    use super::{
        CopyInReceiver, CopyInSink, SinkState, copy_in_inner, send_initial_copy_message,
        set_before_done_test_hook,
    };
    use crate::client::{Client, CopyMode, ResponseMessages};
    use crate::codec::FrontendMessage;
    use crate::config::{SslMode, SslNegotiation};
    use crate::connection::{Request, RequestMessages};
    use crate::copy_format::CopyResponse;
    use crate::error::{DbError, SqlState};
    use crate::{Error, Statement};
    use bytes::Bytes;
    use bytes::{BufMut, BytesMut};
    use futures_channel::mpsc;
    use futures_util::StreamExt;
    use futures_util::task::noop_waker;
    use postgres_protocol::message::backend::Message;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::marker::PhantomData;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    type CopySinkFixture = (
        Client,
        mpsc::UnboundedReceiver<Request>,
        Pin<Box<CopyInSink<Bytes>>>,
        CopyInReceiver,
        mpsc::Sender<ResponseMessages>,
    );

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

    fn parse_backend_frame(mut frame: BytesMut, description: &str) -> Message {
        Message::parse(&mut frame)
            .unwrap_or_else(|error| panic!("parse {description}: {error}"))
            .unwrap_or_else(|| panic!("{description} was incomplete"))
    }

    fn command_complete(tag: &[u8]) -> Message {
        let mut frame = BytesMut::new();
        frame.put_u8(b'C');
        frame.put_u32(u32::try_from(tag.len() + 5).unwrap());
        frame.extend_from_slice(tag);
        frame.put_u8(0);
        parse_backend_frame(frame, "scripted CommandComplete")
    }

    fn ready_for_query() -> Message {
        parse_backend_frame(
            BytesMut::from(&b"Z\0\0\0\x05I"[..]),
            "scripted ReadyForQuery",
        )
    }

    fn backend_copy_done() -> Message {
        parse_backend_frame(BytesMut::from(&b"c\0\0\0\x04"[..]), "scripted CopyDone")
    }

    fn copy_sink_fixture(state: SinkState) -> CopySinkFixture {
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
        let crate::connection::Request {
            messages,
            sender: response_sender,
            ..
        } = requests
            .try_recv()
            .expect("receive the scripted COPY request");
        let receiver = match messages {
            RequestMessages::CopyIn(receiver) => receiver,
            RequestMessages::Single(_) => panic!("scripted COPY request was not streaming"),
        };
        let sink = Box::pin(CopyInSink::<Bytes> {
            sender,
            responses,
            response: CopyResponse::default(),
            buf: BytesMut::new(),
            state,
            completion: None,
            _p2: PhantomData,
            copy_mode: Some(copy_mode),
        });

        (client, requests, sink, receiver, response_sender)
    }

    #[compio::test]
    async fn duplicate_copy_completion_is_rejected() {
        let (_client, _requests, mut sink, _receiver, mut response_sender) =
            copy_sink_fixture(SinkState::Reading);
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([
                Ok(command_complete(b"COPY 1")),
                Ok(command_complete(b"COPY 2")),
                Ok(ready_for_query()),
            ])))
            .expect("deliver duplicate COPY completions");

        let error = sink
            .as_mut()
            .finish()
            .await
            .expect_err("duplicate CommandComplete messages confirmed the COPY");
        assert_eq!(error.to_string(), "unexpected message from server");
    }

    #[compio::test]
    async fn ready_without_copy_completion_is_rejected() {
        let (_client, _requests, mut sink, _receiver, mut response_sender) =
            copy_sink_fixture(SinkState::Reading);
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([Ok(
                ready_for_query(),
            )])))
            .expect("deliver ReadyForQuery without CommandComplete");

        let error = sink
            .as_mut()
            .finish()
            .await
            .expect_err("ReadyForQuery alone confirmed the COPY");
        assert_eq!(error.to_string(), "unexpected message from server");
    }

    #[compio::test]
    async fn copy_completion_parse_error_survives_a_later_message() {
        let (_client, _requests, mut sink, _receiver, mut response_sender) =
            copy_sink_fixture(SinkState::Reading);
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([
                Ok(command_complete(b"COPY \xff")),
                Ok(backend_copy_done()),
                Ok(ready_for_query()),
            ])))
            .expect("deliver malformed COPY completion");

        let error = sink
            .as_mut()
            .finish()
            .await
            .expect_err("a later CopyDone erased the malformed command tag");
        assert_eq!(error.to_string(), "error parsing response from server");
    }

    #[compio::test]
    async fn pending_copy_flush_keeps_the_sink_active() {
        let (_client, _requests, mut sink, mut receiver, mut response_sender) =
            copy_sink_fixture(SinkState::Active);
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        for byte in [b'a', b'b'] {
            assert!(matches!(
                futures_util::Sink::poll_ready(sink.as_mut(), &mut context),
                Poll::Ready(Ok(()))
            ));
            futures_util::Sink::start_send(sink.as_mut(), Bytes::from(vec![byte; 4097]))
                .expect("queue backpressured COPY data");
        }

        assert!(matches!(
            sink.as_mut().poll_finish(&mut context),
            Poll::Pending
        ));
        assert!(matches!(sink.as_ref().get_ref().state, SinkState::Active));
        assert!(matches!(
            receiver.next().await,
            Some(FrontendMessage::CopyData(_))
        ));

        assert!(matches!(
            sink.as_mut().poll_finish(&mut context),
            Poll::Pending
        ));
        assert!(matches!(
            sink.as_ref().get_ref().state,
            SinkState::ReadingAfterClose
        ));
        assert!(matches!(
            receiver.next().await,
            Some(FrontendMessage::CopyData(_))
        ));
        match receiver
            .next()
            .await
            .expect("resumed finish omitted CopyDone + Sync")
        {
            FrontendMessage::Raw(bytes) => {
                assert_eq!(&bytes[..], &[b'c', 0, 0, 0, 4, b'S', 0, 0, 0, 4]);
            }
            FrontendMessage::CopyData(_) => panic!("the COPY terminal was encoded as data"),
        }

        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([
                Ok(command_complete(b"COPY 2")),
                Ok(ready_for_query()),
            ])))
            .expect("deliver COPY completion after backpressure");
        let rows = sink
            .as_mut()
            .finish()
            .await
            .expect("the resumed COPY finish failed");
        assert_eq!(rows, 2);
    }

    /// A backpressured `Sink::poll_flush` must yield `Pending` and KEEP the
    /// staged bytes, not report the flush done and drop them.
    ///
    /// `poll_flush` is the path a caller uses to force staged rows out without
    /// finishing the COPY. When the connection cannot accept another frame the
    /// only safe answer is `Pending`: returning `Ready(Ok(()))` would tell the
    /// caller its rows had been handed over while they were still sitting in
    /// `buf`, and the next `poll_flush` would be the only thing that could
    /// still send them.
    ///
    /// The small trailing item is staged WITHOUT a preceding `poll_ready` on
    /// purpose - by then readiness is already `Pending`, and a sub-4096 item
    /// never touches the channel, so buffering it is the only way to reach a
    /// flush that has something to write and nowhere to write it.
    #[compio::test]
    async fn a_backpressured_copy_flush_keeps_its_staged_bytes() {
        let (_client, _requests, mut sink, mut receiver, _response_sender) =
            copy_sink_fixture(SinkState::Active);
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        // Fill the one-slot channel so the sender stops being ready.
        for byte in [b'a', b'b'] {
            assert!(matches!(
                futures_util::Sink::poll_ready(sink.as_mut(), &mut context),
                Poll::Ready(Ok(()))
            ));
            futures_util::Sink::start_send(sink.as_mut(), Bytes::from(vec![byte; 4097]))
                .expect("queue COPY data ahead of the backpressure");
        }
        assert!(matches!(
            futures_util::Sink::poll_ready(sink.as_mut(), &mut context),
            Poll::Pending
        ));

        futures_util::Sink::start_send(sink.as_mut(), Bytes::from_static(b"staged"))
            .expect("stage a sub-threshold row behind the backpressure");
        assert_eq!(
            sink.as_ref().get_ref().buf.len(),
            b"staged".len(),
            "the small row did not stage into the buffer"
        );

        assert!(
            matches!(
                futures_util::Sink::poll_flush(sink.as_mut(), &mut context),
                Poll::Pending
            ),
            "a flush with nowhere to write reported itself complete"
        );
        assert_eq!(
            sink.as_ref().get_ref().buf.len(),
            b"staged".len(),
            "the backpressured flush discarded the bytes it could not send"
        );
        assert!(matches!(sink.as_ref().get_ref().state, SinkState::Active));

        // Control, one variable away: drain the channel and the same flush
        // completes and hands the staged bytes over.
        assert!(matches!(
            receiver.next().await,
            Some(FrontendMessage::CopyData(_))
        ));
        assert!(matches!(
            futures_util::Sink::poll_flush(sink.as_mut(), &mut context),
            Poll::Ready(Ok(())) | Poll::Pending
        ));
        assert!(
            sink.as_ref().get_ref().buf.is_empty(),
            "the unblocked flush left the staged bytes behind"
        );
    }

    /// `Sink::send` can never reach `start_send`'s finished-guard, because
    /// `poll_ready` refuses first. That makes the guard second-line defence
    /// against a caller driving the sink by hand and skipping `poll_ready` -
    /// which the `Sink` contract permits it to get wrong. Both refusals are
    /// asserted here so the ordering stays visible; do not delete the
    /// `start_send` arm as unreachable.
    #[compio::test]
    async fn a_finished_sink_refuses_data_sent_without_polling_ready() {
        let (_client, _requests, mut sink, mut receiver, _response_sender) =
            copy_sink_fixture(SinkState::Finished(7));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        // The guard a well-behaved caller hits first.
        let ready = futures_util::Sink::poll_ready(sink.as_mut(), &mut context);
        match ready {
            Poll::Ready(Err(error)) => {
                assert_eq!(error.to_string(), "COPY IN sink is already finished");
            }
            _ => panic!("a finished sink accepted poll_ready"),
        }

        // A large item bypasses the staging buffer, so without the guard this
        // would queue a CopyData frame after the COPY was already terminated.
        let error = futures_util::Sink::start_send(sink.as_mut(), Bytes::from(vec![b'a'; 4097]))
            .expect_err("a finished sink accepted a large unsolicited item");
        assert_eq!(error.to_string(), "COPY IN sink is already finished");
        assert!(matches!(
            futures_util::Stream::poll_next(Pin::new(&mut receiver), &mut context),
            Poll::Pending
        ));

        // A small item stages into `buf` instead, and `poll_flush` returns
        // early on a finished sink - so without the guard those bytes would
        // accumulate with nothing left to drain them.
        let error = futures_util::Sink::start_send(sink.as_mut(), Bytes::from_static(b"tail"))
            .expect_err("a finished sink accepted a small unsolicited item");
        assert_eq!(error.to_string(), "COPY IN sink is already finished");
        assert!(sink.as_ref().get_ref().buf.is_empty());

        assert!(matches!(
            sink.as_ref().get_ref().state,
            SinkState::Finished(7)
        ));
    }

    #[compio::test]
    async fn disconnected_finish_preserves_a_queued_server_error() {
        let (client, _requests, mut sink, receiver, mut response_sender) =
            copy_sink_fixture(SinkState::Active);
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([Err(
                Error::from_db_error(admin_shutdown()),
            )])))
            .expect("deliver terminal COPY error");
        drop(receiver);

        let error = sink
            .as_mut()
            .finish()
            .await
            .expect_err("the disconnected COPY producer hid its server error");
        assert_eq!(error.code(), Some(&SqlState::ADMIN_SHUTDOWN));
        client
            .inner()
            .send(RequestMessages::Single(FrontendMessage::Raw(
                Bytes::from_static(b"next request"),
            )))
            .unwrap_or_else(|error| panic!("finish left COPY mode armed: {error}"));
    }

    #[compio::test]
    async fn done_send_close_race_preserves_a_queued_server_error() {
        let (client, _requests, mut sink, receiver, mut response_sender) =
            copy_sink_fixture(SinkState::Active);
        response_sender
            .try_send(ResponseMessages::Observed(VecDeque::from([Err(
                Error::from_db_error(admin_shutdown()),
            )])))
            .expect("deliver terminal COPY error");
        set_before_done_test_hook(Box::new(move || drop(receiver)));

        let error = sink
            .as_mut()
            .finish()
            .await
            .expect_err("the raced COPY producer close hid its server error");
        assert_eq!(error.code(), Some(&SqlState::ADMIN_SHUTDOWN));
        client
            .inner()
            .send(RequestMessages::Single(FrontendMessage::Raw(
                Bytes::from_static(b"next request"),
            )))
            .unwrap_or_else(|error| panic!("raced finish left COPY mode armed: {error}"));
    }

    #[test]
    fn probationary_copy_in_reparses_immediately_before_bind() {
        let (request_sender, mut requests) = mpsc::unbounded();
        let client = Client::new(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );
        let statement = Statement::unnamed_with_copy_in(Vec::new(), Vec::new(), true);
        let mut copy = Box::pin(copy_in_inner::<Bytes>(
            client.inner(),
            statement,
            Some("COPY cpg_encoding_selection FROM STDIN"),
        ));

        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(copy.as_mut().poll(&mut context), Poll::Pending));

        let crate::connection::Request { messages, .. } = requests
            .try_recv()
            .expect("probationary COPY IN did not enqueue its request");
        let mut receiver = match messages {
            RequestMessages::CopyIn(receiver) => receiver,
            RequestMessages::Single(_) => panic!("probationary COPY IN was not streaming"),
        };
        let initial = match receiver.poll_next_unpin(&mut context) {
            Poll::Ready(Some(initial)) => initial,
            Poll::Ready(None) => panic!("the initial COPY batch was missing"),
            Poll::Pending => panic!("the initial COPY batch was not ready"),
        };
        let FrontendMessage::Raw(bytes) = initial else {
            panic!("the initial COPY batch was encoded as COPY data");
        };

        assert_eq!(
            bytes[0], b'P',
            "probationary COPY IN did not re-Parse immediately before Bind"
        );
    }

    async fn assert_pre_bind_mismatch_suppresses_copy_terminal(
        unnamed_sql: Option<&str>,
        unexpected: Message,
        expect_replay_permitted: bool,
    ) {
        let (request_sender, mut requests) = mpsc::unbounded();
        let client = Client::new(
            request_sender,
            SslMode::Disable,
            SslNegotiation::Postgres,
            0,
            Some(0.into()),
            None,
        );

        let copy = copy_in_inner::<Bytes>(
            client.inner(),
            Statement::unnamed(Vec::new(), Vec::new()),
            unnamed_sql,
        );
        let connection = async move {
            let request = requests
                .next()
                .await
                .expect("receive the scripted COPY request");
            let crate::connection::Request {
                messages,
                sender: mut response_sender,
                ..
            } = request;
            let mut receiver = match messages {
                RequestMessages::CopyIn(receiver) => receiver,
                RequestMessages::Single(_) => {
                    panic!("COPY IN did not enqueue a streaming request")
                }
            };

            response_sender
                .try_send(ResponseMessages::Observed(VecDeque::from([Ok(unexpected)])))
                .expect("deliver the unexpected pre-Bind response");

            match receiver
                .next()
                .await
                .expect("COPY IN omitted its initial frontend batch")
            {
                FrontendMessage::Raw(_) => {}
                FrontendMessage::CopyData(_) => {
                    panic!("COPY IN encoded its initial frontend batch as CopyData")
                }
            }
            assert!(
                receiver.next().await.is_none(),
                "a pre-Bind protocol mismatch emitted a COPY terminal frame"
            );
        };

        let (result, ()) = futures_util::future::join(copy, connection).await;
        let failure = match result {
            Ok(_) => panic!("the unexpected pre-Bind response started COPY IN"),
            Err(failure) => failure,
        };
        let (error, before_bind_complete) = failure.into_parts();
        assert_eq!(
            before_bind_complete, expect_replay_permitted,
            "the pre-Bind mismatch was classified for the wrong protocol phase"
        );
        assert_eq!(error.to_string(), "unexpected message from server");
    }

    #[compio::test]
    async fn unexpected_parse_slot_message_suppresses_copy_terminal() {
        assert_pre_bind_mismatch_suppresses_copy_terminal(
            Some("COPY scripted FROM STDIN"),
            Message::BindComplete,
            true,
        )
        .await;
    }

    #[compio::test]
    async fn unexpected_bind_slot_message_suppresses_copy_terminal() {
        assert_pre_bind_mismatch_suppresses_copy_terminal(None, Message::ParseComplete, true).await;
    }

    /// A `CopyInResponse` standing where `BindComplete` was due means the server
    /// is ALREADY in copy mode. `ExecutionError::BeforeBindComplete` is
    /// documented as the phase that "can describe PostgreSQL rejecting the
    /// named statement itself", and it is what licenses the stale-cache replay
    /// in `Client::copy_in`. A server past Bind is not a rejected statement, so
    /// replaying re-sends Parse while the backend expects CopyData.
    ///
    /// Real PostgreSQL always sends `BindComplete` first, so this needs a
    /// non-conforming peer or proxy - the same threat model `hostile_peer.rs`
    /// exists for.
    #[compio::test]
    async fn a_copy_in_response_standing_in_for_bind_complete_forbids_replay() {
        // 'G': Int8 overall format, Int16 column count.
        let mut frame = BytesMut::new();
        frame.put_u8(b'G');
        frame.put_u32(4 + 3);
        frame.put_u8(0);
        frame.put_i16(0);
        let copy_in_response = Message::parse(&mut frame)
            .expect("parse the scripted CopyInResponse")
            .expect("the scripted CopyInResponse was incomplete");

        assert_pre_bind_mismatch_suppresses_copy_terminal(None, copy_in_response, false).await;
    }

    /// The same reasoning one exchange earlier: a server that answers the
    /// re-`Parse` with a copy response is past Parse *and* Bind.
    #[compio::test]
    async fn a_copy_in_response_standing_in_for_parse_complete_forbids_replay() {
        let mut frame = BytesMut::new();
        frame.put_u8(b'G');
        frame.put_u32(4 + 3);
        frame.put_u8(0);
        frame.put_i16(0);
        let copy_in_response = Message::parse(&mut frame)
            .expect("parse the scripted CopyInResponse")
            .expect("the scripted CopyInResponse was incomplete");

        assert_pre_bind_mismatch_suppresses_copy_terminal(
            Some("COPY scripted FROM STDIN"),
            copy_in_response,
            false,
        )
        .await;
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

    #[compio::test]
    async fn flush_after_cancelled_close_preserves_copy_completion() {
        let rows = compio::time::timeout(std::time::Duration::from_secs(1), async {
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
            let mut receiver = match messages {
                RequestMessages::CopyIn(receiver) => receiver,
                RequestMessages::Single(_) => panic!("scripted COPY request was not streaming"),
            };

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

            let close_poll = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(futures_util::Sink::poll_close(sink.as_mut(), cx))
            })
            .await;
            assert!(
                matches!(close_poll, std::task::Poll::Pending),
                "the first close poll unexpectedly completed: {close_poll:?}"
            );
            assert!(
                !matches!(
                    sink.as_ref().get_ref().state,
                    SinkState::Active | SinkState::Finished(_)
                ),
                "the pending close poll did not reach response reading"
            );
            assert!(
                sink.as_ref().get_ref().sender.is_closed(),
                "the pending close poll did not close its own sender"
            );

            match receiver
                .next()
                .await
                .expect("the cancelled close omitted CopyDone + Sync")
            {
                FrontendMessage::Raw(bytes) => {
                    assert_eq!(&bytes[..], &[b'c', 0, 0, 0, 4, b'S', 0, 0, 0, 4]);
                }
                FrontendMessage::CopyData(_) => panic!("the COPY terminal was encoded as data"),
            }

            let mut command_frame = BytesMut::new();
            command_frame.put_u8(b'C');
            command_frame.put_u32(11);
            command_frame.extend_from_slice(b"COPY 0\0");
            let command = Message::parse(&mut command_frame)
                .expect("parse COPY CommandComplete")
                .expect("COPY CommandComplete is complete");
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
            drop(response_sender);

            std::future::poll_fn(|cx| futures_util::Sink::poll_flush(sink.as_mut(), cx))
                .await
                .unwrap_or_else(|error| {
                    panic!("flush swallowed the completed COPY response: {error}")
                });
            let first = sink.as_mut().finish().await?;
            let repeated = sink.as_mut().finish().await?;
            assert_eq!(repeated, first, "repeated finish changed the COPY count");
            Ok::<_, Error>(first)
        })
        .await
        .expect("cancelled-close regression timed out")
        .expect("the completed COPY failed after the cancelled close");

        assert_eq!(rows, 0);
    }
}
