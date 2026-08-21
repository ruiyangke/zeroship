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
//! only when the runtime's `Rc` is uniquely held
//! (`compio-runtime-0.11.0/src/runtime/mod.rs:392`), and a task parked on an
//! io_uring submission holds a clone of that `Rc` inside the pending `Submit`
//! (`compio-runtime-0.11.0/src/runtime/future.rs:46`). A parked connection task
//! is therefore exactly the case the reclaim skips: the runtime, its driver,
//! the task and the socket all leak, and the server-side backend stays live for
//! the rest of the PROCESS. Measured in
//! `tests/integration.rs::a_connection_does_not_outlive_the_runtime_that_opened_it`.
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
//! the socket and the server backend alive. `shutdown(Both)` changes the shared
//! socket state reached through every dup, so the session ends immediately.
//!
//! [`ConnectionDropRelease`] is an RAII guard: it lives in `Connection` before
//! `run`, then in a local spanning the run future. Its `Drop` is the same plain
//! syscall as the client-side release and therefore needs no executor. The
//! guard holds only a `Weak` view of the client's dup. If the client goes away
//! first, its drop must still close that descriptor even when compio never
//! reclaims the parked connection task described above.

use std::{
    net::Shutdown,
    sync::{Arc, Weak},
};

/// Shuts down the connection's socket when dropped.
///
/// Lives in `InnerClient`, so it drops when the last handle to a given
/// connection's client half drops - which is the point at which no caller can
/// issue another query on it.
#[derive(Debug)]
pub(crate) struct ConnectionRelease {
    /// A dup of the connection's socket. Owned, so dropping this closes it;
    /// `shutdown` on it reaches the same underlying socket the connection task
    /// is using.
    socket: Arc<socket2::Socket>,
}

/// A non-owning shutdown guard for every connection-side exit.
#[derive(Clone, Debug)]
pub(crate) struct ConnectionDropRelease {
    socket: Weak<socket2::Socket>,
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
#[cfg(unix)]
impl ConnectionRelease {
    pub(crate) fn dup_of<F: std::os::fd::AsFd>(handle: &F) -> Option<Self> {
        handle
            .as_fd()
            .try_clone_to_owned()
            .ok()
            .map(|owned| Self {
                socket: Arc::new(socket2::Socket::from(owned)),
            })
    }
}

#[cfg(windows)]
impl ConnectionRelease {
    pub(crate) fn dup_of<F: std::os::windows::io::AsSocket>(handle: &F) -> Option<Self> {
        handle
            .as_socket()
            .try_clone_to_owned()
            .ok()
            .map(|owned| Self {
                socket: Arc::new(socket2::Socket::from(owned)),
            })
    }
}

impl ConnectionRelease {
    pub(crate) fn connection_guard(&self) -> ConnectionDropRelease {
        ConnectionDropRelease {
            socket: Arc::downgrade(&self.socket),
        }
    }

    /// End the session synchronously while client handles still exist.
    ///
    /// Timeout recovery uses this when it cannot prove that the connection
    /// reached `ReadyForQuery`. Closing only the request channel would let the
    /// connection task keep draining an unacknowledged cancelled query, so the
    /// backend could remain occupied indefinitely.
    pub(crate) fn shutdown(&self) {
        // A peer close or another release guard may win the race. In every
        // error case the socket is already unusable, which is the requested
        // postcondition, so there is no useful error to propagate.
        let _ = self.socket.shutdown(Shutdown::Both);
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
    /// End the physical session from a connection-side fatal I/O path.
    ///
    /// In particular, compio does not promise that cancelling a submitted
    /// read promptly releases its shared descriptor. A socket-read timeout
    /// therefore performs this synchronous shutdown before waiting for the
    /// main connection task to finish its teardown.
    pub(crate) fn shutdown(&self) {
        if let Some(socket) = self.socket.upgrade() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for ConnectionRelease {
    fn drop(&mut self) {
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
