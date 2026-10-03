//! The runtime a live control test runs on, and the background tasks it owns.
//!
//! A compio runtime is freed only once nothing else holds it, and every I/O
//! operation in flight holds it. A task still parked on a socket when the test
//! body returns - a connection driver, a mock server's accept loop, an HTTP
//! connection a client pool kept - therefore keeps its runtime alive, and with
//! it the runtime's `io_uring` ring, until the process exits. Each ring is
//! charged to the locked-memory limit, so a test binary that strands one per
//! case exhausts that limit partway through and every later case fails to
//! create its runtime.
//!
//! A live case leaves nothing parked. Its runtime runs the test body to the
//! end, then
//!
//! 1. cancels every task the body started through [`spawn`] and waits until
//!    each one is dropped - bounded, and a task that will not stop fails the
//!    case naming itself rather than hanging the binary,
//! 2. waits until every `PostgreSQL` connection the thread opened has closed,
//! 3. keeps running the event loop until every connection the case opened to
//!    one of its own servers has closed, so a connection a library task was
//!    closing - an HTTP client after a `Connection: close` response - finishes
//!    before the runtime its task runs on goes,
//!
//! and only then lets the runtime go. It then checks that the runtime's ring
//! was released, and fails the case if anything still held it. A body that
//! panics gets the same teardown, ring check included, before its panic
//! resumes; the body's panic wins if the ring check also fails.
//!
//! Servers a case points production HTTP clients at end every connection after
//! its response ([`CloseConnections`], or a `connection: close` header), because
//! some of those clients live per thread and would otherwise keep one idle.
//!
//! The connections of the third wait are the client ends this process holds to
//! a listening port the case registered through [`register_listener`], found by
//! the process's socket table. Only those ports are considered, so cases that
//! run at the same time in one process wait for their own servers and not for
//! each other's; the registration is thread-local, which is the scope a case
//! runs in. A connection to a server the case did not register is not waited
//! for, and a socket whose local port is one of the process's listening ports
//! is the server end of a connection, not a client end.
//!
//! A case is attached through compio's own test attribute, whose `crate`
//! argument names the module that holds `runtime::Runtime`:
//!
//! - `#[compio::test(crate = "crate::support::live")]` runs the body on a plain
//!   compio runtime;
//! - `#[compio::test(crate = "crate::support::live::system")]` runs it inside an
//!   ntex system, for a body that needs one.

#![allow(
    clippy::future_not_send,
    reason = "a live case runs on one compio thread"
)]

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::SocketAddr;
use std::panic::{resume_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use futures::FutureExt;

/// How long a finished case waits for a task to stop or a connection to close.
/// A task or connection whose handles are all gone stops in a round trip; one
/// that is still held never does, and the case fails naming it.
const CONNECTION_CLOSE: Duration = Duration::from_secs(10);

/// A task the running case owns, with the number that identifies it in a
/// failure.
struct Owned {
    id: u64,
    task: compio::runtime::JoinHandle<()>,
}

thread_local! {
    /// The tasks the running case owns. `None` outside a case.
    static OWNED: RefCell<Option<Vec<Owned>>> = const { RefCell::new(None) };
    /// The ring of the runtime the running case is on.
    static RING: Cell<Option<Ring>> = const { Cell::new(None) };
    /// Listening ports servers on this thread have registered for a case. A
    /// case takes them when it starts, which consumes anything registered
    /// before it on this thread, and takes again at the end for servers it
    /// started while its body ran.
    static LISTENERS: RefCell<BTreeSet<u16>> = const { RefCell::new(BTreeSet::new()) };
    /// The number of the next task started by a case on this thread.
    static NEXT_TASK: Cell<u64> = const { Cell::new(0) };
}

/// Register a listening server's address so the case running on this thread
/// waits for the connections it opened to it.
///
/// A server constructor calls this with the address it bound. The registration
/// is thread-local and is consumed by the case on this thread, so a case that
/// runs beside another in one process waits only for its own servers.
pub fn register_listener(address: SocketAddr) {
    LISTENERS.with(|listeners| listeners.borrow_mut().insert(address.port()));
}

/// Start `task` on the current runtime, owned by the running live case.
///
/// The case cancels it when the body returns, so a task that never finishes on
/// its own - an accept loop, a connection driver whose client is still held -
/// cannot outlive the case. A task that ended in a panic fails the case.
///
/// # Panics
///
/// Outside a live case. Nothing would cancel the task there, and a task parked
/// on I/O when its runtime is dropped strands that runtime's ring.
pub fn spawn(task: impl Future<Output = ()> + 'static) {
    OWNED.with(|owned| {
        let mut owned = owned.borrow_mut();
        let owned = owned.as_mut().expect(
            "a background task was started outside a live case, so nothing would cancel it; \
             run the test as a live case (tests/support/live.rs)",
        );
        let id = NEXT_TASK.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        owned.push(Owned {
            id,
            task: compio::runtime::spawn(task),
        });
    });
}

/// Run `body`, then tear the case down as the module describes.
async fn case<T>(body: impl Future<Output = T>) -> T {
    RING.with(|ring| ring.set(Some(Ring::of_current_runtime())));
    let listeners = LISTENERS.with(|listeners| std::mem::take(&mut *listeners.borrow_mut()));
    let connections_before: BTreeSet<u64> = tcp_connections(&listeners).into_keys().collect();
    OWNED.with(|owned| {
        let previous = owned.borrow_mut().replace(Vec::new());
        assert!(
            previous.is_none(),
            "a live case started inside another live case on the same thread"
        );
    });
    let outcome = AssertUnwindSafe(body).catch_unwind().await;
    let background = cancel_owned().await;
    OWNED.with(|owned| owned.borrow_mut().take());
    let closed = compio_postgres::drain_connections(CONNECTION_CLOSE).await;
    let listeners = LISTENERS.with(|current| {
        let mut all = listeners;
        all.extend(std::mem::take(&mut *current.borrow_mut()));
        all
    });
    let still_open = close_tcp_connections(&listeners, &connections_before).await;
    let value = match outcome {
        Ok(value) => value,
        Err(panic) => resume_unwind(panic),
    };
    if let Some(panic) = background {
        resume_unwind(panic);
    }
    assert!(
        closed,
        "{} PostgreSQL connection(s) this case opened are still open after its body \
         returned and its tasks were cancelled; a handle to them outlived the case",
        compio_postgres::live_connections()
    );
    assert!(
        still_open.is_empty(),
        "TCP connection(s) opened while this case ran are still open after its body \
         returned: {still_open:?}. A client pool kept one, or a server the case \
         points production code at holds its connections open"
    );
    value
}

/// Keep the event loop running until every TCP connection to a registered
/// listener that is not in `before` has closed, or [`CONNECTION_CLOSE`] passes.
/// Returns the ones still open.
///
/// A connection is closed when its socket is, which a task does as it
/// finishes: the task can still have that last turn to run. So the wait ends
/// only once a full turn of the loop has passed with nothing open.
async fn close_tcp_connections(
    listeners: &BTreeSet<u16>,
    before: &BTreeSet<u64>,
) -> Vec<String> {
    let deadline = Instant::now() + CONNECTION_CLOSE;
    let mut closed_for_a_turn = false;
    loop {
        let open: Vec<String> = tcp_connections(listeners)
            .into_iter()
            .filter(|(inode, _)| !before.contains(inode))
            .map(|(_, endpoints)| endpoints)
            .collect();
        if open.is_empty() {
            if closed_for_a_turn {
                return open;
            }
            closed_for_a_turn = true;
        } else if Instant::now() >= deadline {
            return open;
        } else {
            closed_for_a_turn = false;
        }
        compio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The client ends of the TCP connections this process holds to one of
/// `listeners`, by socket inode, with their endpoints.
///
/// A client end is where a closing library task lives, on the runtime that
/// opened it. A socket whose local port is one of the process's listening
/// ports is the server end of a connection and is left out, as are connections
/// to servers this case did not register - including servers another case runs
/// at the same time - and connections to a server the case has already
/// dropped, whose port is not in the live set. A connection to `PostgreSQL` is
/// drained by its own count.
///
/// `/proc/net/tcp` is required: without it the wait cannot tell a client end
/// from a server end, so the case fails loudly. `/proc/net/tcp6` is skipped
/// when the kernel does not offer it, which is what a host with IPv6 disabled
/// looks like.
fn tcp_connections(listeners: &BTreeSet<u16>) -> BTreeMap<u64, String> {
    const LISTEN: &str = "0A";
    let held: BTreeSet<u64> = std::fs::read_dir("/proc/self/fd")
        .expect("list this process's descriptors")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let target = std::fs::read_link(entry.path()).ok()?;
            let target = target.to_str()?;
            target
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect();
    let mut listening = BTreeSet::new();
    let mut sockets = Vec::new();
    for (table_path, required) in [("/proc/net/tcp", true), ("/proc/net/tcp6", false)] {
        let table = match std::fs::read_to_string(table_path) {
            Ok(table) => table,
            Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("read {table_path}: {error}"),
        };
        for row in table.lines().skip(1) {
            let fields: Vec<&str> = row.split_whitespace().collect();
            let (Some(local), Some(remote), Some(state), Some(inode)) =
                (fields.get(1), fields.get(2), fields.get(3), fields.get(9))
            else {
                continue;
            };
            let Ok(inode) = inode.parse::<u64>() else {
                continue;
            };
            if !held.contains(&inode) {
                continue;
            }
            let (Some(local_port), Some(remote_port)) = (port_of(local), port_of(remote)) else {
                continue;
            };
            if *state == LISTEN {
                listening.insert(local_port);
            } else {
                sockets.push((
                    inode,
                    (*local).to_owned(),
                    (*remote).to_owned(),
                    local_port,
                    remote_port,
                ));
            }
        }
    }
    sockets
        .into_iter()
        .filter(|(_, _, _, local_port, remote_port)| {
            !listening.contains(local_port)
                && listeners.contains(remote_port)
                && listening.contains(remote_port)
        })
        .map(|(inode, local, remote, _, _)| (inode, format!("{local} -> {remote}")))
        .collect()
}

/// The port of a `/proc/net/tcp[6]` endpoint, `address:port` with each half in
/// hex. The address is compared by port, so a wildcard bind covers the
/// loopback clients that reach it.
fn port_of(endpoint: &str) -> Option<u16> {
    let (_, port) = endpoint.rsplit_once(':')?;
    u16::from_str_radix(port, 16).ok()
}

/// Cancel every owned task and wait until each is dropped, including tasks an
/// owned task starts while the earlier ones are being cancelled. Each wait is
/// bounded; a task that will not stop fails the case naming the case and the
/// task. Returns the first panic an owned task had already ended with.
async fn cancel_owned() -> Option<Box<dyn Any + Send>> {
    let case = std::thread::current()
        .name()
        .unwrap_or("live case")
        .to_owned();
    let mut first_panic = None;
    loop {
        let batch = OWNED.with(|owned| {
            std::mem::take(
                owned
                    .borrow_mut()
                    .as_mut()
                    .expect("the case's task list is installed until it is cancelled"),
            )
        });
        if batch.is_empty() {
            return first_panic;
        }
        for owned in batch {
            match compio::time::timeout(CONNECTION_CLOSE, owned.task.cancel()).await {
                Ok(Some(Err(panic))) => {
                    first_panic.get_or_insert(panic);
                }
                Ok(_) => {}
                Err(elapsed) => panic!(
                    "live case {case:?}: task #{} did not stop within {CONNECTION_CLOSE:?} of \
                     its body returning ({elapsed}); it is not reaching a cancellation point (a \
                     blocking call or a synchronous spin)",
                    owned.id
                ),
            }
        }
    }
}

/// One `io_uring` ring, by the inode of its descriptor. Every ring gets its own
/// inode, so the identity survives its descriptor number being reused.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Ring {
    device: u64,
    inode: u64,
}

impl Ring {
    fn of_descriptor(path: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn of_current_runtime() -> Self {
        use std::os::fd::AsRawFd;
        let descriptor = compio::runtime::Runtime::with_current(AsRawFd::as_raw_fd);
        Self::of_descriptor(std::path::Path::new(&format!("/proc/self/fd/{descriptor}")))
            .expect("the running case's ring has a descriptor")
    }

    fn is_open(self) -> bool {
        std::fs::read_dir("/proc/self/fd")
            .expect("list this process's descriptors")
            .filter_map(Result::ok)
            .any(|entry| Self::of_descriptor(&entry.path()) == Some(self))
    }
}

/// Fail the case if the runtime it ran on, now dropped, kept its ring.
fn assert_ring_released() {
    let ring = RING
        .with(Cell::take)
        .expect("a live case recorded the ring it ran on");
    assert!(
        !ring.is_open(),
        "this case's runtime kept its io_uring ring after the case ended: a task the case \
         did not start through `live::spawn` is still parked on I/O in it - an HTTP \
         connection a client pool kept, or a driver nothing closed. The ring counts \
         against the locked-memory limit for the rest of the binary."
    );
}

/// Middleware that ends every HTTP connection after its response.
///
/// Production code keeps some HTTP clients per thread, the JWKS client among
/// them, and their pools hold an idle connection parked on a read until the
/// server closes it. A server a live case points such a client at wraps its
/// app in this, so no pooled connection outlives the case's runtime.
#[derive(Clone, Copy, Debug)]
pub struct CloseConnections;

impl<S> ntex::service::Middleware<S, ntex::service::cfg::SharedCfg> for CloseConnections {
    type Service = CloseConnectionsService<S>;

    fn create(&self, service: S, _: ntex::service::cfg::SharedCfg) -> Self::Service {
        CloseConnectionsService { service }
    }
}

#[derive(Debug)]
pub struct CloseConnectionsService<S> {
    service: S,
}

impl<S, E> ntex::service::Service<ntex::web::WebRequest<E>> for CloseConnectionsService<S>
where
    S: ntex::service::Service<ntex::web::WebRequest<E>, Response = ntex::web::WebResponse>,
{
    type Response = ntex::web::WebResponse;
    type Error = S::Error;

    ntex::forward_poll!(service);
    ntex::forward_ready!(service);
    ntex::forward_shutdown!(service);

    async fn call(
        &self,
        request: ntex::web::WebRequest<E>,
        context: ntex::service::ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let mut response = context.call(&self.service, request).await?;
        response
            .response_mut()
            .head_mut()
            .set_connection_type(ntex::http::ConnectionType::Close);
        Ok(response)
    }
}

/// Run the body and the ring check, then re-raise whichever panic the body and
/// the check produced. The body's panic wins when both fire.
fn resume_after_ring_check<T>(
    outcome: std::thread::Result<T>,
    ring: std::thread::Result<()>,
) -> T {
    match outcome {
        Ok(value) => match ring {
            Ok(()) => value,
            Err(panic) => resume_unwind(panic),
        },
        Err(panic) => resume_unwind(panic),
    }
}

/// The plain compio runtime of a live case.
pub mod runtime {
    use std::future::Future;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// What `#[compio::test(crate = "crate::support::live")]` builds.
    pub struct Runtime(compio::runtime::Runtime);

    impl Runtime {
        /// # Errors
        /// When compio cannot create its runtime.
        pub fn new() -> std::io::Result<Self> {
            compio::runtime::Runtime::new().map(Self)
        }

        pub fn block_on<F: Future>(self, body: F) -> F::Output {
            let outcome =
                catch_unwind(AssertUnwindSafe(|| self.0.block_on(super::case(body))));
            drop(self.0);
            let ring = catch_unwind(super::assert_ring_released);
            super::resume_after_ring_check(outcome, ring)
        }
    }
}

/// A live case run inside an ntex system, for a body that needs one.
pub mod system {
    /// The runtime `#[compio::test(crate = "crate::support::live::system")]`
    /// builds.
    pub mod runtime {
        use std::future::Future;
        use std::panic::{catch_unwind, AssertUnwindSafe};

        /// Builds the ntex system `#[ntex::test]` would, around a live case.
        pub struct Runtime;

        impl Runtime {
            /// Infallible: the system is built by [`Self::block_on`]. The
            /// `Result` is the shape compio's test attribute calls.
            ///
            /// # Errors
            /// Never.
            pub fn new() -> std::io::Result<Self> {
                Ok(Self)
            }

            pub fn block_on<F>(self, body: F) -> F::Output
            where
                F: Future + 'static,
                F::Output: 'static,
            {
                ntex::util::enable_test_logging();
                let name = std::thread::current()
                    .name()
                    .unwrap_or("live-case")
                    .to_owned();
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    ntex::rt::System::build()
                        .name(name)
                        .testing()
                        .build(ntex::rt::DefaultRuntime)
                        .block_on(super::super::case(body))
                }));
                let ring = catch_unwind(super::super::assert_ring_released);
                super::super::resume_after_ring_check(outcome, ring)
            }
        }
    }
}
