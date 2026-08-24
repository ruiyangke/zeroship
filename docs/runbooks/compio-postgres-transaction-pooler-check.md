# Verify compio-postgres through a transaction-mode pooler

## Why

The suite normally runs against a direct PostgreSQL server, where the driver
owns the backend for the whole connection. A transaction-mode pooler does not
give it that: the backend can be handed to another client between statements,
and anything the driver expects to arrive "later" - a second `ReadyForQuery`, a
response to a barrier nobody asked for - can be discarded with the backend.

Measured 2026-08-24, this is not hypothetical. Simple-protocol `CopyFail`
already earns its own `ReadyForQuery`; the recovery path also sent a `Sync`,
which earned a SECOND one that the driver had to invent a response slot for. A
pooler releasing the backend after the first terminator strands that slot and
the next query waits forever. The fix sends `CopyFail` alone for simple
protocol and keeps `CopyFail + Sync` for extended.

## Prerequisites

- Docker, and the usual test server (5455 below).
- A free host port for the pooler. 6548 below; check with `docker ps`.

## Steps

The pooler runs in a container, so `DB_HOST` must be the PostgreSQL
**container's own IP**. A published port like `127.0.0.1:5455` is bound to the
host loopback and is NOT reachable from another container - neither by
`127.0.0.1` nor via the bridge gateway.

```bash
PGIP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' zs-cpg-review-5455)

docker run -d --name zs-cpg-pgb -p 127.0.0.1:6548:5432 \
  -e DB_HOST=$PGIP -e DB_PORT=5432 \
  -e DB_USER=postgres -e DB_PASSWORD=zeroship -e DB_NAME=zeroship \
  -e POOL_MODE=transaction -e MAX_CLIENT_CONN=50 -e DEFAULT_POOL_SIZE=1 \
  -e AUTH_TYPE=plain -e ADMIN_USERS=postgres \
  edoburu/pgbouncer:latest
```

`DEFAULT_POOL_SIZE=1` is deliberate: one backend behind many client
connections is what forces a handoff between statements. A larger pool can
hand each client its own backend and hide the very thing being measured.

The image listens on **5432** inside the container, not 6432. Publishing
`6548:6432` yields a container that starts, logs nothing wrong, and refuses
every connection.

Confirm the settings actually took, rather than trusting the env vars:

```bash
docker exec zs-cpg-pgb grep -E '^(pool_mode|default_pool_size|auth_type)' \
  /etc/pgbouncer/pgbouncer.ini
```

Expect `pool_mode = transaction` and `default_pool_size = 1`.

Run the suite through it:

```bash
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:6548/zeroship \
  cargo test -p compio-postgres --features test-utils -- --test-threads=1
```

Tear down when finished:

```bash
docker rm -f zs-cpg-pgb
```

## Reading the result

**Confirm the tests reached the pooler at all.** A suite that connected
somewhere else passes just as quietly:

```bash
docker logs zs-cpg-pgb 2>&1 | grep -c "login attempt"
```

Zero logins means the run never touched this pooler, whatever it printed.
`closing because: client unexpected eof` is normal - the driver drops sockets
without a `Terminate`.

**Expect fewer failures here than on a direct server, not more.** With one
backend, the FIRST test to leave the session wedged takes the pool with it, and
later tests fail early with `Closed` - or pass, because they never got far
enough to exercise anything. Measured on the redundant-`Sync` regression: 4
failures on a direct server, 2 through the pooler, with
`batch_copy_abort_settles_before_the_follow_up_query` among the direct failures
and NOT among the pooler ones. A pooler run is a way to reach protocol
behaviour a direct server cannot show; it is a worse instrument for counting
which tests are broken.

So triage a pooler failure by re-running the single test alone
(`-- --test-threads=1 <name>`), where pool contamination cannot reach it.

## Tests that cannot pass through a pooler

Anything that depends on session state surviving between statements: `LISTEN`
registrations, `SET` that a later statement reads, session-local temp tables,
prepared statements held across transactions, and replication connections. A
failure in one of those is the pooler being a pooler. Give a test that must
survive a handoff a DURABLE fixture, as
`batch_copy_abort_settles_before_the_follow_up_query` does - a temp table turns
the handoff into an unrelated "relation does not exist" and the test stops
measuring the protocol.
