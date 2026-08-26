# compio-postgres

A native, asynchronous PostgreSQL client for compio/io_uring - a hand-written
port of `tokio-postgres` 0.7.18 that runs no tokio reactor. Standalone and
publishable: it depends on nothing in this workspace.

It is a SUPERSET of tokio-postgres's client surface. On top of that crate's API
it adds a connection pool, logical replication with a pgoutput decoder, a
rustls transport, four timeout clocks, an opt-in prepared-statement cache, a
password-file and `pg_service.conf` reader, TLS key logging, a configurable
maximum message size, and
`is_dirty` / `process_id` / `transaction_status` / `query_events`.

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

## Running the tests

Everything needs a live server; nothing skips. A missing database is a FAILED
run, not a green one - see the header of `tests/common/mod.rs` for why.

```bash
# The ordinary suite.
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:5455/zeroship \
  cargo test -p compio-postgres -- --test-threads=1
```

`--test-threads=1` is not superstition: several tests measure server-visible
state (backend counts, replication slots, prepared statements) that concurrent
tests would perturb.

### The suite MODES

The same test bodies, run against a different shape. Each exists because a
whole class of behaviour was otherwise measured on exactly one configuration.

```bash
# Over TLS. Needs tests/tls_live_setup.sh first (see below).
cargo test -p compio-postgres --features suite-over-tls -- --test-threads=1

# With the implicit prepared-statement cache on (it is OFF by default, so its
# eviction and stale-plan retry are otherwise barely exercised).
PG_TEST_URL=... cargo test -p compio-postgres --features suite-with-statement-cache -- --test-threads=1

# The TLS-specific suite: negotiation, verification modes, CRLs, client
# certificates, channel binding, direct SSL. Not built without the feature.
cargo test -p compio-postgres --features tls,live-tls-tests --test tls_live -- --test-threads=1
```

`suite-over-tls` deliberately excludes `serialized_loop.rs` and
`prefer_attestation_fallback.rs`: those files choose their own transport, so
forcing them onto the encrypted server would measure the mode rather than the
claim.

### The fixtures

```bash
# Six PostgreSQL servers for the TLS suites, ports 5447-5452.
libs/compio-postgres/tests/tls_live_setup.sh
libs/compio-postgres/tests/tls_live_setup.sh --down
```

ONE CHECKOUT AT A TIME unless you pass different ports. The script generates a
fresh CA into the tree it runs from and mounts it into containers with FIXED
names, so running it from a second worktree makes every TLS connection from the
first fail `InvalidCertificate(BadSignature)` - which reads exactly like a
driver defect. The script's own header says this at more length.

```bash
# A server whose Unix socket is reachable, for tests/unix_socket_live.rs.
libs/compio-postgres/tests/unix_socket_setup.sh
libs/compio-postgres/tests/unix_socket_setup.sh --down

cargo test -p compio-postgres --features live-unix-socket --test unix_socket_live -- --test-threads=1
```

The socket directory has to be SHORT. `sun_path` is 108 bytes and the server
appends `/.s.PGSQL.<port>`, so a fixture under an ordinary scratch path is
unreachable - which is what the previous one was, 110 bytes deep, leaving
`Host::Unix` reaching a real server asserted nowhere. The script refuses a
directory that would not fit rather than creating another unusable fixture.

Two more shapes have runbooks rather than scripts, because they answer a
question rather than gate a change:

- `docs/runbooks/compio-postgres-transaction-pooler-check.md` - the suite
  through PgBouncer in transaction mode. Read the `IGNORE_STARTUP_PARAMETERS`
  note first, or you will measure pgbouncer refusing this suite's
  schema-isolation `options` rather than measuring the driver.
- `docs/runbooks/compio-postgres-cross-version-check.md` - a second server
  version. The runbook exists because a protocol claim measured on one version
  is a claim about that version: 16.14 streams a rolled-back transaction and
  sends `StreamAbort`, while 18.4 sends nothing at all. Both are green as of
  2026-08-25, same totals on each; the runbook records the figure and says to
  re-measure rather than trust it.

## The oracles

Bugs in a port hide in the places where it is NOT a transcription, so the suite
leans on things that can disagree with it:

- `tests/differential_tokio.rs` runs `tokio-postgres` beside this crate against
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
- `tests/frame_fuzz.rs` and `tests/pgoutput_fuzz.rs` feed seeded corpora to the
  backend-frame and replication decoders. Both assert termination, no panic,
  and a decoder that is still usable afterwards - plus FLOORS on what the
  corpus actually reached, because a fuzzer rejected before it enters the
  parser passes those assertions perfectly while testing nothing.
- `tests/libpq_parameter_parity.rs` rules on every libpq connection parameter:
  implemented, or refused with the key NAMED in the error's source chain. The
  state it exists to prevent is a parameter accepted and silently ignored,
  which is indistinguishable from support at the call site.
- `tests/connection_churn.rs` opens ~90 connections including ones that end
  badly, and requires both `live_connections()` and the server's backend count
  back to baseline.

## Before you push

```bash
./tests/clippy_gate.sh    # the workspace lint authority; a bare cargo clippy
                          # stops at the first failing crate and prints what a
                          # clean crate prints
```

Note it does NOT deny `unused_imports`; a plain `cargo build --tests` is the
only thing that reports those.
