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
