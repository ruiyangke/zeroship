# Measure what compio-postgres leaves in a PostgreSQL log on TLS teardown

## Why

A TLS session that ends without a `close_notify` makes PostgreSQL log

```
LOG:  could not receive data from client: Connection reset by peer
```

It is not data loss - the client is closing either way - but it reads exactly
like a driver defect to whoever is holding the server log, and it was nearly
diagnosed as one. The driver sends the alert from `src/release.rs`; this is how
to check that it still does.

Read this before changing anything in `release.rs` or `tls_sansio.rs`.

## The trap this runbook exists for

**One connection is not enough.** The teardown had THREE separate paths - the
client release, the connection-side guard, and `ConnectionRelease::shutdown()`
called directly by six sites - and after fixing each one a single-connection
probe read CLEAN while the other paths were still broken. Two of the three were
found only by looking at the code, not by measuring.

So measure at both scales, and in this order: the whole suite first, because it
is the one that can surprise you.

## Prerequisites

The TLS fixture's servers, which `compio_postgres_testkit::tls` boots on first
use and removes once no test process has held them for their idle grace. Its
containers carry the lease directory they serve as a label, so they are found
by role rather than by name:

```bash
tls_containers() {
  docker ps --filter label=zeroship.testkit.dir \
    --format '{{.ID}} {{.Label "zeroship.testkit.dir"}}' | grep '/compio-postgres-tls'
}
# The `tls` server itself: its lease directory has no role suffix.
tls=$(tls_containers | grep -E '/compio-postgres-tls-[0-9a-f]+$' | cut -d' ' -f1)
```

## Whole-suite measurement

Start with no TLS fixture container running (`tls_containers` prints nothing
once the previous run's idle grace has passed), so the servers the run boots
log nothing but this run. `suite-over-tls` drives the entire suite through the
encrypted server. Count within the idle grace after the run ends:

```bash
cargo nextest run -p compio-postgres --features suite-over-tls

tls_containers | while read -r id dir; do
  printf "%-60s could-not-receive=%s\n" "${dir##*/}" \
    "$(docker logs "$id" 2>&1 | grep -c 'could not receive data from client')"
done
```

## Reading the number

**The expected value is not zero.** `connection_churn` runs
`BAD_CONNECTION_ITERATIONS` sessions that are abandoned mid-query on purpose,
so a write is still in flight when the release runs and `take_close_notify`
DECLINES - emitting the alert there would place a later TLS sequence number
ahead of a record already handed to the socket. Skipping is the correct choice,
so those lines are the design working.

Attribute rather than assume. Re-run each suite alone, while the servers are
still up, and count:

```bash
for t in connection_churn cancel_request backend_termination socket_release \
         query_backpressure connect_failure_diagnosis; do
  case $t in
    socket_release) filter='binary(socket_release)' ;;
    *) filter="test(/^integration::$t::/)" ;;
  esac
  B=$(docker logs "$tls" 2>&1 | wc -l)
  cargo nextest run -p compio-postgres --features suite-over-tls -E "$filter" >/dev/null 2>&1
  sleep 2
  printf "%-28s -> %s\n" "$t" \
    "$(docker logs "$tls" 2>&1 | tail -n +$((B+1)) | grep -c 'could not receive data from client')"
done
```

MEASURED 2026-08-25: `connection_churn` 7, every other binary above 0. Its
count varies run to run (7 and 9 seen), because whether the write has drained
by the time the client drops is a race.

So the claim to hold is "about ten, essentially all from `connection_churn`".
A count that grows in a DIFFERENT binary is the finding, and it means a
teardown path nobody wired the alert into.

## Single-connection check

Useful for confirming a specific path, useless for proving completeness. Drop
the CLIENT and drop an unrun CONNECTION separately - they are different guards
and were broken independently:

```bash
B=$(docker logs "$tls" 2>&1 | wc -l)
# ... run one connection, one case ...
docker logs "$tls" 2>&1 | tail -n +$((B+1))
```

The controls that make a clean result mean something:

- The SAME drop against the fixture's `plain` server must log nothing. If it
  logs, the problem is not TLS and not this runbook.
- libpq over TLS must log nothing:
  `docker exec "$tls" psql "host=127.0.0.1 port=5432 user=postgres dbname=postgres sslmode=require" -tAc "SELECT 1"`

Without those two, "zero lines" is also what a probe that never connected
prints.

## Where the automated coverage lives

`libs/compio-postgres/tests/integration/hostile_peer.rs`, three tests, each driving a scripted in-process TLS
server that waits for the alert and asserts `peer_has_closed()`:
`dropping_a_tls_client_sends_close_notify`,
`dropping_an_unrun_tls_connection_sends_close_notify`, and
`a_command_timeout_closes_the_tls_session_cleanly`. They need no Docker, which
is why they and not this runbook are what gates a change.

They are `#[cfg(feature = "tls")]`, so `cargo test --workspace` does NOT build
them - the crate's default feature set is empty. CHECKED 2026-08-25: CI runs
`cargo test -p compio-postgres --features tls` as its own step (`ci.yml`), and
that step builds this target, so the three do gate every push. If that step is
ever dropped, these tests stop running everywhere except by hand, and the
symptom is silence rather than a failure.
