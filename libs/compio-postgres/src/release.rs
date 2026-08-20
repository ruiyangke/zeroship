//! Releasing a connection's socket when the client half goes away.
//!
//! # Why a synchronous shutdown, and not the connection task's own teardown
//!
//! The connection task already shuts the socket down cleanly: dropping the
//! [`Client`](crate::Client) closes the request channel, the task writes
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
//! reuse: if the connection task has already closed the socket - the ordinary
//! outcome when a client is dropped inside a live runtime - the number may
//! since have been handed to an unrelated socket, and this would tear down a
//! stranger's connection. Owning a `dup` removes the race by construction: the
//! descriptor stays valid because we hold it, and `shutdown` on a dup acts on
//! the same underlying socket, so the server still sees the FIN.
//!
//! Not a `compio::driver::SharedFd` clone, which would also pin the descriptor
//! and cost nothing: `SharedFd` is refcounted with `Rc` unless compio's `sync`
//! feature is on, so a `SharedFd` field would make [`Client`](crate::Client)
//! `!Send`.

use std::net::Shutdown;

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
    socket: socket2::Socket,
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
                socket: socket2::Socket::from(owned),
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
                socket: socket2::Socket::from(owned),
            })
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
        let _ = self.socket.shutdown(Shutdown::Both);
        // `self.socket` drops next, closing the dup.
    }
}
