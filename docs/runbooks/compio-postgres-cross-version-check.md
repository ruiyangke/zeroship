# Verify compio-postgres against a second PostgreSQL version

## Why

The suite normally runs against one server. Every protocol claim it makes -
message layouts, streaming framing, two-phase frames, abort behaviour - was
measured on THAT server, so a test can pin behaviour that only one version
has and nothing will say so.

Measured 2026-08-24, this is not hypothetical: PostgreSQL 16.14 streams a
rolled-back transaction and then sends `StreamAbort`; 18.4 sends **nothing at
all** for the same workload and simply ends the stream. Two tests required the
abort and failed on 18.4 while passing on 16.14.

## Prerequisites

- Docker.
- The usual test server on 5455 (or whatever `PG_TEST_URL` names).
- A free port for the second server. 5459 below; check with `docker ps`.

Do **not** reuse another project's container (there is a `postgres:18` on 5434
belonging to `zero-migrate`). Concurrent suites on one server contend for
replication slots and hang, and you would also be interfering with someone
else's run.

## Steps

Start a dedicated server. The three settings are load-bearing: logical
decoding for replication, prepared transactions for the two-phase tests, and
enough slots that a run does not exhaust them.

```bash
docker run -d --name zs-cpg-pg18-5459 -p 127.0.0.1:5459:5432 \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_DB=zeroship \
  postgres:18 postgres \
    -c wal_level=logical \
    -c max_prepared_transactions=10 \
    -c max_replication_slots=20
```

Wait for it to accept **TCP**, not just to exist:

```bash
for i in $(seq 1 40); do
  docker exec zs-cpg-pg18-5459 \
    psql "postgres://postgres:zeroship@127.0.0.1:5432/zeroship" -tAc "SELECT 1" \
    >/dev/null 2>&1 && { echo "ready"; break; }
  sleep 2
done
```

`pg_isready` is NOT sufficient here: it checks the unix socket and reports
ready while the TCP listener is still refusing connections.

Confirm the settings actually took, rather than assuming the flags applied:

```bash
docker exec zs-cpg-pg18-5459 \
  psql "postgres://postgres:zeroship@127.0.0.1:5432/zeroship" -tAc \
  "SELECT current_setting('server_version'), current_setting('wal_level'),
          current_setting('max_prepared_transactions')"
```

Run the suite against it:

```bash
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:5459/zeroship \
  cargo test -p compio-postgres -- --test-threads=1
```

Expected: the same pass count as the primary server, 0 failed. Anything else
is either a real version difference or a test that pinned one version's
behaviour - triage below.

MEASURED 2026-08-25: **1738 passed, 0 failed on 18.4**, the same totals as
16.14 on 5455 in the same session. Re-measure rather than carrying that number
forward - it moves whenever the suite grows, and the claim worth holding is
"the same as the primary server on the same day", not any particular figure.
It read 1720 earlier the same day, before three commits added 18 tests.

Wait for the run to EXIT, not for its output to go quiet.
`pgoutput_subtransactions` streams for minutes on 18 without printing, so a
"has the log stopped growing" check calls the run finished at roughly half the
binaries - 39 of 78 - and prints a clean 0 failures for the half it saw. Poll
`pgrep -f 'cargo[ ]test -p compio-postgres'` instead, and confirm the binary
count as well as the failure count.

Tear down when finished:

```bash
docker rm -f zs-cpg-pg18-5459
```

## Triaging a failure

1. **Read the server log first.** `docker logs <container> | grep -iE
   "ERROR|FATAL"`. An empty result means the server did not refuse anything and
   the difference is in what it CHOSE to send.
2. **Get a second instrument.** Reproduce with
   `pg_logical_slot_peek_binary_changes(...)`, which decodes without a
   walsender. If the walsender and the peek agree, it is behaviour; if they
   disagree, suspect the test harness.
3. **Vary the size.** A streaming difference can be a spill threshold rather
   than a protocol change. 4000 rows AND 40000 rows behaving the same rules
   that out.
4. **Run the control.** Does the same workload work when it COMMITS? If yes,
   streaming itself is fine and only the abort path differs.
5. **Never reuse a slot between experiments.** A slot positioned before two
   transactions decodes both, so a later commit will masquerade as the earlier
   abort. Fresh slot per measurement.

## What to do about a genuine version difference

Do not pin the test to one version, and do not delete it. Find the invariant
that holds on both and assert THAT, keeping the richer check conditional. For
the abort case: every version guarantees an aborted transaction is never
reported as committed, so that is the assertion; the abort message's shape is
still checked whenever a server sends one.
