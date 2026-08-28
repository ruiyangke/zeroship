# Verification record: how this codebase's tests lie, and what it cost

Extracted from `2026-08-26-runtime-db-binding-design.md` on 2026-08-27, where it
had grown into a third of the document and was crowding out the design.

**Why it is its own document.** The defect register expires as fixes land, and
the design itself will be rewritten once the subscription
transport question (L12) is settled. This does not expire. Every entry below is
a way a test suite reported something other than a red when something was
wrong - each one measured on this tree, on a dated run, with the instance that
found it. Nothing here is a general warning about testing; it is a list of
mechanisms that fired here.

**How to use it.** Before citing any acceptance arm as evidence, check it
against this list. An arm is evidence only if it was **built, ran, and ruled on
something** - and this document exists because "it passed" turned out to be
compatible with none of those being true.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## The classes this document tracks

Two class names are used as vocabulary across this document set - arms that
**cannot pass** on any implementation, and arms that **cannot fail** on today's
code - and **no document in the set defines either.** Checked 2026-08-27 by
grep over `docs/proposals/2026-08-26-*`: both phrases occur only as verdicts on
a specific arm, never as a definition. That is recorded here as owed rather than
answered, because writing a definition after the fact risks describing something
other than what the instances meant.

What the instances are, so the vocabulary at least has referents:

- **`cannot pass`.** The design's SC-2 acceptance row rules out an "ops" arm for
  SQLite autocommit concurrency: "SQLite has one writer per database on any
  number of connections, so an 'ops' arm cannot pass" - the arm is written
  against a property the engine does not have. SC-2 carries a second: an earlier
  draft required a dropped operation to roll back unconditionally, and "that
  cannot pass: the actor may commit and send its reply before the caller ever
  polls, and nothing dropped afterwards can un-commit it."
- **`cannot fail`.** L13 in the defect register is the plainest instance: a cap
  enforced only on a path production never calls, whose test calls that path
  directly, so the cap "is green and dead simultaneously". SC-3 names the shape
  outright for an arm that greps a path which was never created - it "matches
  nothing and reports success, so this arm passes today, passes if the refactor
  is abandoned, and passes if the file is deleted later" - and records that "it
  appeared three times across these documents". SC-1 raises it twice more, once
  for a success-path arm and once for a durability half that is "worse than
  'cannot fail in principle' - it is already written". The design's SC-6 row
  states the same hazard for a deny-only ceiling arm, which "passes on an
  implementation where the ceiling read is broken and everything is denied".

Everything below is a class this document measured for itself, on this tree.

## A third defect class: arms whose verdict is unreliable

Those two classes come from the set around this document. Implementation
surfaced a third, and it is worse than either because it produces no consistent
signal at all: **an arm running in a racing suite.**

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

## A fourth: the arm that was never built

Four independent mechanisms in this repository let a test report nothing while
looking like a pass, and all four were hit while implementing against this
design.

### `required-features`

`cargo test -p zeroship-plugin-db` does not build the `integration` target at
all, because that target declares `required-features = ["test-helpers"]`. Cargo
does not warn - it filters the target out. Every `--lib` run in this session
reported "638 passed" while the entire integration suite, including the
logical-decoding tests this design depends on, was never compiled.

**Measured across the workspace (2026-08-27, from `cargo metadata`): 11 of 162
test/bench targets are gated this way, and FIVE of them are in
`zeroship-plugin-db`** - `integration`, `missing_role`, `native_transaction`,
`sqlite_integration` and `distributed_live`. A default run of this design's own
crate builds none of them.

That has a direct consequence for this document. Invariant 14 in SC-1 cites
`crates/zeroship-plugin-db/tests/native_transaction.rs:977` (the L8 regression
test) as already covering the poisoned-commit case - and `native_transaction`
is one of the five. The test is real and it passes, but **a default run never
builds it**, so citing it as standing coverage overstates what the routine
command verifies. Any arm this design cites must name the exact invocation that
runs it, features included.

The full gated list, for the same reason: `compio-postgres::tls_live` and
`unix_socket_live`; `zeroship-control::live_db` and `workflow_engine_test`;
`zeroship-migrate-adapter::platform_migrate`;
`zeroship-migrated::apply_api_test`. AGENTS.md already documents this trap for
`compio-postgres` and for the clippy gate; it is the same mechanism, and it is
not documented for plugin-db.

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
`required-features` gap; it is a dated instance of it, found by finally running
the target.

`distributed_live` fails differently - `anchor readiness failed: status=500`
from a worker it spins up - and is **not** claimed here as a code defect: the
database it ran against has an empty `zeroship` schema, so a provisioning cause
is at least as likely. It is recorded as unresolved rather than attributed,
which is the honest state. This is the same shape AGENTS.md already documents
for `compio-postgres` (`default = []` making a run feature-blind) and for the
clippy gate (a target whose required-features are unmet is filtered out of the
expectation rather than counted as unlinted). It recurs because the failure is a
*smaller* run, and a smaller run prints a smaller green, not a red.

### The skip that counts as a pass

Ten tests guard on `pg_has_logical_wal(&pool)` and, when false, call `skip(...)`
and `return` (`crates/zeroship-plugin-db/tests/integration.rs:2045` and nine
siblings). A server with `wal_level=replica` therefore produces a run whose
totals are **identical** to one where all ten passed. Measured 2026-08-27: the
canonical test port `127.0.0.1:5440` was held by an unrelated container running
`wal_level=replica` and `max_prepared_transactions=0` - both of the settings
`tests/provision_test_backends.sh` calls load-bearing.

The provisioning script had a check for exactly this and it was bound to the
wrong container: it interrogated the *compose-managed* postgres, which is empty
in precisely the case it was written for (a hand-started container squatting the
port), so it fell through to a generic ownership warning and reported no
settings at all. Fixed 2026-08-27 to follow the port rather than the compose
project; the before/after was confirmed by mutation.

### The arm that aborts the binary

A third way, found by finally running the thing.
`per_app_role_cannot_read_sibling_schema_or_touch_slots` - a role-isolation
*security* test - overflows its thread stack and takes the whole process down
with `SIGABRT` at test 77 of 99, so the 22 tests after it never run either.
Diagnosed 2026-08-27: it is an oversized async future in a debug build, not
recursion. `RUST_MIN_STACK=33554432` makes the same test pass unchanged, which
both identifies the cause and gives the workaround.

**Pre-existing, established by control rather than asserted.** The obvious
reading - "the branch doing this work broke it" - was checked instead of
assumed: a detached worktree at `main`, built from scratch, reproduces the
identical `SIGABRT` in the same test. Worth the cost, because the cheap
inference ran the other way and would have sent someone hunting a regression in
a branch that only touched three files, none of them in `auth::bootstrap`.

### And the same blindness covers LINTS, doubly

Measured 2026-08-27: the crate this design is about held **two standing
deny-level clippy errors** that no routine command could surface, hidden behind
two independent mechanisms. First, a deny-level lint elsewhere - eight `doc list
item without indentation` errors in `zeroship-migrate-policy`, untouched by this
branch - **aborts the workspace run before plugin-db is ever scheduled**, so the
crate lints as neither pass nor fail but as absent. Second, both of plugin-db's
own errors live in test targets that only exist under `--all-targets
--all-features`, which the `required-features` gating above makes a rare
invocation.

The errors themselves were small - a bare `std::env::var_os("RUST_LOG")` where
the workspace's `disallowed_methods` lint wants the typed declared-env accessor,
and a doc line beginning `- ` that clippy reads as an unindented list item. Both
are now fixed and `cargo clippy -p zeroship-plugin-db --all-targets
--all-features --no-deps` exits 0. The size of the errors is not the point: **a
crate that cannot be linted accumulates them silently**, and AGENTS.md already
records this exact pattern biting once before, when the clippy gate reported
"148 of 148" on a workspace declaring 158 targets and the ten missing ones
included a crate holding eleven standing deny-level errors.

## The rule, and the invocation this design's acceptance work must use

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

**SC-1 carries a second invocation, and it is not a duplicate of this one.** The
command above targets `--test integration`; SC-1's acceptance arms run under
`--test native_transaction`, which is separately gated by the same
`required-features` mechanism. Two targets, two commands; naming one does not
discharge the other.

## A fifth: the arm that already passed before the work began

Moved here from SC-5 on 2026-08-27, because it is a mechanism rather than a
service-ownership decision. **Its citations were re-derived against the tree
before the move**, and all three had gone stale - the `lib.rs` range, the
`context.rs` line, and both symbol names. A finding whose named evidence no
longer exists would have imported a stale claim into the most durable document
in the set. The corrections are recorded in place below.

**The sharing arm was already satisfied, and an implementation claiming to fix
it has now been written and had to be corrected.** `DbPlugin` carries `url`,
`worker_id` and `meter` - **no backend and no pool**
(`crates/zeroship-plugin-db/src/lib.rs:301-311`; SC-5 cited
`crates/zeroship-plugin-db/src/lib.rs:311-320`, which is now the `Debug` impl).
The pool and backend live in the thread-local
`THREAD_DB_CTX`, so two isolates on one OS thread have always shared them.
Minting a fresh `DbPlugin` per `build_runtime` never produced two backends.

A commit on the implementation branch nonetheless described itself as making
"two isolates on one thread share a backend instead of each resolving their
own". That is false. What the change actually buys is avoiding the re-mint of
the plugin vector and its `Arc`s - and one S3 client construction - per isolate
build. Worth having, an order of magnitude less than claimed, and a reminder
that this arm reads as if it is about backends when the code makes it about
allocations.

**Every arm must be checked for the inverse failure too: an arm that already
passes on today's code proves nothing about the change.** Two arms in an earlier
draft of SC-5's acceptance list did exactly that, and the reason is worth
stating because it will catch the next author as well: the context is declared
`thread_local!` while its own doc comment called it "the per-isolate DB context"
and its type was named `IsolateDbContext`. It is per-**thread**. So two isolates
on one thread *already* share one context, and an arm asserting they share one
slot passes before the work is done.

**That citation has now been re-derived twice, and SC-5's own note about the
first re-derivation is worth keeping.** SC-5 cited
`crates/zeroship-plugin-db/src/context.rs:956` and recorded that "the
declaration moved when the frame-effect work landed, and this citation pointed
at unrelated prose until it was re-derived". It moved again, and the names moved
with it: the declaration is now `THREAD_DB_CTX`
(`crates/zeroship-plugin-db/src/context.rs:977`), the type is `ThreadDbContext`
(`:133`), and the doc comment now says "The DB context shared by all isolates on
this worker thread" - the opposite of what SC-5 quoted. `ISOLATE_CTX` and
`IsolateDbContext` no longer occur anywhere under `crates/`. The renaming landed
as part of the L10 fix, where the false name was identified as the thing that
made the bug survive review. The arm is still the wrong arm; what changed is
that the code no longer misleads the author writing it - and that a line-number
citation into this file has now gone stale twice in two weeks, which is its own
small lesson about what a `file:line` buys.

---

## Class 6: the guard whose SUBJECT was deleted (found 2026-08-28, in this
## document set's own instrument)

The 12-arm verification harness
(`~/.claude/harnesses/dbbind_verify_impl.sh`) contains this, and it was added
for a real reason - it is the direct response to regression 1, where a
long-lived database still carrying `__zeroship_admin` reported green over a
commit that had deleted the schema's callers:

```sh
PG_DB="zs_verify_$$"
... -c "DROP DATABASE IF EXISTS $PG_DB" -c "CREATE DATABASE $PG_DB"
stale=$(... -d "$PG_DB" -tAc
  "SELECT count(*) FROM pg_namespace WHERE nspname = '__zeroship_admin'")
if [ "$stale" != "0" ]; then echo "REFUSING: ..."; exit 1; fi
```

Its comment says *"A stale `__zeroship_admin` is the exact contamination this
block exists to stop"*, which reads as a guard against database REUSE. **It is
not, and it cannot be.** The database is created fresh two lines above the
check, so the only way the count is non-zero is if `template1` itself carries
the schema. And since decision 7 deleted `__zeroship_admin` outright
(`390f4b97b`), nothing in the tree can install it into `template1` either.

**So the arm now rules on a condition that our own code can no longer produce.**
This is not the same failure as the five classes above - the guard is not
vacuous by construction, and template contamination is a genuine if narrow
condition. It is a **third** shape worth naming: a guard that was live, fired
once, and then had its subject removed by a later decision, leaving an arm that
still runs, still passes, and still reads in review as protection.

The correct response is not deletion. It is the gate-arm convention applied
honestly: **re-derive what the arm rules on and say so**, because the sentence
in the comment is now the load-bearing false claim, not the code. Its floor,
under `tests/lib/gate_arms.sh` terms, is a count of databases inspected, and
that count is 1 - a database the harness created itself.

### The version the harness cannot see

Separately and more consequentially for the TDD phase: the harness pins
**PostgreSQL 16.14** (`zs-adminfix-pg-5471`, `server_version_num=160014`), while
the platform corpus was verified end to end on **17** and two **18.4** servers
are running locally (`zs-cpg-pg18-5459`, `zero-migrate-postgres-1`). The flip
and the IR both touch `RETURNING` shape, index storage options and generated
columns, all of which have version-sensitive behaviour.

A cross-version arm is therefore available on **two genuinely distinct servers**
- which is the part usually faked. `AGENTS.md` records the precedent: a
cross-version verdict was published from runs where a feature flag had silently
redirected the DSN, so "both versions agreed perfectly" because both were one
server. Any arm added here must print the server it reached, not the variable it
read.
