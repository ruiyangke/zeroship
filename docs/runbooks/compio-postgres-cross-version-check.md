# Verify compio-postgres against a second PostgreSQL version

## Why

The suite normally runs against one server. Every protocol claim it makes -
message layouts, streaming framing, two-phase frames, abort behaviour - was
measured on THAT server, so a test can pin behaviour that only one version
has and nothing will say so.

Measured again 2026-08-26, this is not hypothetical: PostgreSQL 16.14 streams a
rolled-back transaction and then sends `StreamAbort`; 18.4 sends **no pgoutput
messages** for the same workload and leaves the replication stream open. A
fresh-slot `pg_logical_slot_peek_binary_changes` probe agreed at both 4000 and
40000 rows. The earlier claim that 18.4 "simply ends the stream" was wrong: the
driver failed to answer `PrimaryKeepalive.reply_requested`, PostgreSQL killed
the idle walsender after exactly 60 seconds, and the test folded that transport
error into an empty result. The regression now runs the walsender with a
one-second feedback deadline and observes for three seconds, so healthy silence
and a dead stream cannot print the same result.

## Prerequisites

- Docker.
- The usual test server on 5455 (or whatever `PG_TEST_URL` names).
- A free port for the second server. 5459 below; check with `docker ps`.

Do **not** reuse another project's container (there is a `postgres:18` on 5434
belonging to `zero-migrate`). Concurrent suites on one server contend for
replication slots and hang, and you would also be interfering with someone
else's run.

## `psql --version` DOES NOT NAME THE LIBPQ THAT CONNECTS

When the thing under test is CLIENT behaviour - a connection parameter's
default, unit, or refusal - the oracle is the libpq library, and `psql` is only
the program that calls it. Those two carry SEPARATE versions, and in the
`zs-cpg-review-5455` image they disagree:

```console
$ docker exec zs-cpg-review-5455 psql --version
psql (PostgreSQL) 16.14 (Debian 16.14-1.pgdg13+1)
$ docker exec zs-cpg-review-5455 dpkg -l | grep libpq5
ii  libpq5:amd64  18.4-1.pgdg13+1  amd64  PostgreSQL C client library
```

So every "measured against libpq 16.14" reading taken through that container
was taken against **libpq 18.4**. This is not hypothetical: it put a wrong
comment in `config.rs` claiming `connect_timeout=1` "is honoured as one second,
on libpq 16.15 AND 18.4", concluding "there is no floor to match". Both
readings were 18.4. PostgreSQL 16's `connectDBComplete` does hold the floor -
`if (timeout < 2) timeout = 2;`, `fe-connect.c:2439` on `REL_16_STABLE`, with
the comment "insist on at least two seconds" - and 18 removed it. Re-measured
2026-08-26 through that container: 1101 / 2104 ms for `connect_timeout=1` / `2`
against the blackhole `192.0.2.1`, both ending in "timeout expired", which is
18.4's behaviour and not 16's.

Check the LIBRARY before attributing a client reading to a version:

```bash
docker exec <container> dpkg -l | grep libpq5      # the version that matters
docker exec <container> psql --version             # only the caller
```

To measure a specific libpq, run a container whose `libpq5` is that version and
verify it with the first command - do not infer it from the image tag or from
the server's `SELECT version()`, neither of which constrains the client library.

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

MEASURED 2026-08-26: **79 binaries, 1779 passed, 0 failed on 18.4**, the same
totals the primary server on 5455 reported the same day. Re-measure rather than
carrying that number forward - it moves whenever the suite grows, and the claim
worth holding is "the same as the primary server on the same day", not any
particular figure. It read 1738 on 2026-08-25 and 1720 earlier that day, before
three commits added 18 tests.

THE LAYOUT CHANGED LATER THAT DAY, so read the figures above as the old shape.
Folding the test files into one binary took the crate from 79 test targets to 5
and the reported count from 1779 to 895 WITHOUT changing a case: `common`'s 13
tests had been re-executed in 70 separate binaries. Compare like with like.

RE-MEASURED 2026-08-26 after that consolidation: **5 binaries, 895 passed,
0 failed on 18.4**, again the same totals as the primary server the same day.
The SUMMED in-test time barely moved, 270.6s before and 268.8s after - folding
the files saves linking and disk, NOT execution, because the same tests do the
same I/O either way. Do not expect this run to get faster.

EVERY FIGURE ABOVE IS A DEFAULT-FEATURES RUN, and this crate declares
`default = []`. Such a run compiles no `tls`-gated test at all, so it says
nothing about the TLS surface on either version. Measured 2026-08-26 with both
live fixture sets generated (`tls_live_setup.sh` and `unix_socket_setup.sh`):

## DO NOT CROSS VERSIONS WITH `--all-features`. IT IGNORES `PG_TEST_URL`.

`--all-features` turns on `suite-over-tls`, and that feature does not merely
add TLS - it `#[cfg]`-replaces `common::test_url()` so the whole suite reads
its DSN from `tests/data/live/tls_live.conf` instead. `PG_TEST_URL` is then
DEAD, and the run measures the TLS fixture server no matter what you set.

This is not theoretical. Measured 2026-08-27 with `PG_TEST_URL` pointed at the
18.4 container on 5459, using the suite's own oracle line:

```text
--all-features                          -> server_version_num=160015  protocol=V3_0
--features tls,live-tls-tests,live-unix-socket -> server_version_num=180004  protocol=V3_2
```

The first is the 16.15 fixture server. A whole cross-version verdict was
reported off runs shaped like that: both "versions" passed identically because
both were the same server, and the identical totals read as CONFIRMATION rather
than as the tell they were. Two runs agreeing perfectly is a reason to ask what
they were pointed at.

So use the TLS features WITHOUT `suite-over-tls`. That set compiles every
`tls`-gated test and still honours `PG_TEST_URL`:

```bash
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:5459/zeroship \
  cargo test -p compio-postgres --features tls,live-tls-tests,live-unix-socket \
  -- --test-threads=1
```

Confirm the server before believing any cross-version figure, rather than
trusting the variable you exported:

```bash
... --test suite -- --nocapture cancel_request::raw_cancel_interrupts_running_query_and_preserves_session
# prints: cancel oracle: server_version_num=... protocol=... backend_key_len=...
```

The TLS suite proper still talks to its OWN servers on 5447-5452 regardless of
`PG_TEST_URL` - that part of the old note was right - and the fixture set
includes a PostgreSQL 18 `directtls` server for the direct-SSL case. What was
wrong was the conclusion that the rest of the suite therefore crossed versions.

The superseded figure, kept so it is not re-derived as if it were sound:
**1062 passed, 0 failed, exit 0** was recorded on 2026-08-26 as "18.4" from an
`--all-features` run. It was the fixture server. Nothing is known about 18.4
from it.

Count BINARIES as well as tests. Both numbers come from the same log and only
the pair is evidence: every test passing across HALF the binaries would print a
clean `0 failed` for the half it reached.

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

## The floor is PostgreSQL 16, and that is measured rather than assumed

MEASURED 2026-08-25 against **15.19** (`postgres:15`, same three settings, port
5460): **1752 passed, 2 failed** out of the 1754 that pass on 16.14 and 18.4.
Both failures are pgoutput OPTION support, not driver defects, and both are the
server refusing an option it does not have:

- `the_origin_none_option_drops_changes_replayed_from_a_peer` -
  `unrecognized pgoutput option: origin`. The `origin` option arrived in 16.
- `prepared_transactions_expose_every_two_phase_frame` -
  `streaming requires a Boolean value`. 15's pgoutput takes only a boolean
  there; `parallel` arrived in 16. The discriminator is that 16.14 given the
  same `streaming 'parallel'` complains about the PROTO VERSION instead
  (`does not support parallel streaming, need 4 or higher`), which is a server
  that knows the word.

So the driver works against 15 for everything except those two pgoutput
options, and it does not silently downgrade them - the request goes as written
and the refusal reaches the caller. If a deployment needs 15, that is the
limit to state; the option docs in `src/replication.rs` now carry it.

Nothing below 15 has ever been measured.

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
