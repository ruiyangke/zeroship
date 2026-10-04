# compio-postgres

A native, asynchronous PostgreSQL client for compio/io_uring - a hand-written
port of `tokio-postgres` 0.7.18 that runs no tokio reactor. Standalone and
publishable: it depends on nothing in this workspace.

It is a SUPERSET of tokio-postgres's client surface. On top of that crate's API
it adds a connection pool, logical replication with a pgoutput decoder, a
rustls transport, client-side timeout clocks, an opt-in prepared-statement cache, a
password-file and `pg_service.conf` reader, TLS key logging, a configurable
maximum message size, wire protocol 3.2 with negotiated fallback to 3.0, and
`is_dirty` / `process_id` / `transaction_status` / `query_events` /
`protocol_version`.

Two of those are worth knowing before comparing behaviour with libpq. This
driver requests protocol **3.2** by default where libpq 18 defaults to 3.0, so
it gets 3.2's longer cancel key wherever the server offers it and negotiates
down everywhere else. And `protocol_version` reports what the session SETTLED
on, which is the only way to find out: PostgreSQL exposes no server-side view
of it, which is why libpq keeps its own and `psql`'s `\conninfo` prints that.

It reads NO environment variables. libpq defaults most parameters from the
environment; a published library takes resolved options from its caller
instead, and the workspace enforces that (see the "The environment" section of
`Config`'s documentation). The practical consequence is that `passfile` and
`servicefile` want paths rather than finding their own.

## Important files

| File | What it owns |
| --- | --- |
| `src/connection.rs` | The two run loops. `run_multiplexed` reads and writes concurrently on split socket halves; `run_serialized` is the fallback for a transport that refuses to split. |
| `src/buf_stream.rs` | Buffered IO and the `SplitStream` trait that decides which loop runs. |
| `src/tls_sansio.rs` | Drives rustls directly against a compio socket, so the socket stays ours and TLS can split. The crate's only `unsafe`. |
| `src/tls_rustls.rs` | `MakeRustlsConnect`: builds a rustls config from `sslmode`/`sslcert`/`sslcrl`/... and attests to what it will honour. |
| `src/pool.rs` | Pool, waiters, idle management, lifecycle hooks, `max_lifetime`. |
| `src/replication.rs` | Replication protocol and the pgoutput decoder (stateful: a stream chunk adds an xid prefix nothing in the bytes advertises). |
| `src/release.rs` | Synchronous, drop-safe socket release, so a dropped `Client` frees its backend promptly. Also where a TLS session's `close_notify` is serialized and sent, because the alert has to leave before the socket is shut down. Both guards route through one `shutdown()` for that reason: the alert lived in `Drop` alone until 2026-08-25, and the six sites that call `shutdown()` without dropping ended every TLS session with a reset. |
| `src/config.rs` | Every libpq connection parameter: implemented, or REFUSED BY NAME. Never accepted and ignored. |
| `src/passfile.rs` | `~/.pgpass` lookup: the file consulted when no password is set. Its matching rules were derived by probing libpq, not read off the format description - see the note on `match_field`. |
| `src/service.rs` | `pg_service.conf` lookup: a named section supplying connection parameters. Explicitly given parameters win over the service's, in any order. Its whitespace, comment, header and duplicate-key rules were probed out of libpq rather than read off the format description, which describes none of them - `docs/runbooks/compio-postgres-libpq-parameter-probing.md`. |

## Pool ownership and lifecycle

`Pool::acquire()` returns an owned `PoolConnection`. The lease keeps the pool
alive across callbacks and can outlive the handle that acquired it. Ordinary
queries and transactions use this same lease type.

`Pool` is a cloneable handle over private `Rc` state. Clones share capacity,
the FIFO queue, acquisition settings, metrics, and shutdown. Handles and leases
stay on their compio thread. `Pool::metrics()` exposes the shared counters.

The FIFO queue has no tenant identity. Long-held transaction leases can occupy
capacity needed by ordinary queries; tenant admission policy belongs above the
pool.

```text
pool -> acquire -> lease -> drop -> cleanup if needed -> reuse
                     |
                     +---- discard -> close socket -> release capacity
```

`PoolConfig::acquire_timeout` bounds each checkout: waiting for a connection,
opening one on demand, validation, and hooks. `PoolConfig::warm_up_timeout`
bounds constructing the pool: its warm-up connections, their retries, and their
`after_connect` hooks. They are separate budgets because the work differs - a
checkout makes at most one handshake, warm-up several with retries - so a short
checkout limit does not cut startup short. Use `Error::is_pool_timeout()` and
`Error::is_pool_closed()` to classify pool failures, and
`Error::pool_timeout_budget()` to see which budget expired; command deadlines
and transport failures retain their own meanings.

`discard()` consumes the lease, closes its physical connection, and
releases capacity without running a reuse hook. Use it when cleanup cannot be
confirmed. Ordinary return rolls back an unfinished transaction before reuse;
it preserves session settings and prepared statements.

`Pool::close()` rejects new acquisitions and wakes pending acquisitions even
inside connection setup or hooks. Their next poll cancels the pending work and
drops its candidate. Close waits for checked-out leases to return; it does not
depend on acquisition futures being polled again. Checked-out clients remain
usable during that drain.

Dropping the last pool handle or lease closes idle connections and cancels the
housekeeper. Closing any handle explicitly starts shutdown for every clone.

These ownership and lifecycle boundaries follow the patterns described by
[SQLx's pool](https://docs.rs/sqlx/latest/sqlx/struct.Pool.html) and
[connection leases](https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolConnection.html).
The executor and socket lifecycle remain compio-native.

## Running the tests

Everything needs a live server; nothing skips. A missing database is a FAILED
run, not a green one - see the header of `tests/support/mod.rs` for why.

The servers are the suite's own. `compio_postgres_testkit::server` starts the
PostgreSQL server every suite and live bench dials, in Docker, the first time a
test process of the worktree asks for it; every other process joins it, and
its watchdog removes it once no process has held it for its idle grace. Docker
is the one prerequisite, and no environment variable or file points the suite
at any other server. `tests/integration/fixture_server.rs` holds the suite to
that: the server it dials must be a container this checkout's fixture started.

```bash
# The ordinary suite. nextest runs each test in a process of its own against
# the one shared server; every test scopes its objects to itself.
cargo nextest run -p compio-postgres

# The rustls transport, its certificate verifiers and the live TLS suite
# (negotiation, verification modes, CRLs, client certificates, channel
# binding, direct SSL), which exist only with the `tls` feature.
cargo nextest run -p compio-postgres --features tls
```

### The suite MODES

The same test bodies, run against a different shape. Each exists because a
whole class of behaviour was otherwise measured on exactly one configuration.

```bash
# Over TLS, against the TLS fixture's `tls` server.
cargo nextest run -p compio-postgres --features suite-over-tls

# With the implicit prepared-statement cache on (it is OFF by default, so its
# eviction and stale-plan retry are otherwise barely exercised).
cargo nextest run -p compio-postgres --features suite-with-statement-cache

# Against PostgreSQL 18 instead of 16; with suite-over-tls as well, over TLS
# against the TLS fixture's PostgreSQL 18 server.
cargo nextest run -p compio-postgres --features suite-on-postgres-18
```

`suite-over-tls` deliberately excludes `serialized_loop.rs` and
`prefer_attestation_fallback.rs`: those files choose their own transport, so
forcing them onto the encrypted server would measure the mode rather than the
claim.

### The fixtures

`compio_postgres_testkit::tls` starts the six servers the TLS suites dial -
`tls`, `plain`, `mismatch`, `sslonly`, `clientcert` and the PostgreSQL 18
`directtls` - each the control for one claim, and checks each with libpq as it
boots. The CA, the server and client certificates, the CRL and the encrypted
client key are generated when the fixture's image is built and never leave it
except as the client-side copies the fixture takes into this worktree's
`target`; nothing is committed and no setup step runs by hand.

`compio_postgres_testkit::unix` starts the server
`tests/integration/unix_socket_live.rs` reaches through a socket file on this
host. The socket path has to be SHORT: `sun_path` is 108 bytes and the server
appends `/.s.PGSQL.<port>`, so the fixture hands out a short link to the
directory its container shares with the host, and refuses a path that would
not fit rather than serving a socket nothing can reach.

The PostgreSQL 18 run is gated like the other modes: CI and `verify.sh` run
`suite-on-postgres-18`. `docs/runbooks/compio-postgres-cross-version-check.md`
says why a second version matters - a protocol claim measured on one version is
a claim about that version, and the versions differ in what some transactions
put on the replication stream - and records the feedback-timeout trap.

One more shape has a runbook rather than a script, because it answers a
question rather than gates a change:

- `docs/runbooks/compio-postgres-tls-teardown-noise.md` - what this driver
  leaves in a server log when a TLS session ends. Read it before touching
  `release.rs` or `tls_sansio.rs`: the teardown has three separate paths, and
  after fixing each one a single-connection probe read clean while the others
  were still broken. The expected whole-suite figure is about ten lines,
  essentially all from `connection_churn` abandoning sessions on purpose.

## The oracles

Bugs in a port hide in the places where it is NOT a transcription, so the suite
leans on things that can disagree with it:

- `tests/integration/differential_tokio.rs` runs `tokio-postgres` beside this crate against
  the same server and compares observable results. tokio is a
  `[dev-dependencies]` exemption to the workspace zero-tokio rule (AGENTS.md
  records the decision). Two divergences are DELIBERATE and pinned in that file
  rather than left to drift.

  The oracle is pinned to an EXACT version, because an oracle is only an oracle
  for the version it is. A `"0.7"` requirement resolved to 0.7.17 while this
  crate tracks 0.7.18 - whose sole source change is a panic fix this crate
  already carries - so the suite was comparing a fixed driver against an
  unfixed one, and a difference would have read as OUR defect. Move the pin
  when the port moves, never by resolution.
- `tests/integration/frame_fuzz.rs` and `tests/integration/pgoutput_fuzz.rs` feed seeded corpora to the
  backend-frame and replication decoders. Both assert termination, no panic,
  and a decoder that is still usable afterwards - plus FLOORS on what the
  corpus actually reached, because a fuzzer rejected before it enters the
  parser passes those assertions perfectly while testing nothing.
- `tests/integration/libpq_parameter_parity.rs` rules on every libpq connection parameter:
  implemented, or refused with the key NAMED in the error's source chain. The
  state it exists to prevent is a parameter accepted and silently ignored,
  which is indistinguishable from support at the call site.
- `tests/integration/connection_churn.rs` opens ~90 connections including ones that end
  badly, and requires both `live_connections()` and the server's backend count
  back to baseline.

## Before you push

```bash
cargo clippy --workspace --all-targets --all-features
```

Note it does NOT deny `unused_imports`; a plain `cargo build --tests` is the
only thing that reports those.
