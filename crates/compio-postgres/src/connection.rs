// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The tokio version is a hand-cranked `Future` with `poll_read`,
// `poll_write`, `poll_flush`, `poll_shutdown` methods over
// `Framed<S, PostgresCodec>`. Compio has no `tokio_util::codec::Framed`
// equivalent; instead we use the async-fn `read_backend` /
// `write_frontend` primitives defined in `codec.rs` over a `BufStream`.
//
// ## THE cancel-safety invariant (load-bearing — do not weaken)
//
// compio is completion-based: dropping a future whose io_uring read
// submission is in flight can silently lose bytes the kernel has already
// moved into the owned buffer. So the cancel-unsafe primitive — the
// socket `read` inside `read_backend` — must NEVER be dropped while a
// submission is outstanding. Both run-loops below honour this; neither
// ever races-and-cancels a `read_backend`.
//
// ## Two run-loops (`run` picks one)
//
// * `run_multiplexed` — the splittable plain-socket path (always taken by
//   the connection pool, which uses NoTls). The socket is split into two
//   owned halves. A DEDICATED, DETACHED read task owns the read half and
//   loops `read_backend` forever, forwarding frames over a bounded
//   channel; the cancel-unsafe read lives entirely inside that task's
//   own loop and is never dropped mid-submission (even on teardown the
//   task is detached, so its current read RESOLVES — to EOF — before the
//   task exits). The main loop owns the write half and only ever
//   `select`s over CHANNELS (read channel, request receiver, COPY
//   receiver, a sender's `poll_ready`) plus a fully-awaited write flush —
//   all cancel-safe. Reads and writes therefore proceed concurrently:
//   COPY-IN no longer deadlocks (COPY-1/IO-1), idle listeners receive
//   notifications (IO-2), and later requests are written without waiting
//   for earlier responses (IO-3). See `run_multiplexed` for the full
//   FIFO / back-pressure argument.
//
// * `run_serialized` — the fallback for unsplittable streams (TLS:
//   rustls keeps shared session state across read/write, so the halves
//   cannot run independent submissions). Reads and writes never overlap;
//   every `read_backend().await` runs to completion before control
//   returns to the dispatch point. This is the original loop, preserved
//   verbatim. Its documented trade-off stands: an idle connection does
//   not read (notifications wait for the next request), and a COPY-IN the
//   server rejects mid-stream can deadlock. TLS is not used by the pool,
//   so this path carries no real platform traffic.
//
// Both paths share `Dispatch` (backend-frame routing) and the
// `pending_responses` back-pressure stash, so message handling and FIFO
// batch ordering are identical across them.

use crate::buf_stream::{BufReadHalf, BufStream, BufWriteHalf, SplitStream};
use crate::codec::{BackendMessage, BackendMessages, FrontendMessage, read_backend, write_frontend};
use crate::copy_in::CopyInReceiver;
use crate::error::DbError;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::{AsyncMessage, Error, Notification};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use futures_channel::mpsc;
use futures_util::{SinkExt, StreamExt};
use log::trace;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use std::task::Poll;

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

    /// Dispatch a single decoded backend frame (serialized path).
    /// Thin wrapper that borrows the dispatch-relevant fields and defers
    /// to the shared [`Dispatch`] logic, so the serialized and
    /// multiplexed loops route messages identically.
    fn handle_message(&mut self, message: BackendMessage) -> Result<(), Error> {
        Dispatch {
            parameters: &mut self.parameters,
            responses: &mut self.responses,
            pending_responses: &mut self.pending_responses,
            async_sender: self.async_sender.as_ref(),
        }
        .handle_message(message)
    }

    /// Handle a request received from the client (serialized path).
    /// Pushes the response channel onto `responses` and writes the
    /// frontend messages onto the unsplit stream.
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
                // server until the receiver signals end-of-stream. The
                // serialized loop CANNOT read while writing (see the
                // cancel-safety note at the top of this file), so a COPY
                // that the server rejects mid-stream can deadlock here —
                // this is exactly the COPY-1 limitation the multiplexed
                // path fixes. The TLS fallback retains this behaviour.
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

/// Borrowed view of the connection's message-dispatch state, shared by
/// the serialized and multiplexed loops so a decoded backend frame is
/// routed identically on both paths.
///
/// It holds four **disjoint** mutable borrows (plus the shared async
/// sender). The multiplexed loop keeps the socket's read/write halves in
/// separate locals, so building a `Dispatch` over the remaining state
/// never aliases the carried read future — that is what lets the read
/// future stay alive across a `deliver_batch`/`handle_message` call.
struct Dispatch<'a> {
    parameters: &'a mut HashMap<String, String>,
    responses: &'a mut VecDeque<Response>,
    pending_responses: &'a mut VecDeque<(mpsc::Sender<BackendMessages>, BackendMessages)>,
    async_sender: Option<&'a mpsc::UnboundedSender<AsyncMessage>>,
}

impl Dispatch<'_> {
    /// Dispatch a single decoded backend frame.
    fn handle_message(&mut self, message: BackendMessage) -> Result<(), Error> {
        match message {
            BackendMessage::Async(m) => route_async(self.parameters, self.async_sender, m),
            BackendMessage::Normal {
                messages,
                request_complete,
            } => self.deliver_batch(messages, request_complete),
        }
    }

    /// Deliver a normal batch to the front response channel. Mirrors
    /// tokio-postgres's `poll_read` dispatch: try to land the batch on the
    /// sender; if the slot is full, stash it in `pending_responses` and
    /// let the next loop iteration re-poll via `poll_ready`.
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

/// Map a terminal read event (`Err` frame or channel-closed) observed by the
/// multiplexed loop into the `Result` the loop should return. Shared by the
/// main `select`'s `Read` arms and the in-flush read-draining
/// ([`flush_with_read_draining`]) so both classify EOF / error / close
/// identically.
///
/// `Some(Err(e))` -> clean close (`Ok(())`) iff EOF with the response queue
/// drained, else propagate `e`. `None` (channel closed without a terminal
/// error) -> clean iff nothing is in flight, else `Error::closed()`. A
/// `Some(Ok(_))` never reaches here (those frames are dispatched inline).
fn classify_read_terminal(
    terminal: Option<Result<BackendMessage, Error>>,
    responses: &VecDeque<Response>,
) -> Result<(), Error> {
    match terminal {
        Some(Err(e)) => {
            // EOF with the response queue drained is a clean close; otherwise
            // it is a genuine error (IO-4: EOF mid-response is always an error
            // on the multiplexed path).
            if is_eof(&e) && responses.is_empty() {
                Ok(())
            } else {
                Err(e)
            }
        }
        Some(Ok(_)) | None => {
            // The read task ended (channel closed) without a terminal error.
            // Clean only if nothing is in flight.
            if responses.is_empty() {
                Ok(())
            } else {
                Err(Error::closed())
            }
        }
    }
}

/// What the main multiplexed loop's per-iteration `select` resolved to.
/// Exactly one event is returned per wake.
enum MuxEvent {
    /// A backend frame arrived from the dedicated read task (`None` =
    /// the read task ended, i.e. its channel closed).
    Read(Option<Result<BackendMessage, Error>>),
    /// A stashed `pending_responses` front sender now has capacity.
    SenderReady,
    /// A new client request was dequeued (normal mode).
    Request(Option<Request>),
    /// A COPY-IN frame was dequeued, or the COPY stream ended (`None`).
    CopyFrame(Option<FrontendMessage>),
}

/// Flush `write_half` to completion while concurrently draining the read
/// channel — the cancel-safe interleave that keeps the multiplexed loop's
/// "reads and writes proceed in the same poll" property even across a large
/// write (MUX-DEADLOCK-1).
///
/// A bare `write_half.flush().await` could deadlock: with the cap-1 read
/// channel, a write larger than the kernel send buffer issued while the
/// server is simultaneously flooding a large response wedges in the cycle
/// flush-blocked -> server-recv-full -> server-send-blocked -> client-not-
/// reading -> read-task-send-blocked -> main-loop-not-consuming. Upstream
/// tokio-postgres avoids this by polling read and write in the same poll;
/// this helper restores that by draining inbound frames (via the SAME
/// `Dispatch` / `handle_message` / `pending_responses` machinery the main
/// loop uses) until the flush resolves.
///
/// Cancel-safety: the owned `flush` future is polled to completion and is
/// NEVER dropped mid-submission — the never-drop invariant holds (it borrows
/// only `write_half`; the read-draining touches only the disjoint response
/// state, so there is no borrow conflict and no `read_backend` future is
/// raced here either).
///
/// FIFO is preserved: a stashed batch is delivered before any further inbound
/// frame is dispatched (the read branch is gated on `pending_responses`
/// being empty, exactly as in the main loop), and pending delivery is driven
/// concurrently so the socket keeps draining even when a consumer is briefly
/// behind.
///
/// Returns once the flush resolves. If a terminal read event (`Err` /
/// channel-closed) arrived from the read task during the flush, it is
/// returned as the second tuple element for the caller to act on with the
/// same logic as the main loop's `Read` arms; further read polling stops
/// after the first terminal event (but the flush is still driven to
/// completion, and any already-stashed batches keep draining).
#[allow(clippy::type_complexity)]
async fn flush_with_read_draining<W>(
    write_half: &mut BufWriteHalf<W>,
    read_rx: &mut mpsc::Receiver<Result<BackendMessage, Error>>,
    parameters: &mut HashMap<String, String>,
    responses: &mut VecDeque<Response>,
    pending_responses: &mut VecDeque<(mpsc::Sender<BackendMessages>, BackendMessages)>,
    async_sender: Option<&mpsc::UnboundedSender<AsyncMessage>>,
) -> (
    Result<(), Error>,
    Option<Option<Result<BackendMessage, Error>>>,
)
where
    W: AsyncWrite + Unpin,
{
    // The cancel-unsafe primitive is the socket read in the detached read
    // task; here we only own the WRITE flush plus channel ops, all of which
    // are safe to poll repeatedly. The flush is the only future we hold
    // across polls and we never drop it before it resolves.
    let flush = write_half.flush();
    futures_util::pin_mut!(flush);

    // At most one terminal read event (Err / closed) is recorded; once seen
    // we stop polling the read channel but keep driving the flush.
    let mut read_terminal: Option<Option<Result<BackendMessage, Error>>> = None;

    let flush_result = poll_fn(|cx| -> Poll<Result<(), Error>> {
        // (1) Drive the flush first; finishing it is the whole point.
        if let Poll::Ready(res) = flush.as_mut().poll(cx) {
            return Poll::Ready(res);
        }

        // Keep making inbound progress until the flush is ready. Loop so a
        // delivered batch immediately frees the FIFO gate for the next read
        // within this single wake.
        loop {
            // (2) Deliver a stashed batch whose sender now has room. Honour
            // the FIFO gate: while a batch is stashed, no new inbound frame
            // may be dispatched ahead of it.
            if let Some((sender, _)) = pending_responses.front_mut() {
                match sender.poll_ready(cx) {
                    Poll::Ready(_) => {
                        // Err (consumer hung up) still counts as "ready":
                        // start_send below is a no-op drop in that case.
                        if let Some((mut sender, messages)) = pending_responses.pop_front() {
                            let _ = sender.start_send(messages);
                        }
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // (3) FIFO gate clear (no stashed batch) -> dispatch one inbound
            // frame, unless we already saw a terminal read event.
            if read_terminal.is_none() {
                match read_rx.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        if let Err(e) = (Dispatch {
                            parameters,
                            responses,
                            pending_responses,
                            async_sender,
                        })
                        .handle_message(msg)
                        {
                            // A dispatch error is terminal; surface it after
                            // the flush completes (do not drop the flush).
                            read_terminal = Some(Some(Err(e)));
                        }
                        continue;
                    }
                    Poll::Ready(other) => {
                        // Err frame or channel-closed: record and stop reading.
                        read_terminal = Some(other);
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // Flush still pending, nothing left to drain this wake.
            return Poll::Pending;
        }
    })
    .await;

    (flush_result, read_terminal)
}

impl<S, T> Connection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin + SplitStream,
    T: AsyncRead + AsyncWrite + Unpin,
    // The read half is moved into a detached read task, which must be
    // `'static`. Always satisfied by the real socket halves
    // (`OwnedReadHalf<TcpStream>` / `OwnedReadHalf<UnixStream>`).
    <S as SplitStream>::ReadHalf: 'static,
{
    /// Drive the connection until the client is dropped and all
    /// outstanding requests have completed, or a fatal I/O error occurs.
    ///
    /// Splits the socket into owned read/write halves and runs the
    /// [multiplexed loop](Self::run_multiplexed) when possible (always,
    /// for the plain-socket path the pool uses). TLS streams cannot be
    /// split, so they fall back to the [serialized
    /// loop](Self::run_serialized).
    pub async fn run(mut self) -> Result<(), Error> {
        // Drain async messages captured during the handshake (e.g.
        // notices from `read_info`) before any socket I/O.
        while let Some(msg) = self.delayed_notices.pop_front() {
            route_async(&mut self.parameters, self.async_sender.as_ref(), msg)?;
        }

        // Decompose so the stream can be consumed by the split; on the
        // unsplittable (TLS) path we put the pieces back together and run
        // the serialized loop. `delayed_notices` is already empty.
        let Connection {
            stream,
            parameters,
            receiver,
            delayed_notices,
            responses,
            pending_responses,
            async_sender,
        } = self;

        match stream.try_into_split() {
            Ok((read_half, write_half)) => {
                Self::run_multiplexed(read_half, write_half, parameters, receiver, async_sender)
                    .await
            }
            Err(stream) => {
                let conn = Connection {
                    stream,
                    parameters,
                    receiver,
                    delayed_notices,
                    responses,
                    pending_responses,
                    async_sender,
                };
                conn.run_serialized().await
            }
        }
    }

    /// The multiplexed run-loop: reads and writes proceed concurrently.
    /// Fixes the serialized loop's COPY-IN deadlock (COPY-1/IO-1), the
    /// idle-listener notification gap (IO-2), and write-serialized
    /// pipelining (IO-3).
    ///
    /// ## The never-drop-an-in-flight-read invariant
    ///
    /// compio is completion-based: dropping a future whose io_uring
    /// submission is in flight can silently lose bytes the kernel has
    /// already moved into the owned buffer. The cancel-unsafe primitive is
    /// the socket `read` inside `read_backend`. Concurrency here is built
    /// so that future is **never dropped**:
    ///
    /// * A dedicated **read task** owns `read_half` outright and loops
    ///   `read_backend` forever, forwarding each decoded frame over a
    ///   bounded channel. Its in-flight `read` lives entirely inside that
    ///   task's own `loop`; nothing ever drops it mid-submission. Even on
    ///   teardown the task is detached (never cancelled): when the main
    ///   loop goes away, the task's *next* channel send fails and it exits
    ///   — after the current read has fully resolved, not during it.
    /// * The **main loop** owns `write_half` and the request/response
    ///   bookkeeping. It only ever `select`s over **channels** (the read
    ///   channel, the request receiver, the COPY receiver, a sender's
    ///   `poll_ready`) and `await`s the write **flush** to completion.
    ///   Channel ops and a fully-awaited flush are cancel-safe, so the
    ///   select dropping a not-ready branch loses nothing.
    ///
    /// Because the read task drains the socket independently, the main
    /// loop can `flush().await` a COPY frame to completion without
    /// deadlocking: the server's ErrorResponse is read concurrently by the
    /// task, unblocking the server so it keeps draining our COPY data.
    ///
    /// ## FIFO framing
    ///
    /// The read task forwards frames in wire order over a FIFO channel.
    /// `responses` is the in-order queue of awaited requests; a batch is
    /// always delivered to its front entry. When a downstream sender is
    /// full, the batch is stashed in `pending_responses` and **the main
    /// loop stops consuming the read channel** until it drains (gate: the
    /// read branch is disabled while `pending_responses` is non-empty).
    /// This is the exact ordering guarantee of tokio-postgres's
    /// `poll_response` (pending replayed before the socket is read again),
    /// so a later batch for the same request can never overtake an earlier
    /// one. The bounded read channel propagates that back-pressure to the
    /// socket (the task blocks on send → stops reading).
    async fn run_multiplexed(
        read_half: BufReadHalf<<S as SplitStream>::ReadHalf>,
        mut write_half: BufWriteHalf<<S as SplitStream>::WriteHalf>,
        mut parameters: HashMap<String, String>,
        mut receiver: mpsc::UnboundedReceiver<Request>,
        async_sender: Option<mpsc::UnboundedSender<AsyncMessage>>,
    ) -> Result<(), Error> {
        // ---- Spawn the dedicated read task. It OWNS `read_half` and loops
        // `read_backend` forever, forwarding each frame over a bounded
        // (capacity 1) channel. Bounded so back-pressure propagates to the
        // socket: when the main loop stops consuming, the task blocks on
        // `send` after one buffered frame and stops reading. On the first read
        // error it forwards the error and exits; if the main loop has gone,
        // its next `send` fails and it exits.
        //
        // The JoinHandle is RETAINED (not detached) so every exit path can
        // stop the task — see the `teardown:` block below. Without that, a
        // main loop returning for a WRITE reason (a `?`-propagated error on a
        // flush) against a half-open/partitioned peer would leave this task
        // parked forever in `read_backend().await`, leaking the task, its
        // buffers, and the shared refcounted fd (compio `into_split` clones
        // ONE fd; dropping `write_half` alone does not close it while the read
        // task still holds its clone). MUX-1.
        let (mut read_tx, mut read_rx) =
            mpsc::channel::<Result<BackendMessage, Error>>(1);
        let read_handle = compio::runtime::spawn(async move {
            let mut read_half = read_half;
            loop {
                let res = read_backend(&mut read_half).await;
                let is_err = res.is_err();
                // SinkExt::send awaits channel capacity (back-pressure).
                if read_tx.send(res).await.is_err() {
                    // Main loop dropped the receiver — connection is done.
                    break;
                }
                if is_err {
                    // Forwarded the terminal error; stop reading.
                    break;
                }
            }
        });

        let mut responses: VecDeque<Response> = VecDeque::new();
        let mut pending_responses: VecDeque<(mpsc::Sender<BackendMessages>, BackendMessages)> =
            VecDeque::new();
        // COPY-IN frame source, set while streaming a `COPY ... FROM STDIN`.
        let mut copy_in: Option<CopyInReceiver> = None;
        // The `Client` handle dropped (`receiver` closed). We do NOT send
        // `Terminate` until the response queue has drained (mirrors
        // tokio-postgres `poll_write`), then send it exactly once.
        let mut client_gone = false;
        let mut terminate_sent = false;

        // Run the loop inside an inner future so that EVERY exit path — a
        // clean `return Ok(())`, a `?`-propagated write/read error, all of it
        // — falls through to the `teardown:` block below, which stops the read
        // task and emits a FIN. `?` and `return` inside resolve this block.
        let run_result: Result<(), Error> = async {
            loop {
                // ---- Shutdown sequencing. Once the client is gone and no
                // response/COPY work remains, send Terminate once, then emit a
                // FIN via the write half's `shutdown` (mirrors the serialized
                // path; MUX-4). The server then closes; the read task forwards
                // EOF; the Read arm returns Ok.
                if client_gone && !terminate_sent && responses.is_empty() && copy_in.is_none() {
                    trace!("client gone + queue drained, sending Terminate (multiplexed)");
                    let buf = inner_encode_terminate();
                    write_frontend(&mut write_half, FrontendMessage::Raw(buf))?;
                    write_half.flush().await?;
                    // Emit a clean TCP FIN on the write side, matching the
                    // serialized loop's `stream.shutdown()`. Swallow the
                    // expected BrokenPipe / NotConnected if the server already
                    // half-closed. This also nudges a well-behaved peer to
                    // close, which resolves the read task's parked read to EOF.
                    if let Err(e) = write_half.shutdown().await {
                        use std::io::ErrorKind::{BrokenPipe, NotConnected};
                        if !matches!(e.kind(), BrokenPipe | NotConnected) {
                            trace!("write-half shutdown non-fatal error: {e}");
                        }
                    }
                    terminate_sent = true;
                }

            // ---- Gating for this iteration.
            // Read is consumed only when no stashed batch is waiting — that
            // gate preserves FIFO batch ordering (a stashed batch is always
            // re-delivered before the next inbound frame is dispatched).
            let accept_read = pending_responses.is_empty();
            let has_pending = !pending_responses.is_empty();
            // Stop accepting new requests once the client has gone or we are
            // mid-COPY. COPY frames always drain so an in-flight sink can
            // complete.
            let accept_request = !client_gone && copy_in.is_none();
            let accept_copy = copy_in.is_some();

            // ---- Single-event select. Every branch is a channel op or a
            // `poll_ready` — all cancel-safe, so a not-ready branch being
            // dropped here loses nothing (the cancel-unsafe socket read lives
            // in the detached read task, never here).
            let event = poll_fn(|cx| -> Poll<MuxEvent> {
                // (1) Inbound frame from the read task — highest priority so
                // the server's send buffer keeps draining (delivers idle
                // NOTIFYs and races COPY ErrorResponses).
                if accept_read {
                    match read_rx.poll_next_unpin(cx) {
                        Poll::Ready(item) => return Poll::Ready(MuxEvent::Read(item)),
                        Poll::Pending => {}
                    }
                }

                // (2) Back-pressure: the front stashed batch's sender has room.
                if has_pending
                    && let Some((sender, _)) = pending_responses.front_mut()
                    && let Poll::Ready(ready) = sender.poll_ready(cx)
                {
                    // Err (consumer hung up) still counts as "ready"; the
                    // SenderReady arm discards the batch in that case.
                    let _ = ready;
                    return Poll::Ready(MuxEvent::SenderReady);
                }

                // (3) New client request (normal mode).
                if accept_request
                    && let Poll::Ready(req) = receiver.poll_next_unpin(cx)
                {
                    return Poll::Ready(MuxEvent::Request(req));
                }

                // (4) COPY-IN frame (copy mode).
                if accept_copy
                    && let Some(rx) = copy_in.as_mut()
                    && let Poll::Ready(frame) = rx.poll_next_unpin(cx)
                {
                    return Poll::Ready(MuxEvent::CopyFrame(frame));
                }

                Poll::Pending
            })
            .await;

            match event {
                // ---------- Inbound frame ----------
                MuxEvent::Read(Some(Ok(msg))) => {
                    Dispatch {
                        parameters: &mut parameters,
                        responses: &mut responses,
                        pending_responses: &mut pending_responses,
                        async_sender: async_sender.as_ref(),
                    }
                    .handle_message(msg)?;
                }
                MuxEvent::Read(terminal @ (Some(Err(_)) | None)) => {
                    // EOF / error / channel-close classification, shared with
                    // the in-flush read-draining path.
                    return classify_read_terminal(terminal, &responses);
                }

                // ---------- Stashed batch deliverable ----------
                MuxEvent::SenderReady => {
                    if let Some((mut sender, messages)) = pending_responses.pop_front() {
                        // The poll above observed Ready; start_send may still
                        // fail if the consumer hung up between poll and now.
                        let _ = sender.start_send(messages);
                    }
                }

                // ---------- New request ----------
                MuxEvent::Request(Some(request)) => {
                    responses.push_back(Response {
                        sender: request.sender,
                    });
                    match request.messages {
                        RequestMessages::Single(msg) => {
                            write_frontend(&mut write_half, msg)?;
                            // Flush to completion while concurrently draining
                            // the read channel (cancel-safe: the flush future
                            // is never dropped). Interleaving read+write in the
                            // same poll is what prevents a large bidirectional
                            // exchange from wedging the cap-1 channel
                            // (MUX-DEADLOCK-1).
                            let (res, terminal) = flush_with_read_draining(
                                &mut write_half,
                                &mut read_rx,
                                &mut parameters,
                                &mut responses,
                                &mut pending_responses,
                                async_sender.as_ref(),
                            )
                            .await;
                            res?;
                            if let Some(terminal) = terminal {
                                return classify_read_terminal(terminal, &responses);
                            }
                        }
                        RequestMessages::CopyIn(rx) => {
                            // Enter COPY mode; frames stream via branch (4).
                            copy_in = Some(rx);
                        }
                    }
                }
                MuxEvent::Request(None) => {
                    // The Client handle dropped. Defer Terminate until the
                    // response queue drains (top of the loop). Stop accepting
                    // further requests.
                    trace!("receiver closed (multiplexed)");
                    client_gone = true;
                }

                // ---------- COPY-IN frame ----------
                MuxEvent::CopyFrame(Some(msg)) => {
                    write_frontend(&mut write_half, msg)?;
                    // Same cancel-safe flush+read-drain interleave as the
                    // request path: a COPY frame the server rejects mid-stream
                    // (ErrorResponse) is read concurrently, and a large COPY
                    // frame cannot wedge the cap-1 channel (MUX-DEADLOCK-1).
                    let (res, terminal) = flush_with_read_draining(
                        &mut write_half,
                        &mut read_rx,
                        &mut parameters,
                        &mut responses,
                        &mut pending_responses,
                        async_sender.as_ref(),
                    )
                    .await;
                    res?;
                    if let Some(terminal) = terminal {
                        return classify_read_terminal(terminal, &responses);
                    }
                }
                MuxEvent::CopyFrame(None) => {
                    // COPY stream ended (the receiver already appended
                    // CopyDone+Sync / CopyFail+Sync as its terminal frame).
                    copy_in = None;
                }
            }
            }
        }
        .await;

        // ---- teardown: stop the read task on EVERY exit path. ----
        //
        // On a clean close the FIN above already made the server close, so the
        // task's parked `read_backend` has resolved (EOF) and the task is
        // exiting on its own. But on a WRITE-error exit against a half-open /
        // partitioned peer, no FIN/RST is coming and the read is parked
        // forever. `JoinHandle::cancel().await` cancels the task (compio
        // defers the in-flight read's io_uring buffer reclaim — memory-safe;
        // losing in-flight bytes is fine since the connection is going away)
        // and drops the task's `read_half`, releasing its clone of the shared
        // fd. With `write_half` dropped on return, the last fd reference goes
        // and the socket actually closes. Awaiting `cancel` also guarantees no
        // orphaned task / buffers outlive `run_multiplexed`. MUX-1 / MUX-4.
        let _ = read_handle.cancel().await;

        run_result
    }
}
