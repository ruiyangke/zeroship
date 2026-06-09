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
        let mut client = None;
        loop {
            // Synchronously pop the freshest clean idle entry (and its parked
            // timestamp). A dirty / not-fully-drained conn must never be
            // handed out (it would splice the previous caller's pending reply
            // into ours), so discard and keep scanning. Drop's own barrier
            // should prevent these from landing here, but checkout is the last
            // line of defence.
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

            // Hot conn (used recently): reuse without a probe.
            if now.duration_since(parked_at) <= self.inner.borrow().config.liveness_probe_after {
                client = Some(conn);
                break;
            }

            // Stale conn: test-on-borrow. PING it; on success reuse it, on
            // failure DISCARD it (drop) and try the next idle entry. The
            // probe is OUTSIDE the `RefCell` borrow (it awaits).
            match conn.ping().await {
                Ok(()) => {
                    client = Some(conn);
                    break;
                }
                Err(_) => {
                    // Dead/stale idle conn — drop it, do not hand it out, and
                    // do not count it busy. Loop to try the next idle entry.
                    drop(conn);
                    continue;
                }
            }
        }

        // Reserve a busy slot for the conn we are about to hand out (whether a
        // probed-live idle conn or a fresh on-demand connect). The reservation
        // happens AFTER probing so a discarded stale conn never leaks a slot.
        {
            let mut inner = self.inner.borrow_mut();
            if client.is_none() && inner.busy >= inner.config.max_size {
                return Err(Error::Pool(format!(
                    "pool exhausted — max_size={}, busy={}",
                    inner.config.max_size, inner.busy
                )));
            }
            inner.busy += 1;
        }

        // RAII reservation: from the moment `busy` was incremented above
        // (BOTH the idle-reuse and on-demand paths) a `BusyGuard` owns that
        // increment and will back it out on Drop — including if THIS
        // `acquire` future is cancelled while parked at the `connect().await`
        // below (caller dropped, enclosing timeout). It is disarmed only once
        // the `PooledConn` is successfully built, at which point the
        // `PooledConn`'s own Drop takes over the decrement. Exactly one of
        // the two is armed at any instant, so `busy` is never double-counted.
        let busy_guard = BusyGuard::new(self.inner.clone());

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

    // Positive control: a HOT (recently-used) idle conn is NOT probed, so a
    // mock that would FAIL a PING but answer the real command still works on
    // reuse — proving hot conns skip the probe (zero added latency / round
    // trips on the common path).
    #[compio::test]
    async fn hot_connection_is_not_probed_on_reuse() {
        // This mock answers EVERY command with the bulk reply — including a
        // PING (which `ping()` would reject as Unexpected, since it's not
        // `+PONG`). So if the pool probed a hot conn, the probe would error
        // and the conn would be discarded; we assert it is reused instead.
        let url = spawn_replying_mock(b"$5\r\nhello\r\n").await;
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

        // First use parks a hot idle conn.
        {
            let mut c = pool.acquire().await.expect("acquire 1");
            let v = c.get("k").await.expect("get 1");
            assert_eq!(v.as_deref(), Some(b"hello".as_ref()));
        }
        assert_eq!(pool.idle_len(), 1, "clean conn parked as hot idle");

        // Second use: the conn is hot, so it must be reused WITHOUT a probe
        // (a probe would hit the bulk-answering mock and error out).
        let mut c = pool.acquire().await.expect("acquire 2 (reuse hot conn)");
        let v = c.get("k").await.expect("get 2 on reused hot conn");
        assert_eq!(v.as_deref(), Some(b"hello".as_ref()));
        // And no extra connection was opened: still exactly one conn in play.
        assert!(pool.busy_count() <= 1);
    }
}
