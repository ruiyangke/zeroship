//! Minimal connection pool. Simpler than compio-postgres's — Redis
//! connections are cheap and stateless (no transactions to preserve,
//! no prepared statements), so a basic "idle stack + on-demand new
//! conn" strategy is plenty.
//!
//! Lifecycle:
//! - `acquire()` pops from the idle stack; if empty and under max_size,
//!   opens a fresh connection.
//! - Returning via drop of a `PooledConn` guard pushes back onto idle —
//!   but ONLY if the connection is clean (not dirty and its read buffer
//!   drained). A connection left dirty by a timeout/error/cancellation
//!   mid-reply is dropped instead of recycled, so a later (possibly
//!   cross-tenant) caller can never read the previous caller's reply.
//!   Checkout applies the same barrier as a last line of defence.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub max_size: usize,
    /// Warm-up connections opened eagerly in `connect()`. Zero means
    /// open on first acquire.
    pub min_idle: usize,
    /// Max time a connection can stay idle before being dropped.
    pub idle_timeout: Duration,
    /// Test-on-borrow threshold (RED-POOL-3). A connection that has sat
    /// idle longer than this is PINGed before being handed to the next
    /// caller; if the probe fails (the server closed it while idle — a
    /// Redis `timeout`, restart, or RST), it is discarded and the acquire
    /// loop tries the next idle entry, else opens a fresh connection. The
    /// probe replaces the dead conn BEFORE the caller's command is written,
    /// so there is no double-execution risk for non-idempotent ops.
    ///
    /// Hot conns (used more recently than this) are NOT probed — zero added
    /// latency on the common path. `Duration::ZERO` means always probe.
    pub liveness_probe_after: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_size: 16,
            min_idle: 1,
            idle_timeout: Duration::from_secs(600),
            liveness_probe_after: Duration::from_secs(30),
        }
    }
}

struct Inner {
    url: String,
    config: PoolConfig,
    /// Idle connections, LIFO for hottest-first reuse.
    idle: Vec<(Client, Instant)>,
    /// Currently-in-use count.
    busy: usize,
}

/// Pool handle — cheap to clone.
#[derive(Clone)]
pub struct Pool {
    inner: Rc<RefCell<Inner>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("Pool")
            .field("url", &"redis://…(redacted)")
            .field("max_size", &inner.config.max_size)
            .field("idle", &inner.idle.len())
            .field("busy", &inner.busy)
            .finish()
    }
}

impl Pool {
    pub async fn connect(url: &str, max_size: usize) -> Result<Self> {
        Self::connect_with(url, PoolConfig { max_size, ..PoolConfig::default() }).await
    }

    pub async fn connect_with(url: &str, config: PoolConfig) -> Result<Self> {
        // Warm up at least one connection so the URL is validated and
        // auth/select ran at least once before any user code runs.
        let mut idle = Vec::with_capacity(config.min_idle.max(1));
        let warm = config.min_idle.max(1);
        for _ in 0..warm {
            let c = Client::connect(url).await?;
            idle.push((c, Instant::now()));
        }
        Ok(Pool {
            inner: Rc::new(RefCell::new(Inner {
                url: url.to_string(),
                config,
                idle,
                busy: 0,
            })),
        })
    }

    /// Number of connections currently parked on the idle stack.
    /// Test-only introspection accessor for the pool-barrier regression tests.
    #[cfg(test)]
    pub(crate) fn idle_len(&self) -> usize {
        self.inner.borrow().idle.len()
    }

    /// Number of connections currently checked out (or reserved mid-connect).
    /// Test-only introspection accessor for the pool-barrier regression tests.
    #[cfg(test)]
    pub(crate) fn busy_count(&self) -> usize {
        self.inner.borrow().busy
    }

    /// Acquire a connection, opening a new one if idle is empty and
    /// capacity permits.
    pub async fn acquire(&self) -> Result<PooledConn> {
        // Fast path: take from idle stack, dropping timed-out entries, and
        // probe any STALE idle conn (one that has sat idle past
        // `liveness_probe_after`) before reusing it. A conn that died while
        // idle — the server closed it (a Redis `timeout`, restart, or RST) —
        // is otherwise handed to the next caller, who eats a spurious
        // connection error on their FIRST command (RED-POOL-3). Test-on-borrow
        // replaces the corpse BEFORE the caller's command is written, so it is
        // safe even for non-idempotent ops (no blind reconnect-retry, no
        // double-execution risk). Hot conns skip the probe → zero added
        // latency on the common path.
        let now = Instant::now();
        // `client` + `busy_guard` are CLAIMED together: the moment we pop an
        // idle candidate (or decide to connect on-demand) we reserve a `busy`
        // slot ATOMICALLY — in the same `borrow_mut()` as the pop, with no
        // `.await` in between — and hand it to a `BusyGuard`. The guard then
        // protects that reservation across EVERY subsequent `.await` (the
        // probe, the connect). This is what closes the over-subscription
        // window: single-threaded compio is cooperative, so a concurrent
        // acquire on the same `Pool` interleaves at each `.await`; because the
        // claim already moved the conn idle->busy, that other acquire sees the
        // reservation (idle shorter, `busy` higher) and cannot over-create.
        let mut client = None;
        let mut busy_guard: Option<BusyGuard> = None;
        loop {
            // Synchronously pop the freshest clean idle entry (and its parked
            // timestamp) AND reserve its busy slot in the same borrow. A dirty
            // / not-fully-drained conn must never be handed out (it would
            // splice the previous caller's pending reply into ours), so discard
            // and keep scanning. Drop's own barrier should prevent these from
            // landing here, but checkout is the last line of defence.
            //
            // Popping an idle conn moves it idle->busy (total unchanged), so no
            // `max_size` re-check is needed on this path — only an atomic claim.
            let candidate = {
                let mut inner = self.inner.borrow_mut();
                // Drop stale-by-idle-timeout conns from the top of the stack.
                let timeout = inner.config.idle_timeout;
                while let Some((_, ts)) = inner.idle.last() {
                    if now.duration_since(*ts) > timeout {
                        inner.idle.pop();
                    } else {
                        break;
                    }
                }
                let mut picked = None;
                while let Some((c, ts)) = inner.idle.pop() {
                    if c.is_dirty() || !c.is_rx_empty() {
                        drop(c); // discard without counting it busy
                    } else {
                        // Atomic claim: reserve the slot for THIS conn before
                        // releasing the borrow, so the probe `.await` below
                        // cannot expose an unreserved window.
                        inner.busy += 1;
                        picked = Some((c, ts));
                        break;
                    }
                }
                picked
            };

            let Some((mut conn, parked_at)) = candidate else {
                // Idle exhausted — fall through to the on-demand connect path.
                break;
            };

            // The popped conn's reservation is now guard-protected: a probe
            // failure below, OR a cancellation while parked in the probe,
            // backs it out via `release()` / Drop.
            let guard = BusyGuard::new(self.inner.clone());

            // Hot conn (used recently): reuse without a probe.
            if now.duration_since(parked_at) <= self.inner.borrow().config.liveness_probe_after {
                client = Some(conn);
                busy_guard = Some(guard);
                break;
            }

            // Stale conn: test-on-borrow. PING it; on success reuse it, on
            // failure DISCARD it (drop) and try the next idle entry. The
            // probe is OUTSIDE the `RefCell` borrow (it awaits) but the
            // reservation is already held by `guard`.
            match conn.ping().await {
                Ok(()) => {
                    // The probe round-trip may have left trailing pipelined
                    // bytes in `rx` (a server replying `+PONG\r\n` plus extra
                    // frames), or otherwise dirtied the conn. Handing it out
                    // now would splice that leftover into the caller's next
                    // reply, so re-apply the checkout barrier: discard and try
                    // the next idle entry, releasing this reservation.
                    if conn.is_dirty() || !conn.is_rx_empty() {
                        drop(conn);
                        guard.release();
                        continue;
                    }
                    client = Some(conn);
                    busy_guard = Some(guard);
                    break;
                }
                Err(_) => {
                    // Dead/stale idle conn — drop it, do not hand it out, and
                    // release its reservation. Loop to try the next idle entry.
                    drop(conn);
                    guard.release();
                    continue;
                }
            }
        }

        // RAII reservation across the rest of `acquire`. For the idle-reuse
        // path it was already claimed (above) at pop time; for the on-demand
        // path we gate `busy < max_size` and claim it now, BEFORE `connect()`.
        // Either way the guard backs the increment out on Drop — including if
        // THIS `acquire` future is cancelled while parked at `connect().await`
        // (caller dropped, enclosing timeout). It is disarmed only once the
        // `PooledConn` is built, at which point the `PooledConn`'s own Drop
        // takes over the decrement. Exactly one of the two is armed at any
        // instant, so `busy` is never double-counted.
        let busy_guard = match busy_guard {
            Some(g) => g,
            None => {
                let mut inner = self.inner.borrow_mut();
                if inner.busy >= inner.config.max_size {
                    return Err(Error::Pool(format!(
                        "pool exhausted — max_size={}, busy={}",
                        inner.config.max_size, inner.busy
                    )));
                }
                inner.busy += 1;
                BusyGuard::new(self.inner.clone())
            }
        };

        let client = match client {
            Some(c) => c,
            None => {
                let url = self.inner.borrow().url.clone();
                // On Err the guard (still armed) backs out busy; on
                // cancellation here the guard's Drop does the same.
                Client::connect(&url).await?
            }
        };
        let conn = PooledConn {
            pool: self.inner.clone(),
            client: Some(client),
        };
        // Hand the busy decrement over to `PooledConn::drop`.
        busy_guard.disarm();
        Ok(conn)
    }
}

/// RAII reservation for a `busy` slot. Decrements `Inner::busy` on Drop
/// unless `disarm`ed. Guarantees the counter is backed out on the error
/// AND the cancellation path of `acquire` (a dropped future runs Drop but
/// not the post-`await` code), closing the pool-exhaustion DoS where a
/// cancelled mid-connect acquire would otherwise leak a slot forever.
struct BusyGuard {
    inner: Option<Rc<RefCell<Inner>>>,
}

impl BusyGuard {
    fn new(inner: Rc<RefCell<Inner>>) -> Self {
        Self { inner: Some(inner) }
    }

    /// Relinquish ownership of the reserved slot (its decrement now belongs
    /// to the constructed `PooledConn`).
    fn disarm(mut self) {
        self.inner = None;
    }

    /// Back the reservation out NOW (decrement `busy`) and consume the guard.
    /// Used when a claimed idle conn is discarded mid-`acquire` (probe failed,
    /// or the probe left it dirty) so its slot is freed immediately instead of
    /// waiting for Drop — letting the same loop iteration's `continue` re-pop
    /// under an accurate count. Equivalent to letting the guard Drop, but
    /// explicit at the call site.
    fn release(self) {
        // `self` drops here; its Drop performs the decrement.
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let mut inner = inner.borrow_mut();
            inner.busy = inner.busy.saturating_sub(1);
        }
    }
}

/// Guard that auto-returns the connection on drop.
pub struct PooledConn {
    pool: Rc<RefCell<Inner>>,
    client: Option<Client>,
}

impl PooledConn {
    pub fn as_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("client taken")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        let mut inner = self.pool.borrow_mut();
        inner.busy = inner.busy.saturating_sub(1);
        if let Some(c) = self.client.take() {
            // Automatic desync barrier: only a CLEAN, fully-drained
            // connection is safe to recycle. A connection that timed out,
            // errored, or was cancelled mid-reply is left dirty (and may
            // still have a pending/partial reply on the socket or in its
            // read buffer); returning it would let the next — possibly
            // cross-tenant — caller read this caller's reply. Drop it
            // instead; a fresh connection opens on the next acquire.
            if !c.is_dirty() && c.is_rx_empty() {
                inner.idle.push((c, Instant::now()));
            }
        }
    }
}

impl std::ops::Deref for PooledConn {
    type Target = Client;
    fn deref(&self) -> &Client { self.client.as_ref().expect("client taken") }
}

impl std::ops::DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Client { self.client.as_mut().expect("client taken") }
}

// =====================================================================
// Red-team regression tests (R2 — pool barriers).
//
// These drive the REAL `Pool` against a small in-process mock Redis
// (a `compio::net::TcpListener` speaking — or refusing to speak — raw
// RESP) so we can reproduce the adversarial conditions (a command that
// times out mid-reply, a cancelled acquire) a real server won't hand us.
// =====================================================================
#[cfg(test)]
mod red_team_tests {
    use super::*;
    use compio::io::{AsyncRead, AsyncWriteExt};
    use compio::net::TcpListener;

    /// Spawn a mock Redis that accepts connections forever. For each
    /// connection it reads one command chunk and then NEVER replies,
    /// holding the socket open — so any command on a `PooledConn` against
    /// this server hits the command timeout (and is left dirty). Warm-up
    /// (`Client::connect` with no auth/db) still succeeds because it
    /// exchanges no bytes. Returns a `redis://ip:port` URL.
    async fn spawn_silent_mock() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                compio::runtime::spawn(async move {
                    let buf = vec![0u8; 1024];
                    let _ = stream.read(buf).await;
                    // Never reply; hold the conn open so the client must
                    // hit its command timeout rather than see EOF.
                    compio::time::sleep(Duration::from_secs(30)).await;
                    drop(stream);
                })
                .detach();
            }
        })
        .detach();
        format!("redis://{}:{}", addr.ip(), addr.port())
    }

    /// Spawn a mock Redis whose FIRST accepted connection is closed
    /// immediately (simulating an idle connection the server reaped — a
    /// Redis `timeout`, restart, or RST), and every SUBSEQUENT connection
    /// stays alive and answers commands: `+PONG\r\n` to a request whose
    /// bytes contain `PING` (the liveness probe), otherwise `reply`.
    ///
    /// This reproduces RED-POOL-3: the pool's warm-up connection dies while
    /// idle, and the next `acquire()` must NOT hand the corpse to the caller.
    /// With test-on-borrow it probes the stale conn (PING fails on the dead
    /// socket), discards it, and opens a fresh (second) connection that
    /// answers the user's command.
    async fn spawn_idle_death_then_alive_mock(reply: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            let mut conn_no = 0u32;
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                conn_no += 1;
                if conn_no == 1 {
                    // First (warm-up) connection: drop it right away so it is
                    // a dead/half-closed socket by the time the pool reuses it.
                    drop(stream);
                    continue;
                }
                compio::runtime::spawn(async move {
                    loop {
                        let buf = vec![0u8; 1024];
                        let compio::BufResult(n, b) = stream.read(buf).await;
                        let n = match n {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let is_ping = b[..n].windows(4).any(|w| w == b"PING");
                        let out: &[u8] = if is_ping { b"+PONG\r\n" } else { reply };
                        let compio::BufResult(w, _r) = stream.write_all(out).await;
                        if w.is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        })
        .detach();
        format!("redis://{}:{}", addr.ip(), addr.port())
    }

    /// Spawn a mock Redis whose FIRST connection answers the liveness PING
    /// with `+PONG\r\n` IMMEDIATELY FOLLOWED, in the same write, by an extra
    /// unsolicited frame (`trailing`) — modelling a server that pipelines a
    /// reply or push so the probe leaves leftover bytes in the client's `rx`.
    /// Every SUBSEQUENT connection is clean: `+PONG\r\n` to a PING, `reply`
    /// otherwise.
    ///
    /// This reproduces FIX C: a probe that returns `Ok(())` can still leave
    /// the conn non-rx-empty; handing it out would splice `trailing` into the
    /// caller's next command reply (intra-acquire desync). With the post-probe
    /// re-check the conn is discarded and a clean (second) conn is handed out.
    async fn spawn_ping_with_trailing_mock(
        reply: &'static [u8],
        trailing: &'static [u8],
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            let mut conn_no = 0u32;
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                conn_no += 1;
                let dirty_pong = conn_no == 1;
                compio::runtime::spawn(async move {
                    loop {
                        let buf = vec![0u8; 1024];
                        let compio::BufResult(n, b) = stream.read(buf).await;
                        let n = match n {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let is_ping = b[..n].windows(4).any(|w| w == b"PING");
                        let out: Vec<u8> = if is_ping {
                            let mut v = b"+PONG\r\n".to_vec();
                            if dirty_pong {
                                // Append the leftover frame to the PONG so the
                                // probe decodes PONG but leaves `trailing` in rx.
                                v.extend_from_slice(trailing);
                            }
                            v
                        } else {
                            reply.to_vec()
                        };
                        let compio::BufResult(w, _r) = stream.write_all(out).await;
                        if w.is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        })
        .detach();
        format!("redis://{}:{}", addr.ip(), addr.port())
    }

    /// Spawn a mock Redis that answers commands like `spawn_replying_mock`,
    /// but DELAYS its reply to a PING by `ping_delay` (the liveness probe).
    /// This deliberately widens the probe-`await` window so a second,
    /// concurrent `acquire()` can interleave while the first is parked in its
    /// probe — the precondition for the FIX A over-subscription bug.
    /// Non-PING commands are answered immediately with `reply`.
    async fn spawn_slow_ping_mock(reply: &'static [u8], ping_delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                compio::runtime::spawn(async move {
                    loop {
                        let buf = vec![0u8; 1024];
                        let compio::BufResult(n, b) = stream.read(buf).await;
                        let n = match n {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let is_ping = b[..n].windows(4).any(|w| w == b"PING");
                        if is_ping {
                            // Hold the PONG back so the probe stays parked,
                            // keeping the acquire that issued it in its window.
                            compio::time::sleep(ping_delay).await;
                            let compio::BufResult(w, _r) = stream.write_all(b"+PONG\r\n").await;
                            if w.is_err() {
                                break;
                            }
                        } else {
                            let compio::BufResult(w, _r) = stream.write_all(reply).await;
                            if w.is_err() {
                                break;
                            }
                        }
                    }
                })
                .detach();
            }
        })
        .detach();
        format!("redis://{}:{}", addr.ip(), addr.port())
    }

    /// Spawn a mock Redis that, for every connection, reads one command
    /// chunk and replies with a fixed RESP frame, then holds the socket
    /// open. Used as a positive control (a clean command).
    async fn spawn_replying_mock(reply: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        compio::runtime::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                compio::runtime::spawn(async move {
                    loop {
                        let buf = vec![0u8; 1024];
                        let compio::BufResult(n, _b) = stream.read(buf).await;
                        match n {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let compio::BufResult(w, _r) = stream.write_all(reply).await;
                                if w.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .detach();
            }
        })
        .detach();
        format!("redis://{}:{}", addr.ip(), addr.port())
    }

    /// Spawn a mock Redis that answers EVERY command (PING included) with the
    /// bulk `reply` on its FIRST accepted connection, but slams the door on
    /// any SECOND connection (accepts then immediately closes it). It tallies
    /// accepted connections in the returned counter.
    ///
    /// This makes a wrongful reconnect OBSERVABLE: if the pool ever discards
    /// the (live) first conn and opens a second — e.g. because a hot conn was
    /// wrongly probed, the bulk reply failed `ping()`'s `+PONG` check, and the
    /// conn got recycled into a reconnect — the command on that second conn
    /// hits a closed socket and ERRORS, and the counter reads 2. The correct
    /// hot-skip path never opens a second conn (counter stays 1) and the
    /// command succeeds.
    async fn spawn_first_conn_only_mock(reply: &'static [u8]) -> (String, Rc<std::cell::Cell<u32>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        let accepted = Rc::new(std::cell::Cell::new(0u32));
        let accepted_task = accepted.clone();
        compio::runtime::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else { break };
                let n = accepted_task.get() + 1;
                accepted_task.set(n);
                if n >= 2 {
                    // Second (and later) connection: refuse to serve it so a
                    // wrongful reconnect's command fails loudly.
                    drop(stream);
                    continue;
                }
                compio::runtime::spawn(async move {
                    loop {
                        let buf = vec![0u8; 1024];
                        let compio::BufResult(rn, _b) = stream.read(buf).await;
                        match rn {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let compio::BufResult(w, _r) = stream.write_all(reply).await;
                                if w.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .detach();
            }
        })
        .detach();
        (format!("redis://{}:{}", addr.ip(), addr.port()), accepted)
    }

    // -----------------------------------------------------------------
    // FIX 1 — RED-POOL-1 / REDIS-DESYNC-1: a dirty (timed-out / desynced)
    // connection must NOT be returned to the idle stack on drop, or the
    // next (cross-tenant) caller reads the previous caller's reply.
    // -----------------------------------------------------------------
    #[compio::test]
    async fn dirty_connection_is_not_returned_to_idle() {
        let url = spawn_silent_mock().await;
        // Warm-up succeeds (no bytes exchanged); idle now has 1 clean conn.
        let pool = Pool::connect_with(
            &url,
            PoolConfig { max_size: 4, min_idle: 1, ..PoolConfig::default() },
        )
        .await
        .expect("pool warm-up");

        {
            let mut c = pool.acquire().await.expect("acquire");
            // Short command timeout so the silent server trips it fast.
            c.set_cmd_timeout(Duration::from_millis(150));
            let res = c.get("k").await;
            let err = res.expect_err("silent server must time the command out");
            assert!(
                matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut),
                "expected TimedOut, got {err:?}"
            );
            // The connection is now dirty (a reply may still be in flight).
            assert!(c.is_dirty(), "timed-out conn must be dirty");
            // c drops here -> must be DROPPED, not pushed back onto idle.
        }

        assert_eq!(
            pool.idle_len(),
            0,
            "a dirty/desynced connection must NOT be returned to the idle stack"
        );
        assert_eq!(pool.busy_count(), 0, "busy must settle to 0 after drop");
    }

    // -----------------------------------------------------------------
    // FIX 1 positive control — a CLEAN command's connection IS returned.
    // -----------------------------------------------------------------
    // -----------------------------------------------------------------
    // FIX 2 — RED-POOL-2: the busy counter must NOT leak when an
    // `acquire()` future is cancelled while parked at `connect().await`.
    // A leak means every cancelled mid-connect acquire permanently
    // consumes a slot until `busy >= max_size` bricks the pool (DoS).
    // -----------------------------------------------------------------
    /// A `Waker` that does nothing — lets us poll a future by hand exactly
    /// once and then drop it (cancellation) without a runtime driving it.
    /// Built via the safe `std::task::Wake` trait (no `unsafe`).
    fn noop_waker() -> std::task::Waker {
        use std::sync::Arc;
        use std::task::Wake;
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
            fn wake_by_ref(self: &Arc<Self>) {}
        }
        Arc::new(Noop).into()
    }

    #[compio::test]
    async fn cancelled_acquire_does_not_leak_busy() {
        use std::future::Future;
        use std::task::Context;

        let url = spawn_replying_mock(b"$5\r\nhello\r\n").await;
        let pool = Pool::connect_with(
            &url,
            PoolConfig { max_size: 2, min_idle: 1, ..PoolConfig::default() },
        )
        .await
        .expect("pool warm-up");

        // Hold the single warm conn so idle is empty -> the next acquire
        // MUST take the on-demand connect path. busy is now 1.
        let _held = pool.acquire().await.expect("hold warm conn");
        let baseline_busy = pool.busy_count();
        assert_eq!(baseline_busy, 1, "held conn -> busy=1, idle empty");
        assert_eq!(pool.idle_len(), 0);

        // Drive an on-demand acquire to its first await (the connect), then
        // cancel it by dropping the future before it can resolve.
        {
            let mut fut = Box::pin(pool.acquire());
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let p = fut.as_mut().poll(&mut cx);
            // It must be parked at connect().await (busy already reserved),
            // not resolved — otherwise we aren't exercising the cancel path.
            assert!(
                p.is_pending(),
                "acquire should park at connect().await on first poll"
            );
            assert_eq!(
                pool.busy_count(),
                baseline_busy + 1,
                "the parked acquire reserved a busy slot"
            );
            drop(fut); // cancel mid-connect
        }

        // The reserved slot MUST be backed out by the cancellation.
        assert_eq!(
            pool.busy_count(),
            baseline_busy,
            "cancelled mid-connect acquire must NOT leak the busy counter"
        );

        // And the pool must not be bricked: another acquire still works.
        let _c2 = pool.acquire().await.expect("pool still usable after cancel");
        assert!(pool.busy_count() <= 2, "busy must stay within max_size");
    }

    #[compio::test]
    async fn clean_connection_is_returned_to_idle() {
        let url = spawn_replying_mock(b"$5\r\nhello\r\n").await;
        let pool = Pool::connect_with(
            &url,
            PoolConfig { max_size: 4, min_idle: 1, ..PoolConfig::default() },
        )
        .await
        .expect("pool warm-up");
        assert_eq!(pool.idle_len(), 1, "warm-up -> 1 idle conn");

        {
            let mut c = pool.acquire().await.expect("acquire");
            assert_eq!(pool.idle_len(), 0, "acquire moves the conn out of idle");
            assert_eq!(pool.busy_count(), 1);
            let v = c.get("k").await.expect("clean get");
            assert_eq!(v.as_deref(), Some(b"hello".as_ref()));
            assert!(!c.is_dirty(), "clean command must leave conn clean");
            // c drops here -> clean, must be returned to idle.
        }

        assert_eq!(
            pool.idle_len(),
            1,
            "a clean connection must be returned to the idle stack for reuse"
        );
        assert_eq!(pool.busy_count(), 0);
    }

    // -----------------------------------------------------------------
    // FIX 3 — RED-POOL-3 / REDIS-RECONNECT-1: a connection that died while
    // IDLE (server-side close: redis `timeout`, restart, RST) is still
    // popped from the idle stack and handed to the next caller, who gets a
    // spurious connection error on their FIRST command. The dirty barrier
    // (R2) only covers conns that errored *in use*, not idle-death.
    //
    // FIX (test-on-borrow): when a popped idle conn has been idle longer
    // than `liveness_probe_after`, PING it first; if the probe fails,
    // DISCARD it and continue the acquire loop (next idle entry, else a
    // fresh connection). The probe replaces the conn BEFORE the user's
    // command is sent, so there is no double-execution risk.
    // -----------------------------------------------------------------
    #[compio::test]
    async fn stale_idle_connection_is_probed_and_replaced() {
        // The mock closes its FIRST (warm-up) connection immediately, so the
        // pool's lone idle conn is dead; the SECOND connection answers PING
        // + the command.
        let url = spawn_idle_death_then_alive_mock(b"$5\r\nhello\r\n").await;
        // `liveness_probe_after = 0` => always probe on borrow (no waiting on
        // an idle clock). max_size>=2 so a fresh conn can open after discard.
        let pool = Pool::connect_with(
            &url,
            PoolConfig {
                max_size: 4,
                min_idle: 1,
                liveness_probe_after: Duration::ZERO,
                ..PoolConfig::default()
            },
        )
        .await
        .expect("pool warm-up");
        assert_eq!(pool.idle_len(), 1, "warm-up parked one (now-dead) idle conn");

        // Acquire + run a command. With the fix: the dead warm conn is
        // probed -> PING fails -> discarded -> a fresh live conn opens ->
        // the command SUCCEEDS. Pre-fix: the dead conn is handed out and the
        // command fails with a connection error (UnexpectedEof / reset).
        let mut c = pool.acquire().await.expect("acquire must yield a LIVE conn");
        let v = c
            .get("k")
            .await
            .expect("command on a freshly-probed live conn must succeed");
        assert_eq!(v.as_deref(), Some(b"hello".as_ref()));
    }

    // -----------------------------------------------------------------
    // FIX C — RED-POOL-PROBE-DIRTY: a liveness probe that returns Ok(()) can
    // still leave the conn NON-rx-empty (server pipelined a reply/push after
    // `+PONG`, arriving in the same TCP segment). The checkout barrier
    // (~154) and the drop barrier both refuse such a conn, but the post-probe
    // hand-out skipped that re-check — so the leftover frame would splice into
    // the caller's FIRST command reply (intra-acquire desync).
    //
    // FIX: after `ping()` returns Ok, re-apply `is_dirty() || !is_rx_empty()`;
    // if it trips, discard the conn (release its reservation) and continue —
    // exactly as a probe FAILURE does. Here the warm conn's PONG carries a
    // trailing `$3\r\nXXX\r\n`, so the probe leaves rx dirty; the fixed pool
    // discards it and hands out a clean second conn whose GET returns "hello".
    // Pre-fix the dirty conn is handed out and `get("k")` decodes the buffered
    // leftover ("XXX") instead of the real reply.
    #[compio::test]
    async fn probe_leaving_trailing_bytes_discards_conn() {
        // Warm conn's PING answer = `+PONG\r\n` + a leftover bulk frame.
        let url = spawn_ping_with_trailing_mock(b"$5\r\nhello\r\n", b"$3\r\nXXX\r\n").await;
        let pool = Pool::connect_with(
            &url,
            PoolConfig {
                max_size: 4,
                min_idle: 1,
                // Always probe on borrow so the warm conn is the one probed.
                liveness_probe_after: Duration::ZERO,
                ..PoolConfig::default()
            },
        )
        .await
        .expect("pool warm-up");
        assert_eq!(pool.idle_len(), 1, "warm-up parked one idle conn");

        // Acquire: the stale warm conn is probed -> PONG decodes but leaves
        // `$3\r\nXXX\r\n` in rx. With the fix that conn is discarded and a
        // fresh (clean) conn opens, so GET returns the REAL value. Without the
        // fix the dirty conn is handed out and GET reads the buffered "XXX".
        let mut c = pool.acquire().await.expect("acquire a clean conn");
        let v = c
            .get("k")
            .await
            .expect("GET must succeed on a clean conn (not a desynced one)");
        assert_eq!(
            v.as_deref(),
            Some(b"hello".as_ref()),
            "a probe that left trailing bytes must NOT be handed out — the \
             leftover frame would be spliced into this reply"
        );
    }

    // Positive control: a HOT (recently-used) idle conn is NOT probed.
    //
    // FIX B (was a partial tautology): the old test's only assertion was
    // `busy_count() <= 1`, which a BROKEN hot-skip also satisfies — a wrongful
    // probe -> discard -> reconnect path also lands at `busy == 1`. This
    // version is DISCRIMINATING: the mock serves only its FIRST connection and
    // tallies accepts, so a wrongful reconnect is observable two ways —
    //   (a) the reused command FAILS (the second conn is closed), and
    //   (b) the accept counter reads 2 instead of 1.
    // The correct hot-skip path reuses the first conn (no `ping()`, no
    // reconnect): the command succeeds, `idle_len()==0 && busy_count()==1`
    // while held, and exactly ONE connection was ever accepted.
    //
    // How I confirmed it discriminates: temporarily neutering the hot-skip
    // early-return (`if false && …` so every borrow probes) makes the test
    // FAIL — the first borrow PINGs the lone served conn, the mock answers
    // with the bulk reply (not `+PONG`) so `ping()` errors, the conn is
    // discarded, and the on-demand reconnect lands on conn #2 which the mock
    // slams shut, so the very next `get` fails (and `accepted` would read 2).
    // The OLD test could NOT catch this: against `spawn_replying_mock` (which
    // serves every connection) a wrongful reconnect's `get` still succeeds and
    // `busy` still settles to 1, so its lone `busy_count() <= 1` assert stayed
    // GREEN. (Sabotage reverted; the shipped hot-skip keeps `accepted == 1`.)
    #[compio::test]
    async fn hot_connection_is_not_probed_on_reuse() {
        let (url, accepted) = spawn_first_conn_only_mock(b"$5\r\nhello\r\n").await;
        let pool = Pool::connect_with(
            &url,
            PoolConfig {
                max_size: 4,
                min_idle: 1,
                // Large threshold: a just-parked conn is "hot" => never probed.
                liveness_probe_after: Duration::from_secs(3600),
                ..PoolConfig::default()
            },
        )
        .await
        .expect("pool warm-up");
        // NB: don't read `accepted` here — the client's TCP connect completes
        // at the kernel before the mock's userspace `accept().await` returns
        // (and bumps the counter), so an early read races to 0. We sample the
        // counter only AFTER a command round-trip, which proves the conn was
        // accepted and served.

        // First use parks a hot idle conn.
        {
            let mut c = pool.acquire().await.expect("acquire 1");
            let v = c.get("k").await.expect("get 1");
            assert_eq!(v.as_deref(), Some(b"hello".as_ref()));
        }
        assert_eq!(pool.idle_len(), 1, "clean conn parked as hot idle");

        // Second use: the conn is hot, so it must be reused WITHOUT a probe.
        // If the hot-skip were broken, the probe would PING the bulk-answering
        // mock (-> `ping()` errors -> conn discarded -> on-demand RECONNECT),
        // and that reconnect's conn #2 is force-closed by the mock, so this
        // command would FAIL. It succeeding proves no probe/reconnect happened.
        let mut c = pool.acquire().await.expect("acquire 2 (reuse hot conn)");
        // Exactly the SAME single conn is in flight: moved out of idle, no new
        // connection opened.
        assert_eq!(pool.idle_len(), 0, "hot conn moved out of idle (not discarded)");
        assert_eq!(pool.busy_count(), 1, "exactly one conn busy — no reconnect");
        let v = c.get("k").await.expect("get 2 on reused hot conn");
        assert_eq!(v.as_deref(), Some(b"hello".as_ref()));

        // The decisive check: the pool never opened a second connection. A
        // wrongful probe-discard-then-reconnect would have made this 2.
        assert_eq!(
            accepted.get(),
            1,
            "a hot conn must be reused WITHOUT probing/reconnecting (only the \
             warm-up connection should ever be accepted)"
        );
    }

    // -----------------------------------------------------------------
    // FIX A — RED-POOL-PROBE-WINDOW: the liveness probe must NOT open a
    // window in which the pool over-subscribes `max_size`. The follow-up
    // (11d7974c) MOVED `busy += 1` to AFTER `conn.ping().await`, so while
    // acquire A is parked in its probe (having already popped its idle
    // candidate out of `idle` but NOT yet reserved `busy`), a concurrent
    // acquire B sees an empty idle stack, passes the on-demand gate
    // (`busy < max_size`), reserves a slot, and connects. When A's probe
    // then succeeds it does an UNCONDITIONAL `busy += 1` (the gate is on
    // the on-demand path only), pushing `busy` past `max_size`.
    //
    // Single-threaded compio is COOPERATIVE: distinct tasks sharing one
    // `Pool` interleave at every `.await`, so this is reachable. The mock
    // delays its PONG to widen A's probe window deterministically.
    //
    // Pre-fix: `busy` reaches 2 for `max_size=1` -> RED.
    // Post-fix (reserve atomically at pop, before the probe): the claim
    // moves the conn idle->busy with no intervening `.await`, so B sees the
    // reservation and is refused (Pool exhausted) -> `busy` stays <= 1.
    #[compio::test]
    async fn probe_window_does_not_oversubscribe_max_size() {
        // PING (the probe) is held back ~250ms so acquire A parks in its
        // probe long enough for acquire B to interleave.
        let url = spawn_slow_ping_mock(b"$5\r\nhello\r\n", Duration::from_millis(250)).await;
        let pool = Pool::connect_with(
            &url,
            PoolConfig {
                max_size: 1,
                min_idle: 1,
                // ZERO => the lone warm conn is ALWAYS treated as stale and
                // probed on borrow, so acquire A definitely awaits the probe.
                liveness_probe_after: Duration::ZERO,
                ..PoolConfig::default()
            },
        )
        .await
        .expect("pool warm-up");
        assert_eq!(pool.idle_len(), 1, "warm-up parked one idle conn");
        assert_eq!(pool.busy_count(), 0);

        // Acquire A: pops the warm conn and parks in the (slow) probe.
        let pool_a = pool.clone();
        let task_a = compio::runtime::spawn(async move {
            let _g = pool_a.acquire().await.expect("acquire A");
            // Hold the conn briefly so it overlaps with B's reservation.
            compio::time::sleep(Duration::from_millis(150)).await;
            // Return the busy count observed WHILE A holds its conn.
            pool_a.busy_count()
        });

        // Let A get firmly parked inside its probe `await` (idle now empty,
        // but A has NOT reserved busy yet under the buggy ordering).
        compio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            pool.idle_len(),
            0,
            "A popped the only idle conn before parking in its probe"
        );

        // Acquire B, concurrently. idle is empty, so B takes the on-demand
        // path. Under the bug it passes `busy(0) < max_size(1)`, reserves a
        // slot and connects -> a SECOND live conn for a max_size=1 pool.
        let b_result = pool.acquire().await;

        // Sample the over-subscription. Under the bug, by now A's probe has
        // either resolved (busy incremented to 2) or is about to; give it a
        // beat so A's post-probe `busy += 1` lands, then read the peak.
        compio::time::sleep(Duration::from_millis(250)).await;
        let peak_busy = pool.busy_count();

        let a_observed = task_a.await.expect("task A joins");

        let max = pool.inner.borrow().config.max_size;
        assert!(
            peak_busy <= max && a_observed <= max,
            "probe window over-subscribed: max_size={max}, peak busy={peak_busy}, \
             A-observed busy={a_observed} (B acquired ok? {})",
            b_result.is_ok(),
        );
        // The discriminating post-fix contract: A's atomic claim took the lone
        // slot while it probed, so B (idle empty, no free slot) MUST be refused
        // with pool exhaustion rather than over-create. Pre-fix B succeeded
        // (and `peak_busy` hit 2); this asserts the window is truly closed, not
        // merely that the counter happened to settle low.
        assert!(
            matches!(b_result, Err(Error::Pool(_))),
            "B should be refused (pool exhausted) while A holds the only slot \
             (B acquired ok? {})",
            b_result.is_ok(),
        );
    }
}
