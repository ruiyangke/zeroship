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
