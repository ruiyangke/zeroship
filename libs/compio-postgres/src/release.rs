//! Releasing a connection's socket when either half goes away first.
//!
//! # Why a synchronous shutdown, and not the connection task's own teardown
//!
//! The connection task has its own clean teardown: dropping the
//! [`Client`](crate::Client) closes the request channel, and the task writes
//! `Terminate`, emits a FIN and returns (`connection.rs`, the
//! `client_gone && !terminate_sent` branch). That teardown is *asynchronous* -
//! it only happens if something polls the task afterwards.
//!
//! Nothing guarantees anything will. compio's `Runtime::drop` reclaims a task
//! only when the runtime's `Rc` is uniquely held, and a task parked on an
//! io_uring submission holds a clone of that `Rc` inside the pending `Submit`.
//! A parked connection task is therefore exactly the case the reclaim skips:
//! the runtime, its driver, the task and the socket all leak, and the
//! server-side backend stays live for the rest of the PROCESS. See
//! `tests/integration/integration.rs::a_connection_does_not_outlive_the_runtime_that_opened_it`.
//!
//! So the socket's release cannot be left to a future. It has to be a plain
//! syscall on a descriptor this crate owns, run from a `Drop` that needs no
//! executor - which is what [`ConnectionRelease`] is.
//!
//! # Why a dup(2) and not the connection's own descriptor
//!
//! Holding the raw descriptor number and shutting *that* down races descriptor
//! reuse. The client half can outlive its connection - a server-initiated close
//! or a fatal I/O error ends the task and closes the socket while the `Client`
//! is still in the caller's hand - and once the number is free the kernel will
//! reissue it, so a shutdown on it would tear down a stranger's connection.
//! Nothing here can observe that from the client side, so it cannot be checked;
//! owning a `dup` removes the race by construction instead. The descriptor
//! stays valid because we hold it, and `shutdown` on a dup acts on the same
//! underlying socket, so the server still sees the FIN.
//!
//! Not a `compio::driver::SharedFd` clone, which would also pin the descriptor
//! and cost nothing: `SharedFd` is refcounted with `Rc` unless compio's `sync`
//! feature is on, so a `SharedFd` field would make [`Client`](crate::Client)
//! `!Send`.
//!
//! # Why the connection half also needs a shutdown guard
//!
//! The mirror cases are every way the connection half can end while the client
//! is still live: it can be discarded without being run, its `run` future can
//! be cancelled, the future can unwind, or the task can return. Dropping the
//! connection's descriptor only removes one reference; the client's dup keeps
//! the socket and the server backend alive.
//!
//! A plain `shutdown(Both)` is not enough to end the session promptly. It sends
//! a FIN, but a peer blocked writing into our full receive window does not send
//! again until its zero-window probe timer, which backs off - so the backend can
//! outlive the driver by seconds. A fatal read error therefore shuts the shared
//! descriptor down, so an idle peer still sees a FIN at once, and then closes
//! it. The receive queue still holds the bytes the peer sent into that
//! full window, and closing the LAST descriptor of a socket with unread data
//! resets the connection, failing the peer's blocked write at once. The client
//! half still owns the slot, so [`ConnectionDropRelease`] upgrades a `Weak` and
//! takes the socket out of it; closing that dup is what makes the connection
//! task's own close the last one. Every other connection-side exit keeps the
//! graceful `shutdown`.
//!
//! [`ConnectionDropRelease`] is an RAII guard: it lives in `Connection` before
//! `run`, then in a local spanning the run future. Its `Drop` is the same plain
//! syscall as the client-side release and therefore needs no executor. The
//! guard holds only a `Weak` view of the client's dup. If the client goes away
//! first, its drop must still close that descriptor even when compio never
//! reclaims the parked connection task described above.
//!
//! # Why TLS state is attached to the client-side release
//!
//! A TLS session must send an encrypted `close_notify` before the socket is
//! shut down. The record cannot be prepared during the handshake because its
//! sequence number follows every application write. The rustls connection and
//! its ordered ciphertext queue are therefore shared through `SharedSession`.
//! `ConnectionRelease::drop` leases that live state without holding its state
//! mutex, serializes the alert, sends it nonblocking on the owned dup, and only
//! then calls `shutdown(Both)`. It skips the alert if another TLS operation
//! holds the session lease, because waiting in `Drop` can deadlock while
//! overtaking that operation would make the alert invalid. Every step remains
//! best effort because `Drop` has no caller to report an error to and the
//! physical session is ending regardless.

use std::{
    fmt,
    net::Shutdown,
    sync::{Arc, Weak},
};

#[cfg(test)]
thread_local! {
    pub(crate) static TLS_BEFORE_SHUTDOWN_PROBE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Shuts down the connection's socket when dropped.
///
/// Lives in `InnerClient`, so it drops when the last handle to a given
/// connection's client half drops - which is the point at which no caller can
/// issue another query on it.
pub(crate) struct ConnectionRelease {
    /// A dup of the connection's socket, shared with the connection-side guard
    /// so an error exit can close it while a `Client` handle still holds this
    /// half. `Some` until a guard closes it; `shutdown` on the inner socket
    /// reaches the same underlying socket the connection task is using.
    socket: Arc<parking_lot::Mutex<Option<socket2::Socket>>>,
    /// The live rustls state, including ciphertext not yet handed to the
    /// socket. Present only when this physical connection negotiated TLS.
    #[cfg(feature = "tls")]
    tls_session: Option<crate::tls_sansio::SharedSession>,
    /// Serializes "serialize the alert, send it, THEN shut the socket down"
    /// against the connection-side guard, which runs the identical sequence on
    /// the SAME socket.
    ///
    /// Losing the shutdown race is harmless; losing it MID-SEQUENCE is not.
    /// `take_close_notify` sets `close_notify_sent`, so serializing the alert
    /// CONSUMES it: if the other guard shuts the socket between our serialize
    /// and our send, the alert is destroyed and cannot be regenerated, and the
    /// session ends with the reset this machinery exists to avoid.
    ///
    /// A blocking mutex is safe here even though one holder is a `Drop`: the
    /// critical section is a serialize, one non-blocking send and a
    /// `shutdown(2)`, and it never waits on the rustls session, which is taken
    /// with `try_with`.
    #[cfg(feature = "tls")]
    alert_then_shutdown: Arc<parking_lot::Mutex<()>>,
}

impl fmt::Debug for ConnectionRelease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ConnectionRelease");
        debug.field("socket", &self.socket);
        #[cfg(feature = "tls")]
        debug.field("has_tls_session", &self.tls_session.is_some());
        debug.finish()
    }
}

/// A non-owning shutdown guard for every connection-side exit.
#[derive(Clone)]
pub(crate) struct ConnectionDropRelease {
    socket: Weak<parking_lot::Mutex<Option<socket2::Socket>>>,
    /// Shared with the client-side guard; see its field for why.
    #[cfg(feature = "tls")]
    alert_then_shutdown: Arc<parking_lot::Mutex<()>>,
    /// The same live rustls state the client-side release carries. This guard
    /// needs it for the same reason: every exit it covers ends the physical
    /// session, and ending a TLS session without an alert makes the server log
    /// a reset. Cloning the `Arc` is what shares it, not a second session.
    #[cfg(feature = "tls")]
    tls_session: Option<crate::tls_sansio::SharedSession>,
}

impl fmt::Debug for ConnectionDropRelease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ConnectionDropRelease");
        debug.field("socket", &self.socket);
        #[cfg(feature = "tls")]
        debug.field("has_tls_session", &self.tls_session.is_some());
        debug.finish()
    }
}

/// Serialize and send `close_notify` on `socket`, best effort.
///
/// Shared by both guards: the client half and the connection half each end the
/// physical session, and both must close TLS cleanly. Everything here is best
/// effort because a `Drop` has no caller to report to and the session is
/// ending regardless.
#[cfg(feature = "tls")]
#[allow(clippy::significant_drop_tightening)]
fn send_close_notify_on(
    socket: &socket2::Socket,
    session: Option<&crate::tls_sansio::SharedSession>,
) {
    let Some(session) = session else {
        return;
    };

    // `Drop` must not wait for socket backpressure. Setting O_NONBLOCK on the
    // shared file description is harmless here because shutdown is the very
    // next operation on this connection, successful alert or not.
    if socket.set_nonblocking(true).is_err() {
        return;
    }

    // Keep the exclusive lease through the synchronous sends so no
    // connection-task write can change the record sequence between
    // serialization and delivery. Never wait for that lease: its holder can
    // be in a caller-supplied rustls callback waiting for this Drop. A callback
    // panic poisons the rustls state; catch it here because this best-effort
    // Drop path must still reach the socket shutdown.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.try_with(|tls| {
            let ciphertext = tls.take_close_notify()?;

            // TERMINAL FROM HERE, and set WHILE THIS LEASE IS STILL HELD.
            // The lease is released when this closure ends, but the socket
            // shutdown happens outside it, and in that window a concurrent
            // writer could otherwise acquire the session and put a record
            // BEHIND the alert - which the peer must never see. Setting it
            // after `try_with` returns would leave exactly that gap; setting
            // it before would make `try_with` refuse its own lease and send
            // no alert at all.
            session.mark_close_notify_sent();

            let mut remaining = ciphertext.as_slice();
            while !remaining.is_empty() {
                #[cfg(target_os = "linux")]
                let sent = socket
                    .send_with_flags(remaining, nix::sys::socket::MsgFlags::MSG_NOSIGNAL.bits());
                #[cfg(not(target_os = "linux"))]
                let sent = socket.send(remaining);

                match sent {
                    Ok(0) => break,
                    Ok(count) => remaining = &remaining[count..],
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            Ok(())
        })
    }));
    #[cfg(test)]
    TLS_BEFORE_SHUTDOWN_PROBE.with(|slot| {
        if let Some(probe) = slot.borrow_mut().take() {
            probe();
        }
    });
}

/// Duplicates the descriptor behind `handle` and takes ownership of the copy.
///
/// Returns `None` if the duplication fails. A connection whose socket cannot be
/// duplicated still works; it only loses the deterministic release, which is
/// strictly better than refusing to connect over it.
///
/// Written twice, once per descriptor family, because the borrow types differ
/// (`BorrowedFd` / `BorrowedSocket`) and both offer `try_clone_to_owned` - so
/// neither arm needs `unsafe`, which this crate denies.
///
/// The `cfg(windows)` arm is compiled by nothing in this repository: CI is
/// Linux-only and no job names a Windows target, so neither a local build nor
/// the clippy gate's `--all-features` sweep type-checks it. Keep the blind arm
/// as SMALL as possible rather than merely watched. It holds a trait bound, an
/// accessor and a `map`; the struct literal - the part a new field changes -
/// lives once in [`ConnectionRelease::from_owned`], which Linux compiles, so
/// adding a field fails the build at exactly ONE site. What is left blind is a
/// bound and an accessor, which break loudly against a changed `std` or
/// `socket2` API rather than quietly. Read both arms when touching either.
///
/// The `cfg(not(target_os = "linux"))` send in `send_close_notify_on` can be
/// checked here by flipping its `cfg` to `all()` and building, because it calls
/// a `socket2` method that exists on Linux too. The `cfg(windows)` arm cannot
/// be, because `std::os::windows` does not exist on this target.
impl ConnectionRelease {
    /// Build one from an owned handle, whatever the platform calls it.
    ///
    /// THE STRUCT LITERAL LIVES HERE AND NOWHERE ELSE, deliberately: this is
    /// the part that changes when a field is added, and the `cfg(windows)`
    /// arm below is compiled by nothing in this repository. Each arm carries a
    /// bound, an accessor and a `map`, so a new field cannot diverge between
    /// them.
    ///
    /// One `Into` bound covers both because socket2 implements `From<OwnedFd>`
    /// and `From<OwnedSocket>` for `Socket` on Unix and Windows respectively.
    fn from_owned(owned: impl Into<socket2::Socket>) -> Self {
        Self {
            socket: Arc::new(parking_lot::Mutex::new(Some(owned.into()))),
            #[cfg(feature = "tls")]
            tls_session: None,
            #[cfg(feature = "tls")]
            alert_then_shutdown: Arc::new(parking_lot::Mutex::new(())),
        }
    }
}

#[cfg(unix)]
impl ConnectionRelease {
    pub(crate) fn dup_of<F: std::os::fd::AsFd>(handle: &F) -> Option<Self> {
        handle
            .as_fd()
            .try_clone_to_owned()
            .ok()
            .map(Self::from_owned)
    }
}

#[cfg(windows)]
impl ConnectionRelease {
    pub(crate) fn dup_of<F: std::os::windows::io::AsSocket>(handle: &F) -> Option<Self> {
        handle
            .as_socket()
            .try_clone_to_owned()
            .ok()
            .map(Self::from_owned)
    }
}

impl ConnectionRelease {
    #[cfg(feature = "tls")]
    pub(crate) fn set_tls_session(&mut self, session: crate::tls_sansio::SharedSession) {
        self.tls_session = Some(session);
    }

    pub(crate) fn connection_guard(&self) -> ConnectionDropRelease {
        ConnectionDropRelease {
            socket: Arc::downgrade(&self.socket),
            // The guard is built after `configure_release` has attached the
            // session, so it gets the same handle rather than nothing.
            #[cfg(feature = "tls")]
            tls_session: self.tls_session.clone(),
            // The SAME lock, not a second one: it exists to order this guard
            // against the client-side one.
            #[cfg(feature = "tls")]
            alert_then_shutdown: Arc::clone(&self.alert_then_shutdown),
        }
    }

    /// End the session synchronously while client handles still exist.
    ///
    /// Timeout recovery uses this when it cannot prove that the connection
    /// reached `ReadyForQuery`. Closing only the request channel would let the
    /// connection task keep draining an unacknowledged cancelled query, so the
    /// backend could remain occupied indefinitely.
    pub(crate) fn shutdown(&self) {
        // The alert belongs HERE rather than in `Drop`, because six production
        // sites call this without dropping: pool command-timeout recovery
        // through `Client::force_close`, and four replication cleanup paths.
        // With it in `Drop` only, every one of those ended a TLS session with
        // no alert, and the later `Drop` could not make up for it - by then
        // this socket is already down and the send fails.
        // NON-BLOCKING, and the whole pair is inside it. The other guard runs
        // this same alert-then-shutdown pair on this same socket, and a
        // shutdown landing between our serialize and our send destroys an
        // alert that cannot be rebuilt.
        //
        // On contention we do NOTHING rather than wait: the holder is already
        // committed to both steps, so it delivers the alert AND the shutdown.
        // Waiting here instead would be a blocking wait inside a `Drop`, which
        // is the very thing `try_with` exists to avoid - and it deadlocks
        // outright when the holder is parked mid-sequence.
        #[cfg(feature = "tls")]
        let Some(_ordered) = self.alert_then_shutdown.try_lock() else {
            return;
        };

        let guard = self.socket.lock();
        if let Some(socket) = guard.as_ref() {
            #[cfg(feature = "tls")]
            send_close_notify_on(socket, self.tls_session.as_ref());

            // A peer close or another release guard may win the race. In every
            // error case the socket is already unusable, which is the requested
            // postcondition, so there is no useful error to propagate.
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    /// Whether this half still owns the shared descriptor.
    ///
    /// Test-only: a connection-side `abort` takes the socket out of the shared
    /// slot, while a graceful `shutdown` leaves it for this half to close.
    #[cfg(test)]
    pub(crate) fn shared_socket_is_open(&self) -> bool {
        self.socket.lock().is_some()
    }
}

impl Drop for ConnectionDropRelease {
    fn drop(&mut self) {
        // Drop cannot report an error, and a concurrent client release or peer
        // close can legitimately win this shutdown race.
        self.shutdown();
    }
}

impl ConnectionDropRelease {
    /// Shut the shared socket down without closing it.
    ///
    /// The graceful mirror of the client half's release: every connection-side
    /// exit that is not a fatal read error, and the guard's own `Drop`, end the
    /// session with a FIN. A fatal read error uses [`Self::abort`], which closes
    /// the shared descriptor so the final close resets a blocked writer.
    pub(crate) fn shutdown(&self) {
        let Some(socket_slot) = self.socket.upgrade() else {
            return;
        };
        // Same lock, same non-blocking rule, same reason as the client half:
        // the pair must not interleave with the other guard's pair, and the
        // loser defers to a holder already committed to both steps.
        #[cfg(feature = "tls")]
        let Some(_ordered) = self.alert_then_shutdown.try_lock() else {
            return;
        };

        let guard = socket_slot.lock();
        if let Some(socket) = guard.as_ref() {
            // The alert first, then the shutdown, in that order and for the
            // same reason as the client half: once the socket is down for
            // reading there is nothing left to write the alert through.
            #[cfg(feature = "tls")]
            send_close_notify_on(socket, self.tls_session.as_ref());

            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    /// End the physical session from a fatal read error.
    ///
    /// Two effects, in this order. `shutdown(Both)` sends a FIN at once, so a
    /// peer that is idle - not blocked, nothing unread on our side - still
    /// learns the session ended promptly. `shutdown` does not purge the receive
    /// queue, so the bytes a blocked peer sent into our full window stay
    /// queued. The socket is then taken out of the shared slot and dropped; the
    /// connection task's own descriptor close becomes the last one, and the
    /// kernel resets a socket whose receive queue still holds unread data,
    /// failing the blocked write at once. The client half still holds the shared
    /// slot. See the module docs.
    pub(crate) fn abort(&self) {
        let Some(socket_slot) = self.socket.upgrade() else {
            return;
        };
        // BLOCKING, not `try_lock` like the client half. `abort` is reached only
        // from the multiplexed read task, never from a `Drop`, so waiting cannot
        // stall a destructor. It must not lose the ordering lock: returning
        // without ending the session would leave the peer blocked exactly as
        // before. The holder is only ever inside a synchronous serialize and a
        // nonblocking send, so the wait is bounded.
        #[cfg(feature = "tls")]
        let _ordered = self.alert_then_shutdown.lock();

        let mut guard = socket_slot.lock();
        if let Some(socket) = guard.as_mut() {
            #[cfg(feature = "tls")]
            send_close_notify_on(socket, self.tls_session.as_ref());

            let _ = socket.shutdown(Shutdown::Both);
        }
        guard.take();
    }
}

impl Drop for ConnectionRelease {
    fn drop(&mut self) {
        // `shutdown` sends the TLS alert first; see it for why that is not
        // done here.
        //
        // `Both`, not `Write`: a half-close would leave this side reading, and
        // the point is that the server observes the end of the session now.
        //
        // The error is discarded on purpose. Every failure mode here means the
        // socket is already down - the server closed first, the connection task
        // shut it down, the peer reset it - and there is no caller left to tell:
        // the client half is being dropped.
        self.shutdown();
        // `self.socket` drops next, closing the dup.
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[derive(Debug)]
    enum Peer {
        Blocked,
        Failed(ErrorKind),
        StillBlocked,
    }

    /// Drive the full-window harness with `release_action` and report what the
    /// blocked peer observed within a bounded wait.
    ///
    /// The shape is the driver's: `release` owns the client half's dup and stays
    /// alive (a live `Client`), `client` stands in for the connection task's own
    /// descriptor, and only the connection-side guard runs - the read task's
    /// fatal-error exit. The peer blocks after the shrunk buffers fill, so the
    /// receive queue holds its unread bytes.
    fn full_window_peer_after(release_action: impl FnOnce(&ConnectionDropRelease)) -> Peer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind release regression listener");
        let address = listener.local_addr().expect("release regression address");
        let client = TcpStream::connect(address).expect("connect release regression client");
        let (mut peer, _) = listener.accept().expect("accept release regression peer");

        socket2::SockRef::from(&client)
            .set_recv_buffer_size(4096)
            .expect("shrink the client receive buffer");
        socket2::SockRef::from(&peer)
            .set_send_buffer_size(4096)
            .expect("shrink the peer send buffer");
        peer.set_write_timeout(Some(Duration::from_millis(100)))
            .expect("bound the peer write");

        let release = ConnectionRelease::dup_of(&client).expect("duplicate the release socket");
        let guard = release.connection_guard();

        let (signal_tx, signal_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let chunk = vec![b'x'; 65536];
            let started = std::time::Instant::now();
            let mut blocked = false;
            loop {
                match peer.write(&chunk) {
                    Ok(_) => {}
                    Err(error)
                        if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                    {
                        if !blocked {
                            blocked = true;
                            let _ = signal_tx.send(Peer::Blocked);
                        }
                        if started.elapsed() > Duration::from_secs(2) {
                            let _ = signal_tx.send(Peer::StillBlocked);
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = signal_tx.send(Peer::Failed(error.kind()));
                        return;
                    }
                }
            }
        });

        match signal_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the peer never filled the window")
        {
            Peer::Blocked => {}
            other => panic!("the peer failed before the release: {other:?}"),
        }

        release_action(&guard);
        // The connection task's own descriptor closes after its guard; the
        // client half's dup stays alive, exactly as a live `Client` is.
        drop(client);

        let outcome = signal_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the peer never reported after the connection-side release");
        writer.join().expect("the peer writer thread panicked");
        drop(release);
        outcome
    }

    /// A fatal read error's release must close the shared descriptor, so a peer
    /// blocked writing into our full receive window fails at once instead of
    /// waiting for a FIN to be noticed.
    ///
    /// The blocked peer left unread data in the receive queue; closing the last
    /// descriptor of a socket with unread data makes the kernel send RST, while
    /// the `shutdown` half of the release only sends a FIN that a zero-window
    /// peer does not act on.
    #[test]
    fn a_connection_side_release_unblocks_a_peer_stuck_on_a_full_receive_window() {
        match full_window_peer_after(ConnectionDropRelease::abort) {
            Peer::Failed(kind) => assert!(
                matches!(
                    kind,
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ),
                "the connection-side release failed the peer with {kind:?} instead of a reset"
            ),
            other => panic!("the blocked peer did not fail after the abort: {other:?}"),
        }
    }

    /// The negative control for the test above: the same shrunk buffers and the
    /// same blocked peer, but the graceful `shutdown` half alone leaves the peer
    /// blocked. The client-half release keeps its dup open for the whole
    /// observation, so the socket cannot reach its last close and no reset can
    /// end the wait early; the bound only has to be long enough to observe it.
    #[test]
    fn a_graceful_connection_side_release_leaves_a_blocked_peer_blocked() {
        match full_window_peer_after(ConnectionDropRelease::shutdown) {
            Peer::StillBlocked => {}
            other => panic!("the graceful release unblocked a peer that must stay blocked: {other:?}"),
        }
    }

    /// A fatal read error must also end an idle session promptly: `abort` sends a
    /// FIN before it closes the shared descriptor, so a peer that is not blocked
    /// and has nothing unread on our side still observes EOF.
    ///
    /// The client half's dup and a second descriptor stay open, so this release
    /// does not close the socket; the peer's EOF can only come from the FIN
    /// `abort` sends.
    #[test]
    fn a_connection_side_release_sends_a_fin_to_an_idle_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind idle release listener");
        let address = listener.local_addr().expect("idle release address");
        let client = TcpStream::connect(address).expect("connect idle release client");
        let (mut peer, _) = listener.accept().expect("accept idle release peer");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("bound the idle peer read");

        let release = ConnectionRelease::dup_of(&client).expect("duplicate the idle release socket");
        let guard = release.connection_guard();
        // A second descriptor, kept open so the socket cannot close under the
        // peer: the EOF it observes must be the FIN, not the last close.
        let held = client.try_clone().expect("hold a second idle release descriptor");

        guard.abort();
        drop(client);

        let mut byte = [0u8; 1];
        let observed = peer.read(&mut byte);
        drop(held);
        drop(release);

        match observed {
            Ok(0) => {}
            Ok(read) => panic!("the idle peer received {read} byte(s) instead of EOF"),
            Err(error) => panic!("the idle peer observed {error} instead of a FIN"),
        }
    }
}
