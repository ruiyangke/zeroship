# Soak compio-postgres under sustained mixed load

## Why

The ordinary `compio-postgres` suite proves individual operations and exits
after opening a relatively small number of connections. It cannot say much
about behavior over time. A descriptor released on the happy path but retained
after cancellation, a pool permit lost once every few thousand checkouts, or a
buffer whose high-water mark grows on every cycle can all survive a short test
run.

This soak keeps mixed work in flight for a bounded duration and then rules on
what the process and PostgreSQL actually report. It is deliberately opt-in. It
is a `harness = false` bench target driven by a script, so the ordinary
`cargo test -p compio-postgres` suite neither builds nor runs it.
The wrapper enables the crate's existing optional `tls` feature, and the DSN's
`sslmode` selects the transport. The harness adds no dependency, uses the
crate's existing compio runtime, and contains no tokio code. Building the bench
without `tls` remains supported for plaintext-only use.

## Prerequisites and safety

- Run from this repository worktree on Linux. RSS comes from
  `/proc/self/status`.
- `cargo` and GNU `timeout` must be on `PATH`.
- PostgreSQL must already be accepting the fixed test credentials at
  `postgres://postgres:zeroship@127.0.0.1:5455/zeroship`.
- A TLS run needs an already-running TLS server and a DSN whose `sslmode`
  permits TLS. A verifying mode also needs the matching seeded root
  certificate.
- The server role must be able to inspect `pg_stat_activity` and run the
  harness's self-termination probe. The fixed `postgres` role has both
  capabilities.
- Do not run another load test with the same server at the same time. The soak
  scopes its own sessions, but competing work changes latency and RSS pressure
  and makes comparisons harder to interpret.

NEVER run `libs/compio-postgres/tests/tls_live_setup.sh` for this soak. That
script regenerates a CA used by shared containers. Running it from this
worktree can invalidate the main worktree's live TLS fixtures. Use only an
already-running TLS fixture and its seeded certificate. The default plaintext
run uses the existing server on port 5455.

The harness does not create or drop a database, schema, table, or other
persistent PostgreSQL object. Its scope is a unique `application_name` and
session/query state. Normal completion closes every session. If the external
process watchdog has to kill the harness, closing the process sockets is also
the cleanup; use the printed application name with the triage query below to
confirm the server has observed that close.

## Run the default three-minute soak

The values are written explicitly here so a saved command records the server
and measurement window rather than relying on ambient defaults:

```bash
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:5455/zeroship \
SOAK_DURATION_SECS=180 \
SOAK_SAMPLE_INTERVAL_SECS=5 \
  libs/compio-postgres/tests/soak.sh
```

The script defaults to those same three values. It also accepts
`SOAK_BUILD_WATCHDOG_SECS`, whose default is 600 seconds. These are environment
settings for the opt-in script; the driver library itself still reads no
environment variables.

To run the same workload over TLS, supply a TLS-permitting DSN. There is no
separate transport flag; the wrapper uses the same configuration policy as the
pool and direct driver connections:

```bash
PG_TEST_URL="host=localhost port=5447 user=postgres password=PASSWORD dbname=postgres sslmode=require sslrootcert=/path/to/ca.crt" \
SOAK_DURATION_SECS=180 \
SOAK_SAMPLE_INTERVAL_SECS=5 \
  libs/compio-postgres/tests/soak.sh
```

For a short wiring and server-availability check, run the minimum 30-second
window:

```bash
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:5455/zeroship \
SOAK_DURATION_SECS=30 \
SOAK_SAMPLE_INTERVAL_SECS=5 \
  libs/compio-postgres/tests/soak.sh
```

The 30-second run is a smoke check, not a substitute for the default soak.
Durations below 30 seconds are refused. The script also refuses a duration and
sample interval that allow fewer than six periodic sample slots.

Do not invoke the bench executable directly for a recorded measurement. The
script passes its resolved values as `--url`, `--duration-secs`, and
`--sample-interval-secs`, and supplies an external process watchdog in addition
to the harness's async watchdogs.

## What the run does

The shell wrapper has two independently watched phases:

1. It builds the `soak` bench target in the release profile. The default build
   watchdog is 600 seconds.
2. It runs the harness under a process watchdog of `duration + 180` seconds.
   GNU `timeout` sends `TERM` at the limit and escalates ten seconds later if
   the process does not leave.

The external watchdog is not redundant. An async timer only fires while the
compio runtime is being polled; an executor-wide wedge could strand the timer
that was supposed to diagnose it.

Inside the runtime, the harness:

1. Opens a separately tagged observer and records both baselines before load.
2. Warms every workload shape and one 30-second pool-housekeeper cycle before
   the ruled RSS series begins. It also reads RSS once so the instrument's own
   first allocation is outside the series.
3. Sustains four concurrent pooled-query workers while also exercising pool
   acquire/release, periodic large responses, clean direct connection churn,
   server-terminated connections, query cancellation with verified reuse
   of the same pooled lease, and COPY IN / COPY OUT round trips on one
   long-lived session backed by a temporary table.
4. Samples RSS, the driver's live-connection count, and the server's tagged
   backend counts throughout the load.
5. Stops the workers, closes the pool and direct clients, waits for both
   connection instruments to return to baseline, then prints every sample and
   every operation count with its floor.

All driver work and observation of `live_connections()` stay on the same
compio runtime thread. This is required because the driver's live count is
thread-local: reading it from a helper thread would report that helper's zero,
not the connections owned by the runtime.

The server-side instrument is independent. Every load connection carries one
unique `application_name`, and the observer asks PostgreSQL's
`pg_stat_activity` for that exact tag. The observer uses a different tag and is
not counted as load. The baseline is measured rather than assumed to be zero,
then the final count must equal it.

## What is ruled on

Let:

- `D` be `SOAK_DURATION_SECS`;
- `W` be the four pooled-query workers;
- `I` be `SOAK_SAMPLE_INTERVAL_SECS`;
- `/` below mean integer division, rounding down.

Each arm prints the number it examined and the floor it enforced. A total that
passes cannot compensate for a missing workload shape.

| Arm | Required floor |
| --- | --- |
| Verified pooled queries | `D * W` |
| Pool acquire/release cycles | `(D * W) + max(D / 10, 3)` |
| Large-payload queries | `max((D * W) / 16, 1)` |
| Clean direct connections | `max(D / 2, 10)` |
| Bad self-terminated connections | `max(D / 10, 3)` |
| Cancellations with verified same-lease recovery | `max(D / 10, 3)` |
| Total verified work | pooled-query floor + clean-direct floor + bad-connection floor + cancellation floor |
| RSS samples | `(D / I) + 1` |
| Peak server-active tagged pooled queries | `2` |

Each of the four pooled-query workers must also complete at least `D` queries.
This prevents one fast worker from satisfying the aggregate while another
worker never runs.

Large-payload queries are a subset of pooled queries, and pool acquisitions
are the lease boundary for pooled and cancellation work. They are therefore
reported and ruled on separately but are not added again to total verified
work.

The count floors answer the most important harness-validity question: did the
soak actually sustain every promised kind of work? Falling below any floor
invalidates the run even if all leak checks happen to return to baseline.

The remaining rules are exact state and trend checks:

- PostgreSQL's backend count for the unique load `application_name` must return
  to its measured server baseline within the drain watchdog.
- `compio_postgres::live_connections()` must return first to its baseline with
  the observer still open, and then to the process's initial baseline after the
  observer is dropped and the driver drain completes.
- The ruled RSS series must not be nondecreasing with at least one increase
  after warmup. A flat series passes; any decrease makes the series
  non-monotonic. The harness prints the full series, rise and fall counts, and
  endpoint delta; an endpoint pair is not accepted as evidence.
- Every setup, warmup, load, shutdown, and drain phase must complete inside its
  async watchdog, and the entire executable must complete inside the script's
  independent process watchdog.

## Reading the output

Keep the complete output. A final `ok` line without the preceding samples and
arm counts is not a valid soak record.

Each memory sample reports elapsed time and resident KiB, together with the
server and driver connection observations taken during the same run. Read the
series in order. The warmup boundary matters: startup allocation is expected,
while an uninterrupted rise across the ruled post-warmup window is the finding
the trend arm rejects. The final drain sample does not get to erase a climb
during load.

The connection summary should show, at minimum:

- initial, observer-open, peak, post-load, and final driver live counts;
- server baseline, peak, and post-drain tagged backend counts;
- pool connection creation and eviction totals;
- every operation count beside its duration-derived floor;
- the complete RSS series and the trend verdict;
- elapsed time and each watchdog limit.

For a recorded measurement, preserve this context beside the raw output:

```text
date:
commit:
server version:
command:
complete soak output:
```

Do not put an endpoint-only memory summary in `complete soak output`. The
series is the measurement.

## Failure triage

An operation-floor failure means the harness did too little work. It is not a
clean driver result. Check the first workload error, server capacity, and
latency before considering any leak verdict from the same run. Do not lower a
floor to turn that run green.

A process exit status of 124 means GNU `timeout` fired. The script names
whether build or execution exceeded its watchdog. A harness phase-watchdog
failure names the phase and reports the connection state it could still
observe. Both are hang findings; silence after a log line is not a pass.

If the server count does not drain, use the unique application name printed by
the harness rather than looking at every session in the database:

```sql
SELECT pid, state, wait_event_type, wait_event, query
FROM pg_stat_activity
WHERE datname = current_database()
  AND application_name = '<application name printed by the soak>'
ORDER BY pid;
```

If the server returns to baseline but `live_connections()` does not, focus on
the local connection task, split reader, release path, and descriptor
ownership. If both remain high, start with the socket/session teardown path.
If both drain and only RSS rises, preserve the entire series and rerun under
the same duration before narrowing the workload; the connection instruments
have ruled out live sessions, not retained buffers.

Use the 30-second smoke to reproduce wiring or immediate hangs. Reconfirm a
memory or slow-release finding with the three-minute command, because a short
run has too few cycles to characterize an over-time trend.

## What a healthy run looked like, measured

Two runs on 2026-08-25 against the usual 16.14 fixture, so there is a reference
for what normal is. Re-measure rather than trusting these; they move with the
machine and the workload mix.

| | 60s, 5s samples | 900s, 15s samples |
| --- | --- | --- |
| operations | 13,358 | 199,664 |
| RSS samples | 13 | 61 |
| rises / falls | 3 / 5 | 27 / 24 |
| RSS delta | -84 KiB | +120 KiB |
| server backends, before -> after | 0 -> 0 | 0 -> 0 |
| driver live connections, after | 0 | 0 |
| pool created / evicted | 47 / 40 | 423 / 415 |
| acquire timeouts | 0 | 0 |
| bad connections / cancellations | 233 / 198 | 3,488 / 2,961 |

### After the cancellation audit, which is the run this section exists for

MEASURED 2026-08-27 at `388d026ca`, 600s window with 10s samples, against a
DEDICATED 16.15 container on 5461 rather than the shared fixture - two audit
agents were running suites on 5455 and 5459, and the prerequisites above forbid
sharing a server with other load: **131,510 operations**, 61 samples with 23
rises and 21 falls for a net **-76 KiB**, 287 pool connections created against
281 evicted, 0 acquire timeouts, both baselines back to zero
(`server_final=0 driver_final=0`).

The number this run was for is the cancellation pair:

    cancellations=1970   cancellation_recoveries=1970

EXACT equality over 1,970 cancellations, each one followed by verified reuse of
the SAME pooled lease. That day's six merged fixes changed when a cancel
returns, which transport it replays, and whether a lease-scoped token may fire
at all - all of it on the path this counter measures. A short suite proves each
rule once; this says the rules hold together 1,970 times without the pool
drifting.

`pool_acquires` and `pool_releases` also came out identical at 116,199. That
pair is the permit-leak check and it is exact, not approximate - one lost permit
in 116,199 checkouts would show.

The dedicated container is worth the 30 seconds. Sharing 5455 with an agent
running the suite would not have failed the soak; it would have made every RSS
and latency figure uninterpretable, which is worse, because the run still
prints `result=ok`.

### With the prepared-statement cache on

The cache is off by default, so every run above left the driver's most
stateful component - admission, LRU eviction, stale-plan retry, the generation
counter - almost untouched. It needs no code change to exercise: append
`?statement_cache_capacity=32` to the URL. MEASURED 2026-08-25, 600s:
**135,050 operations**, 41 samples with 15 rises and 15 falls for a net
**+104 KiB**, 282 pool evictions, and both baselines returned.

READ THE BASELINE, NOT ONLY THE DRIFT. Final RSS was 8,752 KiB against roughly
6,400 KiB with the cache off. That is the cache doing its job - it retains
prepared statements per connection - and it is a higher resident floor, not a
leak. The leak question is answered by the SHAPE of the series, which is as
balanced here as anywhere else.

### Over TLS, where the teardown changed most

MEASURED 2026-08-25 against the encrypted fixture on 5447, 120s: **26,507
operations**, 391 cancellations, 13 RSS samples with rises 3 and falls 2 for a
net **0 KiB**, and both baselines back to zero. That is the close_notify
teardown and the Arc<Mutex> rustls session under sustained load, which no
plaintext run touches at all.

CHECK THE CONTROL when reading a green TLS run: point the same
`sslmode=require` DSN at the PLAINTEXT server on 5448 and it must FAIL with
`TLS could not be negotiated`. Without that, a run that silently fell back to
plaintext looks exactly like a run that used TLS.

### Against PostgreSQL 18, where protocol 3.2 is actually negotiated

Every run above used the 16.14 fixture, where the driver requests 3.2 and the
server negotiates it DOWN to 3.0 - so none of them exercised the 3.2 code. A
600s run against 18.4 on 5459 did **131,817 operations** across 41 samples,
rises 20 and falls 16 for a net **-108 KiB**, and returned server backends and
driver connections to zero. That is the negotiation exchange and the
variable-length cancel key under sustained load, including thousands of
cancellations, which is the path the longer key runs through.

The long run is the informative one. +120 KiB across 199,664 operations is
about 0.6 bytes per operation, with rises and falls in near-equal number - a
sawtooth, which is what an allocator does, not what a leak does. Final RSS
after shutdown was 6,392 KiB, BELOW every sample in the series, so the pages
were returned rather than merely stable.

## Chaos: restarting the server underneath the load

The workload kills individual backends with `pg_terminate_backend`, which is
NOT the same event as the server going away. A restart drops every connection
at once, refuses new ones for several seconds, and returns with different
backend PIDs. `tests/connection_churn.rs` and the pool's mass-termination test
cover the first; nothing covers the second, because a suite test cannot restart
a server other tests are using.

Do it by hand:

```bash
PG_TEST_URL=postgres://postgres:zeroship@127.0.0.1:5455/zeroship \
  SOAK_DURATION_SECS=180 SOAK_SAMPLE_INTERVAL_SECS=10 \
  libs/compio-postgres/tests/soak.sh &
# once `phase=measure` is running:
docker restart zs-cpg-review-5455
```

MEASURED 2026-08-26. The soak FAILS, and that is not the finding - its floors
and baseline checks assume a stable server. What matters is HOW:

```text
soak result=failed: pooled query worker: run pooled scalar query failed: db error
live_connections_at_failure=0
```

A db error reached the caller, no watchdog fired, and the driver's live
connection count was ZERO at the failure - connections were released rather
than leaked. Separately, a pool asked for work again after the server came
back recovered on the FIRST attempt.

So the shape to look for is: errors surface, `live_connections_at_failure=0`,
no watchdog message. A hang, a panic, or a non-zero live count would each be a
defect, and each looks different from the ordinary failure above.

RE-MEASURED 2026-08-27 at `dd3ad1de8`, on a DEDICATED 16.15 container rather
than the shared fixture - a chaos run RESTARTS the server, so doing it on 5455
would break every other suite on this machine. Restart issued 40s into
`phase=measure`. All three signals reproduced exactly:

```text
soak result=failed: pooled query worker: run pooled scalar query failed: db error
live_connections_at_failure=0
```

no watchdog message, no panic, no hang. Then 49 `pool_` tests passed against
the restarted server on the FIRST attempt.

That re-measurement is the point of this section, not a formality. The nine
cancellation-audit commits merged that day changed when a session is RETIRED
and when its socket is RELEASED - terminal-severity retirement, pool lease
revocation that `force_close`s a session whose token escaped, and replication
retirement after a server ErrorResponse. A defect in any of them shows up here
as a non-zero live count or a hang, and in neither case would the ordinary
suite have noticed: it never restarts a server.

RE-MEASURED 2026-08-29 at `aef4e2e54`, on the dedicated 16.15 container
(`zs-soak-5470`), restart issued 40s into `phase=measure`. All three signals
reproduced again:

```text
soak result=failed: pooled query worker: run pooled scalar query failed: db error
live_connections_at_failure=0
```

No watchdog fired - the only lines containing `watchdog` are the three
phase-start budget declarations, which is worth stating because a grep for
`watchdog` matches those and can be misread as a firing. No panic, no hang.
Recovery: 63 `pool_` tests passed against the restarted server on the FIRST
attempt (7 in `--lib`, 56 in `--test suite`); the 2026-08-27 run recorded 49,
the difference being tests added since.

This run is the check on `ae8ba17f4`, which changed when a cancel retires a
session: a defect there surfaces here as a non-zero live count or a hang, and
the ordinary suite would not notice because it never restarts a server.

## Chaos: freezing the server without closing anything

A restart makes the server CLOSE, which surfaces as an error at once. The
harder failure is a server that stops responding and closes NOTHING - no RST,
no FIN, the socket stays open and a naive client waits forever. That is what a
network partition or a frozen host looks like, and it is what `read_timeout`
exists for. `docker pause` reproduces it exactly, which a scripted peer cannot:
that peer is still a live local socket choosing to withhold bytes.

```bash
# Build the probe first: an example that fails to COMPILE pauses the container
# around nothing and prints a plausible-looking log.
#   cargo build --release -p compio-postgres --example chaos_probe
#   ./target/release/examples/chaos_probe "$PG_TEST_URL" &
docker pause zs-cpg-types-5475      # freeze mid-query
# ... observe ...
docker unpause zs-cpg-types-5475    # ALWAYS, including on failure (use a trap)
```

MEASURED 2026-08-26, with `Config::read_timeout` at 5s and a query issued
against the frozen server:

```text
BH query ended after 5.000468796s: is_closed=true err=socket read timeout expired
BH live_connections=0
```

The clock fired within half a millisecond of its bound, the session was RETIRED
rather than left in limbo, and the connection was released.

**`is_closed` ON THE TIMEOUT ERROR IS THE WRONG SIGNAL, and this section said to
read it until 2026-08-28.** The text above asked for `is_closed=true` and warned
that `false` "would hand a poisoned session to the next caller". Re-measured at
`506bab466` with the committed probe, the timeout error reports:

```text
PROBE query ended after 5.000649053s is_closed=false err=socket read timeout expired
PROBE live_connections=0
PROBE reuse=refused is_closed=true err=connection closed
```

`is_closed()` is `kind == Kind::Closed`, and a read timeout carries
`Kind::ReadTimeout` (`src/error/mod.rs`), so `false` is what the driver MUST
report - the error says why it failed rather than only that the socket is gone.
`Kind::ReadTimeout` predates both earlier measurements (`45313c6a6`,
2026-08-21), so the older transcripts came from a different, uncommitted probe
and cannot be reproduced; that is why the probe is now committed.

Read these three instead, none of which is ambiguous:

- the elapsed time is within a few milliseconds of the configured bound;
- `live_connections=0`, so the descriptor did not outlive the failure;
- **the next query on that client is REFUSED** (`reuse=refused ... connection
  closed`). That is the poisoned-session property stated directly. A
  `reuse=SUCCEEDED` line is the defect the old wording was reaching for.

Set a read timeout before trying this. WITHOUT one there is no clock at all on
this path and the query waits for as long as the freeze lasts - which is the
correct behaviour for a driver told to wait indefinitely, and is why the
parameter exists.

RE-MEASURED 2026-08-27 at `8b4022269`, same 5s bound:

```text
PROBE query ended after 5.000447224s is_closed=true err=socket read timeout expired
PROBE live_connections=0
```

0.45ms over the bound against 0.47ms the previous day - unchanged. That
re-measurement exists because `e87aa8d04` put an EINTR RETRY LOOP inside
`read_with_deadline`, and the obvious way to write one resets the clock on each
retry, so a timeout would fire late or never. A scripted peer cannot show that;
only a real frozen server puts the loop under a deadline it must not restart.

USE A PORT NO CONTAINER ALREADY PUBLISHES, and check with
`docker ps -a --format '{{.Ports}}'`, not only `ss -ltn`. On 2026-08-27 port
5462 looked free by socket state but belonged to another project's
`zs-dbbind-pg`; docker refused the bind, which is the only reason this section's
`docker pause` did not freeze a stranger's database mid-session. A published
port is held by the container whether or not anything is listening right now.

## Chaos: a healthy server with no connection slots left

The third shape, and the one a platform running many apps against one
PostgreSQL meets first. The server is up and answering; it just will not take
you. Reproduce with a small server rather than by exhausting a shared one:

```bash
docker run -d --name zs-cpg-small-5461 -p 127.0.0.1:5461:5432 \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_DB=zeroship \
  postgres:16 postgres -c max_connections=15
```

Hold connections until the server REFUSES - counting to a number is not the
same as reaching exhaustion, and `postgres` is a superuser so it can also take
the `superuser_reserved_connections` slots. MEASURED 2026-08-26 with 15 slots
held:

```text
EX2 pool build refused after 505.325358ms: db error | FATAL: sorry, too many clients already
EX2 recovered: SELECT 42 = 42, live=1
```

Refused in half a second rather than hanging, the CAUSE names the real reason
rather than a generic failure, and the server served again as soon as the
slots freed. Read the chain, not the top line: `Error`'s own `Display` is the
terse `db error` by design, and the actionable text is in the source.

RE-MEASURED 2026-08-27 at `526ba9e5f`, after ~52 merged fixes to error and
retirement paths:

```text
EX held=15 then refused: db error
EX pool build refused after 504.742721ms: db error | FATAL: sorry, too many clients already
EX recovered: SELECT 42 = 42, live=2
```

504.74ms against 505.33ms the previous day, the same FATAL still in the chain,
and recovery once the slots freed. `live=2` rather than `1` is the probe asking
for a two-connection pool, not a behaviour change - state what the probe asked
for when quoting a live count.

That the FATAL is still reachable through the chain is the useful part. Two
sweeps spent that week making the driver prefer a server diagnosis over a local
symptom, and this is the shape where a regression would be invisible: the pool
refuses either way, and only the CAUSE distinguishes "the server is full" from
"something went wrong".

RE-MEASURED 2026-08-28 at `bcd6dcb69` with the committed
`examples/chaos_slots.rs`, which reports BOTH arms because they are different
measurements:

```text
SLOTS held=15 then refused: db error | FATAL: sorry, too many clients already
SLOTS connect refused after 1.106469ms: db error | FATAL: sorry, too many clients already
SLOTS pool build refused after 505.091402ms: db error | FATAL: sorry, too many clients already
SLOTS recovered: SELECT 42 = 42, live=1
```

**Quote the arm, not just the number.** A direct connect is single-shot by
design and refuses in about a millisecond; pool warm-up retries three times,
sleeping 100ms then 400ms between failures, so its refusal costs about 505ms.
The earlier transcripts above are the POOL arm. Reading a 1.1ms direct refusal
against a 505ms pool figure looks like a 380x regression and is neither.

That 505ms is also an independent check on the backoff itself: two sleeps, not
three. `connect_with_retry` guards its sleep with `if attempt < 2`, so the
`delay *= 4` that would produce a third 1.6s wait is computed and discarded. A
run near 2.1s would mean that guard had been lost. The rustdoc claimed the
1.6s sleep happened until 2026-08-28.

## Limits of the measurement

**The RSS rule fails only on a MONOTONIC climb** - the check is
`nondecreasing && rises > 0`, so a series that rises fifty-nine times and dips
once passes. That is deliberate: anything stricter fires on ordinary allocator
sawtooth, and a check that cries wolf gets its floor lowered. But it means a
leak with any jitter at all is invisible to the rule, and only the printed
series shows it. READ THE SERIES; do not just look for `soak result=ok`.

**`delta_kib` MISLEADS IN BOTH DIRECTIONS, because it is last-minus-first and
the first sample is often the process's low-water mark.** Measured 2026-08-27
at `c832bfdd0`, 300s: the run printed `rises=16 falls=8 delta_kib=200`, which
next to the previous run's `rises=11 falls=12 delta_kib=-104` reads like the
start of a leak. It is not. The series opens at 9432 and steps to about 9600
over its first four samples, then oscillates in a 9508-9768 band:

    first-half mean 9619, second-half mean 9636  ->  +18 KiB across the run

So the +200 KiB is sample 0 against sample 30, and the early steps are also
what inflate the rise count. Compare the two HALVES of the series, not its
endpoints, before reading a delta as a trend. A single number over a jittery
series is the wrong statistic whichever way it points.

RSS is resident memory for the whole soak process. It includes the driver,
compio runtime, allocator, Rust standard library, reporting buffers, and the
harness itself. A rising series cannot distinguish a driver leak from allocator
fragmentation or deliberate allocator retention. It is a signal to localize,
not proof of which component owns the pages.

A flat series also does not prove that no leak exists. A leak slower than this
run, one triggered by a workload shape the mix does not contain, or virtual
memory reserved but not resident can remain invisible. Increasing the duration
raises confidence only for behavior exercised during that longer window; it
does not turn a finite soak into a proof of absence.

**THE MIX CONTAINED NO COPY UNTIL 2026-08-27, and every figure recorded above
that date is blind to it.** `benches/soak.rs` had zero occurrences of `copy_in`
or `copy_out`, so the 131,510-operation run and everything before it say
nothing whatever about the COPY subsystem - not its descriptors, not its pool
interaction, not its error paths.

That gap was invisible from the output, which is why it survived. The soak
prints `clean_connections`, `bad_connections`, `cancellations` and their
floors, and a reader watching 131,510 operations pass could reasonably conclude
the driver was exercised end to end. It was not. Three COPY fixes landed that
same day - `1f0012aaf`, `5be471843`, `4056b1be1` - and all three changed state
machines the harness never entered.

A `copy_round_trips` worker now closes it. Each round trip sends 256 rows
through COPY IN, reads them back through COPY OUT, and checks the ROW COUNT AND
THE SUM before truncating - a stream that dropped or duplicated a frame can
still return a plausible byte count. It holds one connection for the whole run
rather than churning, because the temporary table is session state and reusing
the session is also what exposes a COPY that leaves the connection subtly
unusable for whatever runs next. The table is `TEMPORARY`, so the harness still
creates no persistent object.

MEASURED 2026-08-27, 60s window: **copy_round_trips=1034 against a floor of
15**, with `pool_acquires` and `pool_releases` still exactly equal at 11,748.
The floor is deliberately loose - its job is to catch a worker that never ran,
not to bound throughput.

RE-MEASURED the same day at `d8c9d1f37`, 300s: **copy_round_trips=5118** -
roughly 1.31 million rows through COPY IN and the same back out, each round
trip checked on row count and sum. RSS moved **-104 KiB** across 31 samples
(11 rises, 12 falls), `pool_acquires` and `pool_releases` were equal at 58,700,
`cancellations` and `cancellation_recoveries` equal at 987, and both baselines
returned to zero.

That run is the reason the workload was added. Eight COPY changes had landed
between the two measurements, and several of them make a stream WAIT longer
than it used to - COPY OUT now runs on to `CommandComplete` and then
`ReadyForQuery`, COPY IN buffers its completion until Sync, and a locally
detected refusal drains the response FIFO before yielding. Every one of those
is a place where a missed wake or a stranded response would hang rather than
fail, which a short suite is poorly shaped to catch and 5,118 consecutive round
trips is well shaped to catch.

Read the workload list under "What the run does" as the boundary of what a
green result means, and add a shape to the harness rather than stretching a
claim to reach it. That is what this entry is a worked example of.
