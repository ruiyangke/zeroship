# compio-postgres — Full-Fidelity Port of tokio-postgres

**Goal:** Port `tokio-postgres` (8.9k LOC) to compio/io_uring. Full feature parity, no deferred modules.

**Target crate:** `crates/compio-postgres/` (new, parallel to current `crates/pg/`).

**Strategy:** Copy verbatim where tokio-agnostic; rewrite I/O layer + demux loop for compio; keep our hardening (length-field cap, HikariCP pool, TCP keepalive).

**License:** dual MIT / Apache-2.0 per tokio-postgres. Preserve copyright headers in ported files.

---

## Architecture parity

We preserve tokio-postgres' Client/Connection split because it's what gives us pipelining:

```
   user code / pool
         │
         ▼
┌───────────────┐   futures_channel::mpsc::UnboundedSender<Request>   ┌──────────────────┐
│    Client     │ ──────────────────────────────────────────────────▶ │    Connection    │
│  (handle)     │                                                     │  (compio future) │
│  !Send        │   futures_channel::mpsc::Sender<BackendMessages>    │  owns the socket │
│  (Rc-based)   │ ◀─────────────────────────────────────────────────  │                  │
└───────────────┘        one per in-flight request                    └────────┬─────────┘
                                                                               │
                                                                       compio TcpStream
                                                                       + our BufStream
                                                                               │
                                                                              PG
```

Differences from tokio-postgres:
- **Everything is `!Send`** because compio's `TcpStream` is `!Send` (Rc internals).
- **`tokio_util::codec::Framed` is gone**; replaced by a hand-rolled read loop over our existing `BufStream` (keeps our 64MB length cap).
- **`tokio::spawn` → `compio::runtime::spawn`** (LocalSpawn; same-thread only).
- **`tokio::net::TcpStream` → `compio::net::TcpStream`** everywhere.
- **`tokio::net::UnixStream` → `compio::net::UnixStream`**.
- **`futures_channel::mpsc`** retained — it's Send-generic; with non-Send payloads it becomes non-Send naturally. No new channel dep needed.
- **Pool**: our HikariCP pool migrates to manage `Client` handles instead of raw connections. This is a meaningful API shift: one pool entry = one Client + one Connection task.

---

## File-by-file port table

✓ = copy verbatim (possibly minor import changes)
● = thin rewrite (logic preserved, I/O swapped)
◆ = significant rewrite (architecture-specific)

| Source file | LOC | Verdict | Notes |
|---|---|---|---|
| `lib.rs` | 267 | ● | Drop tokio-runtime examples; re-exports similar |
| `config/*.rs` | 1205 | ✓ | Disable `runtime` feature gates; remove `tokio::net` imports in config |
| `config/mod.rs` | | ✓ | |
| `client.rs` | 799 | ● | Keep InnerClient, type cache, request dispatch shape; swap channel types |
| `connection.rs` | 356 | ◆ | Replace `Framed` with `BufStream`; rework `poll_*` to compio async fn |
| `codec.rs` | 98 | ● | Encoder/Decoder logic lifts; framing reuses our BufStream |
| `prepare.rs` | 267 | ✓ | H1 solved — type info queries, describe-based OID resolution |
| `query.rs` | 382 | ● | `encode()` drop-in; response iteration rewritten |
| `simple_query.rs` | 115 | ● | Same pattern as query.rs |
| `row.rs` | 280 | ✓ | Pure deserialization |
| `statement.rs` | 122 | ✓ | Pure data holder |
| `portal.rs` | 50 | ✓ | Pure data holder |
| `transaction.rs` | 348 | ● | Method shape preserved; async bodies re-plumbed |
| `transaction_builder.rs` | 140 | ✓ | Pure data builder |
| `cancel_token.rs` | 67 | ✓ | Pure |
| `cancel_query.rs` | 52 | ● | TcpStream swap |
| `cancel_query_raw.rs` | 31 | ● | Wire protocol only |
| `connect.rs` | 229 | ● | Host resolution + failover; swap `tokio::net::lookup_host` |
| `connect_raw.rs` | 368 | ● | SASL/MD5/password auth state machine; wire to BufStream |
| `connect_socket.rs` | 73 | ● | Socket setup + keepalive (merge our `socket2::SockRef` keepalive code) |
| `connect_tls.rs` | 60 | ● | TLS upgrade state machine |
| `socket.rs` | 75 | ● | TCP/UnixStream wrapper; swap to compio |
| `tls.rs` | 164 | ● | TlsConnect trait; keep abstraction; NoTls drop-in |
| `maybe_tls_stream.rs` | 71 | ● | Enum wrapper |
| `keepalive.rs` | 38 | ✓ | Pure config |
| `copy_in.rs` | 225 | ● | Protocol state machine intact; mpsc usage preserved |
| `copy_out.rs` | 57 | ✓ | Stream wrapper |
| `binary_copy.rs` | 272 | ✓ | Pure encoding/decoding |
| `generic_client.rs` | 376 | ● | Trait shape preserved; async fn impls re-plumbed |
| `error/*.rs` | ~350 | ✓ | Pure error types |
| `bind.rs` | 38 | ✓ | Helper |
| `to_statement.rs` | 59 | ✓ | Trait impl |
| `types.rs` | 6 | ✓ | Re-export |
| **(new)** `pool.rs` | ~700 | (ours) | Port our HikariCP pool; manage `Client`+`Connection` pairs |

**Totals:** ~3,500 LOC copy-verbatim, ~3,300 LOC thin rewrite, ~400 LOC significant rewrite (connection.rs), ~700 LOC new (pool).

---

## Phase plan

Each phase ends with `cargo build -p compio-postgres` clean.

### Phase 1 — Scaffold + pure-logic drop-ins
**Files:**
- Cargo.toml + src/lib.rs skeleton
- Workspace registration
- Copy verbatim (✓) files: config/, row.rs, statement.rs, portal.rs, cancel_token.rs, keepalive.rs, transaction_builder.rs, binary_copy.rs, bind.rs, to_statement.rs, types.rs, error/
- Rename `tokio-postgres` crate dep references to `compio-postgres` in imports
- Disable `runtime` feature gates in config/
- **Does NOT yet compile fully** — stubs for I/O types until Phase 2

**Parallelizable:** yes, single agent touches many files but they're independent reads/writes.

**Deliverable:** `cargo build -p compio-postgres` succeeds with only the pure-logic files.

### Phase 2 — I/O foundation
**Files:**
- `codec.rs` — message framing. Integrate our `BufStream` (port from `crates/pg/src/stream.rs` including the length-cap fix).
- `socket.rs` + `maybe_tls_stream.rs` — TCP/UDS wrapper.
- `tls.rs` — trait abstractions.
- `connect_tls.rs` — TLS upgrade state machine.
- `connect_socket.rs` — merge our socket2 keepalive code.

**Sequential** — architecture-specific.

**Deliverable:** raw socket+TLS+codec compiles; can read/write framed messages end-to-end against a local PG (no auth yet).

### Phase 3 — Connection loop + auth
**Files:**
- `connection.rs` — **the big rewrite.** Replace `poll_*` with a compio async loop. Select over: (a) new request arriving on receiver, (b) next backend message from codec. Preserve FIFO demux of `VecDeque<Response>`. Wake-up dance uses std Future + custom waker or a small `select!` macro from `futures_util`.
- `connect_raw.rs` — startup + SASL/MD5/password handshake against the new codec.
- `connect.rs` — host resolution, failover, bring together socket + tls + raw.

**Sequential.**

**Deliverable:** `connect(config).await?` returns `(Client, Connection)`; spawning Connection on compio runtime works; `select 1` via ad-hoc Request succeeds against Docker PG.

### Phase 4 — Client API + queries
**Files:**
- `client.rs` — InnerClient, Client, type cache (CachedTypeInfo), request dispatch.
- `prepare.rs` — type info queries, OID → Type resolution. Drop-in with I/O glue.
- `query.rs` — Bind/Execute state machine; `RowStream` over response channel.
- `simple_query.rs` — same.
- `cancel_query.rs` + `cancel_query_raw.rs` — CancelRequest on a fresh socket.

**Partly parallel:** prepare/query/simple_query can be tackled in parallel after client.rs stable.

**Deliverable:** `client.query("SELECT $1::int4", &[&42]).await?` works with full type resolution.

### Phase 5 — Transactions, copy, listen/notify
**Files:**
- `transaction.rs` + savepoints.
- `copy_in.rs` + `copy_out.rs`.
- `generic_client.rs` — unify Client + Transaction API.
- LISTEN/NOTIFY: verify the async message path — NotificationResponse gets routed from `connection.rs` into a dedicated `mpsc::UnboundedReceiver<AsyncMessage>` on the Client.

**Parallel after transaction.rs.**

**Deliverable:** BEGIN/COMMIT/ROLLBACK, SAVEPOINT, COPY FROM STDIN, COPY TO STDOUT, NOTIFY/LISTEN all pass integration tests.

### Phase 6 — Pool + migration
**Files:**
- `pool.rs` — port our HikariCP pool to manage `Client`+`Connection` pairs. Each pool entry spawns its Connection task on checkout; housekeeper still evicts.
- Workspace migration: `crates/plugin-db/`, `crates/control/`, `crates/auth/` switch imports from `zeroship_pg` to `compio_postgres`.
- Delete old `crates/pg/` (or leave as deprecated shim for one release).
- End-to-end integration tests + bench comparison.

**Deliverable:** workspace builds, platform E2E tests pass, legacy `crates/pg/` removed.

---

## Risk register

1. **`connection.rs` rewrite is the biggest architectural risk.** The tokio-postgres version uses `poll_read`/`poll_write`/`poll_send` hand-cranked on `Framed`. Compio's natural style is `async fn` with `.await`. The key semantic to preserve is **pipelining with FIFO demux**: multiple requests can be in flight, responses must be dispatched to the originating Response sender in order.

   Approach: use `futures_util::select!` (or manual Future combinators) inside one big `async fn run(mut self) -> Result<...>` on Connection. On each loop iteration, race:
   - `self.receiver.next()` for a new Request
   - `self.read_next_message()` for an incoming backend message
   - (if Terminating) a clean shutdown

   Pending request writes are buffered into BufStream; flushed after each batch. Responses dequeue the front of `responses: VecDeque<Response>`.

2. **Async notifications (NOTIFY)** — must not be dropped. `connection.rs` routes `NotificationResponse` to a dedicated `mpsc::UnboundedSender<AsyncMessage>` stored on InnerClient. Client exposes `client.notifications() -> Notifications` (a Stream).

3. **CopyInReceiver pipelining** — `copy_in.rs` builds a Stream that multiplexes user-provided COPY data frames onto the wire. The existing tokio-postgres code uses `futures_channel::mpsc` exactly as we will; should port cleanly.

4. **TLS trait surface** — we need one compio-compatible impl. Port our existing `compio-tls::TlsStream<TcpStream>` as the MakeTlsConnect backend. Users wanting rustls can add it later.

5. **Type coverage expansion** — our current 11-type list gets replaced by postgres-types's full set. Downstream `plugin-db` must be re-audited for any assumptions about limited type support.

6. **Build time** — the crate will be large; check `cargo build -p compio-postgres --release` stays under a couple minutes.

---

## Success criteria

- `cargo test -p compio-postgres` passes (tokio-postgres's own tests, adapted).
- All 23 existing zeroship-pg integration tests pass against compio-postgres.
- Benchmark: same or better throughput than current zeroship-pg under load.
- Downstream crates (plugin-db, control, auth) compile with only import-path changes.
- Legacy `crates/pg/` can be removed.

---

## Out of scope for this port

- `postgres` crate (blocking shim) — not needed.
- `postgres-derive` / `postgres-derive-test` — proc-macro crates; users can depend on tokio-postgres' for now (macro crates are runtime-agnostic).
- `postgres-native-tls` / `postgres-openssl` — we use `compio-tls` which is native-tls-backed.
