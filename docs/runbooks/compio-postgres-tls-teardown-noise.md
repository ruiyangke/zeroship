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

The TLS fixtures. Do NOT run `tls_live_setup.sh` if another checkout is using
them - it regenerates the CA into shared containers.

```bash
docker ps --format '{{.Names}}' | grep compio-pg
```

## Whole-suite measurement

Count before, run, count after. `suite-over-tls` drives the entire suite
through the encrypted server.

```bash
for c in compio-pg-tls-test compio-pg-sslonly-test compio-pg-directtls-test \
         compio-pg-clientcert-test compio-pg-plain-test; do
  echo "$c $(docker logs $c 2>&1 | wc -l)"
done > /tmp/tlslog-before.txt

cargo test -p compio-postgres --features suite-over-tls --no-fail-fast -- --test-threads=1

while read -r c before; do
  new=$(docker logs "$c" 2>&1 | tail -n +$((before+1)))
  printf "%-30s could-not-receive=%s\n" "$c" \
    "$(printf '%s\n' "$new" | grep -c 'could not receive data from client')"
done < /tmp/tlslog-before.txt
```

MEASURED 2026-08-25 at suite size 1721: **11** on `compio-pg-tls-test`, and
**0** on the other four servers.

## Reading the number

**The expected value is not zero.** `connection_churn` runs
`BAD_CONNECTION_ITERATIONS` sessions that are abandoned mid-query on purpose,
so a write is still in flight when the release runs and `take_close_notify`
DECLINES - emitting the alert there would place a later TLS sequence number
ahead of a record already handed to the socket. Skipping is the correct choice,
so those lines are the design working.

Attribute rather than assume. Re-run binaries alone and count:

```bash
for t in connection_churn cancel_request backend_termination socket_release \
         query_backpressure connect_failure_diagnosis; do
  B=$(docker logs compio-pg-tls-test 2>&1 | wc -l)
  cargo test -p compio-postgres --features suite-over-tls --test $t -- --test-threads=1 >/dev/null 2>&1
  sleep 2
  printf "%-28s -> %s\n" "$t" \
    "$(docker logs compio-pg-tls-test 2>&1 | tail -n +$((B+1)) | grep -c 'could not receive data from client')"
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
B=$(docker logs compio-pg-tls-test 2>&1 | wc -l)
# ... run one connection, one case ...
docker logs compio-pg-tls-test 2>&1 | tail -n +$((B+1))
```

The controls that make a clean result mean something:

- The SAME drop against the plaintext server (5448) must log nothing. If it
  logs, the problem is not TLS and not this runbook.
- libpq over TLS must log nothing:
  `docker exec compio-pg-tls-test psql "host=127.0.0.1 port=5432 user=postgres dbname=postgres sslmode=require" -tAc "SELECT 1"`

Without those two, "zero lines" is also what a probe that never connected
prints.

## Where the automated coverage lives

`tests/hostile_peer.rs`, three tests, each driving a scripted in-process TLS
server that waits for the alert and asserts `peer_has_closed()`:
`dropping_a_tls_client_sends_close_notify`,
`dropping_an_unrun_tls_connection_sends_close_notify`, and
`a_command_timeout_closes_the_tls_session_cleanly`. They need no Docker, which
is why they and not this runbook are what gates a change.
