# Coverage did not move when five real test gaps were closed

Date: 2026-08-28. Measured on `compio-postgres`, not argued.

## The experiment

Two mutation sweeps ran against `src/transaction.rs` (`3743daa15`) and
`src/connection.rs` (`1b7af624a`). Between them they mutated 26 behaviours one at
a time and found **6 that no test detected**. Five got new tests. Each new test
was then independently verified by re-applying its target mutation and
confirming that the NAMED test goes red:

| Mutation | Test that now kills it |
| --- | --- |
| `Transaction::commit` swallows the COMMIT round-trip error | `commit_propagates_a_deferred_constraint_failure` |
| `Transaction::rollback` swallows its round-trip error | `rollback_propagates_a_missing_savepoint_error` |
| A healthy nested commit skips the server-side `RELEASE` | `committed_savepoint_is_removed_from_the_server_stack` |
| The connection encodes no `Terminate` frame on clean teardown | `dropping_a_raw_client_sends_terminate_after_a_completed_query` |
| A terminal read overtakes stashed `pending_responses` | `multiplexed_pending_batches_arrive_before_the_terminal_read` |

Line coverage was measured before (`5734bc81a`) and after (`1b7af624a`) with the
same feature set, `tls,live-tls-tests,live-unix-socket`, on the same server:

    transaction.rs  87.26%   40 of  314  ->  87.26%   40 of  314    3 new tests
    connection.rs   87.90%  536 of 4428  ->  87.88%  536 of 4421    2 new tests
    pool.rs         92.97%  205 of 2915  ->  92.97%  205 of 2915    0, control

## The result

**The missed-line counts are identical.** `connection.rs`'s percentage moved only
because an unrelated dead-code removal shrank its denominator by seven lines; its
536 missed lines did not change. `pool.rs` was untouched by either sweep and is
included as the control showing the measurement is stable.

So five tests, each proven to catch a defect the suite could not previously
detect, changed coverage by nothing.

## Why, and what to do about it

A surviving mutant lives in code that **already executes**. Every one of the five
lines above ran on every suite run before these tests existed - the driver simply
did not check what it did. Coverage instruments execution and cannot distinguish

- "this line ran", from
- "a test would notice if this line were wrong".

The practical consequence for this crate: before 2026-08-28 the driver could
swallow a deferred-constraint failure at COMMIT, swallow a rollback error, leave
a savepoint alive on the server, stop sending `Terminate` on clean teardown, and
deliver responses out of order - **with a green suite and unchanged coverage**.

Use coverage to choose where to LOOK. It is good at that: all six survivors came
from files it ranked 87-88%. Use mutation to decide whether the tests there can
FAIL. **Never quote a coverage delta as evidence that testing improved** - this
document is the counterexample.

## Related

- `docs/reviews/2026-08-28-open-findings-re-derived.md` - the six carried-forward
  findings, none of which survived contact with HEAD.
- A sixth survivor, "reuse one fixed savepoint name", was deliberately left
  untested by the transaction sweep (`3743daa15`).
- The same class was found by accident earlier the same day: an assertion in
  `connection.rs` that read an `Arc` no production path could write through, so
  it could never fail. Removed in `b5483ba1f`.

## The full sweep, for context

The experiment above used the first two modules. Five were swept in total on
2026-08-28, all with the same method:

| Module | Line coverage | Mutations | Survivors |
| --- | ---: | ---: | ---: |
| `transaction.rs` | 87.26% | 12 | 4 |
| `connection.rs` | 87.90% | 14 | 2 |
| `pool.rs` | 92.97% | 12 | 2 |
| `codec.rs` | 88.22% | 28 | 7 |
| `copy_in.rs` | 87.97% | 20 | 4 killed, 3 skipped, 1 wedged |

**86 mutations, 21 survivors.** The best-covered module still had two; the
decoder at 88% had the most. Ordering the modules by coverage does not order
them by survivors.

Beyond the five already listed, these were also true with a green suite:

- the pool could hand a borrower a session whose validation query had FAILED,
  and could drop its warm-up retry backoff entirely;
- the codec's `saw_error_response` could stop being sticky, so a framing error
  would surface INSTEAD of the server's own diagnosis whenever any frame sat
  between them;
- the codec would accept an 11-byte `BackendKeyData` where `protocol.sgml`
  specifies a 12-byte minimum, and an overlong `ReadyForQuery`;
- a large COPY item could drop or duplicate its buffered prefix.

### Two things that make a sweep worth running

**Mutations must be plausible bugs, not obvious vandalism.** The pool backoff
mutation deleted `sleep(delay)` while leaving `delay *= 4` intact, so any test
asserting on the COMPUTED backoff still passed; only a test measuring elapsed
time caught it. "Delete the function body" is killed instantly and teaches
nothing.

**Some mutations WEDGE rather than fail.** Forcing `copy_in.rs`'s disconnected
branch hangs the suite, because `poll_disconnected_diagnosis` loops on
`Poll::Ready(Ok(_))` until an error or `Pending`. The `copy_in` sweep recorded
`exit 124` as a third verdict rather than a kill. A sweep without timeouts reads
a hang as "the mutation was detected" and reports a false clean on the most
dangerous path in the module.

### Pinning a constant means checking it first

Three `codec.rs` survivors were exact protocol bounds. Before accepting the new
tests, each was checked against `/tmp/postgres-rel-18-4/doc/src/sgml/protocol.sgml`
field by field:

    BackendKeyData: Int32 length(4, incl self) + Int32 pid(4) + Byten key,
    "minimum and maximum key length are 4 and 256 bytes"  => 12..=264
    codec.rs enforces (12..=264)                             EXACT

A test that pins the WRONG constant is worse than no test: it enshrines the bug
and makes the eventual correction look like a regression.

## The 2026-08-30 sweeps are the same class, so coverage stays the wrong instrument

Between 2026-08-29 and 2026-08-30 roughly fifty tests were added across
`cancel_query_raw.rs`, `generic_client.rs`, `copy_out.rs`, `buf_stream.rs`,
`socket.rs`, `query.rs`, `connection.rs`, `pool.rs` and `codec.rs`. Every one
binds a guard in code that **already ran on every suite invocation**:

- a delegation that forwards `query_opt` rather than `query_one` - both arms
  execute, only the cardinality differs;
- a COPY OUT state arm (`ReadyForQuery if command_complete`) - the match runs
  regardless, the guard decides whether a malformed sequence is refused;
- `mark_possibly_sent()` sitting BEFORE rather than AFTER the `?` on a cancel
  write - the same statement executes either way.

Each was mutation-proved: remove the guard, a named test goes red. That is
detection, and it is exactly what the experiment above showed line coverage
cannot see. Predicting the coverage delta of this session at roughly zero is not
a guess; it is the measured result of the same experiment on the same crate.

**Practical note, 2026-08-30.** `cargo-llvm-cov` is NOT on PATH in this
environment, so any coverage figure quoted for this crate today would require
installing it and an instrumented rebuild first. The 990 MB `target-cov/` in the
tree is leftover from the 2026-08-28 run above, not a current measurement -
`target/` does not match it, which is why it is gitignored separately.

**What to run instead** when asking "did the suite get better": re-run the
mutation that motivated each test and confirm the named test still goes red.
That is a direct measurement of detection. Coverage answers a different
question, and answering it here has already been tried and reported.

## The complementary property: a mutation-proved test is worthless if it is flaky

Mutation-proof answers "would this test notice the defect". It says nothing
about "does it answer the same way every run". Both are required, and the second
is cheap to measure, so measure it rather than assuming it.

Measured 2026-08-30 at `eedde7d81`. The 19 tests added between `bf8517d0b` and
that commit, extracted from the commits themselves rather than from memory, run
five times each against 5455:

    iter 1: lib 11 passed / 0 failed    suite 8 passed / 0 failed
    iter 2: lib 11 / 0                  suite 8 / 0
    iter 3: lib 11 / 0                  suite 8 / 0
    iter 4: lib 11 / 0                  suite 8 / 0
    iter 5: lib 11 / 0                  suite 8 / 0

Zero variance across five runs.

**Check the arithmetic, not just the zeros.** `11 + 8 = 19`, which equals the
number of names fed in. That equality is the real guard: a name list that
matches nothing prints `test result: ok. 0 passed` and a run of five such
iterations is five identical, meaningless greens. The pass count must reconcile
against the input list, or the stability result is vacuous.

Two helper functions (`assert_cardinality_delegates`,
`assert_prepare_typed_delegate`) were filtered out of the list first - they are
called BY tests and are not tests, so including them would have made the
arithmetic fail to reconcile and sent me looking for a phantom missing test.

## A worked case where the test COUNT went up by less than the binding did

2026-08-30, commit `453a1288d`. I predicted the gate would read 1416 (1407 plus
nine new tests). It read **1415**, and chasing the missing one is instructive.

All nine new tests did run - each appears once in the gate log. The gap is a
test that was DELETED:

    -  every_coalesced_failure_is_attached_to_the_wire_earlier_error_batch
    +  assert_coalesced_failure_is_deferred(..)            <- helper, not a test
    +  a_malformed_header_after_error_response_is_deferred
    +  a_message_ceiling_failure_after_error_response_is_deferred
    +  a_startup_limit_failure_after_error_response_is_deferred
    +  a_copy_metadata_failure_after_error_response_is_deferred
    +  a_ready_for_query_failure_after_error_response_is_deferred

So 1407 + 9 - 1 = 1415. **Predict net, not added** - counting only `+fn` lines
overstates it whenever a test is refactored away.

**The substance moved the other way from the count.** The deleted test asserted
that coalesced failures are deferred, without distinguishing WHICH of the four
identical `if saw_error_response { deferred_error = Some(error); }` arms
produced the deferral. One test, four arms, three of them unbound. The five
replacements bind one arm each, which was proved by mutating them separately:

    mutate codec.rs:431 -> only a_startup_limit_failure_...    FAILED
    mutate codec.rs:448 -> only a_copy_metadata_failure_...    FAILED

Net +8 tests; binding went from 1 arm-agnostic assertion to 5 arm-specific
ones. A reviewer watching the total would see a smaller number than expected
and could read it as a regression. The count is not the measurement - the
one-copy-one-failure result is.
