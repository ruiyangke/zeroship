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

## Running the WHOLE suite through the pooler

Add `IGNORE_STARTUP_PARAMETERS` or you will measure pgbouncer, not the driver:

```bash
  -e IGNORE_STARTUP_PARAMETERS="search_path,default_transaction_isolation,\
default_transaction_read_only,extra_float_digits,options,application_name" \
  -e DEFAULT_POOL_SIZE=20 -e MAX_CLIENT_CONN=200 \
```

pgbouncer REFUSES a startup packet carrying `options` it does not recognise,
with `08P01 unsupported startup parameter in options: search_path` - and that
is pgbouncer talking, not PostgreSQL. This suite scopes almost every test to
its own schema with `options=-c search_path=...`, so without the setting the
refusal lands on nearly everything. MEASURED 2026-08-24: 133 failed without
it, 49 with it, out of the same 1423. The 84-test difference was entirely
pgbouncer's own rejection, and reads at a glance like a driver that cannot
speak to a pooler at all.

A larger pool is right here too. `DEFAULT_POOL_SIZE=1` is for isolating one
protocol question; a whole-suite run needs enough backends not to serialise
1400 tests behind a single one.

## What SHOULD still fail, and why

`--no-fail-fast` IS LOAD-BEARING, and it is not in the command above by
accident. Without it cargo abandons the whole run at the first test BINARY
that fails, which behind a pooler is `backend_termination.rs` - measured
2026-08-25, that reports `285 passed; 3 failed` and stops, having never built
the other 60 targets. A fraction of the suite reads like a catastrophic result
rather than a partial one.

MEASURED 2026-08-24 on the configured pooler, twice: **1374/49** against
PostgreSQL 16, and **1407/50** against 18 after the suite had grown by ~35
tests. MEASURED AGAIN 2026-08-25 at suite size 1720: **1665/55** against 16,
across 18 test binaries, and every one of the 55 fell inside the set below -
nothing outside it. RE-MEASURED later the same day at suite size 1738, after
`close_notify` landed: **1683/55**, and the failing set is IDENTICAL - the same
50 test names, nothing added and nothing dropped, so all 18 tests added that
day pass behind a pooler.

RE-MEASURED 2026-08-25 after protocol 3.2 landed, at suite size 1753:
**1697/56**. The set gained exactly one name and lost none, and that one is
the assignment-dependent temp-table case described below - so requesting
protocol 3.2 by default costs nothing behind a pooler, which is what the
re-measurement was for. PgBouncer does not speak 3.2 and negotiates the client
down to 3.0 rather than refusing.

RE-MEASURED 2026-08-26 at suite size 1779: **79 binaries, 1722 passed, 57
failed, 52 distinct names**, every one inside the set below. The same tree ran
1779/0 against the direct server and against 18.4 the same day, so the whole
residue is the pooler and none of it is the driver.

Those figures are from the OLD test layout. The consolidation later the same
day took the crate from 79 test binaries to 5 and the reported count from 1779
to 895 without changing a case, so re-measure here before comparing - the
residue SET is what carries across, not the totals.

RE-MEASURED 2026-08-26 after that consolidation: **5 binaries, 840 passed, 55
failed, 55 distinct names**. Diffed against the 52 from the run above with the
new module prefix stripped: NOTHING newly fails, and the set lost exactly two -
the assignment-dependent temp-table case below, and
`replacing_observer_preserves_the_in_flight_requests_receiver`. A name leaving
the residue is not a regression; a name ENTERING it is the finding.

RE-MEASURED 2026-08-26 again, after the libpq connection-parameter parity merge
(`fdd1bdf7e`, 30 commits reworking connection defaults, empty values, service
files, passfiles, host and hostaddr slots, and target-session selection):
**900 passed, 55 failed, 55 distinct names, and the SET IS IDENTICAL** - nothing
entered, nothing left. The passed total moved 840 -> 900 because the merge added
tests, which is exactly why the set and not the count is the check. This run was
also the first live exercise of the corrected recipe below: it reported 55, and
the `FAILED` line count reported 55, so the two agree.

RE-MEASURED 2026-08-26 after the prepared-statement cache audit (`701802dfd`):
**504 passed, 60 failed, 60 distinct names**. SIX NAMES ENTERED, which is the
shape this page calls the finding - and here it is not one. All six are tests
that audit ADDED, traced with `git log -S` to `d7794582d` and `7b3301d72`, and
every one needs a prepared statement to survive between transactions, which a
transaction pooler does not provide:

```text
statement_cache_capacity_zero_ignores_the_execution_threshold
statement_cache_concurrent_stale_callers_share_one_replacement
statement_cache_does_not_retry_0a000_after_parameter_input
statement_cache_does_not_retry_after_a_savepoint
statement_cache_eviction_waits_for_a_bound_portal
statement_cache_retries_a_statement_missing_after_discard_all
```

So CHECK PROVENANCE BEFORE CALLING AN ENTRY A REGRESSION: a name that entered
because the test is new belongs to the residue by construction. `git log -S"async
fn <name>" --reverse` answers it in one command, and a name introduced by the
same merge you are testing is not evidence about that merge.

RE-MEASURED 2026-08-27 after the cancellation audit (six merges through
`6aad95ddc`): **507 passed, 65 failed, 65 distinct names**. FIVE NAMES ENTERED,
all of them live cancellation tests the audit added:

```text
a_token_from_a_returned_pool_lease_cannot_cancel_the_next_borrower
cancel_during_copy_in_surfaces_57014_and_preserves_session
cancel_during_copy_out_surfaces_57014_and_preserves_session
cancel_inside_transaction_requires_rollback_then_preserves_session
two_cancels_for_one_running_query_leave_the_session_usable
```

A NAME'S PROVENANCE IS NOT THE WHOLE ANSWER - read the failure message too.
Three cancellation tests were already in the residue, so "new cancel test, same
bucket" is a hypothesis, and the messages are what confirm it. Two of the five
name the cause outright:

```text
COPY progress for backend 1833912399 did not become visible
```

That number CANNOT be a backend pid, and the check is one command rather than
an impression: `cat /proc/sys/kernel/pid_max` on this host reads 4194304, and
the two values observed through pgbouncer 1.25.2 were 1257816711 and
1833912399, both several hundred times larger. So `client.process_id()` under a
pooler is the POOLER's BackendKeyData identifier - which is what lets it route
a later CancelRequest itself - and an observer querying `pg_stat_activity` for
it matches nothing. The other three time out waiting for a cancel that never
reaches the backend running the query. None of the five is evidence about the
driver, and the driver's own cancel oracle - printed by
`raw_cancel_interrupts_running_query_and_preserves_session` under `--nocapture`
- shows what a direct server does instead:

```text
16.14: protocol=V3_0 backend_key_len=4  cancel_packet_len=16 sqlstate=57014
18.4:  protocol=V3_2 backend_key_len=32 cancel_packet_len=44 sqlstate=57014
```

Any live cancellation test is expected to fail here. Do not "fix" one by making
it tolerate a pooler: that would delete the coverage on a direct server, which
is where cancellation has to work.

### A NEW NAME THAT WAS NEITHER A NEW TEST NOR A REGRESSION

RE-MEASURED 2026-08-27 at `b88362a56`: **523 passed, 66 failed**. One name
entered, and it is the most instructive entry on this page because both of the
usual explanations were wrong:

```text
read_timeout::copy_input_time_is_not_charged_as_server_read_silence
```

It is NOT residue-by-construction - `git show 6aad95ddc:...` finds it in the
tree at the previous baseline, where it passed. And it is NOT a regression - it
passes on the direct server, where the same day's gates read 1128/0.

The failure names its own cause:

```text
57014 canceling statement due to statement timeout
where: COPY cpg_read_timeout_copy, line 1: "7"   routine: ProcessInterrupts
```

That is a SERVER-side statement timeout, and the test sets none; it only sleeps
ten seconds mid-COPY on purpose. The chain, each link measured:

1. `SHOW statement_timeout` on the backing server is `0`, so it is no default.
2. pgbouncer proxies to `172.17.0.9`, which is the SAME container as 5455 - so
   the two paths differ only in the pooler, not in the server.
3. That config is `pool_mode = transaction` and defines NO
   `server_reset_query` (grep count 0).
4. Several tests run `SET statement_timeout = '50ms'` or `'4s'`
   (`query_observer.rs`, `command_timeout.rs`, `integration.rs`).

So a session GUC set by one logical client persists on the shared server
connection and lands on a later one. The pooler leaks session state because it
has no reset query - a configuration property, not a driver defect.

WHY IT SURFACED ONLY NOW is the part worth keeping. The COPY IN fixes merged
that day (`5be471843`, `a3422d6b1`, `1db02668d`) make `finish` report the
server's error instead of discarding it. Before them the leaked timeout's 57014
was SWALLOWED and this test passed by accident. The new failure is the fix
working.

So a name entering the residue has THREE possible causes, not two: the test is
new, the driver regressed, or the driver stopped hiding something. Check
provenance first, then the direct server, and only then read the message - the
third case looks exactly like the second until you do.

RE-MEASURED 2026-08-27 at `e4b4d4c6e`: **533 passed, 66 failed, and the SET IS
IDENTICAL** - nothing entered, nothing left. Roughly twenty merges landed in
between, touching client_encoding handling, authentication refusals, the
handshake, the internal catalog lookups, the decoder, and both write loops. The
passed total moved 523 -> 533 because those merges added tests. A set that
holds still through that much churn in the error paths is the strongest thing
this check produces; the count alone would not have said it.

One name LEFT: `statement_cache_propagates_a_second_consecutive_26000`, which no
longer exists - `46e919e2d` renamed it to
`statement_cache_does_not_retry_26000_after_bind_complete`. The replacement is
NOT in the residue, so the rewrite also made that case pooler-agnostic. A
departure can be a rename; grep the source before recording it as a fix.

THAT LINE READ "50 distinct names" UNTIL 2026-08-26, and the 50 was the recipe
below under-reporting, not a smaller set. Re-measured the same day at 55 failed
/ 55 distinct: the count was always the number of `FAILED` lines, and every one
of them is a distinct name. Both defects were in the extraction, and they partly
cancelled, which is why the wrong number looked plausible:

* `[a-z_:]` admits no DIGIT, so it dropped every SQLSTATE-named test -
  `..._returns_57014_...`, `..._retry_0a000_...`, `..._consecutive_26000`,
  `..._once_after_0a000`, `..._announces_utf8_...`. It captured ZERO
  digit-bearing names, and those are the tests most likely to move when error
  handling changes - exactly what this diff exists to catch.
* `-A40` truncates, and the suite binary's block holds 55 names in one run.

Measured against the same log: the original recipe yields 50, fixing only the
character class yields 40 (the truncation then dominates), and fixing both
yields 55, which equals the `FAILED` lines. Prove any replacement against that
last number rather than against this paragraph.

Compare the SET, not the count. Two runs can both report 55 while failing
different tests, and the count alone cannot see that; diffing the sorted
`failures:` names can:

```bash
grep -aA200 '^failures:$' run.log | grep -aE '^    [a-z0-9_:]+$' | sort -u > new.txt
comm -13 old.txt new.txt   # anything here is the finding

# Cross-check the extraction against the count libtest reports, every time.
# These two MUST agree; if they do not, fix the recipe before reading the diff.
grep -acE '^test .* FAILED$' run.log
wc -l < new.txt
```

Do not read the residue as a fixed number - it tracks how
many session-dependent tests the suite contains, so it moves with the suite.
What matters is that every failure is session state a transaction pooler does
not preserve. Treat anything OUTSIDE this set as the finding:

- the implicit statement cache (`0A000` cached plan, `26000` prepared
  statement gone) - `Config::statement_cache_capacity` says in as many words
  to keep it disabled behind such a pooler
- `LISTEN`/`NOTIFY` - the listening session goes back to the pool
- `CancelRequest` - the pooler owns the cancel key, not the backend
- session GUCs, `target_session_attrs` read-only checks, dirty-state and
  hand-off assertions, template-database fixtures
- the pool's own backend-identity tests (`connection_churn.rs`, the
  mass-termination and terminated-backend tests): a pooler hands out whichever
  backend it likes, so "the same connection came back" is not a claim that can
  hold behind one
- ANYTHING THAT TREATS `Client::process_id()` AS A REAL BACKEND PID. It comes
  from `BackendKeyData`, which pgbouncer synthesizes rather than forwards, so it
  is the same residue family as the cancel key above. MEASURED 2026-08-26: two
  separate connections through the pooler both reported process id 18651, which
  is what `a_notification_carries_the_notifying_backends_process_id` asserts
  against ("two connections must be two backends").
  This costs a cycle to diagnose if you do not know it, because the SYMPTOM is
  not a wrong pid - it is a HANG. A test that polls `pg_stat_activity WHERE
  pid = $1` for a state change waits on a row that cannot appear, and reports
  its own watchdog: `abandoned_copy_in_startup_rejection_does_not_poison_the
  _next_operation` fails 3 of 3 behind the pooler at exactly its 10s watchdog
  while passing in 0.32s against 5455. A watchdog message here reads exactly
  like the stranded-response-slot defect this runbook's "Why" section
  describes, so check whether the test dereferences a backend pid BEFORE
  concluding the driver deadlocked.
- `copy_input_time_is_not_charged_as_server_read_silence` - ASSIGNMENT
  DEPENDENT, so it is in the residue on some runs and not others. It creates a
  session-local TEMP table, which a transaction pooler leaves on whichever
  backend served the previous statement; the next borrower then meets
  `relation "cpg_read_timeout_copy" already exists` (42P07), or the delayed
  COPY meets `canceling statement due to statement timeout` (57014). MEASURED
  2026-08-25: absent from a 55-failure run, present in a 56-failure run, and
  passing twice in a row through the same pooler once the leftover temp table
  is dropped. Not a driver defect and not a protocol-version effect - the
  temp-table hazard is the one this runbook's last section already names.
  Finding it means dropping the leftover first:
  `SELECT schemaname, tablename FROM pg_tables WHERE tablename = '<name>'`
  reports `pg_temp_N`, not the per-test schema, so a `DROP` aimed at `public`
  silently does nothing and the next run fails the same way.

FIXED on 2026-08-24, so do NOT expect it any more: `differential_tokio.rs` used
to carry four hardcoded temp-table names, and behind a pooler the two drivers
shared a backend and collided on them - `both_drivers_agree_on_command_tags_and
_sqlstates` reported 42P07 against tokio's Rows(0), which reads as a driver
disagreement and was not one. Every fixture there is now per-driver, measured
24/4 -> 26/2 on that file alone.

None of that is a defect. The value of the run is the ~1400 that pass, and any
failure whose cause is not in the list above.

Run the suite through it:

```bash
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:6548/zeroship \
  cargo test -p compio-postgres --no-fail-fast -- --test-threads=1
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
