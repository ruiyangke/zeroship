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
The harness adds no dependency or feature and uses the crate's existing compio
runtime; it contains no tokio code.

## Prerequisites and safety

- Run from this repository worktree on Linux. RSS comes from
  `/proc/self/status`.
- `cargo` and GNU `timeout` must be on `PATH`.
- PostgreSQL must already be accepting the fixed test credentials at
  `postgres://postgres:zeroship@127.0.0.1:5455/zeroship`.
- The server role must be able to inspect `pg_stat_activity` and run the
  harness's self-termination probe. The fixed `postgres` role has both
  capabilities.
- Do not run another load test with the same server at the same time. The soak
  scopes its own sessions, but competing work changes latency and RSS pressure
  and makes comparisons harder to interpret.

NEVER run `libs/compio-postgres/tests/tls_live_setup.sh` for this soak. That
script regenerates a CA used by shared containers. Running it from this
worktree can invalidate the main worktree's live TLS fixtures. The soak uses
the existing plaintext server on port 5455 and needs no TLS fixture.

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
   server-terminated connections, and query cancellation with verified reuse
   of the same pooled lease.
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

## Limits of the measurement

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
