# Verification record: how this codebase's tests lie, and what it cost

Extracted from `2026-08-26-runtime-db-binding-design.md` on 2026-08-27, where it
had grown into a third of the document and was crowding out the design.

**Why it is its own document.** The defect register in the parent expires as
fixes land, and the design itself will be rewritten once the subscription
transport question (L12) is settled. This does not expire. Every entry below is
a way a test suite reported something other than a red when something was
wrong - each one measured on this tree, on a dated run, with the instance that
found it. Nothing here is a general warning about testing; it is a list of
mechanisms that fired here.

**How to use it.** Before citing any acceptance arm as evidence, check it
against this list. An arm is evidence only if it was **built, ran, and ruled on
something** - and this document exists because "it passed" turned out to be
compatible with none of those being true.

---

### A third defect class: arms whose verdict is unreliable

This document already tracks arms that **cannot pass** on any implementation and
arms that **cannot fail** on today's code. Implementation surfaced a third, and
it is worse than either because it produces no consistent signal at all: **an
arm running in a racing suite.**

`zeroship-plugin-db`'s tests publish into a process-global broker registry and a
process-global suppression map, and cargo runs them on parallel threads. A dozen
of them shared the app ids `"myapp"`, `"xapp"` and `"app_active"`, so tests
popped one another's events; the shared `reset_world` helper called
`drop_app(None)`, tearing down every app's subscriptions process-wide, and
decremented suppression refcounts for keys belonging to other tests - while its
own doc comment described all of this as "thread-local state". Measured: **3
failed runs in 12** on unmodified code, **0 in 20** after scoping each test to
its own app id.

Three things make this worth a section rather than a bug entry:

1. **It defeats mutation testing, which is this design's primary tool for
   judging an arm.** "Break the code, confirm the arm goes red" is
   indistinguishable from a flake, and "restore it, confirm green" is
   indistinguishable from a flake going the other way. Every mutation result
   recorded against a racing suite is `n=1` on a noisy channel.

2. **The first measurement of it was itself wrong.** The initial instrument
   grepped only for failing test names matching `exec::` or `crud::`, so a
   whole second family of failures in `wal_consumer::` printed as unexplained
   nameless failures, and the rate it reported ("2 in 9") was the rate of the
   subset it could see. The corrected instrument found a different and larger
   problem. A flakiness measurement is a measurement, and it fails the same
   ways as the ones this document already catalogues.

3. **The cause is the same one the design is about.** These are per-app entries
   in process-global maps with no per-owner scoping - the test-side instance of
   exactly the flat-global-keyed-by-app-id shape that the cache-bound and L10
   sections identify in the production code. The tests were racing for the same
   structural reason the runtime leaks and mis-invalidates.

The standing requirement that follows: **an acceptance arm may only be cited as
evidence if the suite it runs in has been shown stable over repeated runs.** Not
"it passed", and not "it passed twice" - a stated number of consecutive clean
runs, recorded next to the arm.

**And a fourth: the arm that was never built.** Two independent mechanisms in
this repository let a test report nothing while looking like a pass, and both
were hit while implementing against this design:

- **`required-features`.** `cargo test -p zeroship-plugin-db` does not build the
  `integration` target at all, because that target declares
  `required-features = ["test-helpers"]`. Cargo does not warn - it filters the
  target out. Every `--lib` run in this session reported "638 passed" while the
  entire integration suite, including the logical-decoding tests this design
  depends on, was never compiled.

  **Measured across the workspace (2026-08-27, from `cargo metadata`): 11 of 162
  test/bench targets are gated this way, and FIVE of them are in
  `zeroship-plugin-db`** - `integration`, `missing_role`, `native_transaction`,
  `sqlite_integration` and `distributed_live`. A default run of this design's
  own crate builds none of them.

  That has a direct consequence for this document. Invariant 14 in SC-1 cites
  `crates/zeroship-plugin-db/tests/native_transaction.rs:977` (the L8 regression
  test) as already covering the poisoned-commit case - and `native_transaction`
  is one of the five. The test is real and it passes, but **a default run never
  builds it**, so citing it as standing coverage overstates what the routine
  command verifies. Any arm this design cites must name the exact invocation
  that runs it, features included.

  The full gated list, for the same reason: `compio-postgres::tls_live` and
  `unix_socket_live`; `zeroship-control::live_db` and `workflow_engine_test`;
  `zeroship-migrate-adapter::platform_migrate`; `zeroship-migrated::apply_api_test`.
  AGENTS.md already documents this trap for `compio-postgres` and for the clippy
  gate; it is the same mechanism, and it is not documented for plugin-db.

  **All five plugin-db targets were run for the first time on 2026-08-27.** This
  is the baseline the TDD phase starts from, and it did not exist before:

  | target | features | result |
  | --- | --- | --- |
  | `integration` | `test-helpers` | 93 passed, 0 failed, 6 ignored |
  | `sqlite_integration` | `test-helpers` | 124 passed, 0 failed |
  | `native_transaction` | `test-helpers` | 13 passed, 0 failed |
  | `missing_role` | `test-helpers` | 2 passed, **1 failed** |
  | `distributed_live` | `live-db-tests` | 0 passed, **1 failed** |

  **232 tests that no default command builds** - and one of them has been red
  since **yesterday**. `pool_reconnect_missing_app_shaped_login_role_stays_internal`
  fails on `warm-up after_connect left connection 1 unusable`, and that string
  entered the tree in `07905dde2` ("fix(postgres): recheck entries after
  lifecycle hooks", 2026-08-26), which is an ancestor of `main`. A pool fix
  landed, broke a test that exercises `max_lifetime(ZERO)` warm-up, and **no
  routine command could observe it**. That is not a hypothetical cost of the
  `required-features` gap; it is a dated instance of it, found by finally
  running the target.

  `distributed_live` fails differently - `anchor readiness failed: status=500`
  from a worker it spins up - and is **not** claimed here as a code defect: the
  database it ran against has an empty `zeroship` schema, so a provisioning
  cause is at least as likely. It is recorded as unresolved rather than
  attributed, which is the honest state. This is the same shape AGENTS.md already
  documents for `compio-postgres` (`default = []` making a run feature-blind)
  and for the clippy gate (a target whose required-features are unmet is
  filtered out of the expectation rather than counted as unlinted). It recurs
  because the failure is a *smaller* run, and a smaller run prints a smaller
  green, not a red.
- **The skip that counts as a pass.** Ten tests guard on
  `pg_has_logical_wal(&pool)` and, when false, call `skip(...)` and `return`
  (`crates/zeroship-plugin-db/tests/integration.rs:2045` and nine siblings). A
  server with `wal_level=replica` therefore produces a run whose totals are
  **identical** to one where all ten passed. Measured 2026-08-27: the canonical
  test port `127.0.0.1:5440` was held by an unrelated container running
  `wal_level=replica` and `max_prepared_transactions=0` - both of the settings
  `tests/provision_test_backends.sh` calls load-bearing.

The provisioning script had a check for exactly this and it was bound to the
wrong container: it interrogated the *compose-managed* postgres, which is empty
in precisely the case it was written for (a hand-started container squatting the
port), so it fell through to a generic ownership warning and reported no
settings at all. Fixed 2026-08-27 to follow the port rather than the compose
project; the before/after was confirmed by mutation.

- **A third way, found by finally running the thing: the arm that aborts the
  binary.** `per_app_role_cannot_read_sibling_schema_or_touch_slots` - a
  role-isolation *security* test - overflows its thread stack and takes the
  whole process down with `SIGABRT` at test 77 of 99, so the 22 tests after it
  never run either. Diagnosed 2026-08-27: it is an oversized async future in a
  debug build, not recursion. `RUST_MIN_STACK=33554432` makes the same test pass
  unchanged, which both identifies the cause and gives the workaround.

  **Pre-existing, established by control rather than asserted.** The obvious
  reading - "the branch doing this work broke it" - was checked instead of
  assumed: a detached worktree at `main`, built from scratch, reproduces the
  identical `SIGABRT` in the same test. Worth the cost, because the cheap
  inference ran the other way and would have sent someone hunting a regression
  in a branch that only touched three files, none of them in `auth::bootstrap`.

- **And the same blindness covers LINTS, doubly.** Measured 2026-08-27: the
  crate this design is about held **two standing deny-level clippy errors** that
  no routine command could surface, hidden behind two independent mechanisms.
  First, a deny-level lint elsewhere - eight `doc list item without indentation`
  errors in `zeroship-migrate-policy`, untouched by this branch - **aborts the
  workspace run before plugin-db is ever scheduled**, so the crate lints as
  neither pass nor fail but as absent. Second, both of plugin-db's own errors
  live in test targets that only exist under `--all-targets --all-features`,
  which the `required-features` gating above makes a rare invocation.

  The errors themselves were small - a bare `std::env::var_os("RUST_LOG")` where
  the workspace's `disallowed_methods` lint wants the typed declared-env
  accessor, and a doc line beginning `- ` that clippy reads as an unindented
  list item. Both are now fixed and `cargo clippy -p zeroship-plugin-db
  --all-targets --all-features --no-deps` exits 0. The size of the errors is not
  the point: **a crate that cannot be linted accumulates them silently**, and
  AGENTS.md already records this exact pattern biting once before, when the
  clippy gate reported "148 of 148" on a workspace declaring 158 targets and the
  ten missing ones included a crate holding eleven standing deny-level errors.

So the rule needs one more clause: **an arm is evidence only if it was built,
ran, and ruled on something.** Not built, filtered out, skipped, aborted
partway, and never scheduled are five different ways of printing something other
than a red.

**The invocation this design's acceptance work must actually use**, with each
flag earning its place:

```text
RUST_MIN_STACK=33554432 \
cargo test -p zeroship-plugin-db --test integration --features test-helpers \
  -- --test-threads=1
```

`--features test-helpers` or the target is filtered out unbuilt.
`--test-threads=1` or 32 of 99 fail racing `CREATE SCHEMA plugin_db_test`
against one server. `RUST_MIN_STACK` or the run aborts at test 77. And the
server it points at needs `wal_level=logical` and a nonzero
`max_prepared_transactions`, or ten more tests skip while counting as passes.
Five conditions, four of them silent when unmet.

