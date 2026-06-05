// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The tokio version is a hand-cranked `Future` with `poll_read`,
// `poll_write`, `poll_flush`, `poll_shutdown` methods over
// `Framed<S, PostgresCodec>`. Compio has no `tokio_util::codec::Framed`
// equivalent; instead we use the async-fn `read_backend` /
// `write_frontend` primitives defined in `codec.rs` over a `BufStream`.
//
// ## Architecture
//
//   loop {
//       // Step 1: drain any `pending_responses` batches that couldn't
//       //          land on their sender (bounded mpsc::channel(1) was
//       //          full). This MUST run before any more reads so the
//       //          next batch for the same request can't shadow them.
//       // Step 2: if terminating & no in-flight responses, flush +
//       //          shutdown + return.
//       // Step 3: while responses are in-flight, reads must complete
//       //          atomically (no racing against the receiver) — compio
//       //          io_uring cancellation is best-effort, and a dropped
//       //          Submit may still complete in the kernel with its
//       //          owned buffer lost. Between messages we opportunistic-
//       //          ally drain queued requests via `try_recv()`.
//       // Step 4: idle (no in-flight work) — await a new request with
//       //          `.next().await`. Unsolicited server messages arriving
//       //          while idle stay in the kernel socket buffer and are
//       //          picked up on the next read. (We explicitly do not
//       //          race read vs. recv here because a cancelled read can
//       //          silently drop completed-but-unreaped bytes.)
//   }
//
// The critical cancel-safety invariant: we NEVER drop a read future
// whose associated compio submission may have in-flight bytes. Every
// `read_backend().await` runs to completion before control returns to
// the select loop.

use crate::buf_stream::BufStream;
use crate::codec::{BackendMessage, BackendMessages, FrontendMessage, read_backend, write_frontend};
use crate::copy_in::CopyInReceiver;
use crate::error::DbError;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::{AsyncMessage, Error, Notification};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use futures_channel::mpsc;
use futures_util::StreamExt;
use log::trace;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;

/// A request from a `Client` to be forwarded to the server by the
/// `Connection` task.
pub struct Request {
    pub messages: RequestMessages,
    pub sender: mpsc::Sender<BackendMessages>,
}

/// The payload of a [`Request`]: either a pre-encoded batch of frontend
/// messages (`Parse + Bind + …`) or a streaming COPY IN source.
pub enum RequestMessages {
    /// A fully pre-encoded batch of frontend messages.
    Single(FrontendMessage),
    /// A streaming COPY FROM STDIN source: the connection task drains
    /// frames from the receiver until it terminates with `CopyDone+Sync`
    /// or `CopyFail+Sync`.
    CopyIn(CopyInReceiver),
}

/// Bookkeeping for a single in-flight request: the channel we send
/// response batches back on. Matches the shape of the tokio version.
struct Response {
    sender: mpsc::Sender<BackendMessages>,
}

/// A connection to a PostgreSQL database.
///
/// This is the other half of what is returned when a new connection is
/// established. Call [`Connection::run`] from a background task to
/// drive I/O with the server. `run` resolves only when the connection
/// is closed, either because a fatal error occurred or because the
/// associated [`Client`](crate::Client) has dropped and all outstanding
/// work has completed.
#[must_use = "connection does nothing unless run"]
pub struct Connection<S, T> {
    stream: BufStream<MaybeTlsStream<S, T>>,
    parameters: HashMap<String, String>,
    receiver: mpsc::UnboundedReceiver<Request>,
    /// Async messages queued before the connection task started (notices
    /// captured during `read_info`). Delivered on the first iteration of
    /// `run`.
    delayed_notices: VecDeque<Message>,
    /// In-flight request responses, front = next to receive a batch.
    responses: VecDeque<Response>,
    /// Batches whose receiving `mpsc::Sender<BackendMessages>` slot was
    /// full when we tried to deliver them (the caller's `Responses`
    /// consumer hasn't polled yet). Each iteration drains this first via
    /// `poll_ready().await` + `start_send` before reading any more
    /// socket bytes. Matches tokio-postgres's `pending_responses` field.
    ///
    /// Without this side-channel, a second batch arriving on the wire
    /// before the consumer has drained the first would be silently
    /// dropped by `try_send`; if that batch carries `ReadyForQuery`, the
    /// consumer would block forever on `Responses::poll_next`.
    pending_responses: VecDeque<(mpsc::Sender<BackendMessages>, BackendMessages)>,
    /// Channel for async server->client messages (notices, notifications,
    /// parameter status). `None` until someone calls
    /// `Connection::notifications()`.
    async_sender: Option<mpsc::UnboundedSender<AsyncMessage>>,
}

impl<S, T> Connection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(
        stream: BufStream<MaybeTlsStream<S, T>>,
        delayed_notices: VecDeque<Message>,
        parameters: HashMap<String, String>,
        receiver: mpsc::UnboundedReceiver<Request>,
    ) -> Connection<S, T> {
        Connection {
            stream,
            parameters,
            receiver,
            delayed_notices,
            responses: VecDeque::new(),
            pending_responses: VecDeque::new(),
            async_sender: None,
        }
    }

    /// Returns the value of a runtime parameter for this connection.
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(|s| &**s)
    }

    /// Register a sink for asynchronous server->client messages (notices
    /// and LISTEN/NOTIFY notifications). Returns the receiver half.
    /// Must be called before [`run`](Self::run). If never called, async
    /// messages are logged at info/debug and discarded.
    pub fn notifications(&mut self) -> mpsc::UnboundedReceiver<AsyncMessage> {
        let (tx, rx) = mpsc::unbounded();
        self.async_sender = Some(tx);
        rx
    }

    /// Drive the connection until the client is dropped and all
    /// outstanding requests have completed, or a fatal I/O error occurs.
    pub async fn run(mut self) -> Result<(), Error> {
        // Drain any async messages captured during handshake
        // (for example notices from `read_info`) before entering the
        // main loop.
        while let Some(msg) = self.delayed_notices.pop_front() {
            route_async(&mut self.parameters, self.async_sender.as_ref(), msg)?;
        }

        self.run_serialized().await
    }

    /// The original serialized run-loop: reads and writes never overlap.
    ///
    /// Reads run to completion before control returns to the dispatch
    /// point (the cancel-safety invariant documented at the top of this
    /// file). This is the fallback path for streams that cannot be split
    /// into independent owned read/write halves (the TLS variant — rustls
    /// keeps shared session state, so `read_half` and `write_half` cannot
    /// own disjoint borrows). The splittable plain-socket path uses
    /// [`run_multiplexed`](Self::run_multiplexed) instead.
    async fn run_serialized(mut self) -> Result<(), Error> {
        let mut terminating = false;

        loop {
            // Step A — flush any batches that couldn't be delivered last
            // iteration because the downstream `Sender<BackendMessages>`
            // slot was still occupied. This uses the `poll_ready` +
            // `start_send` back-pressure dance tokio-postgres relies on:
            // when the slot is full we suspend here until the consumer
            // drains. Doing this BEFORE any read keeps batch ordering
            // correct (a second batch for the same request can never
            // overtake the first).
            while let Some((mut sender, messages)) = self.pending_responses.pop_front() {
                // Wait until the downstream channel has capacity.
                let ready = poll_fn(|cx| sender.poll_ready(cx)).await;
                match ready {
                    Ok(()) => {
                        // Consumer may have been dropped between
                        // poll_ready and start_send; ignore SendError.
                        let _ = sender.start_send(messages);
                    }
                    Err(_) => {
                        // Consumer hung up — drop the batch. This matches
                        // tokio-postgres's behaviour at
                        // `connection.rs::poll_read` when poll_ready
                        // returns Err.
                    }
                }
            }

            // Step B — clean shutdown once the client has gone away and
            // all pending work is done.
            if terminating && self.responses.is_empty() {
                self.stream.flush().await?;
                // Best-effort clean socket shutdown: closes the TLS
                // session gracefully (rustls treats silent drop as a
                // truncation attack) and emits a proper TCP FIN/ACK.
                // The server may have already half-closed; swallow the
                // expected `BrokenPipe` / `NotConnected` here.
                if let Err(e) = self.stream.get_mut().shutdown().await {
                    use std::io::ErrorKind::*;
                    if !matches!(e.kind(), BrokenPipe | NotConnected) {
                        trace!("stream shutdown non-fatal error: {e}");
                    }
                }
                trace!("connection closed");
                return Ok(());
            }

            // Step C — terminating branch: Terminate has been sent; we
            // only drain any remaining inbound bytes until we see EOF or
            // finish the in-flight response queue. No new requests.
            if terminating {
                match read_backend(&mut self.stream).await {
                    Ok(msg) => self.handle_message(msg)?,
                    Err(e) => {
                        if is_eof(&e) && self.responses.is_empty() {
                            return Ok(());
                        }
                        return Err(e);
                    }
                }
                continue;
            }

            // Step D — if responses are in-flight we MUST read the
            // socket to completion; racing the read future against
            // `receiver.next()` would drop an in-progress compio
            // submission, whose owned buffer (potentially already
            // written to by the kernel) would be discarded.
            //
            // Between messages we still want to make progress on any
            // queued requests — `receiver.try_recv()` is non-blocking
            // and safe to call between awaits.
            if !self.responses.is_empty() {
                let msg = match read_backend(&mut self.stream).await {
                    Ok(msg) => msg,
                    Err(e) => {
                        if is_eof(&e) && self.responses.is_empty() {
                            return Ok(());
                        }
                        return Err(e);
                    }
                };
                self.handle_message(msg)?;
                // Drain any requests that arrived while we were reading.
                while let Ok(request) = self.receiver.try_recv() {
                    self.handle_request(request).await?;
                }
                continue;
            }

            // Step E — idle. No responses in flight, nothing pending.
            // We await a new request. Unsolicited server messages
            // arriving here (LISTEN/NOTIFY, Notice, ParameterStatus)
            // remain in the kernel socket buffer and will be drained on
            // the next read. Choosing not to race here is a deliberate
            // trade: latency for idle notifications is bounded by the
            // arrival of the next client request, in exchange for
            // cancel-safety of the read side.
            match self.receiver.next().await {
                Some(request) => self.handle_request(request).await?,
                None => {
                    // Client side dropped. Send Terminate and begin
                    // graceful shutdown.
                    trace!("receiver closed, sending Terminate");
                    let buf = inner_encode_terminate();
                    write_frontend(&mut self.stream, FrontendMessage::Raw(buf))?;
                    self.stream.flush().await?;
                    terminating = true;
                }
            }
        }
    }

    /// Dispatch a single decoded backend frame.
    fn handle_message(&mut self, message: BackendMessage) -> Result<(), Error> {
        match message {
            BackendMessage::Async(m) => {
                route_async(&mut self.parameters, self.async_sender.as_ref(), m)
            }
            BackendMessage::Normal {
                messages,
                request_complete,
            } => self.deliver_batch(messages, request_complete),
        }
    }

    /// Deliver a normal batch to the front response channel. Mirrors
    /// tokio-postgres's `poll_read` dispatch (connection.rs lines
    /// 138-169): try to land the batch on the sender; if the slot is
    /// full, stash it in `pending_responses` and let the next loop
    /// iteration re-poll via `poll_ready`.
    fn deliver_batch(
        &mut self,
        mut messages: BackendMessages,
        request_complete: bool,
    ) -> Result<(), Error> {
        // If there's no in-flight request but the server sent us backend
        // data, surface it as an error (matches tokio version).
        let mut response = match self.responses.pop_front() {
            Some(r) => r,
            None => match messages.next().map_err(Error::parse)? {
                Some(Message::ErrorResponse(err)) => return Err(Error::db(err)),
                _ => return Err(Error::unexpected_message()),
            },
        };

        match response.sender.try_send(messages) {
            Ok(()) => {
                if !request_complete {
                    self.responses.push_front(response);
                }
            }
            Err(e) if e.is_full() => {
                // The caller's `Responses` consumer hasn't picked up the
                // previous batch yet. Stash this batch and retry from
                // the top of the loop via `pending_responses`; this is
                // exactly how tokio-postgres handles `Poll::Pending`
                // from `response.sender.poll_ready(cx)` — the batch is
                // queued and the whole loop re-enters `poll_read` on
                // the next wake.
                let messages = e.into_inner();
                self.pending_responses
                    .push_back((response.sender.clone(), messages));
                if !request_complete {
                    self.responses.push_front(response);
                }
            }
            Err(_) => {
                // Consumer dropped (`TrySendError::Disconnected`). Keep
                // paging through remaining messages so the server sees
                // us drain its response stream; just discard this batch.
                if !request_complete {
                    self.responses.push_front(response);
                }
            }
        }
        Ok(())
    }

    /// Handle a request received from the client. Pushes the response
    /// channel onto `responses` and writes the frontend messages.
    async fn handle_request(&mut self, request: Request) -> Result<(), Error> {
        self.responses.push_back(Response {
            sender: request.sender,
        });

        match request.messages {
            RequestMessages::Single(msg) => {
                write_frontend(&mut self.stream, msg)?;
                self.stream.flush().await?;
            }
            RequestMessages::CopyIn(mut receiver) => {
                // COPY FROM STDIN: stream user-supplied frames to the
                // server until the receiver signals end-of-stream. We
                // alternate send/read rather than race them — the
                // receiver IS safe to cancel (it's an mpsc channel), but
                // `read_backend` is NOT (see cancel-safety comments at
                // the top of this file).
                //
                // While the user is sending frames, the server may emit
                // an ErrorResponse (e.g., constraint violation) that we
                // need to surface to the caller via the Response
                // channel. Rather than race reads, we poll the socket
                // non-blockingly between each frame by checking whether
                // any parseable message is already in the read buffer.
                //
                // A full-blown error during copy will still flow through
                // normally: the write will fail once the server shuts
                // its half of the socket, or we'll see the ErrorResponse
                // in the batch after the CopyIn receiver closes and the
                // main run() loop reads it.
                loop {
                    match receiver.next().await {
                        Some(msg) => {
                            write_frontend(&mut self.stream, msg)?;
                            self.stream.flush().await?;
                        }
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }
}

/// Route an async server message (notice / notification / parameter
/// status update).
fn route_async(
    parameters: &mut HashMap<String, String>,
    async_sender: Option<&mpsc::UnboundedSender<AsyncMessage>>,
    msg: Message,
) -> Result<(), Error> {
    match msg {
        Message::NoticeResponse(body) => {
            let error = DbError::parse(&mut body.fields()).map_err(Error::parse)?;
            if let Some(sender) = async_sender {
                let _ = sender.unbounded_send(AsyncMessage::Notice(error));
            } else {
                log::info!("{}: {}", error.severity(), error.message());
            }
        }
        Message::NotificationResponse(body) => {
            let notification = Notification {
                process_id: body.process_id(),
                channel: body.channel().map_err(Error::parse)?.to_string(),
                payload: body.message().map_err(Error::parse)?.to_string(),
            };
            if let Some(sender) = async_sender {
                let _ = sender.unbounded_send(AsyncMessage::Notification(notification));
            } else {
                log::debug!(
                    "notification on channel {}: {}",
                    notification.channel,
                    notification.payload
                );
            }
        }
        Message::ParameterStatus(body) => {
            parameters.insert(
                body.name().map_err(Error::parse)?.to_string(),
                body.value().map_err(Error::parse)?.to_string(),
            );
        }
        _ => return Err(Error::unexpected_message()),
    }
    Ok(())
}

fn inner_encode_terminate() -> bytes::Bytes {
    let mut buf = bytes::BytesMut::new();
    frontend::terminate(&mut buf);
    buf.freeze()
}

/// Check whether an error represents a clean EOF (server closed the
/// connection). On EOF during termination we return Ok(()) rather than
/// propagating.
fn is_eof(e: &Error) -> bool {
    if let Some(io) = e.as_io() {
        matches!(io.kind(), std::io::ErrorKind::UnexpectedEof)
    } else {
        false
    }
}

// Small extension to `Error` so `is_eof` can peek at the underlying
// `io::Error`. We add this as a free function here because Error is
// otherwise opaque (factory methods only).
impl Error {
    fn as_io(&self) -> Option<&std::io::Error> {
        // Pull the source down to the first io::Error in the chain.
        let mut src: &dyn std::error::Error = self;
        loop {
            if let Some(io) = src.downcast_ref::<std::io::Error>() {
                return Some(io);
            }
            match src.source() {
                Some(s) => src = s,
                None => return None,
            }
        }
    }
}
