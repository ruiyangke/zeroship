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

- Docker. The suite starts its PostgreSQL 18 server itself, through
  `compio_postgres_testkit::server::server_on_postgres_18`, with the same
  settings as the PostgreSQL 16 server it runs against by default: logical
  decoding for replication, prepared transactions for the two-phase tests, and
  replication slot budgets the suite's parallelism cannot exhaust. Nothing is
  started by hand and nothing points the suite at a server from outside.

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

Run the ordinary suite against PostgreSQL 18:

```bash
cargo nextest run -p compio-postgres --features suite-on-postgres-18
```

Expected: the same pass count as the default PostgreSQL 16 run, 0 failed.
Anything else is either a real version difference or a test that pinned one
version's behaviour - triage below.

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
nothing about the TLS surface on either version.

## The second major over TLS

`suite-over-tls` and `suite-on-postgres-18` together run the same test bodies
over TLS against the TLS fixture's PostgreSQL 18 server, `directtls`, which
carries the suite's settings for exactly this; `--all-features` turns both on.
The TLS-specific suite talks to its own six servers whatever the variant, one
of them PostgreSQL 18 for the direct-SSL case:

```bash
cargo nextest run -p compio-postgres --features suite-over-tls,suite-on-postgres-18
cargo nextest run -p compio-postgres --features tls -E 'test(/^integration::tls_live::/)'
```

Confirm the server before believing any cross-version figure, rather than
trusting the feature you passed:

```bash
cargo nextest run -p compio-postgres --features suite-on-postgres-18 --no-capture \
  -E 'test(cancel_request::raw_cancel_interrupts_running_query_and_preserves_session)'
# prints: cancel oracle: server_version_num=... protocol=... backend_key_len=...
```

Count BINARIES as well as tests. Both numbers come from the same summary and
only the pair is evidence: every test passing across HALF the binaries would
print a clean `0 failed` for the half it reached.

Wait for the run to EXIT, not for its output to go quiet.
`pgoutput_subtransactions` streams for minutes on 18 without printing, so a
"has the log stopped growing" check calls the run finished early and prints a
clean 0 failures for the part it saw. Wait on the nextest process instead,
and confirm the binary count as well as the failure count.

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
