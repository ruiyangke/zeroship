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
// submission is outstanding on a connection that may be reused. Both
// run-loops below honour this. The one deliberate exception is expiry of the
// configured socket-read deadline: that drops the read, returns a terminal
// `ReadTimeout`, and retires the whole protocol session because its framing
// can no longer be trusted.
//
// ## Two run-loops (`run` picks one)
//
// * `run_multiplexed` — the splittable plain-socket path. A pooled connection
//   takes this path only when its selected transport is plaintext. The socket
//   is split into two owned halves (compio `into_split` clones ONE refcounted
//   shared fd; it does not `dup`). A DEDICATED read task owns the read half
//   and loops `read_backend` forever, forwarding frames over a bounded
//   channel; the cancel-unsafe read lives entirely inside that task's own
//   loop and is never dropped mid-submission during normal operation. The
//   read task's JoinHandle is RETAINED (not detached): on a clean close the
//   server's FIN resolves the parked read to EOF, and on every exit path the
//   teardown step cancels the task — compio defers the in-flight read's
//   io_uring buffer reclaim, so cancelling is memory-safe (losing in-flight
//   bytes is acceptable on a connection that is closing). This closes the
//   half-open write-error leak (MUX-1).
//
//   The main loop owns the write half and only ever `select`s over CHANNELS
//   (read channel, request receiver, COPY receiver, a sender's `poll_ready`)
//   plus a write flush. Each request/COPY flush is interleaved with
//   read-channel draining (`flush_with_read_draining`) so reads and writes
//   make progress in the same poll, mirroring upstream tokio-postgres: the
//   owned flush future is awaited to completion (never dropped) while inbound
//   frames are dispatched, so a large bidirectional exchange cannot wedge the
//   cap-1 channel (MUX-DEADLOCK-1). COPY-IN no longer deadlocks (COPY-1/IO-1),
//   idle listeners receive notifications (IO-2), and later requests are
//   written without waiting for earlier responses (IO-3). See
//   `run_multiplexed` for the full FIFO / back-pressure argument.
//
// * `run_serialized` — the fallback for unsplittable streams (TLS:
//   rustls keeps shared session state across read/write, so the halves
//   cannot run independent submissions). Reads and writes never overlap;
//   every `read_backend().await` runs to completion before control
//   returns to the dispatch point. Its documented trade-off stands: an idle
//   connection does
//   not read (notifications wait for the next request), and a COPY-IN the
//   server rejects mid-stream can deadlock. Pooled connections that negotiate
//   TLS take this path, so both limitations are reachable in pool traffic.
//
// Both paths share `Dispatch` (backend-frame routing) and the
// `pending_responses` back-pressure stash, so message handling and FIFO
// batch ordering are identical across them.

use crate::buf_stream::{
    BufReadHalf, BufStream, BufWriteHalf, ReadDeadline, SplitStream,
};
use crate::client::{QueryObservation, ResponseMessages};
use crate::codec::{BackendMessage, BackendMessages, FrontendMessage, read_backend, write_frontend};
use crate::copy_in::CopyInReceiver;
use crate::error::DbError;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::{AsyncMessage, Error, Notification, Statement};
use compio::io::{AsyncRead, AsyncWrite};
use fallible_iterator::FallibleIterator;
use futures_channel::{mpsc, oneshot};
use futures_util::{SinkExt, StreamExt};
use log::{debug, trace};
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use parking_lot::Mutex;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Instant;

/// A request from a `Client` to be forwarded to the server by the
/// `Connection` task.
pub struct Request {
    pub messages: RequestMessages,
    pub sender: mpsc::Sender<ResponseMessages>,
    pub(crate) disposition: RequestDisposition,
    pub(crate) transaction_effect: TransactionEffect,
    pub(crate) prepare_cleanup: Option<crate::prepare::PrepareCleanup>,
    pub(crate) statement: Option<Statement>,
    pub(crate) observation: Option<QueryObservation>,
}

/// Whether a caller awaits the request outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestDisposition {
    /// A caller holds the response stream and expects delivery.
    Awaited,
    /// Drop-time cleanup whose response stream is discarded at creation.
    Housekeeping,
}

/// Whether a request can authoritatively change the session's transaction status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionEffect {
    /// The request's `ReadyForQuery` status belongs to the session state.
    MayChange,
    /// Protocol maintenance whose `ReadyForQuery` must not replace session state.
    Neutral,
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

/// Reserved transaction-status byte published synchronously by the read task
/// before it reports a terminal transport error. PostgreSQL's legal
/// ReadyForQuery bytes are `I`, `T`, and `E`, so zero cannot collide with a
/// server state. The pool checks this marker during synchronous return and
/// evicts even if the connection task has not yet dropped the request receiver.
pub(crate) const READ_RETIRED_STATUS: u8 = 0;

/// Bookkeeping for a single in-flight request: the channel we send
/// response batches back on. Matches the shape of the tokio version.
struct Response {
    sender: mpsc::Sender<ResponseMessages>,
    disposition: RequestDisposition,
    transaction_effect: TransactionEffect,
    prepare_cleanup: Option<crate::prepare::PrepareCleanup>,
    statement: Option<Statement>,
    observation: Option<QueryObservation>,
    /// Socket-read phase for this one wire response. It is shared with the
    /// write path so an answer that arrives during `flush` can complete the
    /// phase before the successful flush would otherwise activate it.
    read_obligation: ReadObligation,
}

struct PendingResponse {
    sender: mpsc::Sender<ResponseMessages>,
    messages: ResponseMessages,
    disposition: RequestDisposition,
}

impl Drop for Response {
    fn drop(&mut self) {
        if let Some(observation) = &self.observation {
            observation.connection_closed();
        }
    }
}

enum RequestOutcome {
    Continue,
    HousekeepingUndeliverable(Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadObligationState {
    /// Frontend bytes have not finished flushing, so writes cannot consume the
    /// socket-read budget.
    PendingInitialFlush,
    /// The server owes progress for this successfully-flushed response phase.
    Active,
    /// COPY IN entered input mode and PostgreSQL is waiting for caller data.
    PausedForCopyInput,
    /// CopyDone/CopyFail + Sync is flushing; the server does not owe its final
    /// response until that flush succeeds.
    PendingCopyTerminalFlush,
    /// `ReadyForQuery` arrived, possibly while the matching flush was still in
    /// progress. This state never contributes to the shared count.
    Complete,
}

/// One request's contribution to clock (3).
///
/// The state is shared between `Response` (completed by decoded backend
/// frames) and the write path (activated only by a successful flush). That is
/// what keeps a blocked socket write outside the read clock without missing a
/// very fast response that arrives while the flush future is still pending.
#[derive(Clone)]
struct ReadObligation {
    inner: Option<Rc<ReadObligationInner>>,
}

struct ReadObligationInner {
    deadline: Option<ReadDeadline>,
    state: Cell<ReadObligationState>,
}

impl ReadObligation {
    fn new(deadline: Option<&ReadDeadline>, track_copy_state: bool) -> Self {
        // The default (deadline disabled) path must not add a heap allocation
        // to every request. COPY still needs the shared state on serialized
        // transports so its startup response can be read before producer data.
        let inner = (deadline.is_some() || track_copy_state).then(|| {
            Rc::new(ReadObligationInner {
                deadline: deadline.cloned(),
                state: Cell::new(ReadObligationState::PendingInitialFlush),
            })
        });
        Self { inner }
    }

    fn activate_initial(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.state.get() == ReadObligationState::PendingInitialFlush {
            inner.state.set(ReadObligationState::Active);
            if let Some(deadline) = &inner.deadline {
                deadline.begin_response();
            }
        }
    }

    fn pause_for_copy_input(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        match inner.state.get() {
            ReadObligationState::PendingInitialFlush => {
                inner.state.set(ReadObligationState::PausedForCopyInput);
            }
            ReadObligationState::Active => {
                inner.state.set(ReadObligationState::PausedForCopyInput);
                if let Some(deadline) = &inner.deadline {
                    deadline.finish_response();
                }
            }
            ReadObligationState::PausedForCopyInput | ReadObligationState::Complete => {}
            ReadObligationState::PendingCopyTerminalFlush => {
                debug_assert!(false, "CopyInResponse arrived after COPY terminal data");
            }
        }
    }

    /// Mark the terminal COPY frame as flushing. Returns false when the server
    /// rejected the request before `CopyInResponse`, in which case no paused
    /// read phase exists to resume.
    fn prepare_copy_terminal(&self) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        if inner.state.get() == ReadObligationState::PausedForCopyInput {
            inner
                .state
                .set(ReadObligationState::PendingCopyTerminalFlush);
            true
        } else {
            false
        }
    }

    fn activate_copy_terminal(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.state.get() == ReadObligationState::PendingCopyTerminalFlush {
            inner.state.set(ReadObligationState::Active);
            if let Some(deadline) = &inner.deadline {
                deadline.begin_response();
            }
        }
    }

    fn complete(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let previous = inner.state.replace(ReadObligationState::Complete);
        if previous == ReadObligationState::Active
            && let Some(deadline) = &inner.deadline
        {
            deadline.finish_response();
        }
    }

    fn copy_startup_finished(&self) -> bool {
        self.inner.as_ref().is_some_and(|inner| {
            matches!(
                inner.state.get(),
                ReadObligationState::PausedForCopyInput | ReadObligationState::Complete
            )
        })
    }

    fn accepts_copy_input(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.state.get() == ReadObligationState::PausedForCopyInput)
    }

    fn is_complete(&self) -> bool {
        self.inner.as_ref().is_some_and(|inner| {
            inner.state.get() == ReadObligationState::Complete
        })
    }
}

fn has_awaited_response(
    responses: &VecDeque<Response>,
    pending_responses: &VecDeque<PendingResponse>,
) -> bool {
    responses
        .iter()
        .any(|response| response.disposition == RequestDisposition::Awaited)
        || pending_responses
            .iter()
            .any(|response| response.disposition == RequestDisposition::Awaited)
}

fn finish_request_write(
    result: Result<(), Error>,
    disposition: RequestDisposition,
    responses: &VecDeque<Response>,
    pending_responses: &VecDeque<PendingResponse>,
) -> Result<RequestOutcome, Error> {
    match result {
        Ok(()) => Ok(RequestOutcome::Continue),
        Err(error)
            if disposition == RequestDisposition::Housekeeping
                && !has_awaited_response(responses, pending_responses) =>
        {
            Ok(RequestOutcome::HousekeepingUndeliverable(error))
        }
        Err(error) => Err(error),
    }
}

/// A connection to a PostgreSQL database.
///
/// This is the other half of what is returned when a new connection is
/// established. Call [`Connection::run`] from a background task to
/// drive I/O with the server. `run` resolves only when the connection
/// is closed, either because a fatal error occurred or because the
/// associated [`Client`](crate::Client) has dropped and all outstanding
/// awaited work has completed.
#[must_use = "connection does nothing unless run"]
pub struct Connection<S, T> {
    stream: BufStream<MaybeTlsStream<S, T>>,
    /// Server runtime parameters, shared with the `Client` so a caller can read
    /// them (`Client::parameter`). This task is the sole writer.
    parameters: Arc<Mutex<HashMap<String, String>>>,
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
    pending_responses: VecDeque<PendingResponse>,
    /// Channel for async server->client messages (notices, notifications,
    /// parameter status). `None` until someone calls
    /// `Connection::notifications()`.
    async_sender: Option<mpsc::UnboundedSender<AsyncMessage>>,
    /// Transaction status and transaction-capable request count shared with
    /// the client. This task is the sole writer of status because it observes
    /// every server response exactly once and in wire order.
    tx_status: Arc<AtomicU8>,
    in_flight_requests: Arc<AtomicUsize>,
    /// Weak so a task stranded by compio cannot retain the client's dup after
    /// client-first teardown has synchronously released the server session.
    drop_release: Option<crate::release::ConnectionDropRelease>,
    /// Keeps this connection counted in `live_connections()` for exactly as
    /// long as it owns its socket, so `drain_connections` can wait for the
    /// socket to be released instead of guessing at a sleep.
    _live: crate::live::LiveConnectionGuard,
}

impl<S, T> Connection<S, T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// `startup_parameters` are the `ParameterStatus` values captured during
    /// the handshake; `parameters` is the map shared with the `Client`. They
    /// arrive separately because the handshake runs before the `Client` exists.
    pub(crate) fn new(
        stream: BufStream<MaybeTlsStream<S, T>>,
        delayed_notices: VecDeque<Message>,
        startup_parameters: HashMap<String, String>,
        parameters: Arc<Mutex<HashMap<String, String>>>,
        receiver: mpsc::UnboundedReceiver<Request>,
        tx_status: Arc<AtomicU8>,
        in_flight_requests: Arc<AtomicUsize>,
        drop_release: Option<crate::release::ConnectionDropRelease>,
    ) -> Connection<S, T> {
        *parameters.lock() = startup_parameters;
        Connection {
            stream,
            parameters,
            receiver,
            delayed_notices,
            responses: VecDeque::new(),
            pending_responses: VecDeque::new(),
            async_sender: None,
            tx_status,
            in_flight_requests,
            drop_release,
            _live: crate::live::LiveConnectionGuard::new(),
        }
    }

    /// Returns the value of a runtime parameter for this connection.
    ///
    /// Prefer [`Client::parameter`](crate::Client::parameter): this half is
    /// moved into a task by [`run`](Self::run) as soon as the session is
    /// usable, so there is rarely anywhere to call this from.
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<String> {
        self.parameters.lock().get(name).cloned()
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
            while let Some(PendingResponse {
                mut sender,
                messages,
                ..
            }) = self.pending_responses.pop_front()
            {
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
            // all awaited work is done. Drop-time housekeeping has no
            // receiver waiting for its response and must not delay shutdown.
            if terminating
                && !has_awaited_response(&self.responses, &self.pending_responses)
            {
                // NOT `?`: same reasoning as the Terminate write in Step E -
                // the client is gone and the socket may already be released.
                if let Err(e) = self.stream.flush().await {
                    trace!("final flush failed, client already gone: {e}");
                }
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
            // finish the awaited responses. No new requests.
            if terminating {
                match read_backend(&mut self.stream).await {
                    Ok(msg) => self.handle_message(msg)?,
                    Err(e) => {
                        self.record_terminal_read(&e);
                        if is_eof(&e)
                            && !has_awaited_response(&self.responses, &self.pending_responses)
                        {
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
                        self.record_terminal_read(&e);
                        if is_eof(&e)
                            && !has_awaited_response(&self.responses, &self.pending_responses)
                        {
                            return Ok(());
                        }
                        return Err(e);
                    }
                };
                self.handle_message(msg)?;
                // Drain any requests that arrived while we were reading.
                while let Ok(request) = self.receiver.try_recv() {
                    if let RequestOutcome::HousekeepingUndeliverable(error) =
                        self.handle_request(request).await?
                    {
                        debug!(
                            "housekeeping request was not delivered; closing connection: {error}"
                        );
                        return Ok(());
                    }
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
                Some(request) => {
                    if let RequestOutcome::HousekeepingUndeliverable(error) =
                        self.handle_request(request).await?
                    {
                        debug!(
                            "housekeeping request was not delivered; closing connection: {error}"
                        );
                        return Ok(());
                    }
                }
                None => {
                    // Client side dropped. Send Terminate and begin
                    // graceful shutdown.
                    trace!("receiver closed, sending Terminate");
                    let buf = inner_encode_terminate();
                    // NOT `?`, for the reason given on the multiplexed loop's
                    // Terminate: the client half is gone, so there is nobody
                    // left to report to, and the socket has usually been
                    // released already (`crate::release`).
                    let mut goodbye = write_frontend(&mut self.stream, FrontendMessage::Raw(buf));
                    if goodbye.is_ok() {
                        goodbye = self.stream.flush().await;
                    }
                    if let Err(e) = goodbye {
                        trace!("Terminate not delivered, client already gone: {e}");
                    }
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
            parameters: &self.parameters,
            responses: &mut self.responses,
            pending_responses: &mut self.pending_responses,
            async_sender: self.async_sender.as_ref(),
            tx_status: &self.tx_status,
            in_flight_requests: &self.in_flight_requests,
        }
        .handle_message(message)
    }

    fn record_terminal_read(&mut self, error: &Error) {
        // Publish poison before the operation receives its error: a pooled
        // borrower may drop immediately and synchronous return must evict it.
        self.tx_status
            .store(READ_RETIRED_STATUS, Ordering::Release);
        publish_read_timeout(error, &mut self.responses);
    }

    /// Handle a request received from the client (serialized path).
    /// Pushes the response channel onto `responses` and writes the
    /// frontend messages onto the unsplit stream.
    async fn handle_request(&mut self, request: Request) -> Result<RequestOutcome, Error> {
        let Request {
            messages,
            sender,
            disposition,
            transaction_effect,
            prepare_cleanup,
            statement,
            observation,
        } = request;
        let copy_observation = observation.clone();
        let is_copy = matches!(&messages, RequestMessages::CopyIn(_));
        let read_obligation =
            ReadObligation::new(self.stream.read_deadline().as_ref(), is_copy);
        self.responses.push_back(Response {
            sender,
            disposition,
            transaction_effect,
            prepare_cleanup,
            statement,
            observation,
            read_obligation: read_obligation.clone(),
        });

        match messages {
            RequestMessages::Single(msg) => {
                let mut result = write_frontend(&mut self.stream, msg);
                if result.is_ok() {
                    result = self.stream.flush().await;
                }
                if result.is_ok() {
                    // Writes are outside clock (3). Only a frontend batch that
                    // reached the transport makes PostgreSQL owe a response.
                    read_obligation.activate_initial();
                }
                finish_request_write(
                    result,
                    disposition,
                    &self.responses,
                    &self.pending_responses,
                )
            }
            RequestMessages::CopyIn(mut receiver) => {
                debug_assert_eq!(disposition, RequestDisposition::Awaited);
                debug_assert_eq!(transaction_effect, TransactionEffect::MayChange);
                // COPY FROM STDIN begins with the encoded query. Read through
                // CopyInResponse (or the rejecting ReadyForQuery) before
                // waiting for producer data: the producer itself waits for
                // that response, and leaving no read submitted here would both
                // deadlock and leave clock (3) unable to observe server
                // silence. Once COPY input begins the serialized loop still
                // cannot read while writing, so its documented mid-stream
                // rejection limitation remains.
                let mut initial_flushed = false;
                loop {
                    match receiver.next().await {
                        Some(msg) => {
                            let terminal = receiver.is_done();
                            let resume_terminal = terminal
                                && read_obligation.prepare_copy_terminal();
                            if let Some(observation) = &copy_observation {
                                observation.inspect_frontend(&msg);
                            }
                            write_frontend(&mut self.stream, msg)?;
                            self.stream.flush().await?;
                            if !initial_flushed {
                                read_obligation.activate_initial();
                                initial_flushed = true;

                                while !read_obligation.copy_startup_finished() {
                                    let message = match read_backend(&mut self.stream).await {
                                        Ok(message) => message,
                                        Err(error) => {
                                            self.record_terminal_read(&error);
                                            return Err(error);
                                        }
                                    };
                                    self.handle_message(message)?;
                                }

                                // CopyInResponse may have arrived in a second
                                // decoder batch while BindComplete still fills
                                // the consumer's one-slot channel. Deliver it
                                // before awaiting producer data, because the
                                // producer cannot proceed until it sees that
                                // very response.
                                while let Some(PendingResponse {
                                    mut sender,
                                    messages,
                                    ..
                                }) = self.pending_responses.pop_front()
                                {
                                    if poll_fn(|cx| sender.poll_ready(cx)).await.is_ok() {
                                        let _ = sender.start_send(messages);
                                    }
                                }
                                if read_obligation.is_complete() {
                                    return Ok(RequestOutcome::Continue);
                                }
                            } else if resume_terminal {
                                read_obligation.activate_copy_terminal();
                            }
                        }
                        None => break,
                    }
                }
                Ok(RequestOutcome::Continue)
            }
        }
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
    /// Shared, not `&mut`: the map lives behind a `Mutex` so the `Client` can
    /// read it, which also takes one borrow out of the disjointness argument
    /// above.
    parameters: &'a Mutex<HashMap<String, String>>,
    responses: &'a mut VecDeque<Response>,
    pending_responses: &'a mut VecDeque<PendingResponse>,
    async_sender: Option<&'a mpsc::UnboundedSender<AsyncMessage>>,
    tx_status: &'a AtomicU8,
    in_flight_requests: &'a AtomicUsize,
}

impl Dispatch<'_> {
    /// Dispatch a single decoded backend frame.
    fn handle_message(&mut self, message: BackendMessage) -> Result<(), Error> {
        match message {
            BackendMessage::Async { message, .. } => {
                route_async(self.parameters, self.async_sender, message)
            }
            BackendMessage::Normal {
                messages,
                request_complete,
            } => {
                let ready_status = if request_complete {
                    Some(
                        messages
                            .ready_for_query_status()
                            .ok_or_else(Error::unexpected_message)?,
                    )
                } else {
                    None
                };
                self.deliver_batch(messages, ready_status)
            }
        }
    }

    /// Deliver a normal batch to the front response channel. Mirrors
    /// tokio-postgres's `poll_read` dispatch: try to land the batch on the
    /// sender; if the slot is full, stash it in `pending_responses` and
    /// let the next loop iteration re-poll via `poll_ready`.
    fn deliver_batch(
        &mut self,
        mut messages: BackendMessages,
        ready_status: Option<u8>,
    ) -> Result<(), Error> {
        let request_complete = ready_status.is_some();
        let entered_copy_input = !request_complete
            && messages.contains_tag(postgres_protocol::message::backend::COPY_IN_RESPONSE_TAG);
        // If there's no in-flight request but the server sent us backend
        // data, surface it as an error (matches tokio version).
        let mut response = match self.responses.pop_front() {
            Some(r) => r,
            None => match messages.next().map_err(Error::parse)? {
                Some(Message::ErrorResponse(err)) => return Err(Error::db(err)),
                _ => return Err(Error::unexpected_message()),
            },
        };
        let completed_at = (request_complete
            && response
                .observation
                .as_ref()
                .is_some_and(QueryObservation::filters_by_elapsed))
        .then(Instant::now);

        if let Some(cleanup) = response.prepare_cleanup.as_ref() {
            match messages.first_tag() {
                Some(postgres_protocol::message::backend::PARSE_COMPLETE_TAG) => {
                    cleanup.observe(true);
                    response.prepare_cleanup = None;
                }
                Some(postgres_protocol::message::backend::ERROR_RESPONSE_TAG) => {
                    cleanup.observe(false);
                    response.prepare_cleanup = None;
                }
                _ => {}
            }
        }

        // The response consumer may already be gone, but this dispatch point
        // still sees every server error. Retiring poison here keeps a cancelled
        // borrower from returning it to the pool for somebody else to hit.
        if let Some(statement) = response.statement.as_ref()
            && let Some(body) = messages.error_response().map_err(Error::parse)?
        {
            statement.invalidate_cache_on_error(&Error::db(body));
        }

        if let Some(status) = ready_status
            && response.transaction_effect == TransactionEffect::MayChange
        {
            self.tx_status.store(status, Ordering::Relaxed);
            if self
                .in_flight_requests
                .fetch_update(Ordering::Release, Ordering::Relaxed, |count| {
                    count.checked_sub(1)
                })
                .is_err()
            {
                return Err(Error::unexpected_message());
            }
        }

        // Update clock (3) before this batch can be trapped behind its
        // consumer and before the split reader is acknowledged to read ahead.
        // ReadyForQuery completes one successfully-flushed request. A
        // CopyInResponse pauses that request because PostgreSQL is now waiting
        // for client input rather than owing socket bytes.
        if request_complete {
            response.read_obligation.complete();
        } else if entered_copy_input {
            response.read_obligation.pause_for_copy_input();
        }

        let (messages, completion_observation) =
            observe_response_batch(&mut response, messages, completed_at);

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
                self.pending_responses.push_back(PendingResponse {
                    sender: response.sender.clone(),
                    messages,
                    disposition: response.disposition,
                });
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
        if request_complete
            && let Some(observation) = completion_observation
        {
            let completed_at = completed_at.unwrap_or_else(Instant::now);
            observation.server_complete(completed_at);
        }
        Ok(())
    }
}

fn observe_response_batch(
    response: &mut Response,
    mut messages: BackendMessages,
    completed_at: Option<Instant>,
) -> (ResponseMessages, Option<QueryObservation>) {
    let observation = response
        .observation
        .as_ref()
        .filter(|observation| observation.is_execution());
    let completion_filtered = observation.is_some_and(|observation| {
        completed_at.is_some_and(|at| observation.filter_completed_at(at))
    });
    let messages = if let Some(observation) = observation.filter(|_| !completion_filtered) {
        let mut observed = VecDeque::new();
        loop {
            match messages.next() {
                Ok(Some(message)) => {
                    observation.observe_server_message(&message);
                    observed.push_back(Ok(message));
                }
                Ok(None) => break,
                Err(error) => {
                    observed.push_back(Err(Error::parse(error)));
                    break;
                }
            }
        }
        ResponseMessages::Observed(observed)
    } else if completion_filtered {
        ResponseMessages::Filtered(messages)
    } else {
        ResponseMessages::Raw(messages)
    };
    let completion_observation = if completion_filtered {
        response.observation = None;
        None
    } else {
        response.observation.clone()
    };
    (messages, completion_observation)
}

/// Route an async server message (notice / notification / parameter
/// status update).
///
/// Takes the parameter map by shared reference and locks it only in the
/// `ParameterStatus` arm: this runs inside the multiplexed loop, where the
/// carried read future is alive, so a guard must not be held across an await
/// (`clippy::await_holding_lock` is deny-level here). Nothing in this function
/// awaits.
fn route_async(
    parameters: &Mutex<HashMap<String, String>>,
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
            let name = body.name().map_err(Error::parse)?.to_string();
            let value = body.value().map_err(Error::parse)?.to_string();
            parameters.lock().insert(name, value);
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

/// Map a terminal read event (I/O error or reader-channel close) observed by the
/// multiplexed loop into the `Result` the loop should return. Shared by the
/// main `select` and the in-flush read-draining
/// ([`flush_with_read_draining`]) so both classify EOF / error / close
/// identically.
///
/// `Some(e)` -> clean close (`Ok(())`) iff EOF with no awaited response, else
/// propagate `e`. `None` (channel closed without a terminal error) ->
/// clean iff no awaited response is in flight, else `Error::closed()`. A
/// decoded backend frames and ordinary I/O errors share one FIFO channel;
/// ReadTimeout alone arrives on the acknowledged out-of-band path.
fn publish_read_timeout(error: &Error, responses: &mut VecDeque<Response>) {
    if !error.is_read_timeout() {
        return;
    }

    // The driver error remains owned by Connection::run, so each in-flight
    // response needs its own error value. `try_send` cannot block retirement;
    // the ordinary stalled-query case has an empty capacity-one channel and
    // receives the distinguishable error directly. A consumer that has filled
    // its own channel is application backpressure, not a socket-read wait.
    for response in responses {
        let Some(response_error) = error.duplicate_read_timeout() else {
            unreachable!("read-timeout classification changed while publishing it");
        };
        let _ = response.sender.try_send(ResponseMessages::Observed(
            VecDeque::from([Err(response_error)]),
        ));
    }
}

fn classify_read_terminal(
    terminal: Option<Error>,
    responses: &mut VecDeque<Response>,
    pending_responses: &VecDeque<PendingResponse>,
) -> Result<(), Error> {
    match terminal {
        Some(e) => {
            publish_read_timeout(&e, responses);
            // EOF with no awaited response is a clean close; otherwise it is
            // a genuine error (IO-4: EOF mid-awaited-response is always an
            // error on the multiplexed path).
            if is_eof(&e) && !has_awaited_response(responses, pending_responses) {
                Ok(())
            } else {
                Err(e)
            }
        }
        None => {
            // The read task ended (channel closed) without a terminal error.
            // Clean only if nobody awaits an in-flight outcome.
            if has_awaited_response(responses, pending_responses) {
                Err(Error::closed())
            } else {
                Ok(())
            }
        }
    }
}

fn take_captured_non_eof_terminal(
    terminal: &mut Option<Option<Error>>,
) -> Option<Error> {
    if terminal
        .as_ref()
        .is_some_and(|terminal| terminal.as_ref().is_some_and(|error| !is_eof(error)))
    {
        terminal.take().flatten()
    } else {
        None
    }
}

/// One decoded frame plus an optional acknowledgement. Clock (3) enables the
/// acknowledgement so dispatch updates protocol state before read-ahead; the
/// default, deadline-disabled path pays no oneshot allocation or round-trip.
struct ReadEnvelope {
    message: BackendMessage,
    acknowledgement: Option<oneshot::Sender<()>>,
}

/// Wire-ordered output from the dedicated reader. Ordinary EOF and I/O errors
/// travel here after every frame decoded before them; only [`Error::is_read_timeout`]
/// uses the out-of-band terminal path because a configured deadline ACKs every
/// decoded frame before the reader can submit the read that times out.
enum ReadEvent {
    Message(ReadEnvelope),
    Terminal(Error),
}

/// Publish a reader failure without allowing ordinary EOF/I/O errors to pass
/// frames the same reader decoded first. ReadTimeout is the exception: the
/// configured path acknowledges each frame before submitting the next read,
/// so no earlier frame exists when that read's clock expires.
async fn publish_reader_failure(
    error: Error,
    read_tx: &mut mpsc::Sender<ReadEvent>,
    read_terminal_tx: &mpsc::UnboundedSender<Error>,
) {
    if error.is_read_timeout() {
        let _ = read_terminal_tx.unbounded_send(error);
    } else {
        let _ = read_tx.send(ReadEvent::Terminal(error)).await;
    }
}

/// What the main multiplexed loop's per-iteration `select` resolved to.
/// Exactly one event is returned per wake.
enum MuxEvent {
    /// A backend frame arrived from the dedicated read task.
    Read(Option<ReadEvent>),
    /// A read timeout bypassed the backpressured frame channel. `None` means
    /// the channel unexpectedly closed without publishing its timeout.
    ReadTerminal(Option<Error>),
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
/// completion, and any already-stashed batches keep draining). A read timeout
/// is different: the session is already irrecoverable, so the helper returns
/// immediately and deliberately drops a pending write rather than letting it
/// keep `Connection::run` alive forever on a raw stream without a shutdown
/// handle.
#[allow(clippy::type_complexity)]
async fn flush_with_read_draining<W>(
    write_half: &mut BufWriteHalf<W>,
    read_rx: &mut mpsc::Receiver<ReadEvent>,
    read_terminal_rx: &mut mpsc::UnboundedReceiver<Error>,
    parameters: &Mutex<HashMap<String, String>>,
    responses: &mut VecDeque<Response>,
    pending_responses: &mut VecDeque<PendingResponse>,
    async_sender: Option<&mpsc::UnboundedSender<AsyncMessage>>,
    tx_status: &AtomicU8,
    in_flight_requests: &AtomicUsize,
) -> (Result<(), Error>, Option<Option<Error>>)
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
    let mut read_terminal: Option<Option<Error>> = None;

    let flush_result = poll_fn(|cx| -> Poll<Result<(), Error>> {
        // (1) ReadTimeout is out-of-band and outranks every FIFO gate. The
        // reader published pool-visible poison before sending it, so a
        // backpressured response consumer cannot delay retirement.
        if read_terminal.is_none()
            && let Poll::Ready(terminal) = read_terminal_rx.poll_next_unpin(cx)
        {
            read_terminal = Some(terminal);
        }

        if read_terminal.as_ref().is_some_and(|terminal| {
            terminal.as_ref().is_some_and(Error::is_read_timeout)
        }) {
            // This is the one safe write-cancellation point: the timed-out
            // read may already have consumed half a frame, so this connection
            // cannot be reused regardless of how much of the write completed.
            return Poll::Ready(Ok(()));
        }

        // (2) Drive the flush; finishing it is required for cancel safety even
        // after the read side has retired the physical socket.
        if let Poll::Ready(res) = flush.as_mut().poll(cx) {
            return Poll::Ready(res);
        }

        // Keep making inbound progress until the flush is ready. Loop so a
        // delivered batch immediately frees the FIFO gate for the next read
        // within this single wake.
        loop {
            // (3) Deliver a stashed batch whose sender now has room. Honour
            // the FIFO gate: while a batch is stashed, no new inbound frame
            // may be dispatched ahead of it.
            if let Some(response) = pending_responses.front_mut() {
                match response.sender.poll_ready(cx) {
                    Poll::Ready(_) => {
                        // Err (consumer hung up) still counts as "ready":
                        // start_send below is a no-op drop in that case.
                        if let Some(PendingResponse {
                            mut sender,
                            messages,
                            ..
                        }) = pending_responses.pop_front()
                        {
                            let _ = sender.start_send(messages);
                        }
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // (4) FIFO gate clear (no stashed batch) -> dispatch one inbound
            // frame, unless we already saw a terminal read event.
            if read_terminal.is_none() {
                match read_rx.poll_next_unpin(cx) {
                    Poll::Ready(Some(ReadEvent::Message(ReadEnvelope {
                        message,
                        acknowledgement,
                    }))) => {
                        let result = (Dispatch {
                            parameters,
                            responses,
                            pending_responses,
                            async_sender,
                            tx_status,
                            in_flight_requests,
                        })
                        .handle_message(message);
                        // Dispatch has now completed or paused the matching
                        // obligation. Only now may the reader submit another
                        // socket read.
                        if let Some(acknowledgement) = acknowledgement {
                            let _ = acknowledgement.send(());
                        }
                        if let Err(e) = result {
                            // A dispatch error is terminal; surface it after
                            // the flush completes (do not drop the flush).
                            read_terminal = Some(Some(e));
                        }
                        continue;
                    }
                    Poll::Ready(Some(ReadEvent::Terminal(error))) => {
                        read_terminal = Some(Some(error));
                        continue;
                    }
                    Poll::Ready(None) => {
                        // The reader ended without a published I/O error.
                        read_terminal = Some(None);
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
    /// Drive the connection until the client is dropped and all awaited
    /// requests have completed, or a fatal I/O error occurs.
    ///
    /// Splits the socket into owned read/write halves and runs the
    /// [multiplexed loop](Self::run_multiplexed) when possible (always when the
    /// selected transport is plaintext). TLS streams, including pooled
    /// connections that negotiate TLS, cannot be split, so they fall back to
    /// the [serialized loop](Self::run_serialized).
    pub async fn run(mut self) -> Result<(), Error> {
        // The field covers a Connection discarded before `run`; this local
        // spans every poll so cancellation and unwinding release it too.
        let _drop_release = self.drop_release.take();
        self.run_inner(_drop_release.clone()).await
    }

    async fn run_inner(
        mut self,
        read_error_release: Option<crate::release::ConnectionDropRelease>,
    ) -> Result<(), Error> {
        // Drain async messages captured during the handshake (e.g.
        // notices from `read_info`) before any socket I/O.
        while let Some(msg) = self.delayed_notices.pop_front() {
            route_async(&self.parameters, self.async_sender.as_ref(), msg)?;
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
            tx_status,
            in_flight_requests,
            drop_release: _,
            _live,
        } = self;

        let read_deadline = stream.read_deadline();

        match stream.try_into_split() {
            Ok((read_half, write_half)) => {
                // `_live` must outlive the loop: the split halves still own the
                // socket, so the connection is not released until they are.
                let _live = _live;
                Self::run_multiplexed(
                    read_half,
                    write_half,
                    parameters,
                    receiver,
                    async_sender,
                    tx_status,
                    in_flight_requests,
                    read_deadline,
                    read_error_release,
                    _live.clone(),
                )
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
                    tx_status,
                    in_flight_requests,
                    drop_release: None,
                    _live,
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
    ///   task's own `loop`; nothing ever drops it mid-submission during
    ///   normal operation. The task's `JoinHandle` is RETAINED so the teardown
    ///   step can stop it on every exit path (see below); the only place the
    ///   read future is ever dropped is that teardown `cancel`, where compio
    ///   defers the `io_uring` buffer reclaim (memory-safe) and the connection
    ///   is closing anyway so losing in-flight bytes is acceptable.
    /// * The **main loop** owns `write_half` and the request/response
    ///   bookkeeping. It only ever `select`s over **channels** (the read
    ///   channel, the request receiver, the COPY receiver, a sender's
    ///   `poll_ready`) and drives the write **flush** to completion. Channel
    ///   ops are cancel-safe, and each request/COPY flush runs through
    ///   [`flush_with_read_draining`], which carries the owned flush future to
    ///   completion (never dropping it) while concurrently draining inbound
    ///   frames — so the cancel-unsafe read is never raced here either.
    ///
    /// Because the read channel is drained concurrently with the flush (both
    /// by the dedicated read task AND by the in-flush draining), the main loop
    /// can flush a large request or COPY frame to completion without
    /// deadlocking: the server's response / ErrorResponse is read
    /// concurrently, unblocking the server so it keeps draining our data
    /// (MUX-DEADLOCK-1).
    ///
    /// ## Teardown
    ///
    /// The loop runs inside an inner future; on EVERY exit (clean `Ok`, a
    /// `?`-propagated write/read error) control falls through to a teardown
    /// step that `cancel().await`s the read task and (on the clean path)
    /// `shutdown`s the write half to emit a FIN. This guarantees the read task
    /// and its fd clone are released even when the main loop returns for a
    /// write reason against a half-open peer (MUX-1 / MUX-4).
    ///
    /// ## FIFO framing
    ///
    /// The read task forwards frames in wire order over a FIFO channel.
    /// `responses` is the in-order queue of in-flight requests; a batch is
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
        parameters: Arc<Mutex<HashMap<String, String>>>,
        mut receiver: mpsc::UnboundedReceiver<Request>,
        async_sender: Option<mpsc::UnboundedSender<AsyncMessage>>,
        tx_status: Arc<AtomicU8>,
        in_flight_requests: Arc<AtomicUsize>,
        read_deadline: Option<ReadDeadline>,
        read_error_release: Option<crate::release::ConnectionDropRelease>,
        _read_live: crate::live::LiveConnectionGuard,
    ) -> Result<(), Error> {
        // ---- Spawn the dedicated read task. It OWNS `read_half` and loops
        // `read_backend` forever, forwarding each frame over a bounded
        // (capacity 1) channel. Bounded so back-pressure propagates to the
        // socket: when the main loop stops consuming, the task blocks on
        // `send` after one buffered frame and stops reading. On the first read
        // decoded frame, a configured deadline waits for an acknowledgement
        // so dispatch can complete ReadyForQuery / COPY clock transitions
        // before read-ahead. The default path skips that allocation entirely.
        // ReadTimeout uses a separate unbounded channel, synchronously marks
        // pool-visible poison, and shuts down the physical socket, so response
        // backpressure can never make a timed-out pooled entry look healthy.
        // Ordinary EOF/I/O errors stay behind preceding decoded frames in the
        // bounded FIFO; their pool poison is still published immediately.
        //
        // The JoinHandle is RETAINED (not detached) so every exit path can
        // stop the task — see the `teardown:` block below. Without that, a
        // main loop returning for a WRITE reason (a `?`-propagated error on a
        // flush) against a half-open/partitioned peer would leave this task
        // parked forever in `read_backend().await`, leaking the task, its
        // buffers, and the shared refcounted fd (compio `into_split` clones
        // ONE fd; dropping `write_half` alone does not close it while the read
        // task still holds its clone). MUX-1.
        let (mut read_tx, mut read_rx) = mpsc::channel::<ReadEvent>(1);
        let (read_terminal_tx, mut read_terminal_rx) = mpsc::unbounded::<Error>();
        // Only a timeout is sent out of band. Keep this sender alive in the
        // main loop so an ordinary reader exit cannot close the channel and
        // masquerade as a priority terminal event ahead of its FIFO frames.
        let _read_terminal_guard = read_terminal_tx.clone();
        let read_error_status = Arc::clone(&tx_status);
        let acknowledge_reads = read_deadline.is_some();
        let read_handle = compio::runtime::spawn(async move {
            // Keep the one connection count armed until this task releases
            // the split read half, including deferred task cancellation.
            let _read_live = _read_live;
            let mut read_half = read_half;
            loop {
                match read_backend(&mut read_half).await {
                    Ok(message) => {
                        let (acknowledgement, acknowledged) = if acknowledge_reads {
                            let (sender, receiver) = oneshot::channel();
                            (Some(sender), Some(receiver))
                        } else {
                            (None, None)
                        };
                        // SinkExt::send awaits channel capacity
                        // (backpressure). With clock (3), the acknowledgement
                        // prevents a speculative next read before dispatch
                        // updates state for this frame.
                        if read_tx
                            .send(ReadEvent::Message(ReadEnvelope {
                                message,
                                acknowledgement,
                            }))
                            .await
                            .is_err()
                        {
                            // Main loop dropped the frame channel; connection
                            // is already ending.
                            break;
                        }
                        if let Some(acknowledged) = acknowledged
                            && acknowledged.await.is_err()
                        {
                            // Main loop dropped the acknowledgement side;
                            // connection is already ending.
                            break;
                        }
                    }
                    Err(error) => {
                        // Publish poison before waking any observer. Pool
                        // return is synchronous and can race the connection
                        // task; this marker makes that path evict even before
                        // the request receiver drops. Socket shutdown also
                        // wakes a compio operation whose cancellation alone
                        // may not release its fd.
                        read_error_status.store(READ_RETIRED_STATUS, Ordering::Release);
                        if let Some(release) = &read_error_release {
                            release.shutdown();
                        }
                        publish_reader_failure(error, &mut read_tx, &read_terminal_tx).await;
                        break;
                    }
                }
            }
        });

        let mut responses: VecDeque<Response> = VecDeque::new();
        let mut pending_responses: VecDeque<PendingResponse> = VecDeque::new();
        // COPY-IN frame source, set while streaming a `COPY ... FROM STDIN`.
        let mut copy_in: Option<CopyInReceiver> = None;
        let mut copy_in_observation: Option<QueryObservation> = None;
        let mut copy_read_obligation: Option<ReadObligation> = None;
        let mut copy_initial_flushed = false;
        // The `Client` handle dropped (`receiver` closed). We do NOT send
        // `Terminate` until no awaited response remains (mirrors
        // tokio-postgres `poll_write`), then send it exactly once. A discarded
        // housekeeping response does not hold shutdown open.
        let mut client_gone = false;
        let mut terminate_sent = false;

        // Run the loop inside an inner future so that EVERY exit path — a
        // clean `return Ok(())`, a `?`-propagated write/read error, all of it
        // — falls through to the `teardown:` block below, which stops the read
        // task and emits a FIN. `?` and `return` inside resolve this block.
        let run_result: Result<(), Error> = async {
            loop {
                if copy_initial_flushed
                    && copy_read_obligation
                        .as_ref()
                        .is_some_and(ReadObligation::is_complete)
                {
                    copy_in = None;
                    copy_in_observation = None;
                    copy_read_obligation = None;
                    copy_initial_flushed = false;
                }

                // ---- Shutdown sequencing. Once the client is gone and no
                // awaited response/COPY work remains, send Terminate once,
                // then emit a FIN via the write half's `shutdown` (mirrors the
                // serialized path; MUX-4). Housekeeping responses do not hold
                // shutdown open. The server then closes; the read task
                // forwards EOF; the Read arm returns Ok.
                if client_gone
                    && !terminate_sent
                    && !has_awaited_response(&responses, &pending_responses)
                    && copy_in.is_none()
                {
                    trace!("client gone + no awaited responses, sending Terminate (multiplexed)");
                    let buf = inner_encode_terminate();
                    // NOT `?`. `Terminate` is a courtesy to the server and the
                    // client half that would have received an error is, by
                    // definition of this branch, already gone. It is also
                    // routinely undeliverable now: dropping the client releases
                    // the socket synchronously (`crate::release`), so by the
                    // time this task next runs the descriptor is usually shut
                    // down. Propagating that turns every ordinary close into a
                    // connection error in the caller's log.
                    let mut goodbye = write_frontend(&mut write_half, FrontendMessage::Raw(buf));
                    if goodbye.is_ok() {
                        goodbye = write_half.flush().await;
                    }
                    if let Err(e) = goodbye {
                        trace!("Terminate not delivered, client already gone: {e}");
                    }
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
                let accept_copy = copy_in.is_some()
                    && (!copy_initial_flushed
                        || copy_read_obligation
                            .as_ref()
                            .is_some_and(ReadObligation::accepts_copy_input));

                // ---- Single-event select. Every branch is a channel op or a
                // `poll_ready` — all cancel-safe, so a not-ready branch being
                // dropped here loses nothing (the cancel-unsafe socket read lives
                // in the detached read task, never here).
                let event = poll_fn(|cx| -> Poll<MuxEvent> {
                    // (1) ReadTimeout bypasses both the normal frame queue and
                    // response-consumer backpressure. Ordinary I/O errors stay
                    // in the frame FIFO behind bytes decoded before them.
                    match read_terminal_rx.poll_next_unpin(cx) {
                        Poll::Ready(item) => return Poll::Ready(MuxEvent::ReadTerminal(item)),
                        Poll::Pending => {}
                    }

                    // (2) Inbound frame from the read task — next priority so
                    // the server's send buffer keeps draining (delivers idle
                    // NOTIFYs and races COPY ErrorResponses).
                    if accept_read {
                        match read_rx.poll_next_unpin(cx) {
                            Poll::Ready(item) => return Poll::Ready(MuxEvent::Read(item)),
                            Poll::Pending => {}
                        }
                    }

                    // (3) Back-pressure: the front stashed batch's sender has room.
                    if has_pending
                        && let Some(response) = pending_responses.front_mut()
                        && let Poll::Ready(ready) = response.sender.poll_ready(cx)
                    {
                        // Err (consumer hung up) still counts as "ready"; the
                        // SenderReady arm discards the batch in that case.
                        let _ = ready;
                        return Poll::Ready(MuxEvent::SenderReady);
                    }

                    // (4) New client request (normal mode).
                    if accept_request
                        && let Poll::Ready(req) = receiver.poll_next_unpin(cx)
                    {
                        return Poll::Ready(MuxEvent::Request(req));
                    }

                    // (5) COPY-IN frame (copy mode).
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
                    MuxEvent::Read(Some(ReadEvent::Message(ReadEnvelope {
                        message,
                        acknowledgement,
                    }))) => {
                        let result = Dispatch {
                            parameters: &parameters,
                            responses: &mut responses,
                            pending_responses: &mut pending_responses,
                            async_sender: async_sender.as_ref(),
                            tx_status: &tx_status,
                            in_flight_requests: &in_flight_requests,
                        }
                        .handle_message(message);
                        // The reader cannot submit its next socket read until all
                        // clock effects from this decoded frame are visible.
                        if let Some(acknowledgement) = acknowledgement {
                            let _ = acknowledgement.send(());
                        }
                        result?;
                    }
                    MuxEvent::Read(Some(ReadEvent::Terminal(error))) => {
                        return classify_read_terminal(
                            Some(error),
                            &mut responses,
                            &mut pending_responses,
                        );
                    }
                    MuxEvent::Read(None) => {
                        return classify_read_terminal(
                            None,
                            &mut responses,
                            &mut pending_responses,
                        );
                    }
                    MuxEvent::ReadTerminal(terminal) => {
                        return classify_read_terminal(
                            terminal,
                            &mut responses,
                            &mut pending_responses,
                        );
                    }

                    // ---------- Stashed batch deliverable ----------
                    MuxEvent::SenderReady => {
                        if let Some(PendingResponse {
                            mut sender,
                            messages,
                            ..
                        }) = pending_responses.pop_front()
                        {
                            // The poll above observed Ready; start_send may still
                            // fail if the consumer hung up between poll and now.
                            let _ = sender.start_send(messages);
                        }
                    }

                    // ---------- New request ----------
                    MuxEvent::Request(Some(request)) => {
                        let Request {
                            messages,
                            sender,
                            disposition,
                            transaction_effect,
                            prepare_cleanup,
                            statement,
                            observation,
                        } = request;
                        let request_observation = observation.clone();
                        let is_copy = matches!(&messages, RequestMessages::CopyIn(_));
                        let read_obligation =
                            ReadObligation::new(read_deadline.as_ref(), is_copy);
                        responses.push_back(Response {
                            sender,
                            disposition,
                            transaction_effect,
                            prepare_cleanup,
                            statement,
                            observation,
                            read_obligation: read_obligation.clone(),
                        });
                        match messages {
                            RequestMessages::Single(msg) => {
                                let write_result = write_frontend(&mut write_half, msg);
                                // Flush to completion while concurrently draining
                                // the read channel, then attribute a write failure
                                // to the request that caused that flush.
                                let (write_result, mut terminal) = if write_result.is_ok() {
                                    flush_with_read_draining(
                                        &mut write_half,
                                        &mut read_rx,
                                        &mut read_terminal_rx,
                                        &parameters,
                                        &mut responses,
                                        &mut pending_responses,
                                        async_sender.as_ref(),
                                        &tx_status,
                                        &in_flight_requests,
                                    )
                                    .await
                                } else {
                                    (write_result, None)
                                };
                                if let Some(error) =
                                    take_captured_non_eof_terminal(&mut terminal)
                                {
                                    publish_read_timeout(&error, &mut responses);
                                    return Err(error);
                                }
                                if write_result.is_ok() {
                                    // The socket-read clock excludes a blocked
                                    // write. A fast ReadyForQuery handled during
                                    // this flush has already completed the shared
                                    // obligation, so activation becomes a no-op.
                                    read_obligation.activate_initial();
                                }
                                match finish_request_write(
                                    write_result,
                                    disposition,
                                    &responses,
                                    &pending_responses,
                                )? {
                                    RequestOutcome::Continue => {}
                                    RequestOutcome::HousekeepingUndeliverable(error) => {
                                        debug!(
                                            "housekeeping request was not delivered; closing connection: {error}"
                                        );
                                        return Ok(());
                                    }
                                }
                                if let Some(terminal) = terminal {
                                    return classify_read_terminal(
                                        terminal,
                                        &mut responses,
                                        &mut pending_responses,
                                    );
                                }
                            }
                            RequestMessages::CopyIn(rx) => {
                                debug_assert_eq!(disposition, RequestDisposition::Awaited);
                                debug_assert_eq!(transaction_effect, TransactionEffect::MayChange);
                                // Enter COPY mode; frames stream via branch (4).
                                copy_in = Some(rx);
                                copy_in_observation = request_observation;
                                copy_read_obligation = Some(read_obligation);
                                copy_initial_flushed = false;
                            }
                        }
                    }
                    MuxEvent::Request(None) => {
                        // The Client handle dropped. Defer Terminate until awaited
                        // responses drain (top of the loop). Stop accepting
                        // further requests.
                        trace!("receiver closed (multiplexed)");
                        client_gone = true;
                    }

                    // ---------- COPY-IN frame ----------
                    MuxEvent::CopyFrame(Some(msg)) => {
                        let terminal_frame = copy_in
                            .as_ref()
                            .is_some_and(CopyInReceiver::is_done);
                        let resume_terminal = terminal_frame
                            && copy_initial_flushed
                            && copy_read_obligation
                                .as_ref()
                                .is_some_and(ReadObligation::prepare_copy_terminal);
                        if let Some(observation) = &copy_in_observation {
                            observation.inspect_frontend(&msg);
                        }
                        write_frontend(&mut write_half, msg)?;
                        // Same cancel-safe flush+read-drain interleave as the
                        // request path: a COPY frame the server rejects mid-stream
                        // (ErrorResponse) is read concurrently, and a large COPY
                        // frame cannot wedge the cap-1 channel (MUX-DEADLOCK-1).
                        let (res, mut terminal) = flush_with_read_draining(
                            &mut write_half,
                            &mut read_rx,
                            &mut read_terminal_rx,
                            &parameters,
                            &mut responses,
                            &mut pending_responses,
                            async_sender.as_ref(),
                            &tx_status,
                            &in_flight_requests,
                        )
                        .await;
                        if let Some(error) = take_captured_non_eof_terminal(&mut terminal) {
                            publish_read_timeout(&error, &mut responses);
                            return Err(error);
                        }
                        res?;
                        if !copy_initial_flushed {
                            if let Some(obligation) = &copy_read_obligation {
                                obligation.activate_initial();
                            }
                            copy_initial_flushed = true;
                        } else if resume_terminal
                            && let Some(obligation) = &copy_read_obligation
                        {
                            obligation.activate_copy_terminal();
                        }
                        if let Some(terminal) = terminal {
                            return classify_read_terminal(
                                terminal,
                                &mut responses,
                                &mut pending_responses,
                            );
                        }
                    }
                    MuxEvent::CopyFrame(None) => {
                        // COPY stream ended (the receiver already appended
                        // CopyDone+Sync / CopyFail+Sync as its terminal frame).
                        copy_in = None;
                        copy_in_observation = None;
                        copy_read_obligation = None;
                        copy_initial_flushed = false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Socket;
    use crate::connect_raw::connect_raw;
    use crate::connect_tls::Encryption;
    use crate::socket::{SocketReadHalf, SocketWriteHalf};
    use crate::{Config, NoTls};
    use compio::buf::{BufResult, IoBuf, IoBufMut};
    use futures_channel::oneshot;
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc as std_mpsc;
    use std::task::{Context, Wake, Waker};
    use std::time::Duration;

    struct ObservedSocket {
        inner: Socket,
        read_ended: Option<oneshot::Sender<()>>,
    }

    struct ObservedReadHalf {
        inner: SocketReadHalf,
        read_ended: Option<oneshot::Sender<()>>,
    }

    /// A raw socket wrapper that deliberately refuses owned splitting. It
    /// exercises the serialized transport loop without pretending to perform
    /// TLS; encryption and TLS record behaviour are outside this fixture.
    struct UnsplittableSocket {
        inner: Socket,
        split_attempted: Rc<Cell<bool>>,
    }

    struct WriteFailingSplitStream {
        read_half_dropped: Rc<Cell<bool>>,
    }

    struct TimeoutSplitStream {
        read_half_started: Rc<Cell<bool>>,
        read_half_dropped: Rc<Cell<bool>>,
    }

    struct ParkedReadHalf {
        started: Option<Rc<Cell<bool>>>,
        dropped: Rc<Cell<bool>>,
    }

    impl Drop for ParkedReadHalf {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    struct FailingWriteHalf;

    struct SuccessfulWriteHalf;

    struct RetirementCheckingWake {
        tx_status: Arc<AtomicU8>,
        observed: AtomicBool,
    }

    impl RetirementCheckingWake {
        fn observe(&self) {
            assert_eq!(
                self.tx_status.load(Ordering::Acquire),
                READ_RETIRED_STATUS,
                "reader failure became observable before pool poison"
            );
            self.observed.store(true, Ordering::Release);
        }
    }

    impl Wake for RetirementCheckingWake {
        fn wake(self: Arc<Self>) {
            self.observe();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.observe();
        }
    }

    impl AsyncRead for ObservedSocket {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            self.inner.read(buf).await
        }
    }

    impl AsyncWrite for ObservedSocket {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.inner.write(buf).await
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush().await
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            self.inner.shutdown().await
        }
    }

    impl AsyncRead for UnsplittableSocket {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            self.inner.read(buf).await
        }
    }

    impl AsyncWrite for UnsplittableSocket {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.inner.write(buf).await
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush().await
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            self.inner.shutdown().await
        }
    }

    impl AsyncRead for WriteFailingSplitStream {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            std::future::pending::<()>().await;
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for WriteFailingSplitStream {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "scripted write failure",
                )),
                buf,
            )
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsyncRead for TimeoutSplitStream {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            self.read_half_started.set(true);
            std::future::pending::<()>().await;
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for TimeoutSplitStream {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let len = buf.buf_len();
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsyncRead for ParkedReadHalf {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            if let Some(started) = &self.started {
                started.set(true);
            }
            std::future::pending::<()>().await;
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for FailingWriteHalf {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "scripted write failure",
                )),
                buf,
            )
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWrite for SuccessfulWriteHalf {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let len = buf.buf_len();
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl AsyncRead for ObservedReadHalf {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            let result = self.inner.read(buf).await;
            if (result.0.is_err() || matches!(result.0.as_ref(), Ok(&0)))
                && let Some(sender) = self.read_ended.take()
            {
                let _ = sender.send(());
            }
            result
        }
    }

    impl SplitStream for ObservedSocket {
        type ReadHalf = ObservedReadHalf;
        type WriteHalf = SocketWriteHalf;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            let Self { inner, read_ended } = self;
            match inner.try_into_split() {
                Ok((read, write)) => Ok((
                    ObservedReadHalf {
                        inner: read,
                        read_ended,
                    },
                    write,
                )),
                Err(inner) => Err(Self { inner, read_ended }),
            }
        }
    }

    impl SplitStream for UnsplittableSocket {
        type ReadHalf = SocketReadHalf;
        type WriteHalf = SocketWriteHalf;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            self.split_attempted.set(true);
            Err(self)
        }
    }

    impl SplitStream for WriteFailingSplitStream {
        type ReadHalf = ParkedReadHalf;
        type WriteHalf = FailingWriteHalf;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Ok((
                ParkedReadHalf {
                    started: None,
                    dropped: self.read_half_dropped,
                },
                FailingWriteHalf,
            ))
        }
    }

    impl SplitStream for TimeoutSplitStream {
        type ReadHalf = ParkedReadHalf;
        type WriteHalf = SuccessfulWriteHalf;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Ok((
                ParkedReadHalf {
                    started: Some(self.read_half_started),
                    dropped: self.read_half_dropped,
                },
                SuccessfulWriteHalf,
            ))
        }
    }

    fn serialized_deadline_peer(
        answer_query: bool,
    ) -> (
        SocketAddr,
        std_mpsc::Receiver<()>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind serialized peer");
        listener
            .set_nonblocking(true)
            .expect("bound serialized accept");
        let address = listener.local_addr().expect("serialized peer address");
        let (done_tx, done_rx) = std_mpsc::channel();
        let handle = std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut peer = loop {
                    match listener.accept() {
                        Ok((peer, _)) => break peer,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "serialized client did not connect"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("serialized accept failed: {error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("bound serialized peer reads");
                peer.set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("bound serialized peer writes");

                let mut length = [0; 4];
                peer.read_exact(&mut length).expect("read startup length");
                let length = u32::from_be_bytes(length) as usize;
                assert!((8..=1024 * 1024).contains(&length));
                let mut startup = vec![0; length - 4];
                peer.read_exact(&mut startup).expect("read startup body");

                peer.write_all(b"R\0\0\0\x08\0\0\0\0")
                    .expect("write AuthenticationOk");
                peer.write_all(b"K\0\0\0\x0c\0\0\0\x01\0\0\0\x02")
                    .expect("write BackendKeyData");
                peer.write_all(b"Z\0\0\0\x05I")
                    .expect("write startup ReadyForQuery");
                peer.flush().expect("flush startup response");

                let (tag, body) = read_frontend_message(&mut peer);
                assert_eq!(tag, b'Q');
                assert_eq!(body, b"\0");
                if answer_query {
                    peer.write_all(b"I\0\0\0\x04Z\0\0\0\x05I")
                        .expect("write serialized query response");
                    peer.flush().expect("flush serialized query response");
                }

                let mut drain = [0; 64];
                loop {
                    match peer.read(&mut drain) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::NotConnected
                            ) =>
                        {
                            break;
                        }
                        Err(error) => panic!("serialized session was not retired: {error}"),
                    }
                }
            });
            let _ = done_tx.send(());
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        });
        (address, done_rx, handle)
    }

    fn serialized_copy_peer() -> (
        SocketAddr,
        std_mpsc::Receiver<()>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind serialized COPY peer");
        listener
            .set_nonblocking(true)
            .expect("bound serialized COPY accept");
        let address = listener.local_addr().expect("serialized COPY peer address");
        let (done_tx, done_rx) = std_mpsc::channel();
        let handle = std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut peer = loop {
                    match listener.accept() {
                        Ok((peer, _)) => break peer,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "serialized COPY client did not connect"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("serialized COPY accept failed: {error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("bound serialized COPY peer reads");
                peer.set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("bound serialized COPY peer writes");

                let mut length = [0; 4];
                peer.read_exact(&mut length).expect("read startup length");
                let length = u32::from_be_bytes(length) as usize;
                assert!((8..=1024 * 1024).contains(&length));
                let mut startup = vec![0; length - 4];
                peer.read_exact(&mut startup).expect("read startup body");
                peer.write_all(b"R\0\0\0\x08\0\0\0\0")
                    .expect("write COPY AuthenticationOk");
                peer.write_all(b"K\0\0\0\x0c\0\0\0\x03\0\0\0\x04")
                    .expect("write COPY BackendKeyData");
                peer.write_all(b"Z\0\0\0\x05I")
                    .expect("write COPY startup ReadyForQuery");
                peer.flush().expect("flush COPY startup response");

                for expected in [b'B', b'E', b'S'] {
                    let (tag, _) = read_frontend_message(&mut peer);
                    assert_eq!(tag, expected, "unexpected COPY startup frontend frame");
                }
                // Separate writes pin the two response phases that used to
                // deadlock: the producer cannot emit CopyData until it has
                // consumed both BindComplete and CopyInResponse.
                peer.write_all(b"2\0\0\0\x04")
                    .expect("write COPY BindComplete");
                peer.flush().expect("flush COPY BindComplete");
                std::thread::sleep(Duration::from_millis(10));
                peer.write_all(b"G\0\0\0\x07\0\0\0")
                    .expect("write CopyInResponse");
                peer.flush().expect("flush CopyInResponse");

                let (tag, body) = read_frontend_message(&mut peer);
                assert_eq!(tag, b'c', "expected CopyDone after producer delay");
                assert!(body.is_empty());
                let (tag, body) = read_frontend_message(&mut peer);
                assert_eq!(tag, b'S', "expected Sync after CopyDone");
                assert!(body.is_empty());
                peer.write_all(b"C\0\0\0\x0bCOPY 0\0")
                    .expect("write COPY CommandComplete");
                peer.write_all(b"Z\0\0\0\x05I")
                    .expect("write COPY ReadyForQuery");
                peer.flush().expect("flush COPY completion");

                let mut drain = [0; 64];
                loop {
                    match peer.read(&mut drain) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::NotConnected
                            ) =>
                        {
                            break;
                        }
                        Err(error) => panic!("serialized COPY session stayed open: {error}"),
                    }
                }
            });
            let _ = done_tx.send(());
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        });
        (address, done_rx, handle)
    }

    fn scripted_peer() -> (SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted peer");
        let address = listener.local_addr().expect("read scripted peer address");
        let handle = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().expect("accept driver connection");
            peer.set_nodelay(true).expect("disable Nagle on scripted peer");

            let mut length = [0; 4];
            peer.read_exact(&mut length).expect("read startup length");
            let length = u32::from_be_bytes(length) as usize;
            assert!(length >= 4, "startup packet length includes its header");
            let mut startup = vec![0; length - 4];
            peer.read_exact(&mut startup).expect("read startup body");

            peer.write_all(b"R\0\0\0\x08\0\0\0\0")
                .expect("write AuthenticationOk");
            peer.write_all(b"K\0\0\0\x0c\0\0\0\0\0\0\0\0")
                .expect("write BackendKeyData");
            peer.write_all(b"Z\0\0\0\x05I")
                .expect("write startup ReadyForQuery");
            peer.flush().expect("flush startup response");

            let mut tag = [0; 1];
            peer.read_exact(&mut tag).expect("read query tag");
            assert_eq!(tag[0], b'Q', "expected a simple-query request");
            let mut query_length = [0; 4];
            peer.read_exact(&mut query_length).expect("read query length");
            let query_length = u32::from_be_bytes(query_length) as usize;
            assert!(
                query_length >= 4,
                "query packet length includes its header"
            );
            let mut query = vec![0; query_length - 4];
            peer.read_exact(&mut query).expect("read query body");
        });

        (address, handle)
    }

    fn read_frontend_message(peer: &mut std::net::TcpStream) -> (u8, Vec<u8>) {
        let mut tag = [0; 1];
        peer.read_exact(&mut tag).expect("read frontend tag");

        let mut length = [0; 4];
        peer.read_exact(&mut length)
            .expect("read frontend message length");
        let length = u32::from_be_bytes(length) as usize;
        assert!(length >= 4, "frontend length includes its header");

        let mut body = vec![0; length - 4];
        peer.read_exact(&mut body)
            .expect("read frontend message body");
        (tag[0], body)
    }

    fn frontend_cstring(body: &[u8]) -> &[u8] {
        let end = body
            .iter()
            .position(|byte| *byte == 0)
            .expect("frontend cstring has a terminator");
        &body[..end]
    }

    fn housekeeping_read_eof_peer() -> (
        SocketAddr,
        oneshot::Receiver<()>,
        std_mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted peer");
        let address = listener.local_addr().expect("read scripted peer address");
        let (close_written_tx, close_written_rx) = oneshot::channel();
        let (close_peer_tx, close_peer_rx) = std_mpsc::channel();

        let handle = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().expect("accept driver connection");
            peer.set_nodelay(true).expect("disable Nagle on scripted peer");

            let mut length = [0; 4];
            peer.read_exact(&mut length).expect("read startup length");
            let length = u32::from_be_bytes(length) as usize;
            assert!(length >= 4, "startup packet length includes its header");
            let mut startup = vec![0; length - 4];
            peer.read_exact(&mut startup).expect("read startup body");

            peer.write_all(b"R\0\0\0\x08\0\0\0\0")
                .expect("write AuthenticationOk");
            peer.write_all(b"K\0\0\0\x0c\0\0\0\0\0\0\0\0")
                .expect("write BackendKeyData");
            peer.write_all(b"Z\0\0\0\x05I")
                .expect("write startup ReadyForQuery");
            peer.flush().expect("flush startup response");

            let (tag, body) = read_frontend_message(&mut peer);
            assert_eq!(tag, b'P', "expected Parse for statement preparation");
            let statement_name = frontend_cstring(&body).to_vec();
            assert!(!statement_name.is_empty(), "prepared statement is named");
            let (tag, body) = read_frontend_message(&mut peer);
            assert_eq!(tag, b'D', "expected Describe for statement preparation");
            assert_eq!(body.first(), Some(&b'S'), "Describe targets a statement");
            assert_eq!(
                frontend_cstring(&body[1..]),
                statement_name.as_slice(),
                "Describe targets the parsed statement"
            );
            let (tag, body) = read_frontend_message(&mut peer);
            assert_eq!(tag, b'S', "expected Sync after statement preparation");
            assert!(body.is_empty(), "Sync must have an empty body");

            peer.write_all(b"1\0\0\0\x04")
                .expect("write ParseComplete");
            peer.write_all(b"t\0\0\0\x06\0\0")
                .expect("write ParameterDescription");
            peer.write_all(b"n\0\0\0\x04").expect("write NoData");
            peer.write_all(b"Z\0\0\0\x05I")
                .expect("write prepare ReadyForQuery");
            peer.flush().expect("flush prepare response");

            let (tag, body) = read_frontend_message(&mut peer);
            assert_eq!(tag, b'C', "expected Close for dropped statement");
            assert_eq!(
                body.first(),
                Some(&b'S'),
                "expected Close to target a statement"
            );
            assert_eq!(
                frontend_cstring(&body[1..]),
                statement_name.as_slice(),
                "Close targets the prepared statement"
            );
            let (tag, body) = read_frontend_message(&mut peer);
            assert_eq!(tag, b'S', "expected Sync after Close");
            assert!(body.is_empty(), "Sync must have an empty body");

            close_written_tx
                .send(())
                .expect("the test stopped waiting for the Close write");
            close_peer_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the last Client was not dropped within 5 seconds");

            // Send a FIN without discarding a possible client-side Terminate.
            // Draining that half before the socket drops prevents unread data
            // from turning this deliberately scripted EOF into a TCP reset.
            peer.shutdown(Shutdown::Write)
                .expect("close the scripted peer write half without ReadyForQuery");
            peer.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bound the scripted peer drain");
            let mut drain = [0; 64];
            loop {
                let read = peer
                    .read(&mut drain)
                    .expect("the connection driver did not close within 5 seconds");
                if read == 0 {
                    break;
                }
            }
        });

        (address, close_written_rx, close_peer_tx, handle)
    }

    #[test]
    fn housekeeping_write_failure_does_not_hide_a_backpressured_awaited_response() {
        let (sender, _receiver) = mpsc::channel(1);
        let responses = VecDeque::new();
        let pending_responses = VecDeque::from([PendingResponse {
            sender,
            messages: ResponseMessages::Raw(BackendMessages::empty()),
            disposition: RequestDisposition::Awaited,
        }]);
        let write_error = Error::io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "scripted write failure",
        ));

        assert!(
            finish_request_write(
                Err(write_error),
                RequestDisposition::Housekeeping,
                &responses,
                &pending_responses,
            )
            .is_err(),
            "a housekeeping write hid a backpressured awaited response"
        );
    }

    #[test]
    fn transaction_neutral_ready_for_query_does_not_replace_session_status() {
        let (sender, _receiver) = mpsc::channel(1);
        let parameters = Mutex::new(HashMap::new());
        let mut responses = VecDeque::from([Response {
            sender,
            disposition: RequestDisposition::Housekeeping,
            transaction_effect: TransactionEffect::Neutral,
            prepare_cleanup: None,
            statement: None,
            observation: None,
            read_obligation: ReadObligation::new(None, false),
        }]);
        let mut pending_responses = VecDeque::new();
        let tx_status = AtomicU8::new(b'T');
        let in_flight_requests = AtomicUsize::new(1);

        Dispatch {
            parameters: &parameters,
            responses: &mut responses,
            pending_responses: &mut pending_responses,
            async_sender: None,
            tx_status: &tx_status,
            in_flight_requests: &in_flight_requests,
        }
        .deliver_batch(BackendMessages::empty(), Some(b'I'))
        .expect("deliver transaction-neutral ReadyForQuery");

        assert_eq!(tx_status.load(Ordering::Relaxed), b'T');
        assert_eq!(in_flight_requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_captured_read_timeout_outranks_a_concurrent_write_failure() {
        let read_error = Error::read_timeout(Duration::from_millis(75));

        let mut terminal = Some(Some(read_error));
        let surfaced = take_captured_non_eof_terminal(&mut terminal)
            .expect("a concurrent write failure hid the captured read timeout");
        assert!(surfaced.is_read_timeout());
    }

    #[compio::test]
    async fn live_count_covers_a_parked_split_reader_until_its_half_drops() {
        compio::time::timeout(Duration::from_secs(1), async {
            assert_eq!(
                crate::live::live_connections(),
                0,
                "the isolated fixture started with a live connection"
            );
            let read_half_started = Rc::new(Cell::new(false));
            let read_half_dropped = Rc::new(Cell::new(false));
            let stream: BufStream<MaybeTlsStream<_, TimeoutSplitStream>> = BufStream::new(
                MaybeTlsStream::Raw(TimeoutSplitStream {
                    read_half_started: Rc::clone(&read_half_started),
                    read_half_dropped: Rc::clone(&read_half_dropped),
                }),
            );
            let (_request_tx, request_rx) = mpsc::unbounded();
            let connection = Connection::new(
                stream,
                VecDeque::new(),
                HashMap::new(),
                Arc::default(),
                request_rx,
                Arc::new(AtomicU8::new(b'I')),
                Arc::new(AtomicUsize::new(0)),
                None,
            );
            assert_eq!(crate::live::live_connections(), 1);

            let mut driver = Box::pin(connection.run());
            assert!(
                futures_util::poll!(driver.as_mut()).is_pending(),
                "the idle split connection did not park"
            );
            while !read_half_started.get() {
                compio::time::sleep(Duration::from_millis(1)).await;
            }

            let count_with_both_halves = crate::live::live_connections();
            let reader_open_before_driver_drop = !read_half_dropped.get();
            drop(driver);
            let reader_open_after_driver_drop = !read_half_dropped.get();
            let count_after_driver_drop = crate::live::live_connections();

            while !read_half_dropped.get() {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
            let count_after_reader_drop = crate::live::live_connections();

            assert!(
                reader_open_before_driver_drop,
                "the parked reader released its half before driver cancellation"
            );
            assert_eq!(
                count_with_both_halves, 1,
                "one physical connection was counted more than once"
            );
            assert!(
                reader_open_after_driver_drop,
                "dropping the driver synchronously dropped its spawned read half"
            );
            assert_eq!(
                count_after_driver_drop, 1,
                "the count reached zero while the spawned reader still owned its socket half"
            );
            assert_eq!(count_after_reader_drop, 0);
        })
        .await
        .expect("parked split-reader live-count test exceeded its watchdog");
    }

    #[compio::test]
    async fn write_error_teardown_drops_the_parked_read_half() {
        compio::time::timeout(Duration::from_secs(1), async {
            let read_half_dropped = Rc::new(Cell::new(false));
            let stream = BufStream::new(WriteFailingSplitStream {
                read_half_dropped: Rc::clone(&read_half_dropped),
            });
            let (read_half, write_half) = match stream.try_into_split() {
                Ok(halves) => halves,
                Err(_) => panic!("the teardown fixture did not split"),
            };
            let (request_tx, request_rx) = mpsc::unbounded();
            let (response_tx, _response_rx) = mpsc::channel(1);
            request_tx
                .unbounded_send(Request {
                    messages: RequestMessages::Single(FrontendMessage::Raw(
                        bytes::Bytes::from_static(b"scripted request"),
                    )),
                    sender: response_tx,
                    disposition: RequestDisposition::Awaited,
                    transaction_effect: TransactionEffect::MayChange,
                    prepare_cleanup: None,
                    statement: None,
                    observation: None,
                })
                .expect("queue the write-failing request");

            let result =
                Connection::<WriteFailingSplitStream, WriteFailingSplitStream>::run_multiplexed(
                    read_half,
                    write_half,
                    Arc::default(),
                    request_rx,
                    None,
                    Arc::new(AtomicU8::new(b'I')),
                    Arc::new(AtomicUsize::new(1)),
                    None,
                    None,
                    crate::live::LiveConnectionGuard::new(),
                )
                .await;

            let error = result.expect_err("the scripted write unexpectedly succeeded");
            assert_eq!(
                error.as_io().map(std::io::Error::kind),
                Some(std::io::ErrorKind::BrokenPipe)
            );
            assert!(
                read_half_dropped.get(),
                "write-error teardown returned while its read task still owned the read half"
            );
            drop(request_tx);
        })
        .await
        .expect("write-error teardown test exceeded its watchdog");
    }

    #[compio::test]
    async fn reader_poison_precedes_actual_timeout_publication() {
        compio::time::timeout(Duration::from_secs(1), async {
            let read_half_dropped = Rc::new(Cell::new(false));
            let mut stream = BufStream::new(TimeoutSplitStream {
                read_half_started: Rc::new(Cell::new(false)),
                read_half_dropped: Rc::clone(&read_half_dropped),
            });
            stream.set_read_timeout(Some(Duration::ZERO));
            let read_deadline = stream.read_deadline();
            let (read_half, write_half) = match stream.try_into_split() {
                Ok(halves) => halves,
                Err(_) => panic!("the timeout fixture did not split"),
            };

            let tx_status = Arc::new(AtomicU8::new(b'I'));
            let checker = Arc::new(RetirementCheckingWake {
                tx_status: Arc::clone(&tx_status),
                observed: AtomicBool::new(false),
            });
            let waker = Waker::from(Arc::clone(&checker));
            let mut context = Context::from_waker(&waker);
            let (response_tx, mut response_rx) = mpsc::channel(1);
            assert!(
                response_rx.poll_next_unpin(&mut context).is_pending(),
                "the empty response channel was unexpectedly ready"
            );

            let (request_tx, request_rx) = mpsc::unbounded();
            request_tx
                .unbounded_send(Request {
                    messages: RequestMessages::Single(FrontendMessage::Raw(
                        bytes::Bytes::from_static(b"scripted request"),
                    )),
                    sender: response_tx,
                    disposition: RequestDisposition::Awaited,
                    transaction_effect: TransactionEffect::MayChange,
                    prepare_cleanup: None,
                    statement: None,
                    observation: None,
                })
                .expect("queue the timeout request");

            let result = Connection::<TimeoutSplitStream, TimeoutSplitStream>::run_multiplexed(
                read_half,
                write_half,
                Arc::default(),
                request_rx,
                None,
                Arc::clone(&tx_status),
                Arc::new(AtomicUsize::new(1)),
                read_deadline,
                None,
                crate::live::LiveConnectionGuard::new(),
            )
            .await;

            let error = result.expect_err("the zero-budget read unexpectedly succeeded");
            assert!(error.is_read_timeout());
            assert!(
                checker.observed.load(Ordering::Acquire),
                "the operation was not woken with its distinguishable read timeout"
            );
            assert!(read_half_dropped.get());
            drop(request_tx);
        })
        .await
        .expect("reader-poison ordering test exceeded its watchdog");
    }

    #[compio::test]
    async fn ordinary_reader_failure_stays_behind_an_already_decoded_frame() {
        let (mut read_tx, mut read_rx) = mpsc::channel(2);
        let (read_terminal_tx, mut read_terminal_rx) = mpsc::unbounded();
        read_tx
            .send(ReadEvent::Message(ReadEnvelope {
                message: BackendMessage::Normal {
                    messages: BackendMessages::empty(),
                    request_complete: false,
                },
                acknowledgement: None,
            }))
            .await
            .expect("queue scripted decoded frame");

        publish_reader_failure(
            Error::io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "scripted EOF",
            )),
            &mut read_tx,
            &read_terminal_tx,
        )
        .await;

        assert!(
            matches!(read_rx.next().await, Some(ReadEvent::Message(_))),
            "ordinary EOF overtook the decoded frame"
        );
        assert!(
            matches!(read_rx.next().await, Some(ReadEvent::Terminal(error)) if is_eof(&error)),
            "ordinary EOF did not follow the decoded frame in the FIFO"
        );
        assert!(
            read_terminal_rx.try_recv().is_err(),
            "ordinary EOF leaked onto the ReadTimeout priority channel"
        );
    }

    #[compio::test]
    async fn read_timeout_bypasses_a_full_frame_channel() {
        compio::time::timeout(Duration::from_secs(1), async {
            let (mut read_tx, mut read_rx) = mpsc::channel(1);
            let (read_terminal_tx, mut read_terminal_rx) = mpsc::unbounded();
            read_tx
                .send(ReadEvent::Message(ReadEnvelope {
                    message: BackendMessage::Normal {
                        messages: BackendMessages::empty(),
                        request_complete: false,
                    },
                    acknowledgement: None,
                }))
                .await
                .expect("fill scripted frame channel");

            publish_reader_failure(
                Error::read_timeout(Duration::from_millis(75)),
                &mut read_tx,
                &read_terminal_tx,
            )
            .await;

            assert!(
                matches!(read_terminal_rx.next().await, Some(error) if error.is_read_timeout()),
                "read timeout did not reach its out-of-band channel"
            );
            assert!(
                matches!(read_rx.next().await, Some(ReadEvent::Message(_))),
                "read timeout displaced the frame already in the bounded FIFO"
            );
        })
        .await
        .expect("read-timeout priority-channel test exceeded its watchdog");
    }

    #[test]
    fn reader_channel_close_is_an_error_while_an_awaited_response_exists() {
        let (sender, _receiver) = mpsc::channel(1);
        let mut responses = VecDeque::from([Response {
            sender,
            disposition: RequestDisposition::Awaited,
            transaction_effect: TransactionEffect::MayChange,
            prepare_cleanup: None,
            statement: None,
            observation: None,
            read_obligation: ReadObligation::new(None, false),
        }]);
        let pending_responses = VecDeque::new();

        let error = classify_read_terminal(None, &mut responses, &pending_responses)
            .expect_err("reader channel close hid an unfinished response");
        assert!(error.is_closed());
    }

    #[compio::test]
    async fn backend_error_determined_before_the_last_client_drops_is_still_reported() {
        let (address, peer) = scripted_peer();
        let tcp = compio::net::TcpStream::connect(address)
            .await
            .expect("connect to scripted peer");
        let socket = Socket::new_tcp(tcp);
        let release = socket
            .release_handle()
            .expect("duplicate the client release handle");
        let (read_ended_tx, read_ended_rx) = oneshot::channel();
        let stream = ObservedSocket {
            inner: socket,
            read_ended: Some(read_ended_tx),
        };
        let config: Config = "user=test dbname=test sslmode=disable"
            .parse()
            .expect("parse test config");
        let (client, connection) = connect_raw(
            stream,
            NoTls,
            Encryption::Plaintext,
            true,
            &config,
            Some(release),
        )
        .await
        .expect("complete scripted startup");
        let observer = client
            .simple_query_raw("SELECT 1")
            .await
            .expect("queue the observed request");
        let mut driver = Box::pin(connection.run());
        assert!(
            futures_util::poll!(driver.as_mut()).is_pending(),
            "the connection ended before the peer could close it"
        );

        compio::time::timeout(Duration::from_secs(5), read_ended_rx)
            .await
            .expect("the peer did not close within 5 seconds")
            .expect("the EOF observer was dropped");

        drop(client);
        let outcome = compio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("the connection driver did not finish within 5 seconds");
        assert!(
            outcome.is_err(),
            "an EOF determined before the last Client dropped was reported as a clean close"
        );

        drop(observer);
        peer.join().expect("the scripted peer panicked");
    }

    #[compio::test]
    async fn dropping_the_client_after_a_statement_close_write_is_a_clean_read_eof() {
        let (address, close_written, close_peer, peer) = housekeeping_read_eof_peer();
        let tcp = compio::net::TcpStream::connect(address)
            .await
            .expect("connect to scripted peer");
        let socket = Socket::new_tcp(tcp);
        let config: Config = "user=test dbname=test sslmode=disable"
            .parse()
            .expect("parse test config");
        let (client, connection) = config
            .connect_raw(socket, NoTls)
            .await
            .expect("complete scripted startup");
        let driver = compio::runtime::spawn(async move { connection.run().await });

        let statement = client
            .prepare("SET application_name TO 'scripted-peer'")
            .await
            .expect("prepare the statement");
        drop(statement);

        compio::time::timeout(Duration::from_secs(5), close_written)
            .await
            .expect("the statement Close was not written within 5 seconds")
            .expect("the Close-write observer was dropped");
        drop(client);
        close_peer
            .send(())
            .expect("the scripted peer stopped before client drop");

        let outcome = compio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("the connection driver did not finish within 5 seconds")
            .expect("the connection task panicked");
        peer.join().expect("the scripted peer panicked");

        assert!(
            outcome.is_ok(),
            "read EOF with only statement-close housekeeping outstanding failed run(): {outcome:?}"
        );
    }

    #[compio::test]
    async fn serialized_transport_reports_and_retires_a_stalled_read() {
        compio::time::timeout(Duration::from_secs(5), async {
            let (address, peer_done, peer) = serialized_deadline_peer(false);
            let tcp = compio::net::TcpStream::connect(address)
                .await
                .expect("connect serialized deadline peer");
            let socket = Socket::new_tcp(tcp);
            let release = socket
                .release_handle()
                .expect("duplicate serialized release handle");
            let split_attempted = Rc::new(Cell::new(false));
            let stream = UnsplittableSocket {
                inner: socket,
                split_attempted: Rc::clone(&split_attempted),
            };
            let mut config: Config = "user=test dbname=test sslmode=disable"
                .parse()
                .expect("parse serialized test config");
            config.read_timeout(Duration::from_millis(75));
            let (client, connection) = connect_raw(
                stream,
                NoTls,
                Encryption::Plaintext,
                true,
                &config,
                Some(release),
            )
            .await
            .expect("complete serialized startup");
            let driver = compio::runtime::spawn(async move { connection.run().await });

            let error = client
                .simple_query("")
                .await
                .expect_err("silent serialized peer answered the query");
            assert!(error.is_read_timeout());
            let driver_error = driver
                .await
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                .expect_err("serialized read timeout ended cleanly");
            assert!(driver_error.is_read_timeout());
            assert!(client.is_closed());
            assert!(split_attempted.get(), "test did not enter serialized loop");

            drop(client);
            peer_done
                .recv_timeout(Duration::from_secs(2))
                .expect("serialized silent peer did not observe retirement");
            peer.join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        })
        .await
        .expect("serialized stalled-read test exceeded its watchdog");
    }

    #[compio::test]
    async fn serialized_transport_leaves_an_in_budget_read_untouched() {
        compio::time::timeout(Duration::from_secs(5), async {
            let (address, peer_done, peer) = serialized_deadline_peer(true);
            let tcp = compio::net::TcpStream::connect(address)
                .await
                .expect("connect serialized control peer");
            let socket = Socket::new_tcp(tcp);
            let release = socket
                .release_handle()
                .expect("duplicate serialized control release handle");
            let split_attempted = Rc::new(Cell::new(false));
            let stream = UnsplittableSocket {
                inner: socket,
                split_attempted: Rc::clone(&split_attempted),
            };
            let mut config: Config = "user=test dbname=test sslmode=disable"
                .parse()
                .expect("parse serialized control config");
            config.read_timeout(Duration::from_secs(1));
            let (client, connection) = connect_raw(
                stream,
                NoTls,
                Encryption::Plaintext,
                true,
                &config,
                Some(release),
            )
            .await
            .expect("complete serialized control startup");
            let driver = compio::runtime::spawn(async move { connection.run().await });

            client
                .simple_query("")
                .await
                .expect("in-budget serialized query hit its read deadline");
            assert!(!client.is_closed());
            assert!(split_attempted.get(), "test did not enter serialized loop");

            drop(client);
            driver
                .await
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                .expect("serialized control driver failed");
            peer_done
                .recv_timeout(Duration::from_secs(2))
                .expect("serialized control peer did not observe close");
            peer.join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        })
        .await
        .expect("serialized normal-read test exceeded its watchdog");
    }

    #[compio::test]
    async fn serialized_copy_reads_startup_then_excludes_producer_idle_time() {
        compio::time::timeout(Duration::from_secs(5), async {
            let (address, peer_done, peer) = serialized_copy_peer();
            let tcp = compio::net::TcpStream::connect(address)
                .await
                .expect("connect serialized COPY peer");
            let socket = Socket::new_tcp(tcp);
            let release = socket
                .release_handle()
                .expect("duplicate serialized COPY release handle");
            let split_attempted = Rc::new(Cell::new(false));
            let stream = UnsplittableSocket {
                inner: socket,
                split_attempted: Rc::clone(&split_attempted),
            };
            let mut config: Config = "user=test dbname=test sslmode=disable"
                .parse()
                .expect("parse serialized COPY config");
            config.read_timeout(Duration::from_millis(75));
            let (client, connection) = connect_raw(
                stream,
                NoTls,
                Encryption::Plaintext,
                true,
                &config,
                Some(release),
            )
            .await
            .expect("complete serialized COPY startup");
            let driver = compio::runtime::spawn(async move { connection.run().await });

            let statement = Statement::unnamed(Vec::new(), Vec::new());
            let sink = client
                .copy_in::<_, bytes::Bytes>(&statement)
                .await
                .expect("serialized COPY did not read CopyInResponse");
            let mut sink = std::pin::pin!(sink);
            compio::time::sleep(Duration::from_millis(250)).await;
            assert!(
                !client.is_closed(),
                "serialized COPY producer idle time spent the read budget"
            );
            assert_eq!(sink.as_mut().finish().await.expect("finish COPY"), 0);
            assert!(split_attempted.get(), "test did not enter serialized loop");

            drop(client);
            driver
                .await
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                .expect("serialized COPY driver failed");
            peer_done
                .recv_timeout(Duration::from_secs(2))
                .expect("serialized COPY peer did not observe close");
            peer.join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        })
        .await
        .expect("serialized COPY deadline test exceeded its watchdog");
    }
}
