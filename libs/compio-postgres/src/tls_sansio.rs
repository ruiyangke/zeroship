//! Drive a rustls handshake directly on compio I/O, with no poll-based bridge.
//!
//! rustls is SANS-IO: [`ClientConnection`] is a state machine that never
//! touches a socket. It asks for bytes with `read_tls(&mut dyn Read)`, hands
//! bytes back with `write_tls(&mut dyn Write)`, and those are SYNCHRONOUS
//! `std::io` traits over buffers we own. That is a good fit for completion-based
//! I/O rather than an obstacle: we read into an owned buffer, feed the buffer
//! in, drain the reply into another buffer, and write that.
//!
//! # Why this exists rather than `compio-tls`
//!
//! `compio-tls` wraps `futures-rustls` and keeps the session private. Its
//! `TlsStream` is a private enum with no `split`, no `into_inner`, and no
//! accessor for the `ClientConnection` - 0.9.1 and 0.10.0 alike, checked in the
//! vendored source. `tls_rustls.rs` already works around that once, reading
//! channel-binding material off the raw `futures-rustls` stream "before handing
//! the stream to compio-tls, which exposes only the negotiated ALPN".
//!
//! Owning the handshake removes that whole class of question. It also yields
//! `(socket, connection)` as two separate values, which is what lets a TLS
//! connection be split later: the socket halves go one way, the session is
//! shared, and the connection task stops needing a second run-loop. See task
//! #49 - the point of the exercise is that TLS is a transport and must not
//! decide which protocol implementation runs.
//!
//! Nothing here duplicates `futures-rustls`'s poll bridge
//! (`AsyncStream` + `SyncStream`, 524 lines of self-referential pinned
//! futures). The state machine is driven directly.
//!
//! # `close_notify` on teardown is BEST EFFORT, and that is not a bug here
//!
//! `TlsWriteHalf::shutdown` sends `close_notify`, and the multiplexed loop
//! calls it after `Terminate`. That asynchronous fallback frequently does not
//! arrive, for the reason `crate::release` documents about `Terminate` itself:
//! dropping the `Client` shuts the socket down SYNCHRONOUSLY through a dup of
//! the descriptor, so by the time the connection task next runs there is often
//! nothing left to write through. The synchronous release therefore serializes
//! its own alert before it shuts down the socket.
//!
//! What is new under TLS is that the SERVER now has something to say back. It
//! answers a client close with its own `close_notify`, that write lands on a
//! socket already shut down for reading, and the kernel replies RST - so
//! PostgreSQL logs `could not receive data from client: Connection reset by
//! peer`. MEASURED 2026-08-24: the same 16 tests logged 3 of those lines on the
//! TLS server and none at all on the plaintext one.
//!
//! It was log noise rather than data loss - this side is closing either way -
//! but it looks exactly like a driver defect to anyone reading a server log,
//! and it was nearly diagnosed as one. FIXED on 2026-08-25 by sending
//! `close_notify` BEFORE the synchronous release; the shape that took is at
//! the end of this section, and it changed both the release design AND this
//! file.
//!
//! ## What the fix is NOT, measured 2026-08-25
//!
//! Three cheaper repairs were tried against a live TLS server and none of them
//! removes the line. Each was one connection: connect, `SELECT 1`, drop.
//!
//! * **Letting the connection task run.** Sleeping 300ms after dropping the
//!   `Client`, so the task certainly gets polled and reaches its
//!   `Terminate` + `TlsWriteHalf::shutdown` teardown, logs the line anyway.
//!   `ConnectionRelease::drop` has already done `shutdown(Both)` by then, so
//!   the teardown has no writable socket left. The paragraph above says this
//!   write "frequently does not arrive"; it does not arrive AT ALL on this
//!   path, and no amount of scheduling changes that.
//! * **FIN, drain, then close.** Replacing `shutdown(Both)` with
//!   `shutdown(Write)`, draining the receive buffer with a 20ms timeout, then
//!   closing - so that no unread bytes are pending at `close(2)` - also logs
//!   it. The server is not complaining about unread data; it is complaining
//!   that the TLS session ended without `close_notify`.
//! * **Doing nothing special for plaintext.** The same drop against the
//!   plaintext server logs NOTHING, which is the control proving this is the
//!   TLS layer and not the `Terminate` message. libpq over TLS is the other
//!   control: it logs nothing either.
//!
//! ## What the residue is now, measured at SUITE scale
//!
//! Every figure above is one connection in isolation, and that is how two of
//! the three teardown paths were missed: the client release, the connection
//! guard, and `ConnectionRelease::shutdown` called directly all had to be
//! fixed, and single-connection probes read clean after each one.
//!
//! MEASURED 2026-08-25 over a whole `suite-over-tls` run - 1721 tests through
//! the encrypted server - the TLS server logged **11** of these lines, and the
//! other four TLS servers logged NONE. Attributed by re-running binaries
//! alone: `connection_churn` accounts for 7 to 9 of them on its own, and
//! `cancel_request`, `backend_termination`, `socket_release`,
//! `query_backpressure` and `connect_failure_diagnosis` each account for zero.
//!
//! That residue is CORRECT, not a fourth missed path. `connection_churn` runs
//! `BAD_CONNECTION_ITERATIONS` sessions that are abandoned mid-query while
//! holding an advisory lock - the server logs `connection to client lost` for
//! its `cpg_connection_churn_in_flight` statements in the same place - so the
//! write is still in flight when the release runs, and `take_close_notify`
//! declines rather than emit an alert that would overtake an earlier record.
//! Skipping is the documented choice; see `write_in_flight` below.
//!
//! So the number to watch is not zero, it is "11, nearly all from one test
//! that ends connections badly on purpose". A count that grows elsewhere is
//! the finding.
//!
//! The rustls session therefore lives in a `SharedSession` also held by
//! `ConnectionRelease`. `TlsSession` owns both `ClientConnection` and the
//! ordered ciphertext queue; serializing only the former would let queued
//! records be overtaken by the alert. A lease takes that pair out of its state
//! mutex, serializes the queued records plus `close_notify`, and sends them on
//! the dup before making the pair available again. The `Arc` inside the shared
//! handle also keeps `Client` `Send`, unlike compio's default `Rc`-backed
//! `SharedFd`. If an earlier record has already left the queue for an async
//! socket write, release skips the alert rather than sending a later TLS
//! sequence number first. Teardown is best effort, but never misordered.

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use rustls::ClientConnection;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;

use parking_lot::{Condvar, Mutex};

use crate::buf_stream::SplitStream;

#[cfg(test)]
thread_local! {
    static CLOSE_NOTIFY_SERIALIZED_PROBE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Bytes requested per socket read while handshaking.
///
/// A TLS record is at most 16 KiB of plaintext plus overhead, so this holds a
/// whole record in the common case without over-allocating for the handshake,
/// which is a handful of records.
const READ_CHUNK: usize = 16 * 1024;

/// How much unsent ciphertext the outbound queue holds before `collect_outgoing`
/// stops draining rustls.
///
/// Matches rustls' own outbound limit, so the two together bound what one
/// connection can hold to roughly twice it rather than to nothing at all.
/// SOFT because it is checked before a `write_tls`, not inside one: a single
/// call may carry the queue past it, and that is fine - the point is that the
/// queue cannot grow without end, not that it never exceeds one figure.
const OUTGOING_SOFT_CAP: usize = 64 * 1024;

/// Complete a TLS handshake over `socket`, returning both halves of the result.
///
/// On success the connection is past `is_handshaking`, and any application
/// bytes the server sent alongside the final flight are already inside
/// `connection` - readable through its `reader()`. That matters: those bytes
/// have left the socket, so a caller that kept only the socket would lose them.
/// Returning the connection is what makes the pair safe to separate.
pub(crate) async fn handshake<S>(
    mut socket: S,
    mut connection: ClientConnection,
) -> io::Result<(S, ClientConnection)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        // Flush first, always. rustls will not make progress on a flight it
        // has not been allowed to send, and the server will not answer one it
        // has not received, so reading before writing deadlocks the handshake
        // rather than merely delaying it.
        let mut wrote = false;
        while connection.wants_write() {
            let mut out = Vec::new();
            connection.write_tls(&mut out)?;
            if out.is_empty() {
                break;
            }
            let BufResult(written, _) = socket.write_all(out).await;
            written?;
            wrote = true;
        }
        if wrote {
            socket.flush().await?;
        }

        if !connection.is_handshaking() {
            return Ok((socket, connection));
        }

        let BufResult(read, buffer) = socket.read(Vec::with_capacity(READ_CHUNK)).await;
        let read = read?;
        if read == 0 {
            // A truncated handshake is a TLS-level failure, not an ending:
            // rustls treats silent truncation as an attack. The message names
            // the handshake so the cause is legible in a connect error.
            //
            // Measured 2026-09-03: disabling this does not fail a test, it
            // HANGS - the loop re-reads a socket that will never yield another
            // byte, and the lib run was killed at its 1200s bound instead of
            // reporting. So a timeout here is this guard being load-bearing,
            // not a flaky run; the guard turns that spin into a named error.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer closed the connection during the TLS handshake",
            ));
        }

        // `read_tls` consumes as much as one record boundary allows, so this
        // loops until the chunk is drained. `process_new_packets` must run
        // between reads, not after them: it is what advances the state machine,
        // and it is where a bad certificate or a protocol violation surfaces.
        //
        // This loop does NOT return to `reader()` between steps, which is the
        // shape that broke the steady-state path (see `feed_ciphertext_step`).
        // It is safe HERE and only here: PostgreSQL sends nothing before it has
        // seen the startup packet, and the startup packet cannot be sent until
        // this function returns, so there is no application plaintext to
        // accumulate. `read_tls` refuses on unread PLAINTEXT, and pre-startup
        // there is none. Do not copy this loop into a path that runs later.
        let mut pending = &buffer[..read];
        while !pending.is_empty() {
            connection.read_tls(&mut pending)?;
            connection
                .process_new_packets()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
    }
}

/// A rustls session plus the ciphertext it has produced and not yet handed to
/// a socket.
///
/// Every method here is SYNCHRONOUS and none of them may be made async. That is
/// the invariant that makes sharing the session between two concurrently
/// running halves sound: a mutex guard never spans a suspension point, so the
/// halves can never both hold one.
pub(crate) struct TlsSession {
    conn: ClientConnection,
    outgoing: Vec<u8>,
    /// Ciphertext removed from `outgoing` and owned by an async socket write.
    /// A synchronous release cannot safely overtake that earlier TLS record.
    write_in_flight: bool,
    /// Whether `close_notify` has already been serialized. Both release
    /// guards can reach `take_close_notify`; only the first may act.
    close_notify_sent: bool,
}

impl TlsSession {
    pub(crate) fn new(conn: ClientConnection) -> Self {
        Self {
            conn,
            outgoing: Vec::new(),
            write_in_flight: false,
            close_notify_sent: false,
        }
    }

    /// Move any ciphertext rustls is holding into the outbound queue, up to
    /// [`OUTGOING_SOFT_CAP`].
    ///
    /// The loop stops on no progress rather than on `wants_write()` alone: a
    /// zero-byte `write_tls` that left the flag set would spin forever.
    ///
    /// The cap RESTORES a bound that draining rustls removes. rustls holds its
    /// own outbound ciphertext behind a 64 KiB limit, so a peer cannot make it
    /// buffer without end; `write_tls` moves those bytes into a plain `Vec`
    /// that has no such limit. That matters because the READ path also
    /// produces ciphertext (a `KeyUpdate` or an alert is answered), while only
    /// the WRITE half empties the queue - so on an idle `LISTEN` connection,
    /// which reads forever and never writes, a server that sent key updates in
    /// a loop would grow this `Vec` unboundedly. Leaving the surplus inside
    /// rustls hands the back-pressure back to rustls, which is where the
    /// bookkeeping for it already exists.
    fn collect_outgoing(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            if self.outgoing.len() >= OUTGOING_SOFT_CAP {
                break;
            }
            let before = self.outgoing.len();
            self.conn.write_tls(&mut self.outgoing)?;
            if self.outgoing.len() == before {
                break;
            }
        }
        Ok(())
    }

    /// Hand rustls ONE step of received ciphertext, and report how much it
    /// took.
    ///
    /// One step, not the whole buffer, and that is the contract rather than a
    /// preference. `read_tls` refuses outright - `Err("received plaintext
    /// buffer full")` - once 64 KiB of DECRYPTED bytes are sitting unread, and
    /// rustls' own documentation on `read_tls` says to empty `reader()` after
    /// each `process_new_packets`. A loop that drains a whole socket chunk
    /// through `read_tls`/`process_new_packets` without returning to the
    /// reader in between can cross that line and kill the connection.
    ///
    /// MEASURED 2026-08-24: it did. `concurrent_large_bidirectional_queries_do
    /// _not_deadlock` streams 4 MB parameters while the server floods results
    /// back, and the connection died with `error communicating with the
    /// server` - this error, surfacing as an I/O failure several layers up.
    /// The caller loops back to `reader()` between every step now.
    /// Returns whether the peer has sent `close_notify`, which the caller needs
    /// to tell a clean shutdown from a stall: `read_tls` answers `Ok(0)`
    /// unconditionally once that alert has arrived, so "rustls took nothing"
    /// means END OF STREAM in that case and a protocol failure otherwise.
    fn feed_ciphertext_step(&mut self, src: &mut &[u8]) -> io::Result<bool> {
        self.conn.read_tls(src)?;
        let state = self
            .conn
            .process_new_packets()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let peer_has_closed = state.peer_has_closed();
        self.collect_outgoing()?;
        Ok(peer_has_closed)
    }

    /// Decrypted bytes, or 0 when rustls has none buffered.
    ///
    /// `WouldBlock` from rustls means "no plaintext yet", which is a state and
    /// not a failure - the caller answers it by reading more ciphertext.
    fn read_plaintext(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        match self.conn.reader().read(dst) {
            Ok(n) => Ok(n),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(error) => Err(error),
        }
    }

    /// Encrypt plaintext into the outbound queue.
    pub(crate) fn write_plaintext(&mut self, src: &[u8]) -> io::Result<usize> {
        let n = self.conn.writer().write(src)?;
        self.collect_outgoing()?;
        Ok(n)
    }

    pub(crate) fn take_outgoing(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outgoing)
    }

    fn send_close_notify(&mut self) -> io::Result<()> {
        self.conn.send_close_notify();
        self.collect_outgoing()
    }

    /// Serialize everything already queued followed by a `close_notify`.
    ///
    /// This deliberately bypasses [`OUTGOING_SOFT_CAP`]. The session is being
    /// discarded, so retaining rustls' bounded queue no longer buys anything,
    /// while stopping at the cap could leave the alert itself inside rustls.
    pub(crate) fn take_close_notify(&mut self) -> io::Result<Vec<u8>> {
        if self.write_in_flight {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "TLS ciphertext is still being written",
            ));
        }
        // BOTH release guards can reach this: the client half and the
        // connection half each end the physical session, and whichever loses
        // the race must not queue a SECOND alert onto the session.
        if self.close_notify_sent {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "close_notify has already been serialized for this session",
            ));
        }
        self.close_notify_sent = true;
        self.conn.send_close_notify();
        let mut outgoing = self.take_outgoing();
        while self.conn.wants_write() {
            let before = outgoing.len();
            self.conn.write_tls(&mut outgoing)?;
            // Defence against a rustls invariant violation - `wants_write`
            // answering true while `write_tls` emits nothing - which would
            // otherwise spin this loop forever. Unbound on purpose rather than
            // by omission: disabling it left the lib (768), suite (797) and
            // `tls_live` (48) suites green, because reaching it needs a rustls
            // that contradicts itself. Kept as a termination bound.
            if outgoing.len() == before {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "rustls did not serialize pending TLS ciphertext",
                ));
            }
        }
        #[cfg(test)]
        CLOSE_NOTIFY_SERIALIZED_PROBE.with(|slot| {
            if let Some(probe) = slot.borrow_mut().take() {
                probe();
            }
        });
        Ok(outgoing)
    }
}

const CLOSE_NOTIFY_SENT_MESSAGE: &str = "the TLS session already sent close_notify";

struct CloseNotifySent;

impl std::fmt::Debug for CloseNotifySent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("CloseNotifySent")
            .field(&CLOSE_NOTIFY_SENT_MESSAGE)
            .finish()
    }
}

impl std::fmt::Display for CloseNotifySent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(CLOSE_NOTIFY_SENT_MESSAGE)
    }
}

impl std::error::Error for CloseNotifySent {}

fn close_notify_sent_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, CloseNotifySent)
}

fn is_close_notify_sent_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync + 'static>::is::<CloseNotifySent>)
}

struct SharedSessionState {
    session: Option<TlsSession>,
    owner: Option<ThreadId>,
    poisoned: bool,
}

struct SharedSessionInner {
    state: Mutex<SharedSessionState>,
    available: Condvar,
    /// A write future took ciphertext out of the session and never returned.
    /// The socket may hold any prefix, so no later TLS operation can recover.
    abandoned_write: AtomicBool,
    /// `close_notify` has been serialized out of the session. The TLS session
    /// is OVER at that point: any later record would reach the peer after the
    /// alert, which is a protocol violation. The flag lives here rather than
    /// on the session itself so `lease` can refuse WITHOUT taking it.
    close_notify_out: AtomicBool,
}

/// The handle both halves share.
///
/// A lease serializes access to rustls without holding `state` while rustls is
/// running. `ClientConfig` accepts caller implementations for session storage,
/// key logging, and cryptography, and rustls invokes those traits synchronously.
/// Keeping a non-reentrant mutex locked around the call would deadlock if one of
/// those callbacks dropped or otherwise re-entered the client.
///
/// Other threads wait for the current lease just as they waited for the old
/// mutex guard. Same-thread re-entry returns `WouldBlock`, because waiting for
/// a lease owned by the current callback could never make progress.
#[derive(Clone)]
pub(crate) struct SharedSession {
    inner: Arc<SharedSessionInner>,
}

struct SessionLease<'a> {
    shared: &'a SharedSession,
    session: Option<TlsSession>,
    poisoned: bool,
}

/// Marks ciphertext ownership as lost unless the async socket write returns.
///
/// Drop must not lease the rustls state: the read half can be inside a caller
/// callback on another thread. The atomic marker makes abandonment nonblocking
/// and lets that lease finish before every later operation is refused.
struct WriteFlight<'a> {
    shared: &'a SharedSession,
    armed: bool,
}

impl SharedSession {
    fn lease(&self) -> io::Result<SessionLease<'_>> {
        let owner = std::thread::current().id();
        let mut state = self.inner.state.lock();
        loop {
            if self.inner.abandoned_write.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the TLS session lost ciphertext when a write was abandoned",
                ));
            }
            if self.inner.close_notify_out.load(Ordering::Acquire) {
                return Err(close_notify_sent_error());
            }
            if state.poisoned {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the TLS session was poisoned by a panicking callback",
                ));
            }
            if let Some(session) = state.session.take() {
                debug_assert!(
                    state.owner.is_none(),
                    "available TLS session still had an owner"
                );
                state.owner = Some(owner);
                return Ok(SessionLease {
                    shared: self,
                    session: Some(session),
                    poisoned: false,
                });
            }
            if state.owner.as_ref() == Some(&owner) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the TLS session was re-entered from one of its callbacks",
                ));
            }
            self.inner.available.wait(&mut state);
        }
    }

    fn try_lease(&self) -> io::Result<Option<SessionLease<'_>>> {
        if self.inner.abandoned_write.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the TLS session lost ciphertext when a write was abandoned",
            ));
        }
        if self.inner.close_notify_out.load(Ordering::Acquire) {
            return Err(close_notify_sent_error());
        }

        let Some(mut state) = self.inner.state.try_lock() else {
            return Ok(None);
        };
        if self.inner.abandoned_write.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the TLS session lost ciphertext when a write was abandoned",
            ));
        }
        // This recheck and the one before `try_lock` are REDUNDANT BY DESIGN:
        // the first rejects cheaply, this one closes the window where another
        // thread marks the session terminal while this one waits for the lock.
        // So neither is individually bindable - a single-threaded test that
        // reaches one reaches the other, and disabling either leaves the other
        // returning the same error. Measured 2026-08-31: disabling ONE keeps
        // `a_session_that_sent_close_notify_refuses_a_try_lease` green;
        // disabling BOTH turns it red. The pair is bound, the halves are not.
        if self.inner.close_notify_out.load(Ordering::Acquire) {
            return Err(close_notify_sent_error());
        }
        if state.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the TLS session was poisoned by a panicking callback",
            ));
        }
        let Some(session) = state.session.take() else {
            return Ok(None);
        };
        debug_assert!(
            state.owner.is_none(),
            "available TLS session still had an owner"
        );
        state.owner = Some(std::thread::current().id());
        Ok(Some(SessionLease {
            shared: self,
            session: Some(session),
            poisoned: false,
        }))
    }

    /// Mark the session terminal: `close_notify` is out, so no later lease may
    /// write another record behind it.
    pub(crate) fn mark_close_notify_sent(&self) {
        self.inner.close_notify_out.store(true, Ordering::Release);
    }

    pub(crate) fn with<R>(
        &self,
        f: impl FnOnce(&mut TlsSession) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut lease = self.lease()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(lease.session_mut()))) {
            Ok(result) => result,
            Err(payload) => {
                lease.poisoned = true;
                drop(lease);
                std::panic::resume_unwind(payload)
            }
        }
    }

    pub(crate) fn try_with<R>(
        &self,
        f: impl FnOnce(&mut TlsSession) -> io::Result<R>,
    ) -> io::Result<Option<R>> {
        let Some(mut lease) = self.try_lease()? else {
            return Ok(None);
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(lease.session_mut()))) {
            Ok(result) => result.map(Some),
            Err(payload) => {
                lease.poisoned = true;
                drop(lease);
                std::panic::resume_unwind(payload)
            }
        }
    }

    #[cfg(test)]
    fn mutex_is_locked(&self) -> bool {
        self.inner.state.try_lock().is_none()
    }
}

impl<'a> WriteFlight<'a> {
    fn new(shared: &'a SharedSession) -> Self {
        Self {
            shared,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for WriteFlight<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.shared
                .inner
                .abandoned_write
                .store(true, Ordering::Release);
            self.shared.inner.available.notify_all();
        }
    }
}

impl SessionLease<'_> {
    fn session_mut(&mut self) -> &mut TlsSession {
        self.session
            .as_mut()
            .expect("a live TLS lease owns a session")
    }
}

impl Drop for SessionLease<'_> {
    fn drop(&mut self) {
        let session = self
            .session
            .take()
            .expect("a TLS lease restores its session exactly once");
        let mut state = self.shared.inner.state.lock();
        let replaced = state.session.replace(session);
        state.owner = None;
        state.poisoned |= self.poisoned;
        let poisoned = state.poisoned;
        drop(state);
        debug_assert!(replaced.is_none(), "TLS lease restored over a live session");
        drop(replaced);
        if poisoned {
            self.shared.inner.available.notify_all();
        } else {
            self.shared.inner.available.notify_one();
        }
    }
}

pub(crate) fn share(conn: ClientConnection) -> SharedSession {
    SharedSession {
        inner: Arc::new(SharedSessionInner {
            state: Mutex::new(SharedSessionState {
                session: Some(TlsSession::new(conn)),
                owner: None,
                poisoned: false,
            }),
            available: Condvar::new(),
            abandoned_write: AtomicBool::new(false),
            close_notify_out: AtomicBool::new(false),
        }),
    }
}

/// Copy `src` into a compio buffer and declare that many bytes valid.
///
/// The ONLY `unsafe` in this crate, and the reason is mechanical: compio's
/// `IoBufMut` describes a buffer that may be partly uninitialized, so nothing
/// but the buffer itself can be told how much of it is now valid.
///
/// The copy is not free, and it is not accidental either. Reading rustls'
/// plaintext straight into the caller's buffer would need a `&mut [u8]` over
/// uninitialized memory, which on compio-buf 0.8.1 costs a second `unsafe`
/// (`assume_init_mut` is unstable, so it would be a raw-pointer cast). One
/// memcpy of at most a read chunk is the cheaper thing to be sure of.
fn commit<B: IoBufMut>(buf: &mut B, src: &[u8]) {
    let uninit = buf.as_uninit();
    // The `min` is a safety floor for `set_len`, NOT an expected outcome. The
    // caller sizes its fill by this same buffer's capacity, so a shorter buffer
    // here would mean plaintext rustls has already decrypted gets dropped on
    // the floor - a silent short read, which the protocol layer would see as a
    // truncated frame rather than as a bug here. Assert it so a future change
    // that breaks the sizing fails loudly instead of corrupting a stream.
    debug_assert!(
        src.len() <= uninit.len(),
        "commit would truncate {} decrypted bytes into a {}-byte buffer",
        src.len(),
        uninit.len()
    );
    let n = src.len().min(uninit.len());
    for (slot, byte) in uninit.iter_mut().zip(&src[..n]) {
        // `MaybeUninit::write` is safe: it initializes the slot.
        slot.write(*byte);
    }
    // SAFETY: `set_len` requires (1) `n <= as_uninit().len()`, which the `min`
    // above guarantees, and (2) that every byte in `[buf_len(), n)` is
    // initialized - the loop just wrote all n of them through
    // `MaybeUninit::write`, starting at index 0 because `as_uninit` spans the
    // whole buffer rather than the spare tail (checked in compio-buf 0.8.1,
    // the resolved version: the `[u8]` impl is `as_mut_ptr()` over `len()`).
    // Writing `[0, n)` therefore covers `[buf_len(), n)` for ANY `buf_len` up
    // to n, and a `buf_len` above n only makes `set_len` shrink.
    //
    // So the fresh-buffer property callers happen to have - a handed-over read
    // buffer reports length 0 - is what keeps this CORRECT, not what keeps it
    // SOUND. A caller passing a buffer with a meaningful prefix would have it
    // silently overwritten, which is data loss rather than undefined
    // behaviour. Worth separating, because a future caller change can break
    // the first without touching the second.
    //
    // Nothing here reads session state, which is why the 2026-08-26 move of
    // the rustls session from `Rc<RefCell<..>>` to `Arc<Mutex<..>>` could not
    // affect this block: it never relied on single-threaded access or on a
    // unique borrow of anything but its own two arguments.
    #[allow(unsafe_code)]
    unsafe {
        buf.set_len(n);
    }
}

/// The read side of a TLS session: a source of ciphertext plus the scratch the
/// decrypt loop needs.
///
/// This owns the socket rather than borrowing it because compio's `AsyncRead`
/// is a completion API - buffers and streams are passed by value - and because
/// an inherent `&mut self` method sidesteps the `'static` bound the trait's
/// returned future imposes on any free-standing `&mut [u8]` argument.
pub(crate) struct TlsReader<R> {
    socket: R,
    session: SharedSession,
    /// Ciphertext straight off the socket.
    cipher: Vec<u8>,
    /// Bytes of `cipher` the last socket read produced.
    cipher_len: usize,
    /// How much of that rustls has taken. The gap between the two is fed one
    /// step at a time, draining decrypted bytes in between.
    cipher_read: usize,
    /// Decrypted bytes, staged before being copied into the caller's buffer.
    plain: Vec<u8>,
}

impl<R> TlsReader<R> {
    pub(crate) fn new(socket: R, session: SharedSession) -> Self {
        Self {
            socket,
            session,
            cipher: Vec::new(),
            cipher_len: 0,
            cipher_read: 0,
            plain: Vec::new(),
        }
    }

    fn socket_mut(&mut self) -> &mut R {
        &mut self.socket
    }
}

impl<R> TlsReader<R>
where
    R: AsyncRead + Unpin,
{
    /// Stage up to `cap` plaintext bytes in `self.plain`, reading ciphertext
    /// until some arrive.
    ///
    /// Returns 0 only at a genuine end of stream. Returning 0 merely because
    /// rustls had nothing buffered YET would read to the connection loop as the
    /// server hanging up.
    async fn fill(&mut self, cap: usize) -> io::Result<usize> {
        if cap == 0 {
            return Ok(0);
        }
        if self.plain.len() < cap {
            self.plain.resize(cap, 0);
        }
        loop {
            // ALWAYS the first thing in the loop. Every path back here - a
            // fresh socket chunk, or another step through one already held -
            // returns to the reader before giving rustls more, which is the
            // condition `feed_ciphertext_step` documents.
            let n = self
                .session
                .with(|session| session.read_plaintext(&mut self.plain[..cap]))?;
            if n > 0 {
                return Ok(n);
            }

            // Ciphertext already read but not yet handed over: give rustls one
            // step of it, then go back and drain.
            if self.cipher_read < self.cipher_len {
                let mut src = &self.cipher[self.cipher_read..self.cipher_len];
                let before = src.len();
                let outcome = self
                    .session
                    .with(|session| session.feed_ciphertext_step(&mut src));
                self.cipher_read += before - src.len();
                let peer_has_closed = outcome?;
                if before == src.len() {
                    // rustls took nothing. That is END OF STREAM when the peer
                    // has sent close_notify - `read_tls` answers `Ok(0)`
                    // unconditionally from then on - and a protocol failure
                    // otherwise, where dropping the remainder would silently
                    // desynchronise the stream. Treating the first case as the
                    // second turns an orderly shutdown with trailing buffered
                    // ciphertext into a spurious error.
                    if peer_has_closed {
                        return self
                            .session
                            .with(|session| session.read_plaintext(&mut self.plain[..cap]));
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the TLS session stopped accepting ciphertext",
                    ));
                }
                continue;
            }

            // Read one chunk of ciphertext. Inlined rather than a helper on
            // purpose: every nested async frame here lands in the layout of the
            // caller's async block, and this one sits under the connection task
            // of every consumer of this crate. Splitting it back out pushed
            // five downstream crates past rustc's default query-depth limit.
            if self.cipher.is_empty() {
                self.cipher = vec![0u8; READ_CHUNK];
            }
            let buf = std::mem::take(&mut self.cipher);
            let BufResult(result, buf) = self.socket.read(buf).await;
            self.cipher = buf;
            let read = result?;
            if read == 0 {
                // The socket is done, but anything rustls decrypted before the
                // close is still owed to the caller. Only after that is this a
                // real end of stream.
                return self
                    .session
                    .with(|session| session.read_plaintext(&mut self.plain[..cap]));
            }
            self.cipher_read = 0;
            self.cipher_len = read;
        }
    }

    async fn read_into<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let cap = buf.buf_capacity();
        match self.fill(cap).await {
            Ok(n) => {
                commit(&mut buf, &self.plain[..n]);
                BufResult(Ok(n), buf)
            }
            // Synchronous release serialized close_notify and committed to
            // shutting down the socket. Its typed lease refusal is EOF to the
            // racing read half, just as local teardown is over plaintext. A
            // different I/O or TLS failure stays intact, and awaited-response
            // classification still rejects this EOF when protocol work remains.
            Err(error) if is_close_notify_sent_error(&error) => BufResult(Ok(0), buf),
            Err(error) => BufResult(Err(error), buf),
        }
    }
}

/// Push everything the session has to say to `socket`, including whatever it
/// is still holding back.
///
/// Tops up from rustls on EVERY pass, not just once. That is required by
/// [`OUTGOING_SOFT_CAP`]: `collect_outgoing` stops filling the queue at the
/// cap and leaves the surplus inside rustls, so draining only the queue would
/// return success with ciphertext still undelivered - `flush()` would be a
/// lie, and the bytes would leave only if some later write happened to collect
/// again. Topping up each pass keeps the cap as a bound on what is held AT ONE
/// MOMENT rather than a bound on what is ever sent.
///
/// The queue is taken by value so the session lease ends before the write is
/// awaited; anything the read path appends meanwhile is picked up by the next
/// pass. `write_in_flight` records the gap where the bytes are owned by the
/// async operation, so synchronous release never overtakes them.
async fn flush_outgoing<W>(socket: &mut W, session: &SharedSession) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let pending = session.with(|session| {
            session.collect_outgoing()?;
            let pending = session.take_outgoing();
            session.write_in_flight = !pending.is_empty();
            Ok(pending)
        })?;
        if pending.is_empty() {
            return Ok(());
        }
        let write_flight = WriteFlight::new(session);
        let BufResult(result, _) = socket.write_all(pending).await;
        result?;
        session.with(|session| {
            session.write_in_flight = false;
            Ok(())
        })?;
        write_flight.disarm();
    }
}

async fn write_through<W>(socket: &mut W, session: &SharedSession, src: &[u8]) -> io::Result<usize>
where
    W: AsyncWrite + Unpin,
{
    let n = session.with(|session| session.write_plaintext(src))?;
    flush_outgoing(socket, session).await?;
    Ok(n)
}

/// Flush the transport without allowing an indeterminate buffered ciphertext
/// tail to outlive a cancelled or failed flush.
async fn flush_through<W>(socket: &mut W, session: &SharedSession) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    flush_outgoing(socket, session).await?;
    let write_flight = WriteFlight::new(session);
    socket.flush().await?;
    write_flight.disarm();
    Ok(())
}

async fn shutdown_through<W>(socket: &mut W, session: &SharedSession) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    session.with(TlsSession::send_close_notify)?;
    flush_outgoing(socket, session).await?;
    socket.shutdown().await
}

/// A TLS stream that still owns its socket, and so can be split.
pub(crate) struct TlsStreamCore<S> {
    reader: TlsReader<S>,
}

impl<S> TlsStreamCore<S> {
    pub(crate) fn new(socket: S, session: SharedSession) -> Self {
        Self {
            reader: TlsReader::new(socket, session),
        }
    }

    pub(crate) fn session(&self) -> SharedSession {
        self.reader.session.clone()
    }
}

impl<S> TlsStreamCore<S>
where
    S: SplitStream,
{
    /// Split only the socket, carrying every byte already read from it into
    /// the owned read half. A refused split rebuilds the identical stream.
    #[allow(clippy::type_complexity)]
    pub(crate) fn try_into_split(
        self,
    ) -> Result<(TlsReadHalf<S::ReadHalf>, TlsWriteHalf<S::WriteHalf>), Self> {
        let TlsReader {
            socket,
            session,
            cipher,
            cipher_len,
            cipher_read,
            plain,
        } = self.reader;
        match socket.try_into_split() {
            Ok((read, write)) => Ok((
                TlsReadHalf {
                    reader: TlsReader {
                        socket: read,
                        session: session.clone(),
                        cipher,
                        cipher_len,
                        cipher_read,
                        plain,
                    },
                },
                TlsWriteHalf::new(write, session),
            )),
            Err(socket) => Err(Self {
                reader: TlsReader {
                    socket,
                    session,
                    cipher,
                    cipher_len,
                    cipher_read,
                    plain,
                },
            }),
        }
    }
}

impl<S> AsyncRead for TlsStreamCore<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.reader.read_into(buf).await
    }
}

impl<S> AsyncWrite for TlsStreamCore<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let session = self.reader.session.clone();
        let result = write_through(self.reader.socket_mut(), &session, buf.as_init()).await;
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        let session = self.reader.session.clone();
        flush_through(self.reader.socket_mut(), &session).await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        let session = self.reader.session.clone();
        shutdown_through(self.reader.socket_mut(), &session).await
    }
}

/// Owned read half: the socket's read side plus a share of the session.
pub struct TlsReadHalf<R> {
    reader: TlsReader<R>,
}

impl<R> std::fmt::Debug for TlsReadHalf<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TlsReadHalf")
            .finish_non_exhaustive()
    }
}

impl<R> AsyncRead for TlsReadHalf<R>
where
    R: AsyncRead + Unpin,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.reader.read_into(buf).await
    }
}

/// Owned write half: the socket's write side plus a share of the session.
pub struct TlsWriteHalf<W> {
    socket: W,
    session: SharedSession,
}

impl<W> std::fmt::Debug for TlsWriteHalf<W> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TlsWriteHalf")
            .finish_non_exhaustive()
    }
}

impl<W> TlsWriteHalf<W> {
    pub(crate) fn new(socket: W, session: SharedSession) -> Self {
        Self { socket, session }
    }
}

impl<W> AsyncWrite for TlsWriteHalf<W>
where
    W: AsyncWrite + Unpin,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let result = write_through(&mut self.socket, &self.session, buf.as_init()).await;
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        flush_through(&mut self.socket, &self.session).await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        shutdown_through(&mut self.socket, &self.session).await
    }
}

#[cfg(test)]
pub(crate) use tests::handshaken_pair;

#[cfg(test)]
mod tests {
    use super::*;
    use compio::buf::{IoBuf, IoBufMut};
    use rustls::NamedGroup;
    use rustls::client::{
        ClientSessionStore, Resumption, Tls12ClientSessionValue, Tls13ClientSessionValue,
    };
    use rustls::pki_types::ServerName;
    use std::cell::{Cell, RefCell};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn close_notify_sent_debug_names_its_type() {
        let debug = format!("{CloseNotifySent:?}");

        assert!(
            debug.starts_with("CloseNotifySent("),
            "close-notify error Debug did not name its type: {debug}"
        );
        assert!(
            debug.contains("close_notify"),
            "close-notify error Debug lost its diagnostic message: {debug}"
        );
    }

    /// A peer that accepts every byte and answers every read with EOF.
    ///
    /// Enough to drive the handshake loop through one full iteration: the
    /// ClientHello is flushed, the connection is still handshaking, and the
    /// read that follows finds the peer gone.
    struct SilentPeer {
        pending: usize,
        written: usize,
    }

    impl AsyncRead for SilentPeer {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for SilentPeer {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.pending += buf.buf_len();
            BufResult(Ok(buf.buf_len()), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.written += self.pending;
            self.pending = 0;
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A sink that swallows everything and counts it.
    struct CountingSink {
        written: usize,
    }

    impl AsyncWrite for CountingSink {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.written += buf.buf_len();
            let n = buf.buf_len();
            BufResult(Ok(n), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer that makes observable progress and then never gives its buffer
    /// back. Dropping its future therefore loses an unknowable TLS frame tail.
    struct ParkedWriter {
        wire_prefix: Vec<u8>,
    }

    /// A peer that accepts a prefix, errors once, then recovers. The recovery
    /// makes a second TLS operation test the session's own retirement state.
    struct PartialErrorWriter {
        writes: usize,
        wire_prefix: Vec<u8>,
    }

    /// A buffered transport whose flush exposes one byte and then never
    /// returns. Dropping that flush loses an unknowable ciphertext tail.
    struct ParkedFlushWriter {
        buffered: usize,
        wire_prefix: Vec<u8>,
    }

    /// A buffered transport whose first flush exposes one byte and errors.
    struct PartialErrorFlushWriter {
        buffered: usize,
        flushes: usize,
        wire_prefix: Vec<u8>,
    }

    struct SplitStateSocket;

    struct EofReadHalf;

    #[derive(Default)]
    struct ShutdownCapture {
        wire: Vec<u8>,
        shutdowns: usize,
    }

    impl AsyncRead for EofReadHalf {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for ShutdownCapture {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.wire.extend_from_slice(buf.as_init());
            let n = buf.buf_len();
            BufResult(Ok(n), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            assert!(
                !self.wire.is_empty(),
                "the TLS write half shut down its transport before sending close_notify"
            );
            self.shutdowns += 1;
            Ok(())
        }
    }

    impl SplitStream for SplitStateSocket {
        type ReadHalf = EofReadHalf;
        type WriteHalf = ShutdownCapture;

        fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
            Ok((EofReadHalf, ShutdownCapture::default()))
        }
    }

    #[test]
    fn tls_read_half_debug_names_its_type() {
        let (client, _server) = handshaken_pair();
        let read = TlsReadHalf {
            reader: TlsReader::new(EofReadHalf, share(client)),
        };

        let debug = format!("{read:?}");

        assert!(
            debug.starts_with("TlsReadHalf {"),
            "TLS read-half Debug did not name its type: {debug}"
        );
    }

    #[test]
    fn tls_write_half_debug_names_its_type() {
        let (client, _server) = handshaken_pair();
        let write = TlsWriteHalf::new(ShutdownCapture::default(), share(client));

        let debug = format!("{write:?}");

        assert!(
            debug.starts_with("TlsWriteHalf {"),
            "TLS write-half Debug did not name its type: {debug}"
        );
    }

    impl AsyncWrite for ParkedWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let bytes = buf.as_init();
            assert!(
                !bytes.is_empty(),
                "the TLS fixture tried to park an empty write"
            );
            self.wire_prefix.push(bytes[0]);
            std::future::pending::<()>().await;
            unreachable!("the parked TLS writer completed")
        }

        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWrite for PartialErrorWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.writes += 1;
            let bytes = buf.as_init();
            match self.writes {
                1 => {
                    self.wire_prefix.push(bytes[0]);
                    BufResult(Ok(1), buf)
                }
                2 => BufResult(
                    Err(io::Error::other("scripted partial ciphertext write")),
                    buf,
                ),
                _ => BufResult(Ok(bytes.len()), buf),
            }
        }

        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWrite for ParkedFlushWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.buffered += buf.buf_len();
            BufResult(Ok(buf.buf_len()), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            assert!(
                self.buffered > 0,
                "the TLS fixture tried to park an empty transport flush"
            );
            self.wire_prefix.push(0);
            std::future::pending::<()>().await;
            unreachable!("the parked transport flush completed")
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWrite for PartialErrorFlushWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.buffered += buf.buf_len();
            BufResult(Ok(buf.buf_len()), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 1 {
                assert!(
                    self.buffered > 0,
                    "the TLS fixture tried to fail an empty transport flush"
                );
                self.wire_prefix.push(0);
                return Err(io::Error::other("scripted partial transport flush"));
            }
            self.buffered = 0;
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tls_split_preserves_the_plain_scratch_buffer() {
        let (client, _server) = handshaken_pair();
        let mut stream = TlsStreamCore::new(SplitStateSocket, share(client));
        stream.reader.plain = vec![0xa5; 257];
        let plain_ptr = stream.reader.plain.as_ptr();
        let plain_capacity = stream.reader.plain.capacity();

        let Ok((read, _write)) = stream.try_into_split() else {
            panic!("the state-carry TLS stream refused to split");
        };

        assert_eq!(read.reader.plain, vec![0xa5; 257]);
        assert_eq!(read.reader.plain.capacity(), plain_capacity);
        assert_eq!(
            read.reader.plain.as_ptr(),
            plain_ptr,
            "the split replaced the TLS reader's plaintext scratch allocation"
        );
    }

    /// The sibling test above binds `plain` alone. The recurring claim about
    /// this split is about CIPHERTEXT: bytes already read off the socket that
    /// rustls has not taken yet, which is exactly the window
    /// `cipher[cipher_read..cipher_len]`.
    ///
    /// `try_into_split` carries them because it destructures `TlsReader`
    /// exhaustively, so a new buffer field cannot be dropped without a compile
    /// error. That is a strong guarantee against ADDING state and no guarantee
    /// at all against rewriting these three, which nothing asserted: clearing
    /// them here truncates the TLS record stream with no error anywhere.
    #[test]
    fn tls_split_preserves_unconsumed_ciphertext() {
        let (client, _server) = handshaken_pair();
        let mut stream = TlsStreamCore::new(SplitStateSocket, share(client));
        let record: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
        stream.reader.cipher = record.clone();
        stream.reader.cipher_len = 400;
        stream.reader.cipher_read = 128;
        let cipher_ptr = stream.reader.cipher.as_ptr();

        let Ok((read, _write)) = stream.try_into_split() else {
            panic!("the state-carry TLS stream refused to split");
        };

        assert_eq!(
            read.reader.cipher_len, 400,
            "the split lost how much ciphertext the last socket read produced"
        );
        assert_eq!(
            read.reader.cipher_read, 128,
            "the split lost how much ciphertext rustls had already taken"
        );
        assert_eq!(
            &read.reader.cipher[read.reader.cipher_read..read.reader.cipher_len],
            &record[128..400],
            "the split discarded the ciphertext rustls had not yet taken"
        );
        assert_eq!(
            read.reader.cipher.as_ptr(),
            cipher_ptr,
            "the split replaced the TLS reader's ciphertext allocation"
        );
    }

    #[compio::test]
    async fn tls_write_half_shutdown_sends_close_notify() {
        let (client, mut server) = handshaken_pair();
        let stream = TlsStreamCore::new(SplitStateSocket, share(client));
        let Ok((_read, mut writer)) = stream.try_into_split() else {
            panic!("the shutdown TLS stream refused to split");
        };

        writer
            .shutdown()
            .await
            .expect("shut down the TLS write half");
        assert_eq!(writer.socket.shutdowns, 1);

        let mut ciphertext = writer.socket.wire.as_slice();
        let mut peer_has_closed = false;
        while !ciphertext.is_empty() {
            let accepted = server
                .read_tls(&mut ciphertext)
                .expect("server read close_notify ciphertext");
            assert!(accepted > 0, "the server stopped consuming close_notify");
            peer_has_closed |= server
                .process_new_packets()
                .expect("server process close_notify")
                .peer_has_closed();
        }
        assert!(peer_has_closed, "the TLS write half sent no close_notify");
    }

    /// The OTHER panic-poisoning copy. `with` blocks for the lease and is
    /// covered by `a_panicking_rustls_callback_poisons_the_shared_session`;
    /// `try_with` takes the lease only when it is free and reaches its own
    /// `catch_unwind` arm, which no test entered - the whole 1438-test suite
    /// passed with its `lease.poisoned = true` flipped to `false`. A rustls
    /// session half-unwound through THIS path must be refused just the same,
    /// or the next caller resumes a session whose internal state was abandoned
    /// mid-mutation.
    #[test]
    fn a_panicking_callback_under_try_with_poisons_the_shared_session() {
        let (client, _server) = handshaken_pair();
        let session = share(client);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = session
                .try_with(|_| -> io::Result<()> { panic!("scripted try_with callback panic") });
        }));
        assert!(panic.is_err(), "the try_with callback did not panic");

        let error = session
            .with(|_| Ok(()))
            .expect_err("a partially unwound rustls session was reused after try_with");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[compio::test]
    async fn cancelling_a_tls_write_poisons_the_reused_stream() {
        let (client, _server) = handshaken_pair();
        let session = share(client);
        let mut writer = TlsWriteHalf::new(
            ParkedWriter {
                wire_prefix: Vec::new(),
            },
            session,
        );

        let mut write = Box::pin(writer.write(b"abandoned plaintext".to_vec()));
        assert!(
            futures_util::poll!(write.as_mut()).is_pending(),
            "the TLS write fixture did not park after making progress"
        );
        drop(write);
        assert_eq!(
            writer.socket.wire_prefix.len(),
            1,
            "the cancelled write made no progress, so its frame boundary stayed known"
        );

        let error = writer
            .flush()
            .await
            .expect_err("a cancelled TLS write was reported as a successful reusable stream");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[compio::test]
    async fn a_partial_tls_write_error_poisons_the_reused_stream() {
        let (client, _server) = handshaken_pair();
        let session = share(client);
        let mut writer = TlsWriteHalf::new(
            PartialErrorWriter {
                writes: 0,
                wire_prefix: Vec::new(),
            },
            session,
        );

        let BufResult(first, _) = writer
            .write(b"partially delivered plaintext".to_vec())
            .await;
        first.expect_err("the scripted ciphertext write did not fail");
        assert_eq!(
            writer.socket.wire_prefix.len(),
            1,
            "the failed TLS write made no progress, so no frame tail was lost"
        );

        let error = writer
            .flush()
            .await
            .expect_err("a TLS session that lost ciphertext to a returned write error was reused");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[compio::test]
    async fn cancelling_a_transport_flush_poisons_the_reused_tls_stream() {
        let (client, _server) = handshaken_pair();
        let session = share(client);
        let mut writer = TlsWriteHalf::new(
            ParkedFlushWriter {
                buffered: 0,
                wire_prefix: Vec::new(),
            },
            session,
        );

        let BufResult(written, _) = writer.write(b"buffered plaintext".to_vec()).await;
        written.expect("the fixture transport should buffer the TLS frame");

        let mut flush = Box::pin(writer.flush());
        assert!(
            futures_util::poll!(flush.as_mut()).is_pending(),
            "the TLS fixture did not park after making flush progress"
        );
        drop(flush);
        assert_eq!(
            writer.socket.wire_prefix.len(),
            1,
            "the cancelled transport flush made no progress"
        );

        let BufResult(retry, _) = writer.write(b"next plaintext".to_vec()).await;
        let error = retry.expect_err("a cancelled transport flush left a TLS stream reusable");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[compio::test]
    async fn a_partial_transport_flush_error_poisons_the_reused_tls_stream() {
        let (client, _server) = handshaken_pair();
        let session = share(client);
        let mut writer = TlsWriteHalf::new(
            PartialErrorFlushWriter {
                buffered: 0,
                flushes: 0,
                wire_prefix: Vec::new(),
            },
            session,
        );

        let BufResult(written, _) = writer.write(b"buffered plaintext".to_vec()).await;
        written.expect("the fixture transport should buffer the TLS frame");
        writer
            .flush()
            .await
            .expect_err("the fixture transport flush should fail");
        assert_eq!(
            writer.socket.wire_prefix.len(),
            1,
            "the failed transport flush made no progress"
        );

        let BufResult(retry, _) = writer.write(b"next plaintext".to_vec()).await;
        let error = retry.expect_err("a partial transport flush error left a TLS stream reusable");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    /// A flush must leave NOTHING inside the session, including what the cap
    /// told `collect_outgoing` to leave behind.
    ///
    /// The reachable shape is narrow, and worth stating exactly. A single
    /// `collect_outgoing` on an EMPTY queue always empties rustls, because one
    /// `write_tls` drains its whole send buffer and only then does the cap
    /// check stop the loop. So the surplus exists only when the queue was
    /// ALREADY at the cap when more ciphertext appeared - which is the idle
    /// `LISTEN` case the cap was added for: the read path answers key updates
    /// while nothing drains the queue. A flush arriving in that state used to
    /// write the queue, find it empty, and return success with rustls still
    /// holding bytes.
    ///
    /// This test does NOT prove the cap is enforced - `a_full_outbound_queue_
    /// stops_draining_the_session` does that.
    #[compio::test]
    async fn a_flush_drains_what_the_cap_left_inside_the_session() {
        let (client, _server) = handshaken_pair();
        let session = share(client);
        let mut sink = CountingSink { written: 0 };

        // The state described above: a queue already at the cap, and ciphertext
        // that arrived afterwards. `writer()` directly, not `write_plaintext`,
        // because the latter collects and would hide the very gap under test.
        session
            .with(|session| {
                session.outgoing = vec![0u8; OUTGOING_SOFT_CAP];
                Ok(())
            })
            .expect("fill the outgoing queue");
        session
            .with(|session| {
                session
                    .conn
                    .writer()
                    .write_all(b"held back by the cap")
                    .expect("queue application data");
                Ok(())
            })
            .expect("queue application data in rustls");
        assert!(
            session
                .with(|session| Ok(session.conn.wants_write()))
                .expect("inspect rustls outgoing state"),
            "the fixture must leave ciphertext inside rustls, or this asserts nothing"
        );

        flush_outgoing(&mut sink, &session)
            .await
            .expect("flush the session");

        assert!(
            !session
                .with(|session| Ok(session.conn.wants_write()))
                .expect("inspect rustls outgoing state"),
            "the flush returned success with ciphertext still inside the session"
        );
        assert!(
            session
                .with(|session| Ok(session.outgoing.is_empty()))
                .expect("inspect the outgoing queue"),
            "the outbound queue is not empty after a flush"
        );
        assert!(
            sink.written > OUTGOING_SOFT_CAP,
            "the flush wrote only {} bytes, so it never reached what the cap held back",
            sink.written
        );
    }

    /// A peer that hands over one scripted chunk per read, then EOF.
    ///
    /// One chunk per read is the whole point. `read_tls` will happily swallow
    /// an entire buffer in a single call, so a peer that delivered everything
    /// at once could never leave bytes sitting behind an already-processed
    /// `close_notify` - which is the state under test.
    struct ScriptedPeer {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl AsyncRead for ScriptedPeer {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
            match self.chunks.pop_front() {
                Some(bytes) => {
                    assert!(
                        bytes.len() <= buf.buf_capacity(),
                        "the test peer's chunk must fit in one read"
                    );
                    let n = bytes.len();
                    commit(&mut buf, &bytes);
                    BufResult(Ok(n), buf)
                }
                None => BufResult(Ok(0), buf),
            }
        }
    }

    /// Drive a real client/server handshake entirely in memory and return the
    /// finished client session plus a sink for whatever the server says next.
    ///
    /// A real handshake, not a stub: the state this test is about
    /// (`read_tls` answering `Ok(0)` forever once `close_notify` has arrived)
    /// only exists behind live keys.
    pub(crate) fn handshaken_pair() -> (ClientConnection, rustls::ServerConnection) {
        handshaken_pair_with_store(None)
    }

    /// Only the FIRST `take_close_notify` may serialize an alert. The struct
    /// comment says both release guards can reach it, and the loser must not
    /// queue a second `close_notify` behind the first - that would put a record
    /// after the terminal alert, which the peer may reject outright.
    ///
    /// `concurrent_tls_release_guards_do_not_cut_off_close_notify` drives the
    /// race between the two guards, but the losing guard is stopped earlier by
    /// the shared `close_notify_out` flag and never re-enters here. So this
    /// in-struct guard - the defence that survives if that flag is ever moved
    /// or reordered - had no test at all.
    #[test]
    fn a_second_close_notify_is_refused_by_the_session_itself() {
        let (client, _server) = handshaken_pair();
        let mut session = TlsSession::new(client);

        let first = session
            .take_close_notify()
            .expect("the first close_notify must serialize");
        assert!(
            !first.is_empty(),
            "the first close_notify serialized no ciphertext"
        );

        let error = session
            .take_close_notify()
            .expect_err("a second close_notify must be refused");
        assert_eq!(
            error.kind(),
            io::ErrorKind::AlreadyExists,
            "the second attempt reported the wrong kind: {error}"
        );
        assert!(
            error.to_string().contains("already been serialized"),
            "the refusal must say the alert was already serialized: {error}"
        );
    }

    /// A release landing while a write is still in flight must DECLINE to
    /// serialize the alert rather than emit one.
    ///
    /// This is a real window, not a synthetic state: `flush_outgoing` sets
    /// `write_in_flight` for exactly the span where the queued ciphertext is
    /// owned by the async write, and `release.rs` calls `take_close_notify`
    /// synchronously inside it. An alert queued there would be ordered ahead of
    /// a record the peer has not received, which is what the module docs mean
    /// by declining rather than overtaking - and they attribute real server-log
    /// entries to this path.
    ///
    /// Nothing held it. Disabling the guard left the lib (768), suite (797) and
    /// `tls_live` (48) suites all green.
    #[test]
    fn a_close_notify_is_declined_while_a_write_is_in_flight() {
        let (client, _server) = handshaken_pair();
        let mut session = TlsSession::new(client);
        session.write_in_flight = true;

        let error = session
            .take_close_notify()
            .expect_err("an alert must not overtake ciphertext still being written");
        assert_eq!(
            error.kind(),
            io::ErrorKind::WouldBlock,
            "the refusal reported the wrong kind: {error}"
        );
        assert!(
            error.to_string().contains("still being written"),
            "the refusal must say why it declined: {error}"
        );
        // Declining must not spend the one alert the session is allowed. Were
        // this guard ever moved below the `close_notify_sent` assignment, the
        // alert would be lost for good rather than deferred.
        assert!(
            !session.close_notify_sent,
            "a declined attempt consumed the single permitted close_notify"
        );

        // Control: the refusal turns on the in-flight write and nothing else.
        session.write_in_flight = false;
        let alert = session
            .take_close_notify()
            .expect("the alert must serialize once the write has completed");
        assert!(!alert.is_empty(), "the alert serialized no ciphertext");
    }

    /// `close_notify` is the end of the record stream. A lease taken after it
    /// could encrypt another record behind the peer's terminal alert, which is
    /// a protocol violation the peer is entitled to reject outright.
    #[test]
    fn a_session_that_sent_close_notify_refuses_a_blocking_lease() {
        let (client, _server) = handshaken_pair();
        let session = share(client);
        session.mark_close_notify_sent();

        let error = session
            .with(|_| Ok(()))
            .expect_err("a session that sent close_notify granted a lease");
        assert!(
            is_close_notify_sent_error(&error),
            "wrong refusal for a post-close_notify lease: {error}"
        );
    }

    /// The non-blocking path guards separately, and is reached by the
    /// multiplexed write loop rather than by `with`.
    #[test]
    fn a_session_that_sent_close_notify_refuses_a_try_lease() {
        let (client, _server) = handshaken_pair();
        let session = share(client);
        session.mark_close_notify_sent();

        // `.err().expect(..)` rather than `expect_err`: the Ok type here is
        // `Option<SessionLease>`, which is private and deliberately not Debug.
        let error = session
            .try_lease()
            .err()
            .expect("a session that sent close_notify granted a try_lease");
        assert!(
            is_close_notify_sent_error(&error),
            "wrong refusal for a post-close_notify try_lease: {error}"
        );
    }

    #[test]
    fn tls_release_does_not_wait_for_an_in_flight_session_lease() {
        let (client, _server) = handshaken_pair();
        let session = share(client);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind release test listener");
        let socket = TcpStream::connect(listener.local_addr().expect("release test address"))
            .expect("connect release test socket");
        let (_peer, _) = listener.accept().expect("accept release test socket");
        let mut release = crate::release::ConnectionRelease::dup_of(&socket)
            .expect("duplicate release test socket");
        release.set_tls_session(session.clone());

        let (held_tx, held_rx) = mpsc::channel();
        let (allow_tx, allow_rx) = mpsc::channel();
        let held_session = session.clone();
        let holder = std::thread::spawn(move || {
            held_session
                .with(|_| {
                    held_tx.send(()).expect("report held TLS lease");
                    allow_rx.recv().expect("release held TLS lease");
                    Ok(())
                })
                .expect("hold TLS lease");
        });
        held_rx.recv().expect("wait for held TLS lease");

        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let releaser = std::thread::spawn(move || {
            started_tx.send(()).expect("report release start");
            drop(release);
            done_tx.send(()).expect("report completed release");
        });
        started_rx.recv().expect("wait for release start");
        let finished_while_held = done_rx.recv_timeout(Duration::from_secs(1)).is_ok();

        allow_tx.send(()).expect("allow held TLS lease to finish");
        holder.join().expect("join TLS lease holder");
        releaser.join().expect("join TLS release thread");

        assert!(
            finished_while_held,
            "TLS release waited for an in-flight TLS lease before shutting down the socket"
        );
    }

    #[test]
    fn concurrent_tls_release_guards_do_not_cut_off_close_notify() {
        let (client, mut server) = handshaken_pair();
        let session = share(client);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind release race listener");
        let socket = TcpStream::connect(listener.local_addr().expect("release race address"))
            .expect("connect release race socket");
        let (mut peer, _) = listener.accept().expect("accept release race socket");
        let mut release = crate::release::ConnectionRelease::dup_of(&socket)
            .expect("duplicate release race socket");
        release.set_tls_session(session);
        let connection_release = release.connection_guard();

        let (serialized_tx, serialized_rx) = mpsc::channel();
        let (allow_send_tx, allow_send_rx) = mpsc::channel();
        let releaser = std::thread::spawn(move || {
            CLOSE_NOTIFY_SERIALIZED_PROBE.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    serialized_tx
                        .send(())
                        .expect("report serialized close_notify");
                    allow_send_rx
                        .recv()
                        .expect("allow close_notify transport send");
                }));
            });
            drop(release);
        });
        serialized_rx
            .recv()
            .expect("wait for serialized close_notify");

        drop(connection_release);
        allow_send_tx
            .send(())
            .expect("allow elected TLS release to finish");
        releaser.join().expect("join elected TLS releaser");

        let mut wire = Vec::new();
        peer.read_to_end(&mut wire)
            .expect("read released TLS transport");
        let mut cursor = wire.as_slice();
        let mut peer_has_closed = false;
        while !cursor.is_empty() {
            let accepted = server.read_tls(&mut cursor).expect("server read_tls");
            assert!(
                accepted > 0,
                "the server stopped consuming release ciphertext"
            );
            peer_has_closed |= server
                .process_new_packets()
                .expect("server process close_notify")
                .peer_has_closed();
        }
        assert!(
            peer_has_closed,
            "concurrent TLS release cut off close_notify before shutdown"
        );
    }

    #[test]
    fn tls_release_keeps_the_session_until_socket_shutdown() {
        let (client, _server) = handshaken_pair();
        let session = share(client);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind release window listener");
        let socket = TcpStream::connect(listener.local_addr().expect("release window address"))
            .expect("connect release window socket");
        let (_peer, _) = listener.accept().expect("accept release window socket");
        let mut release = crate::release::ConnectionRelease::dup_of(&socket)
            .expect("duplicate release window socket");
        release.set_tls_session(session.clone());

        let writer_acquired = Arc::new(AtomicBool::new(false));
        let observed = writer_acquired.clone();
        let probe_session = session;
        let mut probe_socket = socket.try_clone().expect("duplicate release window writer");
        crate::release::TLS_BEFORE_SHUTDOWN_PROBE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let writer = std::thread::spawn(move || {
                    // The invariant is that the write does not HAPPEN, not the
                    // shape of the refusal. A terminal session answers `Err`
                    // (like poisoned and abandoned-write do); a merely busy one
                    // answers `Ok(None)`. Only `Ok(Some(_))` means the writer
                    // got the session and put a record behind `close_notify`.
                    let acquired = matches!(
                        probe_session.try_with(|session| {
                            session.write_plaintext(b"late plaintext")?;
                            probe_socket.write_all(&session.take_outgoing())?;
                            Ok(())
                        }),
                        Ok(Some(()))
                    );
                    observed.store(acquired, Ordering::SeqCst);
                });
                writer.join().expect("join late TLS writer");
            }));
        });

        release.shutdown();

        assert!(
            !writer_acquired.load(Ordering::SeqCst),
            "a TLS writer acquired the session after close_notify but before socket shutdown"
        );
    }

    fn handshaken_pair_with_store(
        store: Option<Arc<dyn ClientSessionStore>>,
    ) -> (ClientConnection, rustls::ServerConnection) {
        let stop_before_tickets = store.is_some();
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate a self-signed certificate");
        let cert = rustls::pki_types::CertificateDer::from(issued.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::try_from(issued.signing_key.serialize_der())
            .expect("serialize the test key");

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("server protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server config");

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the test certificate");
        let mut client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("client protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        if let Some(store) = store {
            client_config.resumption = Resumption::store(store);
        }

        let mut client = ClientConnection::new(
            Arc::new(client_config),
            rustls::pki_types::ServerName::try_from("localhost").expect("server name"),
        )
        .expect("client connection");
        let mut server =
            rustls::ServerConnection::new(Arc::new(server_config)).expect("server connection");

        // Pump both directions until neither has anything left to say.
        for _ in 0..16 {
            let mut to_server = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut to_server).expect("client write_tls");
            }
            if !to_server.is_empty() {
                let mut cursor = &to_server[..];
                while !cursor.is_empty() {
                    server.read_tls(&mut cursor).expect("server read_tls");
                    server.process_new_packets().expect("server process");
                }
            }
            // Stop before draining the post-handshake ticket flight. Tests that
            // need it can feed that ciphertext after the client is shared.
            if stop_before_tickets && !client.is_handshaking() && !server.is_handshaking() {
                return (client, server);
            }
            let mut to_client = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut to_client).expect("server write_tls");
            }
            if !to_client.is_empty() {
                let mut cursor = &to_client[..];
                while !cursor.is_empty() {
                    client.read_tls(&mut cursor).expect("client read_tls");
                    client.process_new_packets().expect("client process");
                }
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return (client, server);
            }
        }
        panic!("the in-memory handshake did not converge");
    }

    thread_local! {
        static SESSION_STORE_PROBE: RefCell<Option<SharedSession>> = const { RefCell::new(None) };
        static SESSION_STORE_MUTEX_LOCKED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    #[derive(Debug)]
    struct SessionStoreProbe {
        panic_on_ticket: bool,
    }

    impl ClientSessionStore for SessionStoreProbe {
        fn set_kx_hint(&self, _server_name: ServerName<'static>, _group: NamedGroup) {}

        fn kx_hint(&self, _server_name: &ServerName<'_>) -> Option<NamedGroup> {
            None
        }

        fn set_tls12_session(
            &self,
            _server_name: ServerName<'static>,
            _value: Tls12ClientSessionValue,
        ) {
        }

        fn tls12_session(&self, _server_name: &ServerName<'_>) -> Option<Tls12ClientSessionValue> {
            None
        }

        fn remove_tls12_session(&self, _server_name: &ServerName<'static>) {}

        fn insert_tls13_ticket(
            &self,
            _server_name: ServerName<'static>,
            _value: Tls13ClientSessionValue,
        ) {
            SESSION_STORE_PROBE.with(|slot| {
                if let Some(session) = slot.borrow().as_ref() {
                    SESSION_STORE_MUTEX_LOCKED
                        .with(|locked| locked.set(Some(session.mutex_is_locked())));
                }
            });
            assert!(
                !self.panic_on_ticket,
                "session-store callback panic fixture"
            );
        }

        fn take_tls13_ticket(
            &self,
            _server_name: &ServerName<'static>,
        ) -> Option<Tls13ClientSessionValue> {
            None
        }
    }

    #[test]
    fn a_rustls_session_store_callback_runs_without_the_session_mutex() {
        let (client, mut server) = handshaken_pair_with_store(Some(Arc::new(SessionStoreProbe {
            panic_on_ticket: false,
        })));
        let session = share(client);
        SESSION_STORE_PROBE.with(|slot| *slot.borrow_mut() = Some(session.clone()));
        SESSION_STORE_MUTEX_LOCKED.with(|locked| locked.set(None));

        let mut tickets = Vec::new();
        while server.wants_write() {
            server
                .write_tls(&mut tickets)
                .expect("serialize post-handshake tickets");
        }
        assert!(
            !tickets.is_empty(),
            "the server produced no post-handshake ticket callback fixture"
        );
        let mut ciphertext = tickets.as_slice();
        session
            .with(|session| session.feed_ciphertext_step(&mut ciphertext))
            .expect("process post-handshake tickets");

        let observed = SESSION_STORE_MUTEX_LOCKED.with(Cell::get);
        SESSION_STORE_PROBE.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(
            observed,
            Some(false),
            "rustls invoked the caller session store while the TLS session mutex was locked"
        );
        session
            .with(|_| Ok(()))
            .expect("the session was not returned after the callback");
    }

    #[test]
    fn a_panicking_rustls_callback_poisons_the_shared_session() {
        let (client, mut server) = handshaken_pair_with_store(Some(Arc::new(SessionStoreProbe {
            panic_on_ticket: true,
        })));
        let session = share(client);
        SESSION_STORE_PROBE.with(|slot| *slot.borrow_mut() = Some(session.clone()));
        SESSION_STORE_MUTEX_LOCKED.with(|locked| locked.set(None));

        let mut tickets = Vec::new();
        while server.wants_write() {
            server
                .write_tls(&mut tickets)
                .expect("serialize post-handshake tickets");
        }
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ciphertext = tickets.as_slice();
            let _ = session.with(|session| session.feed_ciphertext_step(&mut ciphertext));
        }));

        SESSION_STORE_PROBE.with(|slot| *slot.borrow_mut() = None);
        assert!(panic.is_err(), "the caller callback did not panic");
        assert_eq!(
            SESSION_STORE_MUTEX_LOCKED.with(Cell::get),
            Some(false),
            "the panicking caller callback ran while the state mutex was locked"
        );
        let error = session
            .with(|_| Ok(()))
            .expect_err("a partially unwound rustls session was reused");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    /// An orderly `close_notify` with ciphertext still buffered behind it is
    /// END OF STREAM, not a protocol error.
    ///
    /// `read_tls` answers `Ok(0)` unconditionally once that alert has been
    /// processed. The decrypt loop reads "rustls took nothing" as a stall and
    /// refuses, which is right for a genuine stall and wrong here - so it has
    /// to ask whether the peer closed before deciding. Anything after the alert
    /// is unreachable by definition, and a trailing byte is what forces the
    /// loop to look at the buffer again after the close.
    ///
    /// This test does NOT cover a stall that is NOT a close: that arm still
    /// returns the protocol error, and nothing here exercises it.
    #[compio::test]
    async fn a_close_notify_with_trailing_ciphertext_ends_the_stream() {
        let (client, mut server) = handshaken_pair();

        let mut wire = Vec::new();
        server
            .writer()
            .write_all(b"payload")
            .expect("queue application data");
        server.send_close_notify();
        while server.wants_write() {
            server.write_tls(&mut wire).expect("server write_tls");
        }
        // A SEPARATE read, delivered after the alert has been processed. It is
        // unreachable by the protocol, and that is the point: the loop has to
        // consult rustls once more with bytes in hand, and see `Ok(0)`. Put in
        // the same chunk it would simply be swallowed with everything else.
        let session = share(client);
        let mut reader = TlsReader::new(
            ScriptedPeer {
                chunks: std::collections::VecDeque::from(vec![wire, vec![0x17]]),
            },
            session,
        );

        let mut out = vec![0u8; 64];
        let n = reader
            .fill(out.len())
            .await
            .expect("the application data before the alert must still arrive");
        out[..n].copy_from_slice(&reader.plain[..n]);
        assert_eq!(&out[..n], b"payload");

        assert_eq!(
            reader
                .fill(64)
                .await
                .expect("a close_notify is an ending, not a protocol failure"),
            0,
            "the stream must report end of file after close_notify"
        );
    }

    fn client_connection() -> ClientConnection {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("default protocol versions")
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("example.invalid").expect("server name"),
        )
        .expect("client connection")
    }

    /// A peer that vanishes mid-handshake is a TLS-level failure, not a quiet
    /// end. rustls treats silent truncation as an attack rather than a close,
    /// and a caller that saw a bare EOF here could not tell "the server hung

    /// Draining rustls must not turn its bounded outbound buffer into an
    /// unbounded one of ours.
    ///
    /// rustls caps what it will hold; `write_tls` moves those bytes into a
    /// plain `Vec`. Since the READ path also produces ciphertext while only
    /// the WRITE half empties the queue, an idle connection whose peer keeps
    /// generating responses would grow that `Vec` forever if nothing stopped
    /// it. The queue is pre-loaded to the cap here, which is the state an idle
    /// connection reaches; a fresh session has only a ClientHello to give and
    /// could never demonstrate the bound.
    #[test]
    fn a_full_outbound_queue_stops_draining_the_session() {
        let mut session = TlsSession::new(client_connection());
        assert!(
            session.conn.wants_write(),
            "the fixture must have something to write, or this asserts nothing"
        );

        session.outgoing = vec![0u8; OUTGOING_SOFT_CAP];
        session
            .collect_outgoing()
            .expect("collecting into a full queue is not an error");
        assert_eq!(
            session.outgoing.len(),
            OUTGOING_SOFT_CAP,
            "a queue already at the cap must not grow"
        );
        assert!(
            session.conn.wants_write(),
            "the ciphertext must still be inside rustls, which is what bounds it"
        );

        // The control: with room, the same call DOES drain. Without this the
        // assertion above would also pass if `collect_outgoing` never worked.
        session.outgoing.clear();
        session
            .collect_outgoing()
            .expect("collecting into an empty queue");
        assert!(
            !session.outgoing.is_empty(),
            "collect_outgoing moved nothing even with an empty queue"
        );
    }
    /// up" from "someone cut the connection during key exchange".
    #[compio::test]
    async fn a_peer_that_closes_mid_handshake_is_refused() {
        let mut peer = SilentPeer {
            pending: 0,
            written: 0,
        };
        let error = handshake(&mut peer, client_connection())
            .await
            .err()
            .expect("a truncated handshake must not succeed");

        let rendered = format!("{error}");
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; ");
        assert!(
            rendered.contains("TLS") || chain.contains("TLS") || chain.contains("handshake"),
            "the refusal must read as a TLS failure: {rendered} / {chain}"
        );
    }

    /// The control for the test above: the ClientHello really was flushed
    /// before the read happened. Without this, the refusal would also be
    /// produced by a driver that read first and never wrote at all - which
    /// would deadlock against a real server, because the server answers a
    /// flight it has not received with nothing.
    #[compio::test]
    async fn the_client_hello_is_flushed_before_the_first_read() {
        let mut peer = SilentPeer {
            pending: 0,
            written: 0,
        };
        let _ = handshake(&mut peer, client_connection()).await;
        assert!(
            peer.written > 0,
            "no ClientHello bytes reached the peer before the handshake read"
        );
    }
}
