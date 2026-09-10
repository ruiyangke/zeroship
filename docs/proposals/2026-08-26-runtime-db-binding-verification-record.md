# Verification record: how a green run can rule on nothing

Seven ways a test in this tree reported something other than a red while
something was wrong. Every class below was measured here, on a dated run, with
the instance that found it. None of this is a general warning about testing;
each is a mechanism that fired in this repository, and a class without its
instance is a platitude, so the instances are the content.

**This document does not expire.** The defect register empties as fixes land and
the design is rewritten as open questions settle. These classes are properties
of the repository's tooling and test layout, and they will fire again.

**How to use it.** Before citing any acceptance arm as evidence, put its class
question to it. An arm is evidence only if it was **built, ran, and ruled on the
thing its name claims** - and this document exists because "it passed" turned
out to be compatible with none of those being true.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

| # | class | the question that finds it |
| --- | --- | --- |
| 1 | the arm that **cannot pass** | On what implementation would this ever be green? |
| 2 | the arm that **cannot fail** | What implementation would make this red? |
| 3 | the arm whose **verdict is noise** | How many consecutive clean runs has this suite had? |
| 4 | the arm that **never ran** | Which exact invocation builds it, and did the binary reach the end? |
| 5 | the arm that was **already green** | Does this pass on the tree *before* the change? |
| 6 | the guard whose **subject was deleted** | Can our code still produce the condition this rules out? |
| 7 | the fixture that **cannot reach the claim** | What state does this fixture make impossible? |

---

## 1. The arm that cannot pass

The arm is written against a property the engine does not have, so no correct
implementation satisfies it. It is a specification defect wearing a test's
clothes, and its cost is that an implementer will weaken working code trying to
reach it.

Two instances, both from SC-2:

- An "ops" arm for SQLite autocommit concurrency. SQLite has one writer per
  database on any number of connections, so an arm demanding concurrent write
  throughput cannot pass.
- An earlier acceptance row required a dropped operation to roll back
  unconditionally. The actor may commit and send its reply before the caller
  ever polls, and nothing dropped afterwards can un-commit it.

**Ask where an arm passes before writing it, not after.** Both of these were
caught by reading the acceptance row against the engine's actual guarantees;
neither would have been caught by running anything.

## 2. The arm that cannot fail

Vacuous by construction: it is green on the code it was written for, green on a
broken implementation, and green on no implementation at all.

- **L13** is the plainest: a cap enforced only on a path production never calls,
  whose test calls that path directly. The cap is green and dead simultaneously.
- **SC-3** names the shape outright for an arm that greps for a path which was
  never created - it matches nothing and reports success, so it "passes today,
  passes if the refactor is abandoned, and passes if the file is deleted later".
  The same shape appeared three times across this document set.
- **SC-1** raises it twice more: once for a success-path arm, and once for a
  durability half that is worse than cannot-fail-in-principle because it is
  already written.
- The design's **SC-6** row states the hazard for a deny-only ceiling arm, which
  passes on an implementation where the ceiling read is broken and everything is
  denied.

The repository's standing defence is the gate-arm convention
(`tests/lib/gate_arms.sh`): every arm declares the number of items **that arm
ruled on** and a floor that number must clear. It catches this class and only
this class - see class 7 for what it cannot catch.

## 3. The arm whose verdict is noise

An arm running in a racing suite produces no consistent signal in either
direction.

`zeroship-data-v8`'s tests publish into a process-global broker registry and a
process-global suppression map, and cargo runs them on parallel threads. A dozen
of them shared the app ids `"myapp"`, `"xapp"` and `"app_active"`, so tests
popped one another's events; the shared `reset_world` helper called
`drop_app(None)`, tearing down every app's subscriptions process-wide, and
decremented suppression refcounts for keys belonging to other tests - while its
own doc comment described all of this as "thread-local state". Measured: **3
failed runs in 12** on unmodified code, **0 in 20** after scoping each test to
its own app id.

Two consequences beyond the flakiness itself:

**It defeats mutation testing, which is this design's primary tool for judging an
arm.** "Break the code, confirm the arm goes red" is indistinguishable from a
flake, and "restore it, confirm green" is indistinguishable from a flake going
the other way. Every mutation result recorded against a racing suite is `n=1` on
a noisy channel.

**The cause is the same one the design is about.** Per-app entries in
process-global maps with no per-owner scoping - the test-side instance of the
flat-global-keyed-by-app-id shape that the cache-bound and L10 sections identify
in the production code. The tests raced for the same structural reason the
runtime leaks and mis-invalidates.

A flakiness instrument is itself subject to every class here. The first one used
grepped for failing test names matching `exec::` or `crud::`, so a whole second
family of failures in `wal_consumer::` printed as unexplained nameless failures
and the rate it reported was the rate of the subset it could see. **Count every
failure, not the ones a name filter admits.**

**Standing requirement: an acceptance arm may only be cited as evidence if the
suite it runs in has been shown stable over repeated runs** - a stated number of
consecutive clean runs, recorded next to the arm. Not "it passed", not "it
passed twice".

## 4. The arm that never ran

Five distinct mechanisms in this repository let a test report nothing while
looking like a pass. Not built, filtered out, skipped, aborted partway, and
deliberately excluded are five different ways of printing something other than a
red, and all five were hit while implementing against this design.

### `required-features` filters the target out, silently

`cargo test -p zeroship-data-v8` does not build the `integration` target at
all, because that target declares `required-features = ["test-helpers"]`. Cargo
does not warn - it filters the target out. Every `--lib` run in this session
reported "638 passed" while the entire integration suite, including the
logical-decoding tests this design depends on, was never compiled.

Derived from `cargo metadata` on 2026-08-28: **10 of 170 test/bench targets in
the workspace are gated this way, and five of them are in
`zeroship-data-v8`** - `integration`, `missing_role`, `native_transaction`,
`sqlite_integration` (all `test-helpers`) and `distributed_live`
(`live-db-tests`). A default run of this design's own crate builds none of them.
The other five: `compio-postgres::tls_live` and `unix_socket_live`,
`zeroship-control::live_db` and `workflow_engine_test`,
`zeroship-migrate-server::apply_api_test`.

That has a direct consequence for the citations in this set. SC-1's invariant 14
cites the L8 regression test as standing coverage of the poisoned-commit case.
The test is real and it passes - it is
`commit_that_postgres_rolled_back_must_not_report_success_l8`
(`crates/zeroship-data-v8/tests/native_transaction.rs`) - but
`native_transaction` is one of the five, so **a default run never builds it**.
**Any arm this design cites must name the exact invocation that runs it, features
included.**

AGENTS.md documents this trap for `compio-postgres` (`default = []` making a run
feature-blind) and for the clippy gate (a target whose required-features are
unmet is filtered out of the expectation rather than counted as unlinted). It is
the same mechanism and it recurs because the failure is a *smaller* run, and a
smaller run prints a smaller green, not a red.

**All five plugin-db targets were run for the first time on 2026-08-27** - the
baseline the TDD phase starts from, and it did not exist before:

| target | features | result |
| --- | --- | --- |
| `integration` | `test-helpers` | 93 passed, 0 failed, 6 ignored |
| `sqlite_integration` | `test-helpers` | 124 passed, 0 failed |
| `native_transaction` | `test-helpers` | 13 passed, 0 failed |
| `missing_role` | `test-helpers` | 2 passed, **1 failed** |
| `distributed_live` | `live-db-tests` | 0 passed, **1 failed** |

**232 tests that no default command builds, and both failures were real.**
`pool_reconnect_missing_app_shaped_login_role_stays_internal` failed on `warm-up
after_connect left connection 1 unusable`; that string entered the tree in
`07905dde2` ("fix(postgres): recheck entries after lifecycle hooks",
2026-08-26), an ancestor of `main`. A pool fix landed, broke a test exercising
`max_lifetime(ZERO)` warm-up, and no routine command could observe it for a day
(fixed in `1b3ad7ade`). `distributed_live` failed on `anchor readiness failed:
status=500`, which turned out to be a missing migration-owned publication after
publication ownership moved out of the worker on 2026-08-16 - **silently red for
eleven days** (L27, fixed in `a0074e154`; the full attribution is in
`2026-08-26-runtime-db-binding-defects-closed.md`).

### The skip that counts as a pass

Ten tests guard on `pg_has_logical_wal(&pool)` and, when false, call `skip(...)`
and `return` (`crates/zeroship-data-v8/tests/integration.rs` and nine
siblings). A server with `wal_level=replica` produces a run whose totals are
**identical** to one where all ten passed. Measured 2026-08-27: the canonical
test port `127.0.0.1:5440` was held by an unrelated container running
`wal_level=replica` and `max_prepared_transactions=0` - both of the settings
`tests/provision_test_backends.sh` calls load-bearing.

The provisioning script had a check for exactly this and it was bound to the
wrong container: it interrogated the *compose-managed* postgres, which is empty
in precisely the case it was written for (a hand-started container squatting the
port), so it fell through to a generic ownership warning and reported no
settings at all. It now follows the port rather than the compose project.

### The arm that aborts the binary

`per_app_role_cannot_read_sibling_schema_or_touch_slots` - a role-isolation
*security* test - overflows its thread stack and takes the whole process down
with `SIGABRT` at test 77 of 99, so the 22 tests after it never run either. The
cause is an oversized async future in a debug build, not recursion:
`RUST_MIN_STACK=33554432` makes the same test pass unchanged.

Pre-existing, established by control rather than asserted: a detached worktree
at `main`, built from scratch, reproduces the identical `SIGABRT` in the same
test. The cheap inference - "the branch doing this work broke it" - would have
sent someone hunting a regression in a branch that touched three files, none of
them in `auth::bootstrap`.

### The omission that is named and still not safe

`verify_impl.sh` *deliberately excluded* `distributed_live`, printing
`distributed_live NOT RUN (needs --features live-db-tests + a multi-node
fixture)` and then printing **`all arms green`** as its final verdict. Both
statements were true and the combination was misleading, because the summary
line does not carry the caveat. **A named blind spot is legible, not safe.** The
target now runs as a twelfth arm with floor 0; a red arm you can see beats a
skipped arm you cannot.

### The same blindness covers lints, doubly

Measured 2026-08-27: `zeroship-data-v8` held **two standing deny-level clippy
errors** that no routine command could surface, hidden behind two independent
mechanisms. A deny-level lint elsewhere - eight `doc list item without
indentation` errors in `zeroship-migrate-policy`, untouched by this branch -
**aborts the workspace run before plugin-db is ever scheduled**, so the crate
lints as neither pass nor fail but as absent. And both of plugin-db's own errors
live in test targets that only exist under `--all-targets --all-features`, which
the `required-features` gating above makes a rare invocation.

The errors themselves were small: a bare `std::env::var_os("RUST_LOG")` where the
workspace's `disallowed_methods` lint wants the typed declared-env accessor, and
a doc line beginning `- ` that clippy reads as an unindented list item. Both are
fixed and `cargo clippy -p zeroship-data-v8 --all-targets --all-features
--no-deps` exits 0. Their size is not the point: **a crate that cannot be linted
accumulates them silently.** AGENTS.md records the same pattern biting before,
when the clippy gate reported "148 of 148" on a workspace declaring 158 targets
and the ten missing ones included a crate holding eleven standing deny-level
errors.

### The invocation this design's acceptance work must use

```text
RUST_MIN_STACK=33554432 \
cargo test -p zeroship-data-v8 --test integration --features test-helpers \
  -- --test-threads=1
```

Each flag earns its place. `--features test-helpers` or the target is filtered
out unbuilt. `--test-threads=1` or 32 of 99 fail racing `CREATE SCHEMA
plugin_db_test` against one server. `RUST_MIN_STACK` or the run aborts at test
77. And the server it points at needs `wal_level=logical` and a nonzero
`max_prepared_transactions`, or ten more tests skip while counting as passes.
**Five conditions, four of them silent when unmet.**

**SC-1 carries a second invocation and it is not a duplicate of this one.** The
command above targets `--test integration`; SC-1's acceptance arms run under
`--test native_transaction`, separately gated by the same mechanism. Two
targets, two commands; naming one does not discharge the other.

## 5. The arm that was already green before the work began

An arm that passes on today's code proves nothing about the change. This is the
inverse of class 2 - the arm is not vacuous, it is just already satisfied - and
it is generally caused by the code lying about its own scope.

**The sharing arm was already satisfied.** `DbPlugin` carries `url`,
`worker_id`, `meter`, a `resource_key` and a `backend: BackendUrl` *decision* -
**no pool and no backend handle** (`crates/zeroship-data-v8/src/lib.rs`, the
`DbPlugin` struct). The pool and the `PostgresBackend` live in the thread-local
`THREAD_DB_CTX` / `ThreadDbContext` (`crates/zeroship-data-v8/src/context.rs`;
cite the symbols, not lines - the declaration moves whenever that file is
touched). Two isolates on one OS thread have therefore always shared them, and
minting a fresh `DbPlugin` per `build_runtime` never produced two backends.

A commit on the implementation branch nonetheless described itself as making
"two isolates on one thread share a backend instead of each resolving their
own". That is false. What the change actually buys is avoiding the re-mint of
the plugin vector and its `Arc`s, and one S3 client construction, per isolate
build. Worth having, an order of magnitude less than claimed.

**The name is what let two such arms into an earlier SC-5 acceptance list.** The
context was declared `thread_local!` while its own doc comment called it "the
per-isolate DB context" and its type was named `IsolateDbContext`. It is
per-**thread**, so two isolates on one thread already shared one context and an
arm asserting they share one slot passed before the work was done. The rename to
`THREAD_DB_CTX` / `ThreadDbContext` landed as part of the L10 fix, where the
false name was identified as the thing that made the bug survive review; the doc
comment now reads "The DB context shared by all isolates on this worker thread".
`ISOLATE_CTX` and `IsolateDbContext` no longer occur under `crates/`. The arm is
still the wrong arm; what changed is that the code no longer misleads the author
writing it.

## 6. The guard whose subject was deleted

A guard that was live, fired once, and then had its subject removed by a later
decision - leaving an arm that still runs, still passes, and still reads in
review as protection. It is not vacuous by construction, which is what
distinguishes it from class 2.

The 12-arm verification harness (`~/.claude/harnesses/dbbind_verify_impl.sh`)
contains this, added as the direct response to a regression where a long-lived
database still carrying `__zeroship_admin` reported green over a commit that had
deleted the schema's callers:

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
check, so the only way the count is non-zero is if `template1` itself carries the
schema - and since decision 7 deleted `__zeroship_admin` outright (`390f4b97b`),
nothing in the tree can install it into `template1` either.

**The correct response is not deletion.** Template contamination is a genuine if
narrow condition, so the arm still rules on something. It is the sentence in the
comment that is now the load-bearing false claim, not the code. Apply the
gate-arm convention honestly: **re-derive what the arm rules on and say so.** Its
floor, in `tests/lib/gate_arms.sh` terms, is a count of databases inspected, and
that count is 1 - a database the harness created itself.

## 7. The fixture that cannot reach the claim

The hardest class to see, because the test **does** exercise real production
code, its assertions **are** true, and its name is **accurate about what it
asserts**. What is wrong is the fixture: it cannot construct the state the
surrounding claim is about.

Three instances, all found 2026-08-28, all in code that had been reviewed:

| the claim | the fixture | what it could not reach |
| --- | --- | --- |
| `updateMany` refuses over `MAX_QUERY_LIMIT` | updates `{ ssn: ... }` on a randomised-**encrypted** schema (`crates/zeroship-data-v8/tests/sqlite_integration.rs`) | the cap sits inside `if per_row_encrypted_update` (`crud/mod.rs:1233`). `ssn` being encrypted is exactly what routes onto the **guarded** branch. The unguarded branch has no cap and renders an unbounded whole-table `UPDATE` |
| CDC events carry the masked value for masked columns (`broker.rs`, `cdc_event_carries_masked_value_for_masked_columns`) | parent column is `"\\x0123..."`, a BYTEA **ciphertext** literal | the leaking shape is a **mask-only** field, whose parent holds plaintext. The same assertion would fail on it; no fixture builds one |
| SC-2 Decision 1: "WAL permits this concurrency" | unqualified `CREATE TABLE t` (`sqlite_integration.rs:9956`), and no `ATTACH` at all | the table lands in `main`, the WAL **control** database. Every app file is pinned to DELETE (`zeroship-migrate-sqlite/src/backend/actor.rs:719-729`). The mechanism was proved on the wrong database |

**What makes this distinct from the other six.** A vacuous arm rules on zero
items and a floor catches it. A wrong assertion is wrong on its own terms and a
careful reader catches it. Here the arm rules on a real item, truthfully, and a
careful reader **agrees with it** - because the reader checks whether the
assertion follows from the fixture, which it does. Nobody checks whether the
fixture can reach the case the *name* implies.

**The name is what does the damage.** All three names describe the general
property (`..._cap_rejects_...`,
`cdc_event_carries_masked_value_for_masked_columns`,
`an_autocommit_read_proceeds_while_the_app_holds_an_open_transaction`) while the
fixture covers one sub-case - and in all three the sub-case chosen was the
**safe** one. A later reader greps the name, finds a green test, and stops.

**The question that finds it is not any of the usual ones**: *what state does
this fixture make impossible?* Not "does it pass", not "is the assertion right",
not even "is the code reached" - the code IS reached. Ask what the fixture
excludes, then ask whether the excluded state is the one the claim is about.

**A gate-arm floor cannot catch this**, which is worth stating because that
convention is this repository's main defence. A floor counts items ruled on, and
these arms rule on a real item. The countermeasure is narrower: **a test whose
name states a general property owes either a fixture matrix over the property's
cases, or a comment naming the cases it does not cover.** The third instance now
carries exactly that, plus a second arm
(`an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal`) pinning the
excluded state so it fails loudly if it changes.

---

## Cross-version arms: print the server, not the variable

The 12-arm harness pins **PostgreSQL 16.14** (`zs-adminfix-pg-5471`,
`server_version_num=160014`), while the platform corpus was verified end to end
on 17 and two **18.4** servers run locally (`zs-cpg-pg18-5459`,
`zero-migrate-postgres-1`). The flip and the IR both touch `RETURNING` shape,
index storage options and generated columns, all of which have version-sensitive
behaviour, so anything the harness proves is proved on 16 only.

A cross-version arm is therefore available on **two genuinely distinct servers**,
which is the part usually faked. AGENTS.md records the precedent: a
cross-version verdict was published from runs where a feature flag had silently
redirected the DSN, so "both versions agreed perfectly" because both were one
server. **Any cross-version arm added here must print the server it reached, not
the variable it read.**
