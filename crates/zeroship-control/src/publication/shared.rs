//! Control's catalog, shared by every request and the lifecycle publisher.
//!
//! An ORM database and its connection belong to the compio thread that opened
//! them, and that thread admits one top-level transaction per binding at a
//! time, so catalog concurrency counts threads rather than connections. The
//! catalog therefore owns [`CatalogOptions::max_connections`] threads, each
//! with a database holding one session, and runs one operation on each. The
//! process holds exactly that many catalog sessions however many workers,
//! requests and background tasks use it, runs that many operations at once,
//! and makes the rest wait for a free thread. Each operation keeps the
//! transaction and lock order its own code defines, and dropping the caller's
//! future cancels the operation exactly as it would have on the caller's own
//! thread.

use super::catalog::{self, CatalogError};
use futures::{
    channel::{mpsc, oneshot},
    future::{self, Either, LocalBoxFuture, Shared},
    FutureExt, StreamExt,
};
use std::{
    future::Future,
    num::NonZeroUsize,
    pin::pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use zeroship_data_orm::orm::Database;

/// The catalog session bound Control uses unless its configuration names one.
pub const DEFAULT_MAX_CONNECTIONS: NonZeroUsize = match NonZeroUsize::new(4) {
    Some(bound) => bound,
    None => unreachable!(),
};

/// How long a closing catalog thread waits for its session to close.
const CLOSE_DRAIN: Duration = Duration::from_secs(5);

/// Bounds of the shared catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogOptions {
    /// Catalog sessions the process may hold at once. Each is one thread with
    /// one database, so this is also the catalog operations that run at once.
    pub max_connections: NonZeroUsize,
}

impl Default for CatalogOptions {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

/// Resolves once the catalog has stopped accepting operations. A background
/// task started by [`Catalog::spawn`] returns when it does, so its thread can
/// close its session while it still has a runtime to close it with.
#[derive(Clone)]
pub struct Closing(Shared<oneshot::Receiver<()>>);

impl std::fmt::Debug for Closing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Closing").finish_non_exhaustive()
    }
}

impl Closing {
    /// Wait until the catalog closes.
    pub async fn wait(self) {
        // The sender is never used: its drop is the signal.
        let _ = self.0.await;
    }
}

type Job = Box<dyn FnOnce(Database, Closing) -> LocalBoxFuture<'static, ()> + Send>;
type Start =
    Box<dyn FnOnce(Database, Closing) -> Result<LocalBoxFuture<'static, ()>, CatalogError> + Send>;

/// What a catalog thread is asked to do.
enum Work {
    /// One operation, which the thread runs alone.
    Run(Job),
    /// A background task to start on the thread and leave running beside its
    /// operations, reporting whether it started.
    Serve(Start, oneshot::Sender<Result<(), CatalogError>>),
}

/// One catalog thread and the operations dispatched to it but not yet done.
struct Worker {
    work: mpsc::UnboundedSender<Work>,
    queued: Arc<AtomicUsize>,
}

/// A handle to Control's shared catalog. Clones share its threads; the catalog
/// closes when the last handle is dropped.
#[derive(Clone)]
pub struct Catalog {
    workers: Arc<[Worker]>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("threads", &self.workers.len())
            .finish_non_exhaustive()
    }
}

impl Catalog {
    /// Open one catalog database per configured session, each on its own
    /// thread, and return once every one of them is connected.
    ///
    /// # Errors
    /// Reports a thread that cannot start and a database that cannot open.
    pub async fn start(url: &str, options: CatalogOptions) -> Result<Self, CatalogError> {
        let mut workers = Vec::with_capacity(options.max_connections.get());
        let mut opening = Vec::with_capacity(options.max_connections.get());
        for index in 0..options.max_connections.get() {
            let (work, queue) = mpsc::unbounded();
            let (opened, ready) = oneshot::channel();
            let queued = Arc::new(AtomicUsize::new(0));
            let (url, counted) = (url.to_owned(), Arc::clone(&queued));
            std::thread::Builder::new()
                .name(format!("control-catalog-{index}"))
                .spawn(move || host(&url, queue, &counted, opened))
                .map_err(|_| CatalogError::Storage("a control catalog thread could not start"))?;
            workers.push(Worker { work, queued });
            opening.push(ready);
        }
        // Dropping `workers` on the way out of this loop closes the queues of
        // the threads that did open, so none is left behind.
        for ready in opening {
            ready.await.map_err(|_| {
                CatalogError::Storage("a control catalog thread stopped while opening")
            })??;
        }
        Ok(Self {
            workers: workers.into(),
        })
    }

    /// Run `operation` on the least loaded catalog thread and return its
    /// result. The operation waits for that thread's current one to finish.
    ///
    /// Dropping the returned future cancels the operation. A transaction it
    /// had begun rolls back unless its commit was already on the wire, which
    /// is the same uncertainty a dropped caller-thread transaction leaves.
    ///
    /// # Errors
    /// Returns the operation's own error, or a storage error when the catalog
    /// has closed or the operation panicked.
    pub async fn run<T, F, Fut>(&self, operation: F) -> Result<T, CatalogError>
    where
        T: Send + 'static,
        F: FnOnce(Database) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, CatalogError>> + 'static,
    {
        let (reply, answer) = oneshot::channel();
        self.dispatch(Work::Run(Box::new(move |database, _closing| {
            Box::pin(async move {
                let mut reply = reply;
                let operation = pin!(operation(database));
                match future::select(operation, reply.cancellation()).await {
                    Either::Left((result, cancellation)) => {
                        drop(cancellation);
                        let _ = reply.send(result);
                    }
                    // The caller is gone. Dropping the operation here cancels
                    // it where it runs.
                    Either::Right(((), _)) => {}
                }
            })
        })))?;
        answer
            .await
            .map_err(|_| CatalogError::Storage("the control catalog abandoned the operation"))?
    }

    /// Start a background task on the catalog's first thread.
    ///
    /// `start` runs there with that thread's database and the catalog's
    /// closing signal, and returns the task or a refusal, which the caller
    /// receives. The task runs beside that thread's operations and shares its
    /// one session with them, so it must return once [`Closing`] resolves.
    ///
    /// # Errors
    /// Returns the refusal of `start`, or a storage error when the catalog has
    /// closed.
    pub async fn spawn<F>(&self, start: F) -> Result<(), CatalogError>
    where
        F: FnOnce(Database, Closing) -> Result<LocalBoxFuture<'static, ()>, CatalogError>
            + Send
            + 'static,
    {
        let (reply, answer) = oneshot::channel();
        self.send(&self.workers[0], Work::Serve(Box::new(start), reply))?;
        answer
            .await
            .map_err(|_| CatalogError::Storage("the control catalog did not start the task"))?
    }

    /// Hand one operation to the thread with the fewest outstanding ones.
    fn dispatch(&self, work: Work) -> Result<(), CatalogError> {
        let worker = self
            .workers
            .iter()
            .min_by_key(|worker| worker.queued.load(Ordering::Relaxed))
            .ok_or(CatalogError::Storage("the control catalog has no thread"))?;
        worker.queued.fetch_add(1, Ordering::Relaxed);
        self.send(worker, work).inspect_err(|_| {
            worker.queued.fetch_sub(1, Ordering::Relaxed);
        })
    }

    fn send(&self, worker: &Worker, work: Work) -> Result<(), CatalogError> {
        worker
            .work
            .unbounded_send(work)
            .map_err(|_| CatalogError::Storage("the control catalog has closed"))
    }
}

/// One catalog thread: open its database, serve until every handle is gone,
/// then close its session while this thread's runtime can still drive it.
fn host(
    url: &str,
    queue: mpsc::UnboundedReceiver<Work>,
    queued: &AtomicUsize,
    opened: oneshot::Sender<Result<(), CatalogError>>,
) {
    let Ok(runtime) = compio::runtime::Runtime::new() else {
        let _ = opened.send(Err(CatalogError::Storage(
            "a control catalog runtime could not start",
        )));
        return;
    };
    runtime.block_on(async move {
        let database = match catalog::connect(url).await {
            Ok(database) => database,
            Err(error) => {
                let _ = opened.send(Err(error.into()));
                return;
            }
        };
        if opened.send(Ok(())).is_err() {
            return;
        }
        serve(database, queue, queued).await;
        if !compio_postgres::drain_connections(CLOSE_DRAIN).await {
            tracing::warn!("a control catalog thread closed with its session still open");
        }
    });
}

/// Run one operation at a time until the queue closes, leaving any background
/// task running beside them, then signal closing and wait for those tasks.
async fn serve(
    database: Database,
    mut queue: mpsc::UnboundedReceiver<Work>,
    queued: &AtomicUsize,
) {
    let (close, closed) = oneshot::channel::<()>();
    let closing = Closing(closed.shared());
    let mut background = Vec::new();
    while let Some(work) = queue.next().await {
        match work {
            Work::Run(job) => {
                report(compio::runtime::spawn(job(database.clone(), closing.clone())).await);
                queued.fetch_sub(1, Ordering::Relaxed);
            }
            Work::Serve(start, reply) => match start(database.clone(), closing.clone()) {
                Ok(task) => {
                    let _ = reply.send(Ok(()));
                    background.push(compio::runtime::spawn(task));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
        }
    }
    drop(close);
    drop(database);
    for task in background {
        report(task.await);
    }
}

fn report(finished: Result<(), Box<dyn std::any::Any + Send>>) {
    if finished.is_err() {
        tracing::error!("a control catalog operation panicked");
    }
}
