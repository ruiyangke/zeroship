# Coverage found a 54-line function that no test executes

Measured 2026-08-31 at `7a8fac9d1`, `cargo llvm-cov --package compio-postgres`
under `tls,live-tls-tests,live-unix-socket,with-chrono-0_4,with-time-0_3`,
`--test-threads=1`, exit 0.

    crate total   34860 lines   3022 missed   91.33% line coverage

Compare 2026-08-28 at `5734bc81a`: 31783 lines, 2991 missed, 90.59%. The tree
grew about 3,000 lines and missed only 31 more, so coverage rose.

## Why this sweep was worth running at all

The mutation sweep over the 50 duplicate-body groups is finished, and it
structurally CANNOT find this class of defect. Mutation asks "does anything
CHECK this line"; it can only ask that of a line some test executes. Coverage
asks "does anything REACH this line". The two are complementary, which
`docs/reviews/2026-08-28-coverage-cannot-see-test-gaps.md` records from the
other direction: closing five mutation gaps moved coverage by nothing, because
those lines already ran.

Ranked by ABSOLUTE missed lines, ignoring the generated `error/sqlstate.rs`
table (235 missed, 12.64%, meaningless) and `test_utils.rs` (scaffolding):

    connection.rs   554 of 5120   89.18%
    tls_rustls.rs   251 of 1428   82.42%
    connect_raw.rs  239 of 2677   91.07%
    pool.rs         205 of 3260   93.71%
    replication.rs  186 of 3356   94.46%
    tls_sansio.rs   153 of 1256   87.82%

## The finding: `drain_available_retirement_read_channel` never runs

`connection.rs:2037-2090`, 54 lines. **43 counted regions, all with count 0.**
Both call sites, `:3016` and `:3040`, are also never executed.

An independent confirmation that the instrument is pointed at something real:
lines `720-723` are in the same never-executed set, and that is the step C
clean-close arm which a `panic!` probe had already proved unreachable by
construction. Two instruments, same verdict, arrived at separately.

## What gates it, measured rather than guessed

The enclosing retirement arm is NOT dead. `connection.rs:3005`,
`DispatchOutcome::RetireAfterDiagnostics(error) => {`, has **count 3**. So the
loop does retire after diagnostics; it never does so in the two states the drain
serves:

- `:3016` sits inside `if copy_read_obligation.as_ref()
  .is_some_and(ReadObligation::accepts_copy_input)` - retirement while
  PostgreSQL is waiting for producer data. No test retires a session in COPY
  mode.
- `:3040` sits inside `if response_count < responses.len()` - a re-drain when
  more responses were registered while the first drain awaited. No test grows
  the response deque during retirement.

Also never entered: `connection.rs:2571`, a second
`DispatchOutcome::RetireAfterDiagnostics` arm, count 0.

## What to do with it

The same two-outcome question that resolved every dead line this week, and it
must be answered before writing a fixture:

- **Unreachable by construction** - then the honest output is a source comment
  giving the argument, exactly as for the step C arm and
  `take_buffered_server_error`. Retirement in COPY mode may be excluded by an
  invariant upstream; if so, name it.
- **Reachable but untested** - then it is a real gap, and a serious one:
  retirement while the peer waits for COPY data is precisely where a session can
  be left wedged, and 54 lines of the recovery path have never run.

Do not assume from the comment at `:3011` ("PostgreSQL is waiting for producer
data, not owing a response") that the state is impossible. That comment explains
why the code does what it does, not that the branch is unreachable.

## Re-measured after the work: the seam closed

Coverage re-run at `5905f885e`, same command and features.

    before   34860 lines   3022 missed   91.33%
    after    35254 lines   2904 missed   91.76%

The tree grew 394 lines and missed 118 FEWER. Per target, aggregating regions
over each span:

    drain_available_retirement_read_channel   43 regions   0 -> 36 executed
    CancelToken Debug                         61 regions   0 -> 54
    RustlsConnect Debug                       15 regions   0 -> 15

That is the check worth doing after any coverage-driven work: re-measure the
SPECIFIC spans that motivated it, not just the crate percentage. A crate figure
moves for many reasons - here the tree also grew - and would not by itself
distinguish "the dead function is now covered" from "the new tests happen to
execute other lines".

Refreshed ranking by absolute missed lines, generated table and scaffolding
excluded:

    connection.rs   543 of 5302   89.76%
    connect_raw.rs  239 of 2686   91.10%
    tls_rustls.rs   232 of 1467   84.19%   <- lowest genuine coverage
    pool.rs         209 of 3276   93.62%
    replication.rs  167 of 3368   95.04%
    tls_sansio.rs   140 of 1282   89.08%

## The same misreading a THIRD time, and this one reached a brief

I briefed an agent that `Pool::query` and `Pool::batch_execute` were "never
executed". Half of that was wrong, and the aggregate check says so plainly:

    Pool::query              15 regions   14 executed   <- already covered
    Pool::query_text_params  15 regions    0 executed   <- actually dead
    Pool::execute            15 regions   14 executed   <- already covered
    Pool::batch_execute      14 regions    0 executed   <- actually dead

The dead spans I had eyeballed were lines 2012-2017 and 2047-2052 - which are
`query_text_params` and `batch_execute`, NOT `query`. I attributed them to the
neighbouring function by reading line numbers instead of resolving the enclosing
`fn`.

Two consequences, both worth stating:

- `pool_batch_execute_runs_the_batch_and_restores_pool_accounting` closes a real
  gap. `pool_query_runs_the_statement_and_restores_pool_accounting` does not -
  `command_timeout::pool_convenience_queries_enter_the_command_scope` already
  calls `pool.query`. It still earns its place, because it asserts the pool
  accounting returns to baseline and the existing tests do not, but it is not a
  never-executed find.
- **`Pool::query_text_params` is still dead** and no test names it. That is the
  gap the brief should have pointed at.

A probe caught the mistake rather than a re-read: the lease-leak mutation failed
THREE tests, two of them pre-existing (`integration::pool_reuse` and the
command-timeout one). A wrapper nothing exercised could not have had two
pre-existing tests fail on it, and that mismatch is what prompted the recheck.

**Resolve the enclosing `fn`, do not read line numbers.** Three misreads now,
all the same shape: auth refusals, the 2PC decoders, and this. The aggregate
form has been right every time it was actually run.

### So the reading was automated: `tests/lib/dead_functions.py`

Three misreads of the same shape is a broken instrument, not three lapses of
attention. The manual step that failed every time - "which `fn` is this line
run inside?" - is now the tool's job. It brace-matches each production `fn`
span, skips every `#[cfg(...test...)]` span (the same predicate-matching the
panic-message sweep needed, so `#[cfg(all(test, unix))]` is not a phantom), and
reports a function only when EVERY counted region inside it is zero.

Validated against both directions on the same JSON before being used:

    drain_available_retirement_read_channel   absent   (covered since 5905f885e)
    Pool::query_text_params                   present  (15 regions, all zero)
    Pool::batch_execute                       present  (14 regions, all zero)

That is agreement with the hand-aggregation, which is all a re-derivation can
show - it does not make either reading true. **The `batch_execute` row is in
fact stale**: `4eaabb40e` covered it after this JSON was produced. Treat the
list as a claim about the commit the JSON came from, never about the tree.

Ranked by what the crate actually depends on rather than by region count:

    pool.rs         Pool::cancel_query        PUBLIC, and called from outside
    pool.rs         transport::cancel_query   the TLS-policy branch it delegates to
    pool.rs         Pool::query_text_params   0 test references
    transaction.rs  Transaction::query_text_params   0 test references

`Pool::cancel_query` is the one that matters. It is not merely untested inside
this crate: `crates/zeroship-plugin-db/src/transaction/cancel.rs:146` calls it
to cancel a running transaction, so the platform's cancellation path runs
through a wrapper no test in the owning crate executes.

### Four of the dead functions should not be tested, they should be deleted

`Client::cancel_query`, `Client::cancel_query_raw`, `Transaction::cancel_query`
and `Transaction::cancel_query_raw` are all `#[deprecated(since = "0.6.0")]`
forwarders to `cancel_token()`, and they are the crate's only four deprecated
items. The caller set is closed: the two `Client` forms are called only by the
two `Transaction` forms, and the `Transaction` forms are called by nothing.
Every other `cancel_query` in the tree resolves to `CancelToken::cancel_query`
or `Pool::cancel_query`, both of which stay.

Under the project's no-back-compat rule the answer is deletion, not coverage.
Worth stating because a dead-code list invites the reflex to cover everything
on it: **the first question for each entry is whether the function should
exist**, and for these four it should not.

## A per-line lookup into the coverage segments is NOT "did this line run"

Recorded because it nearly cost a dispatch. Reading segment counts line by line,
`connect_raw.rs` appeared to show the driver's auth-method refusals unexecuted:

    line 1174  counts=None       GSSAPI
    line 1180  counts=[16]       SSPI
    line 1187  counts=[0, 0]     Kerberos V5
    line 1191  counts=[1473]     SCM credential

Two of those are absurd on their face - nothing runs an SCM-credential refusal
1,473 times - and that implausibility is the tell. A segment is a region
BOUNDARY at a (line, column); a zero-count segment on a line can coexist with
the line executing, because the zero belongs to some sub-expression region
rather than to the statement. Counts also propagate from enclosing regions.

All four refusals are in fact covered, by the table-driven
`authentication_local_refusals_preserve_pending_server_errors`, which drives
codes 7, 9, 2 and 6 and passes. There was no gap.

**Use the aggregate form instead**: take a whole function or impl span and ask
whether ANY region in it has a non-zero count. That is what produced the
retirement-drain finding (43 regions, all zero) and the `Debug` table, and both
were independently confirmed by a `panic!` probe before anything was written.

The rule that saved this: never act on a coverage reading alone. Every
never-executed claim acted on this session was corroborated by a panic probe
first, and the one claim that was not probed is the one that turned out false.

## Companion sweep: the 47 production panic messages

Same day, same principle applied to a different surface. Every production
`expect` / `unwrap` / `unreachable!` / `panic!` outside a `#[cfg(...test...)]`
span, excluding the unconditionally-compiled `test_utils.rs`:

    28 ACCURATE   18 VAGUE   1 WRONG   0 UNGUARDED

Zero UNGUARDED is the reassuring number: no production `unwrap` sits on a path a
hostile or merely unusual peer can reach. The VAGUE ones were bare `unwrap()`
and messages like `unreachable!("handled above")` - true, and useless to the
person reading them once, at 3am, unable to reproduce.

**The one WRONG message, `codec.rs`.** It attributed the guarantee to the async
tag: an async header, it said, means the body is buffered. It does not. A header
may be followed by a partial body; what actually guarantees completeness is the
explicit check at `codec.rs:410`,

    let msg_len = header.len() as usize + 1;
    if stream.buf()[idx..].len() < msg_len { ... break }

which the walk reaches before any async-tag branch. Verified by reading, not
accepted on report. The distinction is the whole value: if that assertion ever
fires, the broken contract is between the length check and `Message::parse`, and
the old message would have sent the reader to tag recognition instead.

**Enumeration trap worth repeating.** A naive `#[cfg(test)]`-only span check
counts 104 sites, 44 of them phantoms in `connect_socket.rs`, because the crate
also uses `#[cfg(all(test, unix))]` and
`#[cfg(all(test, target_os = "linux"))]`. Matching every cfg predicate that
mentions `test` gives 53, of which `connect_socket.rs` contributes 0 - that zero
is the control worth keeping.

## Re-measured at `5cf613841`: 92.00%, and a live gap in savepoint commit

    5905f885e   35254 lines   2904 missed   91.76%
    5cf613841   35607 lines   2848 missed   92.00%

The tree grew 353 lines and missed 56 fewer. Both controls confirm the previous
cycle's work landed: `pool.rs::batch_execute` and `pool.rs::discard_unowned_entry`
have left the dead list.

### The finding: committing a FAILED savepoint is never executed

`Transaction::commit` runs, but one branch inside it does not. At
`transaction.rs:154` the commit path computes

    let nested_failed = self.savepoint.is_some()
        && self.client.transaction_status() == Some(TransactionStatus::Failed);

and when that is true it releases the savepoint through a cleanup-carrying form
(`:159-164`) that issues `ROLLBACK TO SAVEPOINT` if the `RELEASE` fails. That
whole chain is dead:

    transaction.rs:160   start_batch_execute_with_error_cleanup   entry, branch never taken
    simple_query.rs:183  start_batch_execute_with_error_cleanup   0 executed regions
    client.rs:1480       send_with_error_cleanup                  0 executed regions

Nothing names it either: `nested_failed` appears only inside `transaction.rs`,
and no test in the crate references the cleanup pair.

The scenario is ordinary - open a transaction, take a savepoint, run a statement
that errors, then commit - and the surrounding function has already produced one
real bug. The comment at `:171-175` records that `self.done = true` used to be
set BEFORE the COMMIT was enqueued, so an unwind mid-build left the transaction
open on the server with nothing left to undo it. That was fixed on 2026-08-21.
An untested branch in that same function is worth closing.

### One flip this comparison cannot explain

`client.rs::send_with_error_cleanup` and
`simple_query.rs::start_batch_execute_with_error_cleanup` read as COVERED in the
earlier JSON and DEAD in this one, with both files unchanged between the two
commits and the newer run measuring a superset of targets. Coverage cannot
legitimately shrink that way.

The earlier JSON's provenance cannot be reconstructed - the log beside it records
4 targets and 1005 tests against this run's 7 and 1478, but that log may not even
belong to it. So the delta is uninterpretable, and the right response is not to
explain it but to stop relying on it: **the claim to act on is the current run's,
and it gets a `panic!` probe before any test is written**, exactly as every other
never-executed claim this week has.

### Two blind spots closed in the tool itself

**Spans were resolved against the working tree.** A coverage JSON stores line
numbers; the tool read source from disk. Running the same JSON after a commit
that removed 26 lines from `client.rs` moved 6 genuinely dead functions -
including `transaction.rs::query_text_params`, known dead - into a bucket that
printed as though they were fine. `--commit <sha>` now reads source from git at
the stamped sha. The control is exact: pinned reports 71 dead / 35 unjudged,
drifted reports 65 / 41, and every one of the six differences lies in the two
files the intervening commit touched.

**A span with no regions was skipped silently.** That is not "covered" - it is
"this run cannot speak about it". Those are now counted and listed as
`unjudged`. There are 35 of them, mostly trait-method declarations and generics
with no instantiation, and folding them into the pass column is how missing data
reads as a clean result.

## The soak's RSS rule fails about one run in three with no code change

Measured 2026-08-31 at `ce74f9de6`: three soak runs, same commit, same command,
same dedicated container.

    run   pool_acquires  pool_releases  delta_kib  preceding_sum  tail_sum  verdict
    A     82382          82382          36         206204         208024    GROWING
    B     82259          82259          68         209212         208860    stable
    C     82561          82561          136        206556         206972    stable

Run A failed the suite. Nothing changed between the three.

The rule compares the RSS sum over the final quartile against the preceding
window, allowing 64 KiB per sample of noise (`RSS_GROWTH_NOISE_BAND_KIB`,
`benches/soak.rs:56`). Over 21-sample windows that band is 1344 KiB, and the
runs land either side of it by small margins - A exceeded by 476 KiB, and the
previous cycle's passing run cleared it by only 184 KiB.

**The shape says there is no leak.** The RSS series oscillates between about
9.8 MB and 10.1 MB for the whole run, and the whole-run endpoint delta is 36 KiB
- 0.37% of resident size - on a run performing over 100,000 operations. A leak
would climb; this jitters around a flat baseline.

Two things follow, and the second is the useful one:

- **A single red soak is not evidence of a leak.** Re-run before reporting one.
  This instrument has a false-positive rate around 1 in 3 at the current band.
- **`delta_kib` and the verdict measure different things, and can disagree.**
  Run C has the LARGEST endpoint delta of the three (136 KiB) and passes, while
  run A has the smallest (36 KiB) and fails. Quoting `delta_kib` as though it
  were the thing the rule decides on - which is easy to do, since it is printed
  first - would get the reasoning exactly backwards.

Pool accounting was exact in all three runs, which is the part of this soak that
has never been ambiguous: `pool_acquires == pool_releases` to the unit.

## Inserting a test above a bare `fn` line deletes the test that was there

Adding a test to `replication.rs` by anchoring the insertion on

    fn replication_connection_debug_is_bounded_and_omits_private_state() {

silently destroyed that test. Attributes bind to what FOLLOWS them, so the
pre-existing `#[test]` ended up attached to the newly inserted function, and the
function it had belonged to was left bare:

    #[test]                                   <- was for the Debug test
    /// doc comment for the new test
    #[test]
    fn every_pgoutput_decode_error_...() { }  <- now carries TWO #[test]

    fn replication_connection_debug_...() { } <- no attribute, no longer a test

The suite still passed. The new test ran twice and the old one did not run at
all, so the total moved by +1 and looked exactly like a clean addition.

**The check that catches it costs one command**: list the tests and compare the
count against the number of DISTINCT names.

    cargo test -p compio-postgres --features ... --lib -- --list \
      | grep ': test$' | sed 's/: test$//' | sort > listed.txt
    wc -l < listed.txt ; sort -u listed.txt | wc -l

Before the fix: 658 listed, 657 distinct, `uniq -d` naming the doubled test, and
a diff against the same list at HEAD showing exactly which name had been LOST.
After anchoring on `#[test]\n    fn ...` instead - so the attribute cannot be
orphaned - 658 and 658, nothing lost.

A suite total is not enough. It moved by the expected +1 in both the broken and
the correct version; only the distinct-name count told them apart. Any tooling
that inserts tests should anchor on the attribute, or on a doc-comment line
above it, and never on the bare `fn`.

## Reading segments per line was wrong AGAIN, and this time I had already written it down

The rule recorded above - aggregate over a function span, never read a segment
per line - was followed for the DEAD-function list and then quietly abandoned
for the PARTIAL-function ranking, which listed individual zero-count segments as
"unexecuted lines". Those line attributions are not sound, and they picked the
target for a session's work.

`connect_raw.rs:1344` was listed as unexecuted:

    if mechanism != sasl::SCRAM_SHA_256_PLUS {
        handshake.prefer_available_server_error(can_skip_channel_binding(config))?;

A `panic!` there fails **552 of 1531 tests**. The line runs on essentially every
password connection. Nothing was wrong with the code or the profile - the
reading was wrong. A zero-count segment marks a region boundary at a
(line, column); it does not mean the line never executed.

**The fix is to stop interpreting segments and ask llvm-cov for line counts:**

    cargo llvm-cov report --package compio-postgres --show-missing-lines

That prints, per file, the lines with zero coverage. For `connect_raw.rs` it
lists 145 lines and **1344 is not among them**, agreeing with the probe. The
same output gives the genuinely uncovered auth branches this ranking was meant
to find - `1306-1308`, the `channel_binding=require` refusal when the backend
cannot produce a binding, and `1359-1360` / `1377-1378`, the SASL frame-order
refusals.

Two things to carry:

- **The aggregate zero-region COUNT per function is still a fair ranking
  signal** - "15 of 127 regions unexecuted" points at a function worth looking
  at. It is only the mapping from that count to specific LINES that is invalid.
- **A documented trap does not stop you walking into it.** This one was written
  up in this very file, and the ranking that violated it was built afterwards.
  What caught it was the standing habit of probing before acting, not the note.
  Prefer changing the TOOL over recording the hazard: `--show-missing-lines`
  cannot be misread the way raw segments can.

## The insertion hazard has a second form: doc comments, not just attributes

The `#[test]`-stealing case above has a sibling. An agent extracted an inline
`map_err` closure into a named helper so it could be unit-tested - a good move,
since the arm needs a 2 GiB allocation to reach live - and inserted it directly
above `pub async fn execute_text_params`. That function's twelve-line contract,
describing the whole NULL-aware text-format coercion model, was immediately
above it. The helper took the docs; `execute_text_params` was left undocumented.

Nothing failed. It compiles, `cargo doc` is happy, and the suite is green,
because a doc comment attaching to the wrong item is not an error - it is just
wrong.

**Both forms have one cause and one mechanical guard.** Anything that binds to
what FOLLOWS it - `#[test]`, `#[cfg]`, `///`, `#[derive]` - is stolen by an
insertion placed above the item it belongs to. So an insertion helper should
refuse the anchor rather than trusting the author to notice:

    assert not lines[anchor - 1].lstrip().startswith(("///", "//!", "#["))

That is the whole check. Anchor on the doc block's FIRST line, or on the
attribute, and never on the bare `fn`. The two detections are different -
`--list` counts catch a stolen attribute, and only reading catches a stolen doc
comment - which is the argument for preventing both at the insertion point
instead of hunting them afterwards.

**The blast radius was one.** A sweep for doc blocks that name the FOLLOWING
function instead of their own flagged 11 candidates across the crate; reading
the first line of each showed all 11 are ordinary cross-references
("Ensure the read buffer has at least `min_bytes`", "Attempts to cancel the
connection identified by a pool lease's token"). Zero real thefts besides the
one repaired here, which was introduced the same day by an agent insertion
rather than being a long-standing pattern.

## Two thirds of the "uncovered" lines are test scaffolding

`--show-missing-lines` is authoritative about which lines never ran, but it
reports every line, including mock sinks and scripted peers inside
`#[cfg(test)]` modules. Chasing its output directly leads straight into
`CountingSink::shutdown`.

`tests/lib/missing_production_lines.py` intersects that list with the
production spans, reusing the same predicate-matching `#[cfg(...test...)]` walk
the dead-function tool uses so `#[cfg(all(test, unix))]` is not mistaken for
production. Measured at `3ca40878e` over the three largest files:

    connect_raw.rs    62 production of 144 uncovered
    connection.rs     69 production of 281 uncovered
    tls_sansio.rs     44 production of 102 uncovered
    ------------------------------------------------
    total            175 production of 527 uncovered

**352 of 527 were scaffolding.** The genuine production gap in these files is
about a third of what their coverage percentages suggest, and `tls_sansio.rs`
at a reported 89.08% is the most distorted of the three - most of its shortfall
is mock writers that exist to be dropped mid-write.

Two cautions that come with the tool:

- **It reads the profdata of the LAST `cargo llvm-cov` run, not the tree.**
  Lines closed since that run still appear. The SASL frame-order arms at
  `connect_raw.rs:1359-1360` and `:1377-1378` are in the table above and were
  bound afterwards.
- **Scaffolding being uncovered is not automatically fine.** A mock whose
  `shutdown` never runs may mean the test never exercised shutdown, which is a
  gap wearing a disguise. The split is a filter for ranking, not a licence to
  ignore the right-hand column.

## A rewrite's own tests covered its branches but not its plumbing

The flush rewrite (`fe29cc1f7`, 57 production lines) shipped with six tests and
recorded reachability and behaviour proofs for each. A mutation audit scoped to
exactly its changed lines found **five unbound sites**, all of the same kind,
and each is now bound and independently re-proved:

    copy_error_may_owe_extra_ready forwarded into flush dispatch
    terminal_server_error          forwarded into flush dispatch
    the retirement poison ORDERED before the reader acknowledgement
    response_count            the already-flushed prefix a retirement drains
    include_tail_after_flush  whether the tail is drained once flush completes

The last two are fields of the `FlushRetirement` the loop builds. Pinning
`response_count` to 0, or `include_tail_after_flush` to false, each failed
exactly one test out of 1546 - and neither failed anything before this work.

The six original tests exercised the branches inside
`flush_with_read_draining`. What none of them observed was whether the function
hands the CALLER's state to dispatch. Replacing either forwarded reference with
a throwaway `&Cell::new(false)` / `&Mutex::new(None)` left the whole suite green
before this work: the branch still ran, it just wrote its result somewhere
nobody read.

The ordering case is the sharpest. Retirement stores `READ_RETIRED_STATUS`
twice - once before the acknowledgement wakes the reader, once after, in the
`RetireAfterDiagnostics` arm. Deleting the EARLY store leaves the final state
identical, so any test asserting "the status is retired" still passes. Only a
test observing the wake order catches it.

**The lesson is about what a branch test proves.** "This arm executes and
returns the right value" is not "this arm's effect reaches the caller", and a
proof recorded per-branch does not cover the parameter plumbing between them.
When a rewrite threads caller-owned state through a helper, the forwarding is a
separate claim and needs a separate mutation - swap the argument, not the body.
