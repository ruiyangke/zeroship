//! SQLite actor with separate autocommit and per-app transaction connections.
//!
//! A bounded command queue feeds an actor thread that owns all connections.
//! Transaction lanes retain reservations across commands; autocommit reservations
//! settle with their commands. Idle lanes can be evicted to respect `MAX_TX_LANES`.
//! Separate connections prevent unrelated work from joining a creator transaction,
//! but SQLite writes still contend for the database's writer lock.
//!
//! Journal settings apply per database, including attached app files. Runtime
//! connection setup and migration requirements must agree on those settings.
//!
//! Cancellation uses an explicit reservation protocol; dropping a reply receiver
//! alone does not cancel its command. The reservation's terminal handshake decides
//! whether completion or cancellation wins.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, Weak};
use crate::value::Value;

use rusqlite::Connection;

use crate::backend::sqlite::cdc::CommitSender;
use crate::backend::sqlite::error::from_sqlite;
use crate::backend::sqlite::reservation::{
    self, CancelCleanup, CancelIntent, Lane, Reservation, ReservationKind, TerminalOutcome,
    TxLaneId,
};
use zeroship_data_orm::error::DbError;

/// Wire code for "this session cannot open another app's transaction
/// connection right now".
///
/// A code of its own, not another `transaction_connection_busy`, because the
/// remedies are opposite and only one of them is the creator's to act on:
/// `transaction_connection_busy` means *this* app already has a transaction
/// open and the creator must finish it, while this one means the dev process is
/// hosting more apps than it has transaction connections for and the creator's
/// own code is blameless. Defect L22b is exactly what happens when one code
/// covers both.
pub(crate) const TX_LANES_EXHAUSTED: &str = "transaction_lanes_exhausted";

/// One-shot `sqlite-vec` auto-extension registration.
///
/// `sqlite_vec::sqlite3_vec_init` is the C extension's initialiser;
/// `rusqlite::ffi::sqlite3_auto_extension` registers a callback that fires for
/// every subsequent `sqlite3_open*` call in the process. The hook IS
/// process-global by design (it lives inside the linked SQLite amalgamation's
/// auto-extension table), so we register exactly ONCE at the first
/// `SqliteSession::open` and rely on every subsequent connection - including
/// this session's second one, and any connection recycled after a quarantine -
/// inheriting the extension automatically.
///
/// **Safety**: `sqlite3_auto_extension` is FFI-unsafe - the C signature is
/// `int (*)(sqlite3*, char**, const sqlite3_api_routines*)` and we cast
/// `sqlite3_vec_init` (whose signature matches the C contract per the upstream
/// `sqlite-vec` crate) through `std::mem::transmute`. The cast is documented in
/// the upstream `sqlite-vec` crate's own `examples/simple-rust/demo.rs` and is
/// the canonical integration pattern. (That path is in the sqlite-vec
/// repository, not this one.)
static VEC_INIT: Once = Once::new();

#[allow(unsafe_code)]
fn register_sqlite_vec_once() {
    VEC_INIT.call_once(|| {
        // SAFETY: `sqlite3_vec_init` matches the auto-extension callback
        // signature expected by the linked SQLite amalgamation (see
        // the upstream `sqlite-vec` crate's `examples/simple-rust/demo.rs`
        // - that file is the canonical integration recipe). The
        // registration is process-global and fires for every connection
        // opened thereafter; see the module-level rustdoc on `VEC_INIT`.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// One row of a `Query` result. Each cell is captured as a UTF-8
/// string so the result is `Send` (rusqlite's native value types
/// borrow from the statement; we materialise to owned `Option<String>`
/// here so the reply can cross the actor / future boundary).
#[allow(dead_code)]
pub(crate) type Row = Vec<Option<String>>;

/// A SQLite cell that preserves its storage class across the actor boundary.
#[derive(Debug, Clone)]
pub enum TypedCell {
    /// SQLite `NULL`.
    Null,
    /// `INTEGER` storage class. `i64` covers the full 8-byte SQLite
    /// integer range.
    Integer(i64),
    /// `REAL` storage class. SQLite reals are 8-byte IEEE-754.
    Real(f64),
    /// `TEXT` storage class. Decoded to UTF-8; non-UTF-8 TEXT surfaces
    /// as a `DbError::Internal`.
    Text(String),
    /// `BLOB` storage class. Owned bytes - copied out of the
    /// rusqlite-managed buffer at decode time so the reply is `Send`.
    Blob(Vec<u8>),
}

/// Column names and typed rows returned by the public session query contract.
#[derive(Debug, Clone)]
pub struct TypedRows {
    /// Column names in result-set order. Length matches every row's
    /// cell count.
    pub columns: Vec<String>,
    /// One [`TypedCell`] per cell, row-major.
    pub rows: Vec<Vec<TypedCell>>,
}

/// What a terminal command asks the actor to do with its reservation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TerminalIntent {
    Commit,
    Rollback,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Commands queued onto the `SqliteSession` actor.
///
/// Every data command names its reservation, and the actor refuses one whose
/// reservation does not own the connection it would run on. That mismatch is
/// the class of bug the pre-SC-2 shape could not even express: commands then
/// carried SQL and a reply channel and nothing else.
pub(crate) enum Command {
    /// Bind this app's transaction connection to a transaction reservation,
    /// opening the connection if the app does not have one yet. Any transaction
    /// the previous binding left open is rolled back first, so a lease that was
    /// dropped without settling cannot leak its transaction into the next one.
    ///
    /// The `app_id` rides along because the actor - not the caller - owns
    /// connections, so the actor is what has to know which file to bind. The
    /// reply carries the open failure: without it a lane that could not be
    /// opened would be discovered by the next command's ownership check, under
    /// the wrong error.
    Reserve {
        reservation: Arc<Reservation>,
        app_id: String,
        reply: flume::Sender<Result<(), DbError>>,
    },
    /// Retire a transaction reservation, rolling back anything it left open.
    /// Sent best-effort from the lease's `Drop`; the `Reserve` above repeats
    /// the same cleanup, so a lost `Release` cannot strand the lane.
    Release { reservation: Arc<Reservation> },
    /// Run a non-row-returning statement; reply with the SQLite
    /// `Connection::changes()` value (cast to `u64`).
    Exec {
        reservation: Arc<Reservation>,
        sql: String,
        params: Vec<Value>,
        reply: flume::Sender<Result<u64, DbError>>,
    },
    /// Run a row-returning statement; reply with the materialised rows.
    Query {
        reservation: Arc<Reservation>,
        sql: String,
        params: Vec<Value>,
        reply: flume::Sender<Result<Vec<Row>, DbError>>,
    },
    /// Run a row-returning statement; reply with typed rows + column names.
    QueryTyped {
        reservation: Arc<Reservation>,
        sql: String,
        params: Vec<Value>,
        reply: flume::Sender<Result<TypedRows, DbError>>,
    },
    /// Run this transaction reservation's terminal statement and classify the
    /// outcome by sampling `is_autocommit` - never by the result code.
    Settle {
        reservation: Arc<Reservation>,
        intent: TerminalIntent,
        reply: flume::Sender<Result<TerminalOutcome, DbError>>,
    },
    /// Resolve a cancellation. The caller has already set the intent and (if
    /// the actor was running) interrupted the target connection; this command
    /// is how the actor acknowledges, after it has rolled back and retired.
    Cancel {
        reservation: Arc<Reservation>,
        reply: flume::Sender<Result<TerminalOutcome, DbError>>,
    },
    /// `ATTACH DATABASE 'file:{db_path}' AS '<app_id>'` on **both**
    /// connections, and record it so a recycled connection can be rebuilt.
    Attach {
        app_id: String,
        db_path: String,
        reply: flume::Sender<Result<(), DbError>>,
    },
    /// `VACUUM INTO '<dest_path>'` on `op_conn`. Captures the source database
    /// (or a per-app ATTACH alias if `app_id` is `Some`) to a fresh SQLite
    /// file. SQLite takes an implicit shared-snapshot read transaction on the
    /// source: writers keep appending to the WAL during the copy and the dest
    /// matches the snapshot's read-mark commit point.
    ///
    /// The destination path is interpolated as a single-quoted SQL literal
    /// (doubled `'`s); VACUUM INTO does not accept bound parameters for it.
    VacuumInto {
        app_id: Option<String>,
        dest_path: String,
        reply: flume::Sender<Result<(), DbError>>,
    },
    /// Atomic file-swap restore: DETACH the per-app alias on both connections,
    /// `std::fs::rename(temp_file, live_file)`, ATTACH the alias back on both.
    ///
    /// POSIX `rename` is atomic only on the same filesystem; operator-driven
    /// snapshot destinations must live on the same FS as the live per-app file.
    ReattachFile {
        app_id: String,
        temp_path: String,
        live_path: String,
        reply: flume::Sender<Result<(), DbError>>,
    },
    /// Drain the queue and exit the worker thread. Sent best-effort from
    /// `SqliteSession`'s `Drop` impl.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Interrupt registry - the caller-side half of Decision 2
// ---------------------------------------------------------------------------

/// One lane's interrupt handle plus the generation of the connection it
/// targets.
///
/// The generation is what stops a cancellation from landing on the wrong
/// statement: a quarantined connection is closed and reopened, its generation
/// bumped, and an interrupt still aimed at the old generation is refused rather
/// than delivered to the replacement's unrelated work.
struct LaneInterrupt {
    generation: AtomicU64,
    handle: Mutex<rusqlite::InterruptHandle>,
}

/// The interrupt handles for a session's connections, shared between the actor
/// thread (which creates and replaces them) and every caller holding a cancel
/// handle.
///
/// `op` is a field and the transaction lanes are a map because their lifetimes
/// differ in kind: `op_conn` exists for the session's whole life, while an
/// app's transaction connection is opened on that app's first
/// `db.transaction()` and can be evicted again.
pub(crate) struct Interrupts {
    op: LaneInterrupt,
    tx: Mutex<HashMap<TxLaneId, LaneInterrupt>>,
}

impl std::fmt::Debug for Interrupts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interrupts").finish()
    }
}

impl Interrupts {
    fn new(op: rusqlite::InterruptHandle) -> Self {
        Self {
            op: LaneInterrupt {
                generation: AtomicU64::new(0),
                handle: Mutex::new(op),
            },
            tx: Mutex::new(HashMap::new()),
        }
    }

    fn tx_map(&self) -> std::sync::MutexGuard<'_, HashMap<TxLaneId, LaneInterrupt>> {
        self.tx
            .lock()
            .expect("sqlite tx interrupt registry mutex poisoned")
    }

    /// Interrupt `lane`'s connection, but only if it is still the generation
    /// the caller aimed at. Returns whether the interrupt was delivered.
    ///
    /// A transaction lane that no longer exists (evicted, or never opened)
    /// reports `false` for the same reason a stale generation does: there is no
    /// connection this cancellation has any right to touch.
    ///
    /// THIS PARAGRAPH USED TO SAY THE OPPOSITE, AND CARRIED AN
    /// `#[allow(dead_code)]` TO MATCH: "nothing in production cancels a SQLite
    /// command yet... until SC-1 step 9 lands the only callers are this crate's
    /// tests". SC-1 landed. This is reachable from production in three hops -
    /// `interrupt` <- `SqliteCancelHandle::signal` <- `SqliteCancelHandle::cancel`
    /// <- `backend::cancel::CancellationHandle::cancel` in `zeroship-data-orm`,
    /// whose SQLite arm calls `handle.cancel().await`. Two of those hops are
    /// private and the last is in ANOTHER CRATE, which is why the chain reads as
    /// broken from here and why the prose outlived the fact.
    ///
    /// A rustdoc paragraph asserting that an item is dead is the most misleading
    /// artifact in this crate: it reads as an audit that already happened, and
    /// it is what nearly got this function deleted on 2026-09-04.
    fn interrupt(&self, lane: Lane, generation: u64) -> bool {
        fn fire(entry: &LaneInterrupt, generation: u64) -> bool {
            if entry.generation.load(Ordering::SeqCst) != generation {
                return false;
            }
            entry
                .handle
                .lock()
                .expect("sqlite interrupt handle mutex poisoned")
                .interrupt();
            true
        }
        match lane {
            Lane::Op => fire(&self.op, generation),
            Lane::Tx(id) => match self.tx_map().get(&id) {
                Some(entry) => fire(entry, generation),
                None => false,
            },
        }
    }

    /// Publish a connection's handle under `generation`, creating the lane
    /// entry when it is new.
    fn replace(&self, lane: Lane, generation: u64, handle: rusqlite::InterruptHandle) {
        fn store(entry: &LaneInterrupt, generation: u64, handle: rusqlite::InterruptHandle) {
            *entry
                .handle
                .lock()
                .expect("sqlite interrupt handle mutex poisoned") = handle;
            entry.generation.store(generation, Ordering::SeqCst);
        }
        match lane {
            Lane::Op => store(&self.op, generation, handle),
            Lane::Tx(id) => {
                let mut map = self.tx_map();
                match map.get(&id) {
                    Some(entry) => store(entry, generation, handle),
                    None => {
                        map.insert(
                            id,
                            LaneInterrupt {
                                generation: AtomicU64::new(generation),
                                handle: Mutex::new(handle),
                            },
                        );
                    }
                }
            }
        }
    }

    /// Retire a transaction lane's connection: bump its generation so an
    /// interrupt aimed at the closed connection is refused, and return the
    /// generation a replacement must be published under.
    ///
    /// The entry is kept rather than removed. Removing it would make
    /// [`Self::generation`] read `0` again for a reused lane id, which is the
    /// generation a *previous* incarnation's reservation carries - the one
    /// thing the generation exists to tell apart.
    fn retire(&self, id: TxLaneId) -> u64 {
        match self.tx_map().get(&id) {
            Some(entry) => entry.generation.fetch_add(1, Ordering::SeqCst) + 1,
            None => 0,
        }
    }

    /// The generation currently published for `lane`.
    ///
    /// A transaction lane the actor has not opened yet reads `0`. That is a
    /// starting guess, not the authority: the actor stamps the real generation
    /// onto the reservation when it binds it (see
    /// [`super::reservation::Reservation::adopt_generation`]), because only the
    /// actor knows whether the connection it opened is the first incarnation of
    /// that lane or a replacement.
    fn generation(&self, lane: Lane) -> u64 {
        match lane {
            Lane::Op => self.op.generation.load(Ordering::SeqCst),
            Lane::Tx(id) => self
                .tx_map()
                .get(&id)
                .map_or(0, |entry| entry.generation.load(Ordering::SeqCst)),
        }
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Two-connection SQLite actor.
///
/// Owns the `flume::Sender<Command>` head of the actor's command queue; the
/// matching receiver lives on the spawned worker thread. Dropping the session
/// sends `Shutdown` best-effort and the worker joins on the next iteration.
///
/// **Why `std::thread::spawn` and not `compio::runtime::spawn_blocking`**:
/// compio's `spawn_blocking` submits the closure as an `Asyncify` op whose
/// completion the runtime poller observes. The command loop is long-lived, so
/// it would hold the io_uring slot for the whole process - and on shutdown the
/// runtime might already be torn down before the worker observes `Shutdown`. A
/// plain OS thread sidesteps both: the worker runs independently of compio, the
/// caller-side `flume::Sender::send_async` future awaits purely on flume's
/// atomic-park primitives, and clean shutdown is fully signalled by `Shutdown`.
pub(crate) struct SqliteSession {
    tx: flume::Sender<Command>,
    interrupts: Arc<Interrupts>,
    /// Monotonic reservation ids. Shared with nothing else; ids are opaque and
    /// only ever compared for equality.
    next_reservation: Cell<u64>,
    /// Each app's transaction lane id and that lane's current owner, held
    /// weakly. **Keyed by `app_id`, and that is the whole of defect L22b's
    /// fix**: admission is per app, matching SC-1's
    /// `(runtime_instance_id, app_id)`, where one shared entry made it
    /// `(runtime_instance_id, session)`.
    ///
    /// This is the **admission** authority, and it is caller-side deliberately:
    /// a `Weak` that no longer upgrades is proof the previous lease was
    /// dropped, which is a fact only the caller side can observe. The actor's
    /// own binding merely follows, and repeats the rollback cleanup on every
    /// `Reserve`, so a `Release` lost to a full queue cannot strand the lane.
    ///
    /// It weakly holds the **lease**, not the reservation. The distinction is
    /// load-bearing: every queued command carries an `Arc<Reservation>` clone,
    /// so a reservation's strong count stays above zero for as long as the
    /// actor is still holding the command it is replying to - which is
    /// precisely the instant after a caller's `await` returns. Keying
    /// admission on that count made a released lease look live, and a loop
    /// that takes a lease per iteration would fail intermittently. `TxLease`
    /// is never cloned into a command, so its count is exactly lease liveness.
    tx_lanes: RefCell<HashMap<String, TxLaneSlot>>,
    /// Monotonic transaction-lane ids. An app keeps its id for the session's
    /// life even if the actor evicts the connection behind it.
    next_tx_lane: Cell<u32>,
    /// One-shot test gate, shared with **this session's actor and no other**.
    ///
    /// It is per-session deliberately. As a process-global it was taken by
    /// whichever actor happened to receive the next command, so a gate armed
    /// by one test stalled a concurrently running test's actor and satisfied
    /// the arming test's `wait_until_blocked` with a foreign command.
    #[cfg(test)]
    next_command_gate: NextCommandGateSlot,
    /// Worker `JoinHandle`. Held only so the OS thread is tracked (we never
    /// join it from the session side - cancellation is signalled via the
    /// `Shutdown` command + sender drop).
    _worker: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for SqliteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSession").finish()
    }
}

/// One app's slot in [`SqliteSession::tx_lanes`].
struct TxLaneSlot {
    id: TxLaneId,
    owner: Option<Weak<TxLeaseAlive>>,
}

impl TxLaneSlot {
    fn is_live(&self) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|previous| previous.strong_count() > 0)
    }
}

/// How many app transaction connections one session may hold open at once.
///
/// A bound, not a tuning knob: without one, every app that ever opened a
/// transaction keeps a connection (and its file descriptors) for the session's
/// life, which is unbounded per-tenant resource growth on a shared process. An
/// idle lane is evicted to make room; only when every lane is mid-transaction
/// does a new app get [`TX_LANES_EXHAUSTED`] - a refusal with a code of its own
/// so it is never confused with an app's own overlapping transaction.
///
/// Eight is chosen against the vector this backend actually runs in: SQLite is
/// the dev tier, `crates/zeroship-worker/src/main.rs:328-334` refuses to boot
/// on a SQLite DSN, and `crates/zeroship-runtime/src/core/serve.rs:173-179`
/// clamps `zeroship serve` to one worker thread on one, so the live case is a
/// single app and eight is seven spare.
const MAX_TX_LANES: usize = 8;

/// The cap, for the arm that rules on it.
///
/// Exported rather than repeated as a literal in the test: a test that spells
/// its own `8` keeps passing when the cap moves, over a fixture that no longer
/// reaches the boundary it claims to.
#[cfg(test)]
pub const MAX_TX_LANES_FOR_TESTS: usize = MAX_TX_LANES;

/// Startup handshake payload: the worker publishes `op_conn`'s interrupt
/// handle once the PRAGMA bootstrap and CDC install have succeeded. Transaction
/// lanes register theirs later, as they are opened.
type StartupSignal = Result<Arc<Interrupts>, DbError>;

impl SqliteSession {
    /// Open a SQLite database at `db_path`, open its autocommit connection and
    /// spawn the actor.
    ///
    /// Transaction connections open lazily when an app begins a transaction.
    ///
    /// The initial connection completes its PRAGMA bootstrap before this
    /// function returns. Transaction connections do the same before use.
    ///
    /// When a CDC sender is supplied, every write connection publishes its
    /// committed changes. The attached database alias supplies the app identity.
    pub(crate) fn open(
        db_path: &Path,
        packet_tx: Option<CommitSender>,
    ) -> Result<Self, DbError> {
        super::validate_database_path(db_path)?;
        // The bounded queue applies backpressure before the serialized actor.
        let (tx, rx) = flume::bounded::<Command>(64);
        let (startup_tx, startup_rx) = flume::bounded::<StartupSignal>(1);

        let db_path = db_path.to_path_buf();
        let packet_tx_owned = packet_tx;

        #[cfg(test)]
        let next_command_gate: NextCommandGateSlot = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let gate_for_worker = Arc::clone(&next_command_gate);

        let worker = std::thread::Builder::new()
            .name("sqlite-session".to_string())
            .spawn(move || {
                let mut actor = match Actor::open(db_path, packet_tx_owned) {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = startup_tx.send(Err(e));
                        return;
                    }
                };
                #[cfg(test)]
                {
                    actor.next_command_gate = gate_for_worker;
                }
                let _ = startup_tx.send(Ok(Arc::clone(&actor.interrupts)));
                actor.run(&rx);
            })
            .map_err(|e| {
                DbError::internal(format!(
                    "SqliteSession::open: failed to spawn worker thread: {e}"
                ))
            })?;

        // Wait synchronously for the startup signal. The worker runs
        // independently of any compio runtime - flume's `recv()` parks on
        // `std::thread::park`, so blocking briefly here does not stall the
        // runtime. This constructor is called once per backend at boot and the
        // worker reports within one connection's worth of PRAGMAs.
        match startup_rx.recv() {
            Ok(Ok(interrupts)) => Ok(Self {
                tx,
                interrupts,
                next_reservation: Cell::new(1),
                tx_lanes: RefCell::new(HashMap::new()),
                next_tx_lane: Cell::new(0),
                #[cfg(test)]
                next_command_gate,
                _worker: worker,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(DbError::internal(
                "SqliteSession::open: worker exited before sending startup signal",
            )),
        }
    }

    fn mint(&self, lane: Lane, kind: ReservationKind) -> Arc<Reservation> {
        let id = self.next_reservation.get();
        self.next_reservation.set(id + 1);
        Arc::new(Reservation::new(
            id,
            lane,
            kind,
            self.interrupts.generation(lane),
        ))
    }

    /// An ephemeral autocommit reservation, minted per command.
    ///
    /// SC-2: *autocommit reservations settle at command completion.* Making one
    /// per command is that rule made structural - there is no autocommit
    /// reservation that outlives the statement it was minted for.
    fn autocommit_reservation(&self) -> Arc<Reservation> {
        self.mint(Lane::Op, ReservationKind::Autocommit)
    }

    /// **Test-only**: hand an autocommit reservation to the caller so it can be
    /// submitted twice. See
    /// [`crate::backend::sqlite::SqliteBackend::spent_autocommit_reservation_for_tests`].
    #[cfg(test)]
    pub(crate) fn autocommit_reservation_for_tests(&self) -> Arc<Reservation> {
        self.autocommit_reservation()
    }

    /// Reserve `app_id`'s transaction connection for one explicit creator
    /// transaction, opening that connection if the app has none yet.
    ///
    /// Refuses with a typed error while **that app's** lease is live. "Live" is
    /// decided by whether the previous `Weak` still upgrades, so a lease
    /// dropped without settling frees the lane immediately rather than after a
    /// queue round trip.
    ///
    /// ## Policy chosen here
    ///
    /// Two questions the design did not settle. Both are answered by the code
    /// below, so they are stated here rather than discovered - the Postgres
    /// half writes its equivalents into
    /// `zeroship_data_orm::backend::postgres::PostgresBackend`'s
    /// `fixture_session`, and these are this half's.
    ///
    /// 1. **Exhaustion: refuse immediately, do not queue.** A second
    ///    `db.transaction()` *for the same app* gets
    ///    `transaction_connection_busy` at once rather than waiting for the
    ///    lane. Conservative because a refusal is decided from state the caller
    ///    can see and needs no deadline to be safe; the Postgres half queues on
    ///    the pool's `acquire_timeout` instead, so the two arms differ in what a
    ///    creator observes under contention. When SC-1's deadline lands, whether
    ///    this becomes a bounded wait is that decision's to make.
    /// 2. **Scope of the admission key: `(session, app_id)`.** SC-1 keys a
    ///    transaction slot on `(runtime_instance_id, app_id)` and this is the
    ///    same key, because a session is one runtime instance's SQLite
    ///    resources. It was `(runtime_instance_id, session)` until the
    ///    transaction connection became per app - one shared connection meant
    ///    app B's `db.transaction()` was refused while app A held one, which is
    ///    defect L22b.
    ///
    /// Both are visible to creators, so they are also written down in
    /// `docs/reference/sqlite-divergences.md`; keep the two in step.
    pub(crate) async fn reserve_transaction(
        self: &Rc<Self>,
        app_id: &str,
    ) -> Result<Rc<TxLease>, DbError> {
        // The lease is created and published BEFORE the `Reserve` is queued, so
        // that dropping this future mid-send frees the lane rather than
        // stranding it: the `Rc<TxLease>` dies with the future and the `Weak`
        // stops upgrading. A `Release` that then reaches the actor ahead of its
        // own `Reserve` is a no-op (the ids do not match) and the next
        // `Reserve`'s unconditional cleanup covers the binding anyway.
        let lease = {
            let mut lanes = self.tx_lanes.borrow_mut();
            if lanes.get(app_id).is_some_and(TxLaneSlot::is_live) {
                return Err(DbError::validation_hinted(
                    "transaction_connection_busy",
                    format!(
                        "db: app '{app_id}' already holds an open transaction on its SQLite \
                         transaction connection; one explicit transaction at a time"
                    ),
                    "Commit or roll back the open db.transaction(...) before starting another \
                     one for this app.",
                ));
            }
            // Drop slots whose lease is gone before minting a new id, so a
            // process that cycles through app ids does not accumulate them.
            if lanes.len() >= MAX_TX_LANES {
                lanes.retain(|held, slot| held == app_id || slot.is_live());
            }
            let id = match lanes.get(app_id) {
                Some(slot) => slot.id,
                None => {
                    let next = self.next_tx_lane.get();
                    self.next_tx_lane.set(next + 1);
                    TxLaneId(next)
                }
            };
            let alive = Arc::new(TxLeaseAlive);
            lanes.insert(
                app_id.to_string(),
                TxLaneSlot {
                    id,
                    owner: Some(Arc::downgrade(&alive)),
                },
            );
            Rc::new(TxLease {
                alive,
                reservation: self.mint(Lane::Tx(id), ReservationKind::Transaction),
                tx: self.tx.clone(),
            })
        };

        let (reply_tx, reply_rx) = flume::bounded::<Result<(), DbError>>(1);
        self.send(Command::Reserve {
            reservation: Arc::clone(lease.reservation()),
            app_id: app_id.to_string(),
            reply: reply_tx,
        })
        .await?;
        // The lease is dropped on the error path by falling out of scope, which
        // frees the slot: an app whose connection could not be opened must not
        // be left looking busy.
        recv_reply(reply_rx).await??;

        Ok(lease)
    }

    /// **Test-only**: a transaction-lane handle whose reservation the actor was
    /// never told about.
    ///
    /// It is the only way to construct a command that names a reservation the
    /// connection does not belong to - the exact mismatch SC-2 requires be
    /// refused with a typed error, and the class of bug the pre-SC-2 command
    /// shape could not express. It deliberately does NOT publish itself in
    /// `tx_owner`, so it cannot block a legitimate reservation.
    #[cfg(test)]
    pub(crate) fn unregistered_transaction_handle_for_tests(
        self: &Rc<Self>,
    ) -> SqliteSessionHandle {
        // `u32::MAX` is a lane id `next_tx_lane` cannot reach before the
        // process runs out of memory, so the actor has never opened it and
        // never will - which is exactly the "names a connection it does not
        // own" condition this handle exists to produce.
        let lease = Rc::new(TxLease {
            alive: Arc::new(TxLeaseAlive),
            reservation: self.mint(Lane::Tx(TxLaneId(u32::MAX)), ReservationKind::Transaction),
            tx: self.tx.clone(),
        });
        SqliteSessionHandle::with_lease(Rc::clone(self), lease)
    }

    /// Send an `Exec` command on an autocommit reservation and await the reply.
    #[cfg(test)]
    pub async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        self.exec_on(&self.autocommit_reservation(), sql, params)
            .await
    }

    pub(crate) async fn exec_on(
        &self,
        reservation: &Arc<Reservation>,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError> {
        let params: Vec<_> = params.iter().map(|value| Value::from(*value)).collect();
        self.exec_values_on(reservation, sql, params).await
    }

    async fn exec_values_on(
        &self,
        reservation: &Arc<Reservation>,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<u64, DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<u64, DbError>>(1);
        self.send(Command::Exec {
            reservation: Arc::clone(reservation),
            sql: sql.to_string(),
            params,
            reply: reply_tx,
        })
        .await?;
        recv_reply(reply_rx).await?
    }

    /// Send a `Query` command and await the materialised row slice.
    pub(crate) async fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>, DbError> {
        self.query_on(&self.autocommit_reservation(), sql, params)
            .await
    }

    pub(crate) async fn query_on(
        &self,
        reservation: &Arc<Reservation>,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Row>, DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<Vec<Row>, DbError>>(1);
        self.send(Command::Query {
            reservation: Arc::clone(reservation),
            sql: sql.to_string(),
            params: params.iter().map(|s| Value::from(*s)).collect(),
            reply: reply_tx,
        })
        .await?;
        recv_reply(reply_rx).await?
    }

    /// Send a `QueryTyped` command and await typed rows + column names.
    pub(crate) async fn query_typed(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<TypedRows, DbError> {
        self.query_typed_on(&self.autocommit_reservation(), sql, params)
            .await
    }

    pub(crate) async fn query_typed_on(
        &self,
        reservation: &Arc<Reservation>,
        sql: &str,
        params: &[Value],
    ) -> Result<TypedRows, DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<TypedRows, DbError>>(1);
        self.send(Command::QueryTyped {
            reservation: Arc::clone(reservation),
            sql: sql.to_string(),
            params: params.to_vec(),
            reply: reply_tx,
        })
        .await?;
        recv_reply(reply_rx).await?
    }

    /// Run a transaction reservation's terminal statement and return the
    /// classified outcome.
    pub async fn settle(
        &self,
        reservation: &Arc<Reservation>,
        intent: TerminalIntent,
    ) -> Result<TerminalOutcome, DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<TerminalOutcome, DbError>>(1);
        self.send(Command::Settle {
            reservation: Arc::clone(reservation),
            intent,
            reply: reply_tx,
        })
        .await?;
        recv_reply(reply_rx).await?
    }

    /// Send an `Attach` command and await the reply.
    pub(crate) async fn attach(&self, app_id: &str, db_path: &str) -> Result<(), DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<(), DbError>>(1);
        self.send(Command::Attach {
            app_id: app_id.to_string(),
            db_path: db_path.to_string(),
            reply: reply_tx,
        })
        .await?;
        recv_reply(reply_rx).await?
    }

    #[allow(dead_code)]
    pub(crate) async fn vacuum_into(
        &self,
        app_id: Option<&str>,
        dest_path: &str,
    ) -> Result<(), DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<(), DbError>>(1);
        self.send(Command::VacuumInto {
            app_id: app_id.map(str::to_string),
            dest_path: dest_path.to_string(),
            reply: reply_tx,
        })
        .await?;
        recv_reply(reply_rx).await?
    }

    /// Send a `ReattachFile` command and await the reply. `#[allow(dead_code)]`
    /// matches `vacuum_into` above.
    #[allow(dead_code)]
    pub(crate) async fn reattach_file(
        &self,
        app_id: &str,
        temp_path: &str,
        live_path: &str,
    ) -> Result<(), DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<(), DbError>>(1);
        self.send(Command::ReattachFile {
            app_id: app_id.to_string(),
            temp_path: temp_path.to_string(),
            live_path: live_path.to_string(),
            reply: reply_tx,
        })
        .await?;
        recv_reply(reply_rx).await?
    }

    async fn send(&self, cmd: Command) -> Result<(), DbError> {
        self.tx.send_async(cmd).await.map_err(|_| {
            DbError::internal("SqliteSession: worker thread is dead (queue receiver dropped)")
        })
    }

    pub fn try_exec_detached(
        &self,
        reservation: &Arc<Reservation>,
        sql: &str,
        params: &[&str],
    ) -> Result<(), DbError> {
        let (reply_tx, _reply_rx) = flume::bounded::<Result<u64, DbError>>(1);
        let cmd = Command::Exec {
            reservation: Arc::clone(reservation),
            sql: sql.to_string(),
            params: params.iter().map(|s| Value::from(*s)).collect(),
            reply: reply_tx,
        };
        self.tx.try_send(cmd).map_err(|e| {
            DbError::internal(format!(
                "SqliteSession: failed to enqueue detached exec command: {e}"
            ))
        })
    }

    /// A cancel handle for `reservation`.
    ///
    /// Holding one is what makes a cancellation possible at all: a dropped
    /// caller-side future is not observable by the actor, so there has to be an
    /// explicit channel.
    pub fn cancel_handle(&self, reservation: &Arc<Reservation>) -> SqliteCancelHandle {
        SqliteCancelHandle {
            queue: self.tx.clone(),
            interrupts: Arc::clone(&self.interrupts),
            reservation: Arc::clone(reservation),
        }
    }
}

impl Drop for SqliteSession {
    fn drop(&mut self) {
        // Best-effort: tell the worker to break out of its loop. If the queue
        // is full (unlikely on shutdown) or the worker has already exited,
        // ignore - the worker also observes the sender drop and ends the loop.
        let _ = self.tx.try_send(Command::Shutdown);
        // `_worker: JoinHandle<()>` drops here without joining, which detaches
        // the thread. We deliberately do not `.join()`: that would block the
        // dropping context, and whatever SQL the worker is processing right
        // now commits (WAL-durable) or rolls back before the loop ends anyway.
    }
}

// ---------------------------------------------------------------------------
// Leases and cancellation, caller side
// ---------------------------------------------------------------------------

/// A live reservation on `tx_conn`, released when the last handle drops.
///
/// The `Drop` enqueues `Release` best-effort. If that enqueue fails the lane is
/// still not stranded: `reserve_transaction` decides admission from the `Weak`,
/// which this drop has already invalidated, and the next `Reserve` repeats the
/// rollback cleanup.
pub(crate) struct TxLease {
    /// Liveness token. Held ONLY here and never cloned into a command, so its
    /// strong count is exactly "is this lease alive" - which is the question
    /// `reserve_transaction` asks and the reservation's own count cannot
    /// answer.
    #[allow(dead_code)] // never read: the Arc reference count IS the value
    alive: Arc<TxLeaseAlive>,
    reservation: Arc<Reservation>,
    tx: flume::Sender<Command>,
}

/// The value behind [`TxLease::alive`]. It carries no data; its reference
/// count is the whole point.
struct TxLeaseAlive;

impl std::fmt::Debug for TxLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxLease")
            .field("reservation", &self.reservation.id())
            .finish()
    }
}

impl TxLease {
    pub(crate) fn reservation(&self) -> &Arc<Reservation> {
        &self.reservation
    }
}

impl Drop for TxLease {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Command::Release {
            reservation: Arc::clone(&self.reservation),
        });
    }
}

/// The caller-side half of Decision 2.
///
/// `cancel()` sets the intent, interrupts the target connection when the actor
/// is already running the command, and then waits for the actor's
/// acknowledgement - which arrives only after the actor has rolled back and
/// retired the reservation. A cancellation that arrives after the outcome was
/// decided is answered [`TerminalOutcome::AlreadyCompleted`] and **no rollback
/// is claimed**.
#[derive(Clone)]
pub struct SqliteCancelHandle {
    queue: flume::Sender<Command>,
    interrupts: Arc<Interrupts>,
    reservation: Arc<Reservation>,
}

impl std::fmt::Debug for SqliteCancelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteCancelHandle").finish()
    }
}

impl SqliteCancelHandle {
    /// Record the intent and, if the actor is already running the target
    /// command, interrupt its connection. Returns what the intent decided.
    fn signal(&self) -> CancelIntent {
        let intent = self.reservation.request_cancel();
        if matches!(intent, CancelIntent::Interrupt(_)) {
            self.interrupts
                .interrupt(self.reservation.lane(), self.reservation.generation());
        }
        intent
    }

    /// Cancel, and wait for the actor to acknowledge.
    ///
    /// # Errors
    ///
    /// `DbError::Internal` when the actor is gone.
    pub async fn cancel(&self) -> Result<TerminalOutcome, DbError> {
        self.signal();
        let (reply_tx, reply_rx) = flume::bounded::<Result<TerminalOutcome, DbError>>(1);
        self.queue
            .send_async(Command::Cancel {
                reservation: Arc::clone(&self.reservation),
                reply: reply_tx,
            })
            .await
            .map_err(|_| {
                DbError::internal("SqliteSession: worker thread is dead (cancel not delivered)")
            })?;
        recv_reply(reply_rx).await?
    }
}

async fn recv_reply<T>(rx: flume::Receiver<T>) -> Result<T, DbError> {
    rx.recv_async().await.map_err(|_| {
        DbError::internal("SqliteSession: worker dropped reply channel before producing a result")
    })
}

// ---------------------------------------------------------------------------
// Clone-cheap handle
// ---------------------------------------------------------------------------

/// Clone-cheap handle to a `SqliteSession`, optionally bound to a
/// transaction reservation.
///
/// **`DatabaseFixture::Client` association**: `SqliteBackend` reports this type as
/// its `Client`. A handle carrying a `TxLease` routes every command onto
/// `tx_conn` under that reservation; a handle without one mints a fresh
/// autocommit reservation per command and routes onto `op_conn`. That is the
/// whole of Decision 1 at the call site.
#[derive(Clone)]
pub struct SqliteSessionHandle {
    pub(crate) session: Rc<SqliteSession>,
    lease: Option<Rc<TxLease>>,
}

impl std::fmt::Debug for SqliteSessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSessionHandle")
            .field("lease", &self.lease)
            .finish()
    }
}

impl SqliteSessionHandle {
    /// Wrap an existing session in an autocommit-lane handle.
    #[allow(dead_code)]
    pub(crate) fn new(session: Rc<SqliteSession>) -> Self {
        Self {
            session,
            lease: None,
        }
    }

    /// Wrap a session plus a transaction lease.
    pub(crate) fn with_lease(session: Rc<SqliteSession>, lease: Rc<TxLease>) -> Self {
        Self {
            session,
            lease: Some(lease),
        }
    }

    /// The reservation this handle's commands run under: the transaction lease
    /// when it holds one, otherwise a freshly minted autocommit reservation.
    fn reservation(&self) -> Arc<Reservation> {
        match &self.lease {
            Some(lease) => Arc::clone(lease.reservation()),
            None => self.session.autocommit_reservation(),
        }
    }

    /// The transaction reservation this handle holds, if any.
    pub(crate) fn tx_reservation(&self) -> Option<&Arc<Reservation>> {
        self.lease.as_ref().map(|l| l.reservation())
    }

    /// A cancel handle for whatever this handle's commands run under.
    ///
    /// Only meaningful for a transaction handle: an autocommit reservation is
    /// minted per command, so there is nothing stable to cancel.
    #[must_use]
    pub fn cancel_handle(&self) -> Option<SqliteCancelHandle> {
        self.lease
            .as_ref()
            .map(|lease| self.session.cancel_handle(lease.reservation()))
    }

    /// Convenience: forward an `exec` through the underlying session.
    pub async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        self.session.exec_on(&self.reservation(), sql, params).await
    }

    /// Execute a command without converting native parameters through text.
    pub async fn exec_values(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        self.session
            .exec_values_on(&self.reservation(), sql, params.to_vec())
            .await
    }

    pub fn try_exec_detached(&self, sql: &str, params: &[&str]) -> Result<(), DbError> {
        self.session
            .try_exec_detached(&self.reservation(), sql, params)
    }

    /// Run this handle's transaction terminal statement.
    pub async fn settle(&self, intent: TerminalIntent) -> Result<TerminalOutcome, DbError> {
        let Some(reservation) = self.tx_reservation() else {
            return Err(DbError::internal(
                "db: settle called on a SQLite handle that holds no transaction reservation",
            ));
        };
        self.session.settle(reservation, intent).await
    }

    /// Execute native parameters and return the driver's text projection.
    #[cfg(test)]
    pub async fn query_values(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>, DbError> {
        let reservation = self.reservation();
        let (tx, rx) = flume::bounded(1);
        self.session
            .send(Command::Query {
                reservation,
                sql: sql.to_owned(),
                params: params.to_vec(),
                reply: tx,
            })
            .await?;
        rx.recv_async()
            .await
            .map_err(|_| DbError::internal("sqlite actor closed"))?
    }

    /// Forward a `query` through the underlying session.
    ///
    /// Diagnostics can read text results without reaching into the actor.
    /// ORM operations use `query_typed` to preserve native cell types.
    pub async fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>, DbError> {
        self.session
            .query_on(&self.reservation(), sql, params)
            .await
    }

    /// Forward a `query_typed` through the underlying session, preserving cell
    /// types: the encrypted-column read path needs raw `TypedCell::Blob` bytes
    /// rather than the `<N bytes blob>` stringification.
    pub async fn query_typed(&self, sql: &str, params: &[Value]) -> Result<TypedRows, DbError> {
        self.session
            .query_typed_on(&self.reservation(), sql, params)
            .await
    }
}

impl From<Rc<SqliteSession>> for SqliteSessionHandle {
    fn from(s: Rc<SqliteSession>) -> Self {
        Self::new(s)
    }
}

// ---------------------------------------------------------------------------
// Test gate
// ---------------------------------------------------------------------------

#[cfg(test)]
struct NextCommandGateWorker {
    entered_tx: flume::Sender<()>,
    release_rx: flume::Receiver<()>,
}

/// The arming slot one session shares with its own actor thread.
#[cfg(test)]
type NextCommandGateSlot = Arc<Mutex<Option<NextCommandGateWorker>>>;

/// Test helper: stall the next worker command before execution until the
/// returned gate is released.
#[cfg(test)]
#[derive(Debug)]
pub struct NextCommandGate {
    entered_rx: flume::Receiver<()>,
    release_tx: flume::Sender<()>,
}

#[cfg(test)]
impl NextCommandGate {
    pub async fn wait_until_blocked(&self) -> Result<(), DbError> {
        self.entered_rx.recv_async().await.map_err(|_| {
            DbError::internal("SqliteSession test gate: worker dropped entered signal")
        })
    }

    pub fn release(self) {
        let _ = self.release_tx.send(());
    }
}

#[cfg(test)]
impl SqliteSession {
    /// Install a one-shot gate for the next command **this session's** actor
    /// runs. Sessions do not share the slot, so a gate armed here can only ever
    /// be tripped by a command this session was asked to run.
    pub fn arm_next_command_gate_for_tests(&self) -> NextCommandGate {
        let (entered_tx, entered_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded(1);
        let mut slot = self
            .next_command_gate
            .lock()
            .expect("next_command_gate mutex poisoned");
        assert!(slot.is_none(), "next_command_gate already armed");
        *slot = Some(NextCommandGateWorker {
            entered_tx,
            release_rx,
        });
        NextCommandGate {
            entered_rx,
            release_tx,
        }
    }
}

// ---------------------------------------------------------------------------
// The actor
// ---------------------------------------------------------------------------

/// One of the actor's connections.
struct LaneConn {
    conn: Connection,
    generation: u64,
    /// Set when a terminal classification could not prove the connection's
    /// state. A quarantined connection is closed and reopened before the next
    /// reservation touches it - keeping it would hand an unproved transaction
    /// to the next caller.
    quarantined: bool,
    /// Kept alive so the CDC hooks' captured `Arc`s outlive the connection.
    _dispatcher: Option<crate::backend::sqlite::cdc::SqliteCdcDispatcher>,
}

/// One app's transaction connection, plus the bookkeeping that decides who may
/// use it.
struct TxLane {
    conn: LaneConn,
    /// The app whose file this connection has ATTACHed. It has **only** that
    /// one, which is what makes a transaction unable to name another tenant's
    /// tables at all rather than merely by convention.
    app_id: String,
    /// The reservation whose transaction state this connection currently holds.
    bound: Option<u64>,
    /// The command sequence at which this lane was last used, for eviction.
    last_used: u64,
}

struct Actor {
    op: LaneConn,
    /// Per-app transaction connections, keyed by the lane id the session
    /// assigned. Bounded by [`MAX_TX_LANES`]; an idle lane is evicted to make
    /// room for a new app.
    tx: HashMap<TxLaneId, TxLane>,
    interrupts: Arc<Interrupts>,
    /// `(alias, path)` for every ATTACH, replayed onto a recycled connection
    /// and read when a transaction lane is opened for that app.
    attachments: Vec<(String, String)>,
    /// The reservation that last ran a command on `op_conn`.
    ///
    /// `op_conn`'s owner is short-lived by construction - one autocommit
    /// reservation, one command - so this is not an admission gate the way
    /// [`TxLane::bound`] is. It names the lane's current occupant, which is
    /// what a refusal has to report and what a later cancellation has to check.
    op_bound: Option<u64>,
    db_path: PathBuf,
    packet_tx: Option<CommitSender>,
    /// Monotonic command sequence. `0` is the not-running sentinel, so this
    /// starts at 1.
    seq: u64,
    /// This actor's own gate slot, shared with the session that owns it.
    #[cfg(test)]
    next_command_gate: NextCommandGateSlot,
}

const BOOT_PRAGMAS: &str = "\
    PRAGMA journal_mode = WAL; \
    PRAGMA synchronous = NORMAL; \
    PRAGMA busy_timeout = 5000; \
    PRAGMA foreign_keys = ON;";

fn open_lane_connection(
    db_path: &Path,
    packet_tx: Option<&CommitSender>,
) -> Result<
    (
        Connection,
        Option<crate::backend::sqlite::cdc::SqliteCdcDispatcher>,
    ),
    DbError,
> {
    register_sqlite_vec_once();
    let conn = Connection::open(db_path).map_err(from_sqlite)?;
    super::json::register(&conn).map_err(from_sqlite)?;
    super::decimal::register(&conn).map_err(from_sqlite)?;
    super::temporal::register(&conn).map_err(from_sqlite)?;
    conn.execute_batch(BOOT_PRAGMAS).map_err(from_sqlite)?;
    let dispatcher = match packet_tx {
        Some(tx) => Some(crate::backend::sqlite::cdc::install(&conn, tx.clone())?),
        None => None,
    };
    Ok((conn, dispatcher))
}

impl Actor {
    fn open(
        db_path: PathBuf,
        packet_tx: Option<CommitSender>,
    ) -> Result<Self, DbError> {
        let (op_conn, op_dispatcher) = open_lane_connection(&db_path, packet_tx.as_ref())?;
        let interrupts = Arc::new(Interrupts::new(op_conn.get_interrupt_handle()));
        Ok(Self {
            op: LaneConn {
                conn: op_conn,
                generation: 0,
                quarantined: false,
                _dispatcher: op_dispatcher,
            },
            tx: HashMap::new(),
            interrupts,
            attachments: Vec::new(),
            op_bound: None,
            db_path,
            packet_tx,
            seq: 0,
            #[cfg(test)]
            next_command_gate: Arc::new(Mutex::new(None)),
        })
    }

    fn lane_mut(&mut self, lane: Lane) -> Option<&mut LaneConn> {
        match lane {
            Lane::Op => Some(&mut self.op),
            Lane::Tx(id) => self.tx.get_mut(&id).map(|entry| &mut entry.conn),
        }
    }

    /// The connection behind `lane`, or a typed error when the lane is gone.
    ///
    /// Every caller reaches this only after [`Self::check_owner`] has passed,
    /// so the `None` arm is not expected to fire. It is a `Result` rather than
    /// an `expect` because this runs on the actor thread: a panic here would
    /// take the loop down and strand every caller waiting on a reply, which is
    /// a worse failure than the one it would be reporting.
    fn conn_of(&mut self, lane: Lane) -> Result<&Connection, DbError> {
        match self.lane_mut(lane) {
            Some(entry) => Ok(&entry.conn),
            None => Err(DbError::internal(format!(
                "sqlite actor: {} vanished mid-command",
                lane.name()
            ))),
        }
    }

    fn lane_ref(&self, lane: Lane) -> Option<&LaneConn> {
        match lane {
            Lane::Op => Some(&self.op),
            Lane::Tx(id) => self.tx.get(&id).map(|entry| &entry.conn),
        }
    }

    /// The reservation currently bound to `lane`, if any.
    fn lane_bound(&self, lane: Lane) -> Option<u64> {
        match lane {
            Lane::Op => self.op_bound,
            Lane::Tx(id) => self.tx.get(&id).and_then(|entry| entry.bound),
        }
    }

    /// The error a command naming a transaction lane the actor does not hold
    /// gets. It is a non-owner condition, not a distinct class: the connection
    /// this reservation claims is not one the actor has bound to it, which is
    /// the same statement as an id mismatch.
    fn no_such_lane(reservation: &Reservation) -> DbError {
        DbError::validation(
            "reservation_not_owner",
            format!(
                "db: reservation {} names a transaction connection this session does not \
                 hold (the app has no open transaction connection)",
                reservation.id()
            ),
        )
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Close and reopen a quarantined connection, replaying its ATTACHes and
    /// publishing a fresh interrupt handle under a bumped generation.
    ///
    /// The generation bump is the part that matters for cancellation: an
    /// interrupt aimed at the retired connection is refused rather than
    /// delivered to whatever the replacement is doing.
    fn recycle(&mut self, lane: Lane) {
        let (db_path, packet_tx) = (self.db_path.clone(), self.packet_tx.clone());
        // A transaction lane replays only its own app's ATTACH; `op_conn`
        // replays every one. Restoring the full list onto a transaction lane
        // would hand it the reach the per-app split just removed.
        let attachments: Vec<(String, String)> = match lane {
            Lane::Op => self.attachments.clone(),
            Lane::Tx(id) => {
                let Some(entry) = self.tx.get(&id) else {
                    return;
                };
                let owner = entry.app_id.clone();
                self.attachments
                    .iter()
                    .filter(|(alias, _)| alias == &owner)
                    .cloned()
                    .collect()
            }
        };
        let interrupts = Arc::clone(&self.interrupts);
        let Some(entry) = self.lane_mut(lane) else {
            return;
        };
        let generation = entry.generation + 1;
        match open_lane_connection(&db_path, packet_tx.as_ref()) {
            Ok((conn, dispatcher)) => {
                for (alias, path) in &attachments {
                    if let Err(e) = run_attach(&conn, alias, path) {
                        tracing::error!(
                            error = %e,
                            alias = %alias,
                            lane = lane.name(),
                            "sqlite actor: failed to replay ATTACH onto a recycled connection"
                        );
                    }
                }
                let handle = conn.get_interrupt_handle();
                entry.conn = conn;
                entry._dispatcher = dispatcher;
                entry.generation = generation;
                entry.quarantined = false;
                interrupts.replace(lane, generation, handle);
                tracing::warn!(
                    lane = lane.name(),
                    generation,
                    "sqlite actor: recycled a quarantined connection"
                );
            }
            Err(e) => {
                // Reopening failed. Leave the lane quarantined: every command
                // on it is refused with a typed error, which is strictly
                // better than serving one whose transaction state is unproved.
                tracing::error!(
                    error = %e,
                    lane = lane.name(),
                    "sqlite actor: could not recycle a quarantined connection"
                );
            }
        }
    }

    /// Apply an outcome's quarantine verdict, recycling immediately.
    fn apply_outcome(&mut self, lane: Lane, outcome: &TerminalOutcome) {
        if outcome.quarantines() {
            if let Some(entry) = self.lane_mut(lane) {
                entry.quarantined = true;
            }
            self.recycle(lane);
        }
    }

    /// Roll back anything a departing transaction reservation left open on
    /// `id`'s connection.
    fn unbind_tx(&mut self, id: TxLaneId) {
        let Some(entry) = self.tx.get_mut(&id) else {
            return;
        };
        entry.bound = None;
        if entry.conn.conn.is_autocommit() {
            return;
        }
        let raw = entry.conn.conn.execute_batch("ROLLBACK");
        let outcome = reservation::classify_rollback(&entry.conn.conn, raw, false, None);
        if outcome.quarantines() {
            tracing::error!(
                outcome = ?outcome,
                "sqlite actor: a released transaction lease left tx_conn in an unproved state"
            );
        }
        self.apply_outcome(Lane::Tx(id), &outcome);
    }

    /// Open `app_id`'s transaction connection, evicting an idle lane first when
    /// the session is already at [`MAX_TX_LANES`].
    ///
    /// The connection's `main` is the session's own database, exactly as
    /// `op_conn`'s is, so unqualified SQL means the same thing on both. What it
    /// does NOT get is the other apps' ATTACHes: a transaction lane carries one
    /// app's file and no other, so a creator transaction cannot name another
    /// tenant's tables even if a SQL builder were tricked into emitting one.
    fn open_tx_lane(&mut self, id: TxLaneId, app_id: &str) -> Result<u64, DbError> {
        if self.tx.len() >= MAX_TX_LANES {
            let victim = self
                .tx
                .iter()
                .filter(|(held, entry)| **held != id && entry.bound.is_none())
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(held, _)| *held);
            match victim {
                Some(held) => self.close_tx_lane(held),
                None => {
                    return Err(DbError::validation_hinted(
                        TX_LANES_EXHAUSTED,
                        format!(
                            "db: this SQLite session already holds {MAX_TX_LANES} apps' \
                             transaction connections and all of them are mid-transaction, so \
                             app '{app_id}' cannot open one"
                        ),
                        "This is contention with OTHER apps sharing this dev process, not a \
                         fault in your code - retry.",
                    ));
                }
            }
        }

        let path = self
            .attachments
            .iter()
            .find(|(alias, _)| alias == app_id)
            .map(|(_, path)| path.clone());
        let (conn, dispatcher) =
            open_lane_connection(&self.db_path, self.packet_tx.as_ref())?;
        if let Some(path) = &path {
            run_attach(&conn, app_id, path)?;
        }
        let handle = conn.get_interrupt_handle();
        // The generation a previous incarnation of this lane id left behind, or
        // 0 on a first open. Publishing it - rather than always 0 - is what
        // keeps a reservation from the closed connection out of this one.
        let generation = self.interrupts.generation(Lane::Tx(id));
        self.interrupts.replace(Lane::Tx(id), generation, handle);
        let seq = self.seq;
        self.tx.insert(
            id,
            TxLane {
                conn: LaneConn {
                    conn,
                    generation,
                    quarantined: false,
                    _dispatcher: dispatcher,
                },
                app_id: app_id.to_string(),
                bound: None,
                last_used: seq,
            },
        );
        Ok(generation)
    }

    /// Close an idle transaction lane, bumping its generation so an interrupt
    /// aimed at the connection just closed cannot reach its replacement.
    fn close_tx_lane(&mut self, id: TxLaneId) {
        if let Some(entry) = self.tx.remove(&id) {
            debug_assert!(
                entry.bound.is_none(),
                "close_tx_lane must not drop a bound transaction connection"
            );
            drop(entry);
        }
        self.interrupts.retire(id);
    }

    /// Refuse a command whose reservation does not own the connection it would
    /// run on.
    ///
    /// - `tx_conn` has a long-lived owner: whichever transaction reservation
    ///   the actor last bound. A command naming any other one is refused.
    /// - `op_conn` has no long-lived owner at all - autocommit reservations are
    ///   minted per command and settle at that command's completion. So its
    ///   rule is the *lifetime* one: a reservation that has already run a
    ///   command is spent, and a second command naming it is stale.
    fn check_owner(&self, reservation: &Reservation) -> Result<(), DbError> {
        let lane = reservation.lane();
        let Some(entry) = self.lane_ref(lane) else {
            return Err(Self::no_such_lane(reservation));
        };
        if entry.quarantined {
            return Err(DbError::Coded {
                code: "connection_quarantined".to_string(),
                message: format!(
                    "db: {} is quarantined after an unproved terminal statement and could \
                     not be recycled",
                    lane.name()
                ),
                hint: None,
            });
        }
        match lane {
            Lane::Tx(_) if self.lane_bound(lane) != Some(reservation.id()) => {
                Err(DbError::validation(
                    "reservation_not_owner",
                    format!(
                        "db: reservation {} does not own tx_conn (current owner: {:?})",
                        reservation.id(),
                        self.lane_bound(lane)
                    ),
                ))
            }
            Lane::Op if reservation.used() => Err(DbError::validation(
                "reservation_not_owner",
                format!(
                    "db: autocommit reservation {} has already run its one command and no \
                     longer owns op_conn (current owner: {:?})",
                    reservation.id(),
                    self.op_bound
                ),
            )),
            _ => Ok(()),
        }
    }

    fn run(&mut self, rx: &flume::Receiver<Command>) {
        while let Ok(cmd) = rx.recv() {
            #[cfg(test)]
            {
                let armed = self
                    .next_command_gate
                    .lock()
                    .expect("next_command_gate mutex poisoned")
                    .take();
                if let Some(gate) = armed {
                    let _ = gate.entered_tx.send(());
                    let _ = gate.release_rx.recv();
                }
            }
            match cmd {
                Command::Reserve {
                    reservation,
                    app_id,
                    reply,
                } => {
                    let result = self.run_reserve(&reservation, &app_id);
                    let _ = reply.send(result);
                }
                Command::Release { reservation } => {
                    if let Some(id) = reservation.lane().tx_id() {
                        if self.lane_bound(reservation.lane()) == Some(reservation.id()) {
                            self.unbind_tx(id);
                        }
                    }
                }
                Command::Exec {
                    reservation,
                    sql,
                    params,
                    reply,
                } => {
                    let result =
                        self.run_data(&reservation, &sql, |conn| run_exec(conn, &sql, &params));
                    let _ = reply.send(result);
                }
                Command::Query {
                    reservation,
                    sql,
                    params,
                    reply,
                } => {
                    let result =
                        self.run_data(&reservation, &sql, |conn| run_query(conn, &sql, &params));
                    let _ = reply.send(result);
                }
                Command::QueryTyped {
                    reservation,
                    sql,
                    params,
                    reply,
                } => {
                    let result = self.run_data(&reservation, &sql, |conn| {
                        run_query_typed(conn, &sql, &params)
                    });
                    let _ = reply.send(result);
                }
                Command::Settle {
                    reservation,
                    intent,
                    reply,
                } => {
                    let result = self.run_settle(&reservation, intent);
                    let _ = reply.send(result);
                }
                Command::Cancel { reservation, reply } => {
                    let outcome = self.run_cancel(&reservation);
                    let _ = reply.send(Ok(outcome));
                }
                Command::Attach {
                    app_id,
                    db_path,
                    reply,
                } => {
                    let result = self.run_attach_both(&app_id, &db_path);
                    let _ = reply.send(result);
                }
                Command::VacuumInto {
                    app_id,
                    dest_path,
                    reply,
                } => {
                    let result = run_vacuum_into(&self.op.conn, app_id.as_deref(), &dest_path);
                    let _ = reply.send(result);
                }
                Command::ReattachFile {
                    app_id,
                    temp_path,
                    live_path,
                    reply,
                } => {
                    let result = self.run_reattach(&app_id, &temp_path, &live_path);
                    let _ = reply.send(result);
                }
                Command::Shutdown => break,
            }
        }
        // Both connections drop here, closing cleanly. WAL checkpointing
        // happens on close.
    }

    /// Bind an app's transaction connection to `reservation`, opening it if the
    /// app has none.
    ///
    /// Whatever the previous binding left open is rolled back here, not merely
    /// on `Release`: a lease dropped without settling, or a `Release` lost to a
    /// full queue, must not leak its transaction into the next reservation.
    fn run_reserve(&mut self, reservation: &Arc<Reservation>, app_id: &str) -> Result<(), DbError> {
        let Some(id) = reservation.lane().tx_id() else {
            return Err(DbError::internal(
                "sqlite actor: Reserve names the autocommit lane, which is never reserved",
            ));
        };
        // A lane id belongs to one app for the session's life (the session mints
        // a fresh id for an app it has pruned), so a mismatch here is a bug
        // rather than a state to serve. Rebuild instead of running an app's
        // transaction against another app's connection.
        if self.tx.get(&id).is_some_and(|entry| entry.app_id != app_id) {
            tracing::error!(
                lane = id.0,
                app_id,
                "sqlite actor: transaction lane id reused across apps; rebuilding it"
            );
            self.unbind_tx(id);
            if let Some(entry) = self.tx.get_mut(&id) {
                entry.bound = None;
            }
            self.close_tx_lane(id);
        }
        if self.tx.contains_key(&id) {
            self.unbind_tx(id);
        } else {
            self.open_tx_lane(id, app_id)?;
        }
        // Read the generation AFTER the rollback above: `unbind_tx` can
        // quarantine and recycle the connection, which bumps it.
        let Some(generation) = self.tx.get(&id).map(|entry| entry.conn.generation) else {
            return Err(Self::no_such_lane(reservation));
        };
        // Stamp before binding: from the caller's side the reservation becomes
        // usable the instant this command's reply lands, so the generation it
        // reports must already be the connection's.
        reservation.adopt_generation(generation);
        let seq = self.seq;
        if let Some(entry) = self.tx.get_mut(&id) {
            entry.bound = Some(reservation.id());
            entry.last_used = seq;
        }
        Ok(())
    }

    /// Execute one data command under `reservation`.
    ///
    /// The order is the protocol: validate ownership, transition to `Running`
    /// (which is where a pre-start cancellation is observed), issue SQL, leave
    /// `Running`. An autocommit reservation additionally wraps the statement in
    /// `BEGIN DEFERRED ... COMMIT` where SQLite permits it.
    fn run_data<T>(
        &mut self,
        reservation: &Arc<Reservation>,
        sql: &str,
        op: impl FnOnce(&Connection) -> Result<T, RunError>,
    ) -> Result<T, DbError> {
        self.check_owner(reservation)?;
        if reservation.lane() == Lane::Op {
            // Spend the reservation and take the lane, in that order and
            // before anything can fail: a command that reached this point has
            // consumed its one autocommit reservation whether or not it goes
            // on to run, and anything asking later who owns op_conn must see
            // the answer now rather than the one from before this command.
            reservation.mark_used();
            self.op_bound = Some(reservation.id());
        }
        let seq = self.next_seq();
        if !reservation.enter_running(seq) {
            // SC-2 case 1: a cancellation was recorded before this command
            // started. It is neutralised here rather than physically removed
            // from the queue - flume has no mid-queue removal - and the
            // property SC-2 requires holds either way: no BEGIN and no data
            // SQL is ever issued for it.
            //
            // `enter_running` stored the sequence before it refused, so leave
            // it again here: a reservation left reading as Running is one a
            // later cancel handle would aim an interrupt at, and by then the
            // actor is executing something else on that connection.
            reservation.leave_running();
            return Err(cancelled_before_start(reservation));
        }
        let lane = reservation.lane();
        // Three conditions, and the middle one is not defensive. A caller that
        // drove `BEGIN` onto this connection itself owns the transaction; a
        // wrap would issue a nested `BEGIN DEFERRED` and SQLite would refuse
        // with "cannot start a transaction within a transaction". Deferring to
        // `is_autocommit` here is the same authority the terminal classifier
        // uses, applied one step earlier.
        let wrap = reservation.kind() == ReservationKind::Autocommit
            && self.conn_of(lane)?.is_autocommit()
            && permits_explicit_transaction(sql);

        let result = if wrap {
            self.run_wrapped_autocommit(reservation, lane, op)
        } else {
            reservation.mark_began();
            let conn = self.conn_of(lane)?;
            op(conn).map_err(RunError::into_db)
        };
        reservation.leave_running();
        result
    }

    fn run_wrapped_autocommit<T>(
        &mut self,
        reservation: &Arc<Reservation>,
        lane: Lane,
        op: impl FnOnce(&Connection) -> Result<T, RunError>,
    ) -> Result<T, DbError> {
        {
            let conn = self.conn_of(lane)?;
            reservation.mark_began();
            if let Err(e) = conn.execute_batch("BEGIN DEFERRED") {
                return Err(from_sqlite(e));
            }
        }

        let raw = {
            let conn = self.conn_of(lane)?;
            op(conn)
        };

        match raw {
            Err(e) => {
                // SC-2 consequence 3: an autocommit operation error must never
                // be followed by a COMMIT. Terminalize first.
                let conn = self.conn_of(lane)?;
                let rollback = conn.execute_batch("ROLLBACK");
                let outcome = reservation::classify_rollback(conn, rollback, false, None);
                self.apply_outcome(lane, &outcome);
                Err(e.into_db())
            }
            Ok(value) => {
                if !reservation.claim_completed() {
                    // A cancellation claimed the terminal first. Roll back and
                    // report the cancellation; the value is discarded because
                    // its transaction is about to be undone.
                    let conn = self.conn_of(lane)?;
                    let rollback = conn.execute_batch("ROLLBACK");
                    let outcome = reservation::classify_rollback(conn, rollback, true, None);
                    reservation.store_outcome(outcome.clone());
                    self.apply_outcome(lane, &outcome);
                    return Err(outcome.into_result().unwrap_err());
                }
                let conn = self.conn_of(lane)?;
                let commit = conn.execute_batch("COMMIT");
                let outcome = reservation::classify_commit(conn, commit);
                reservation.store_outcome(outcome.clone());
                self.apply_outcome(lane, &outcome);
                match outcome {
                    TerminalOutcome::Committed => Ok(value),
                    other => Err(other.into_result().unwrap_err()),
                }
            }
        }
    }

    fn run_settle(
        &mut self,
        reservation: &Arc<Reservation>,
        intent: TerminalIntent,
    ) -> Result<TerminalOutcome, DbError> {
        self.check_owner(reservation)?;
        let lane = reservation.lane();

        if intent == TerminalIntent::Commit && !reservation.claim_completed() {
            // A cancellation holds the terminal. Do not commit.
            let conn = self.conn_of(lane)?;
            let rollback = conn.execute_batch("ROLLBACK");
            let outcome = reservation::classify_rollback(conn, rollback, true, None);
            reservation.store_outcome(outcome.clone());
            self.apply_outcome(lane, &outcome);
            self.clear_tx_binding(lane);
            return Ok(outcome);
        }
        if intent == TerminalIntent::Rollback {
            reservation.claim_completed();
        }

        let seq = self.next_seq();
        // The return value is deliberately ignored HERE and nowhere else. A
        // terminal statement runs whatever the terminal word says: the commit
        // arm has already lost to a cancellation above and been rerouted to
        // ROLLBACK, and a rollback that a cancellation raced is still a
        // rollback. Refusing to start would leave the transaction open.
        // Storing `Running` still matters - it is what lets an interrupt reach
        // a COMMIT that is blocked on the write lock.
        let _ = reservation.enter_running(seq);
        reservation.mark_began();
        let outcome = {
            let conn = self.conn_of(lane)?;
            match intent {
                TerminalIntent::Commit => {
                    let raw = conn.execute_batch("COMMIT");
                    reservation::classify_commit(conn, raw)
                }
                TerminalIntent::Rollback => {
                    let raw = conn.execute_batch("ROLLBACK");
                    reservation::classify_rollback(conn, raw, false, None)
                }
            }
        };
        reservation.leave_running();
        reservation.store_outcome(outcome.clone());
        self.apply_outcome(lane, &outcome);
        self.clear_tx_binding(lane);
        Ok(outcome)
    }

    /// Does `reservation` still own the connection a cancellation would clean
    /// up on?
    ///
    /// Two ways to lose it, and a cancellation must survive both:
    ///
    /// 1. **The lane moved on.** A lease dropped without settling retires via
    ///    `Release`/`unbind_tx`, a fresh `Reserve` binds the next reservation,
    ///    and `tx_conn` is now carrying somebody else's `BEGIN`.
    /// 2. **The connection was replaced.** A quarantined lane is closed,
    ///    reopened and its generation bumped; nothing this reservation did
    ///    survives on the replacement.
    /// 3. **The lane is gone.** A transaction connection evicted to make room
    ///    for another app took its transaction with it, so there is nothing
    ///    here to roll back either.
    fn cancellation_still_owns_lane(&self, reservation: &Reservation) -> bool {
        let lane = reservation.lane();
        let Some(entry) = self.lane_ref(lane) else {
            return false;
        };
        if entry.generation != reservation.generation() {
            return false;
        }
        self.lane_bound(lane) == Some(reservation.id())
    }

    /// Release `lane`'s binding if it is a transaction lane.
    fn clear_tx_binding(&mut self, lane: Lane) {
        if let Some(id) = lane.tx_id() {
            if let Some(entry) = self.tx.get_mut(&id) {
                entry.bound = None;
            }
        }
    }

    /// Resolve a cancellation: roll the reservation's connection back, retire
    /// the reservation, and only then acknowledge.
    ///
    /// **The two guards below are the whole safety of this method.** Cleanup
    /// here is an unqualified `ROLLBACK` on a shared connection, so reaching it
    /// without the right to must be impossible rather than unlikely. Until
    /// 2026-08-27 `run_cancel` was the only data-bearing command that consulted
    /// neither the terminal claim's uniqueness nor `check_owner`, and both
    /// omissions were reachable: a duplicate `Cancel` (`claim_cancelled` used
    /// to grant a second claim), and - with no duplicate at all - a `Cancel`
    /// for a reservation whose lane a later transaction had taken over. Both
    /// rolled back a stranger's open transaction, and because that stranger's
    /// `tx_bound` was untouched its later `COMMIT` reported
    /// `CommitIndeterminate`: the creator told the fate was unknown for a write
    /// that had been silently destroyed.
    fn run_cancel(&mut self, reservation: &Arc<Reservation>) -> TerminalOutcome {
        if !reservation.claim_cancelled() {
            // A completion already claimed the terminal, and the winner's
            // recorded outcome is the answer. No ROLLBACK is sent; the write
            // stays durable. When the winner recorded nothing this reports an
            // indeterminate rather than a commit - see
            // `reservation::outcome_for_a_claimed_terminal`.
            return reservation::outcome_for_a_claimed_terminal(reservation);
        }

        if !reservation.began() {
            // No BEGIN, no data SQL, ever issued.
            let outcome = TerminalOutcome::Cancelled {
                cleanup: CancelCleanup::NoSqlStarted,
                cause: None,
            };
            reservation.store_outcome(outcome.clone());
            if reservation.lane().is_tx()
                && self.lane_bound(reservation.lane()) == Some(reservation.id())
            {
                self.clear_tx_binding(reservation.lane());
            }
            return outcome;
        }

        if !self.cancellation_still_owns_lane(reservation) {
            // SQL ran, but this reservation has since been retired and its
            // lane belongs to somebody else. Whoever retired it performed the
            // cleanup - `unbind_tx` rolls back anything a departing lease left
            // open, and `Settle` runs the terminal statement. Issuing a
            // `ROLLBACK` here would land on the current owner's transaction.
            let outcome = TerminalOutcome::Cancelled {
                cleanup: CancelCleanup::AlreadyRetired,
                cause: None,
            };
            reservation.store_outcome(outcome.clone());
            return outcome;
        }

        let lane = reservation.lane();
        // One ROLLBACK, unconditionally *within the ownership the guards above
        // established*. An interrupted write may already have been rolled back
        // by SQLite itself while an interrupted read leaves the transaction
        // open; both reach the same end state here, and "ROLLBACK errored
        // because there was no transaction" is classified by `is_autocommit`,
        // not treated as a failure.
        let outcome = {
            // `cancellation_still_owns_lane` above already proved the lane is
            // present, so the `None` arm is unreachable; it reports rather than
            // panicking because this runs on the actor thread.
            let Some(conn) = self.lane_mut(lane).map(|entry| &entry.conn) else {
                let outcome = TerminalOutcome::Cancelled {
                    cleanup: CancelCleanup::AlreadyRetired,
                    cause: None,
                };
                reservation.store_outcome(outcome.clone());
                return outcome;
            };
            let raw = conn.execute_batch("ROLLBACK");
            reservation::classify_rollback(conn, raw, true, None)
        };
        reservation.store_outcome(outcome.clone());
        self.apply_outcome(lane, &outcome);
        match lane {
            Lane::Tx(_) if self.lane_bound(lane) == Some(reservation.id()) => {
                self.clear_tx_binding(lane);
            }
            Lane::Op if self.op_bound == Some(reservation.id()) => self.op_bound = None,
            _ => {}
        }
        outcome
    }

    /// ATTACH on `op_conn` and on this app's transaction connection, or on
    /// neither.
    ///
    /// The asymmetric outcome is the one to avoid: `op_conn` sees the app and
    /// the transaction connection does not, so an ordinary read succeeds and
    /// the same app's transaction fails with "no such table". SQLite refuses
    /// `ATTACH` inside an explicit transaction, so this can still fail on the
    /// transaction connection alone, and the DETACH below is what keeps that a
    /// clean failure rather than a split view.
    ///
    /// **Only this app's transaction connection**, which is the point of the
    /// per-app split: another tenant's lane never learns the alias, so its
    /// transaction cannot name this app's tables even by accident. It also
    /// narrows the failure above - it used to fire while *another* app held a
    /// transaction open, and now only this app's own open transaction can
    /// cause it.
    fn run_attach_both(&mut self, app_id: &str, db_path: &str) -> Result<(), DbError> {
        run_attach(&self.op.conn, app_id, db_path)?;
        let owner = self
            .tx
            .iter()
            .find(|(_, entry)| entry.app_id == app_id)
            .map(|(id, _)| *id);
        let lane_attach = match owner.and_then(|id| self.tx.get(&id)) {
            Some(entry) => run_attach(&entry.conn.conn, app_id, db_path),
            None => Ok(()),
        };
        if let Err(e) = lane_attach {
            let escaped_alias = app_id.replace('"', "\"\"");
            let _ = self
                .op
                .conn
                .execute_batch(&format!("DETACH DATABASE \"{escaped_alias}\""));
            return Err(e);
        }
        self.attachments
            .retain(|(alias, _)| alias.as_str() != app_id);
        self.attachments
            .push((app_id.to_string(), db_path.to_string()));
        Ok(())
    }

    fn run_reattach(
        &mut self,
        app_id: &str,
        temp_path: &str,
        live_path: &str,
    ) -> Result<(), DbError> {
        let owner = self
            .tx
            .iter()
            .find(|(_, entry)| entry.app_id == app_id)
            .map(|(id, _)| *id);
        let result = match owner.and_then(|id| self.tx.get(&id)) {
            Some(entry) => run_reattach_file(
                &self.op.conn,
                Some(&entry.conn.conn),
                app_id,
                temp_path,
                live_path,
            ),
            None => run_reattach_file(&self.op.conn, None, app_id, temp_path, live_path),
        };
        if result.is_ok() {
            self.attachments
                .retain(|(alias, _)| alias.as_str() != app_id);
            self.attachments
                .push((app_id.to_string(), live_path.to_string()));
        }
        result
    }
}

fn cancelled_before_start(reservation: &Reservation) -> DbError {
    let outcome = TerminalOutcome::Cancelled {
        cleanup: CancelCleanup::NoSqlStarted,
        cause: None,
    };
    reservation.store_outcome(outcome.clone());
    outcome
        .into_result()
        .expect_err("a cancellation is never Ok")
}

/// Does SQLite allow this statement inside an explicit transaction?
///
/// A deliberately **lexical** pre-check, not a parser. It inspects the leading
/// keyword of every `;`-separated fragment and refuses to wrap when any of them
/// is one SQLite rejects inside a transaction (`PRAGMA` that writes, `VACUUM`,
/// `ATTACH`, `DETACH`) or one that manages transactions itself.
///
/// Its conservative failure mode is a false negative: it may
/// refuse to wrap something SQLite would have allowed, and the operation then
/// runs unwrapped. A non-empty fragment without a bare alphabetic leading
/// keyword is refused so comments cannot hide a transaction-control statement.
fn permits_explicit_transaction(sql: &str) -> bool {
    const REFUSED: &[&str] = &[
        "PRAGMA",
        "VACUUM",
        "ATTACH",
        "DETACH",
        "BEGIN",
        "COMMIT",
        "END",
        "ROLLBACK",
        "SAVEPOINT",
        "RELEASE",
    ];
    sql.split(';')
        // A fragment that is only whitespace is the gap around a `;`, not a
        // statement. Every other fragment must produce a keyword we recognise.
        .filter(|fragment| !fragment.trim().is_empty())
        .map(|fragment| {
            fragment
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches(|c: char| !c.is_ascii_alphabetic())
                .to_ascii_uppercase()
        })
        .all(|word| !word.is_empty() && !REFUSED.contains(&word.as_str()))
}

// ---------------------------------------------------------------------------
// Worker-side sync helpers
// ---------------------------------------------------------------------------

/// A worker-side failure that has **not** been mapped yet.
///
/// SC-2 consequence 2: the raw `rusqlite` error must survive until the actor
/// decides. Mapping inside `run_exec` / `run_query`, as this file used to,
/// erases the reservation, ownership and cancellation-intent context the
/// classifier needs.
enum RunError {
    Sqlite(rusqlite::Error),
    /// A plugin-side decode failure (bad base64, non-UTF-8 TEXT) that never had
    /// a rusqlite error to preserve.
    Db(DbError),
}

impl RunError {
    fn into_db(self) -> DbError {
        match self {
            Self::Sqlite(e) => from_sqlite(e),
            Self::Db(e) => e,
        }
    }
}

fn run_attach(conn: &Connection, app_id: &str, db_path: &str) -> Result<(), DbError> {
    let escaped_path = db_path.replace('\'', "''");
    let escaped_alias = app_id.replace('"', "\"\"");
    let sql = format!("ATTACH DATABASE 'file:{escaped_path}' AS \"{escaped_alias}\"");
    conn.execute_batch(&sql).map_err(from_sqlite)
}

fn run_exec(conn: &Connection, sql: &str, params: &[Value]) -> Result<u64, RunError> {
    // Text values stay text. Binary conversion is explicit in the SQL.
    let bindings: Vec<_> = params
        .iter()
        .map(crate::backend::sqlite::params::Parameter)
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = bindings.iter().map(|value| value as _).collect();
    let n = conn
        .execute(sql, refs.as_slice())
        .map_err(RunError::Sqlite)?;
    Ok(n as u64)
}

fn run_query(conn: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Row>, RunError> {
    let mut stmt = conn.prepare(sql).map_err(RunError::Sqlite)?;
    let column_count = stmt.column_count();
    let bindings: Vec<_> = params
        .iter()
        .map(crate::backend::sqlite::params::Parameter)
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = bindings.iter().map(|value| value as _).collect();
    let mut rows = stmt.query(refs.as_slice()).map_err(RunError::Sqlite)?;
    let mut out = Vec::<Row>::new();
    while let Some(row) = rows.next().map_err(RunError::Sqlite)? {
        let mut cells = Vec::with_capacity(column_count);
        for i in 0..column_count {
            // Materialise each cell as `Option<String>` regardless of the
            // underlying storage class. SQLite's typing is dynamic, so we
            // inspect the `ValueRef` discriminant and stringify uniformly.
            // NULL -> None; everything else -> Some(...).
            //
            // The Query path is consumed by PRAGMA inspection and by the
            // Catalog PRAGMA walk; both produce INTEGER + TEXT, never
            // BLOB. Refuse accidental binary reads loudly so callers route them
            // through `query_typed` instead of silently receiving a placeholder
            // string that cannot round-trip.
            use rusqlite::types::ValueRef;
            let value_ref = row.get_ref(i).map_err(RunError::Sqlite)?;
            let cell = match value_ref {
                ValueRef::Null => None,
                ValueRef::Integer(n) => Some(n.to_string()),
                ValueRef::Real(f) => Some(f.to_string()),
                ValueRef::Text(bytes) => Some(
                    std::str::from_utf8(bytes)
                        .map_err(|e| {
                            RunError::Db(DbError::internal(format!(
                                "sqlite TEXT cell is not valid UTF-8: {e}"
                            )))
                        })?
                        .to_string(),
                ),
                ValueRef::Blob(bytes) => {
                    return Err(RunError::Db(DbError::internal(format!(
                        "sqlite query path does not materialize BLOB column {i} \
                         ({} bytes); use query_typed instead",
                        bytes.len()
                    ))));
                }
            };
            cells.push(cell);
        }
        out.push(cells);
    }
    Ok(out)
}

/// Typed row materialisation - the vector path's row decoder. Preserves
/// SQLite's storage-class discriminator so a BLOB column reaches the caller as
/// `Vec<u8>` rather than a placeholder string.
fn run_query_typed(conn: &Connection, sql: &str, params: &[Value]) -> Result<TypedRows, RunError> {
    let mut stmt = conn.prepare(sql).map_err(RunError::Sqlite)?;
    let column_count = stmt.column_count();
    // `column_names` borrows from the statement; copy to owned `String` BEFORE
    // the row loop so the reply doesn't borrow from `stmt`.
    let columns: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let bindings: Vec<_> = params
        .iter()
        .map(crate::backend::sqlite::params::Parameter)
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = bindings.iter().map(|value| value as _).collect();
    let mut rows = stmt.query(refs.as_slice()).map_err(RunError::Sqlite)?;
    let mut out = Vec::<Vec<TypedCell>>::new();
    while let Some(row) = rows.next().map_err(RunError::Sqlite)? {
        let mut cells = Vec::with_capacity(column_count);
        for i in 0..column_count {
            use rusqlite::types::ValueRef;
            let value_ref = row.get_ref(i).map_err(RunError::Sqlite)?;
            let cell = match value_ref {
                ValueRef::Null => TypedCell::Null,
                ValueRef::Integer(n) => TypedCell::Integer(n),
                ValueRef::Real(f) => TypedCell::Real(f),
                ValueRef::Text(bytes) => TypedCell::Text(
                    std::str::from_utf8(bytes)
                        .map_err(|e| {
                            RunError::Db(DbError::internal(format!(
                                "sqlite TEXT cell is not valid UTF-8: {e}"
                            )))
                        })?
                        .to_string(),
                ),
                // Copy the BLOB bytes into an owned `Vec<u8>` so the reply
                // crossing the actor boundary doesn't borrow from rusqlite's
                // statement-owned buffer.
                ValueRef::Blob(bytes) => TypedCell::Blob(bytes.to_vec()),
            };
            cells.push(cell);
        }
        out.push(cells);
    }
    Ok(TypedRows { columns, rows: out })
}

/// Worker body for [`Command::VacuumInto`].
///
/// **String injection note**: VACUUM INTO does NOT accept bind parameters for
/// the destination path. We construct the statement inline with SQLite's
/// literal-string escape rule - single quotes are doubled inside the literal.
/// The `app_id` alias is double-quoted using the same rule (`"` -> `""`).
fn run_vacuum_into(
    conn: &Connection,
    app_id: Option<&str>,
    dest_path: &str,
) -> Result<(), DbError> {
    let escaped_path = dest_path.replace('\'', "''");
    let sql = match app_id {
        Some(alias) => {
            let escaped_alias = alias.replace('"', "\"\"");
            format!("VACUUM \"{escaped_alias}\" INTO '{escaped_path}'")
        }
        None => format!("VACUUM INTO '{escaped_path}'"),
    };
    conn.execute_batch(&sql).map_err(from_sqlite)
}

/// Worker body for [`Command::ReattachFile`].
///
/// Atomic-file-swap restore on the per-app alias, across **both** connections:
///
/// 1. `DETACH DATABASE "<app_id>"` on each connection.
/// 2. `std::fs::rename(temp_path, live_path)` - POSIX rename is atomic on the
///    same filesystem. Cross-filesystem rename is NOT atomic; operators must
///    keep snapshot destinations on the same FS as the live per-app DB.
/// 3. `ATTACH DATABASE 'file:<live_path>' AS "<app_id>"` on each connection.
///
/// Both connections are detached before the rename because a lock release must
/// not leave either bound to an obsolete inode. If step 1 fails on `op_conn`,
/// nothing is renamed and the live file is untouched. If it fails on `tx_conn`
/// after `op_conn` detached, `op_conn`'s alias is restored before returning.
fn run_reattach_file(
    op_conn: &Connection,
    tx_conn: Option<&Connection>,
    app_id: &str,
    temp_path: &str,
    live_path: &str,
) -> Result<(), DbError> {
    let escaped_alias = app_id.replace('"', "\"\"");
    let detach_sql = format!("DETACH DATABASE \"{escaped_alias}\"");
    let escaped_live = live_path.replace('\'', "''");
    let attach_live_sql = format!("ATTACH DATABASE 'file:{escaped_live}' AS \"{escaped_alias}\"");

    // Step 1 - DETACH the alias on both connections.
    op_conn
        .execute_batch(&detach_sql)
        .map_err(|e| DbError::Internal {
            message: format!(
                "ReattachFile: DETACH \"{app_id}\" on op_conn failed (live file untouched): {}",
                from_sqlite(e)
            ),
        })?;
    // `tx_conn` is `None` when this app has no transaction connection open -
    // the common case, since a lane exists only after the app's first
    // `db.transaction()`. There is then nothing to detach and nothing to
    // restore.
    let tx_detach = match tx_conn {
        Some(tx_conn) => tx_conn.execute_batch(&detach_sql),
        None => Ok(()),
    };
    if let Err(e) = tx_detach {
        // Restore op_conn's alias so the session does not lose it over a
        // failure that changed nothing on disk.
        let _ = op_conn.execute_batch(&attach_live_sql);
        return Err(DbError::Internal {
            message: format!(
                "ReattachFile: DETACH \"{app_id}\" on tx_conn failed (live file untouched): {}",
                from_sqlite(e)
            ),
        });
    }

    // Step 2 - atomic rename.
    if let Err(e) = std::fs::rename(temp_path, live_path) {
        let _ = op_conn.execute_batch(&attach_live_sql);
        if let Some(tx_conn) = tx_conn {
            let _ = tx_conn.execute_batch(&attach_live_sql);
        }
        return Err(DbError::Internal {
            message: format!(
                "ReattachFile: std::fs::rename({temp_path:?} -> {live_path:?}) failed: {e}; \
                 attempted re-ATTACH of the original live file (live content unchanged on \
                 success). NOTE: same-filesystem rename is the operator contract - cross-FS \
                 destinations cannot complete an atomic restore"
            ),
        });
    }

    // Step 3 - ATTACH the new file under the original alias on both.
    let targets: Vec<(&Connection, &str)> = match tx_conn {
        Some(tx_conn) => vec![(op_conn, "op_conn"), (tx_conn, "tx_conn")],
        None => vec![(op_conn, "op_conn")],
    };
    for (conn, name) in targets {
        conn.execute_batch(&attach_live_sql)
            .map_err(|e| DbError::Internal {
                message: format!(
                    "ReattachFile: ATTACH new file as \"{app_id}\" on {name} failed AFTER \
                     rename - the renamed snapshot is now the live file but that connection \
                     has no alias attached. The app file must be re-attached for this app_id \
                     before the session can serve it again. Underlying error: {}",
                    from_sqlite(e)
                ),
            })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Direct unit tests for the worker-side bodies and the wrap pre-check.
    //! The actor-level protocol (two connections, reservations, cancellation)
    //! is exercised end to end in `crates/zeroship-data-orm/src/tests/sqlite/transactions.rs`, which is the
    //! only place a real actor thread runs.

    use super::*;
    use rusqlite::Connection;

    fn count_rows(conn: &Connection, alias_or_main: &str, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {alias_or_main}.{table}");
        conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
            .unwrap()
    }

    #[test]
    fn vacuum_into_main_db_snapshot_file_is_self_contained() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("src.sqlite");
        let snap = dir.path().join("snap.sqlite");

        let conn = Connection::open(&live).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2), (3);")
            .unwrap();

        run_vacuum_into(&conn, None, snap.to_str().unwrap()).expect("VACUUM INTO main db");

        assert!(snap.exists(), "VACUUM INTO must produce the dest file");
        let snap_conn = Connection::open(&snap).unwrap();
        let count = snap_conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(count, 3, "snapshot must mirror live row count");
    }

    #[test]
    fn vacuum_into_attached_alias_captures_only_that_alias() {
        let dir = tempfile::tempdir().unwrap();
        let app_a_path = dir.path().join("zs-a.sqlite");
        let app_b_path = dir.path().join("zs-b.sqlite");
        let snap = dir.path().join("snap.sqlite");

        {
            let a = Connection::open(&app_a_path).unwrap();
            a.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (10), (20);")
                .unwrap();
            let b = Connection::open(&app_b_path).unwrap();
            b.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1000);")
                .unwrap();
        }

        let conn_file = tempfile::NamedTempFile::new().unwrap();

        let conn = Connection::open(conn_file.path()).unwrap();
        run_attach(&conn, "a", app_a_path.to_str().unwrap()).unwrap();
        run_attach(&conn, "b", app_b_path.to_str().unwrap()).unwrap();

        run_vacuum_into(&conn, Some("a"), snap.to_str().unwrap()).expect("VACUUM 'a' INTO snap");

        let snap_conn = Connection::open(&snap).unwrap();
        let count = snap_conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(count, 2, "snap must capture only the named alias's content");
    }

    #[test]
    fn vacuum_into_escapes_single_quote_in_path() {
        // An unescaped `'` would surface as a `near "x": syntax error` from
        // rusqlite's prepare. The path below does not exist, so a correctly
        // escaped statement fails at open-time instead - a different error
        // class, which is what we assert on.
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (x);").unwrap();

        let bad_path = "/nonexistent/dir/x'y.sqlite";
        let result = run_vacuum_into(&conn, None, bad_path);
        match result {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    !msg.contains("syntax error"),
                    "single-quote escape failed; SQL syntax error surfaced: {msg}"
                );
            }
            Ok(()) => panic!("VACUUM INTO into a nonexistent directory must fail at open-time"),
        }
    }

    #[test]
    fn reattach_file_atomic_swap_round_trip_across_both_connections() {
        let dir = tempfile::tempdir().unwrap();
        let live_path = dir.path().join("zs-app_demo.sqlite");
        let temp_path = dir.path().join("snap-restore.sqlite");

        {
            let live = Connection::open(&live_path).unwrap();
            live.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        {
            let tmp = Connection::open(&temp_path).unwrap();
            tmp.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (10), (20), (30);")
                .unwrap();
        }

        let op_file = tempfile::NamedTempFile::new().unwrap();

        let op = Connection::open(op_file.path()).unwrap();
        let tx_file = tempfile::NamedTempFile::new().unwrap();
        let tx = Connection::open(tx_file.path()).unwrap();
        run_attach(&op, "app_demo", live_path.to_str().unwrap()).unwrap();
        run_attach(&tx, "app_demo", live_path.to_str().unwrap()).unwrap();
        assert_eq!(count_rows(&op, "\"app_demo\"", "t"), 1);
        assert_eq!(count_rows(&tx, "\"app_demo\"", "t"), 1);

        run_reattach_file(
            &op,
            Some(&tx),
            "app_demo",
            temp_path.to_str().unwrap(),
            live_path.to_str().unwrap(),
        )
        .expect("reattach_file swap");

        // BOTH connections must see the restored content. Detaching only one
        // would leave the other bound to the obsolete inode, which is the
        // failure SC-2's `DetachApp` bullet names.
        assert_eq!(count_rows(&op, "\"app_demo\"", "t"), 3);
        assert_eq!(count_rows(&tx, "\"app_demo\"", "t"), 3);
        assert!(!temp_path.exists(), "rename must consume temp file");
        assert!(live_path.exists(), "live path must exist post-restore");
    }

    #[test]
    fn reattach_file_detach_failure_returns_internal_without_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live.sqlite");
        let temp = dir.path().join("temp.sqlite");
        {
            let l = Connection::open(&live).unwrap();
            l.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
                .unwrap();
            let t = Connection::open(&temp).unwrap();
            t.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (99);")
                .unwrap();
        }

        let op_file = tempfile::NamedTempFile::new().unwrap();

        let op = Connection::open(op_file.path()).unwrap();
        let tx_file = tempfile::NamedTempFile::new().unwrap();
        let tx = Connection::open(tx_file.path()).unwrap();

        let result = run_reattach_file(
            &op,
            Some(&tx),
            "never_attached",
            temp.to_str().unwrap(),
            live.to_str().unwrap(),
        );
        assert!(
            matches!(result, Err(DbError::Internal { .. })),
            "DETACH-side failure must surface as Internal; got {result:?}"
        );
        assert!(temp.exists(), "temp must remain when DETACH fails");
        assert!(live.exists(), "live must remain when DETACH fails");
    }

    #[test]
    fn run_query_rejects_blob_cells_on_untyped_path() {
        let conn_file = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(conn_file.path()).unwrap();
        conn.execute_batch("CREATE TABLE t (payload BLOB);")
            .unwrap();
        conn.execute("INSERT INTO t (payload) VALUES (X'0102')", [])
            .unwrap();

        let err = run_query(&conn, "SELECT payload FROM t", &[])
            .expect_err("untyped query path must refuse BLOB cells")
            .into_db();
        match err {
            DbError::Internal { message } => {
                assert!(
                    message.contains("does not materialize BLOB column"),
                    "error should explain the untyped BLOB trap: {message}"
                );
                assert!(
                    message.contains("query_typed"),
                    "error should point callers at the typed path: {message}"
                );
            }
            other => panic!("expected Internal error, got {other:?}"),
        }
    }

    #[test]
    fn the_wrap_precheck_refuses_every_statement_sqlite_bars_from_a_transaction() {
        let refused = [
            "PRAGMA journal_mode = WAL",
            "VACUUM",
            "VACUUM INTO 'x'",
            "ATTACH DATABASE 'f' AS \"a\"",
            "DETACH DATABASE \"a\"",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT zs_sp_1",
            "RELEASE SAVEPOINT zs_sp_1",
            "CREATE TABLE t (x); PRAGMA foreign_keys = OFF;",
            // Comment-prefixed. The leading token is `--` / `/*`, which trims
            // to the empty string - and an empty leading token used to be
            // dropped, leaving `all` vacuously true and reporting that a
            // VACUUM was safe to wrap. These are the arms that failed.
            "-- note\nVACUUM",
            "/* c */ VACUUM",
            "-- leading comment\nPRAGMA journal_mode = WAL",
            "CREATE TABLE t (x);\n-- then\nVACUUM",
        ];
        assert!(!refused.is_empty(), "the refusal set must not be empty");
        for sql in refused {
            assert!(
                !permits_explicit_transaction(sql),
                "{sql:?} must not be wrapped in BEGIN DEFERRED"
            );
        }

        let wrapped = [
            "INSERT INTO t (x) VALUES (?)",
            "SELECT * FROM t",
            "CREATE TABLE t (x INTEGER); CREATE INDEX i ON t (x);",
            "UPDATE t SET x = 1 WHERE x = 2",
            "  delete from t  ",
        ];
        assert!(!wrapped.is_empty(), "the wrapped set must not be empty");
        for sql in wrapped {
            assert!(
                permits_explicit_transaction(sql),
                "{sql:?} should be wrapped in BEGIN DEFERRED"
            );
        }
    }
}
