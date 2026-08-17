# s40: worker platform-write grant vs the platform-write invariant

Scratch notes. Committed as findings land, before a conclusion exists.

## The collision (restated from the brief, both halves re-verified here)

- `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:735-766` asserts
  `zeroship_worker` holds no INSERT/UPDATE/DELETE/TRUNCATE/REFERENCES/TRIGGER on
  ANY relation in schema `zeroship`, no column-level equivalent, and no
  USAGE/UPDATE on any sequence there. Failure string at :766 is
  "zeroship_worker can write a platform relation".
- `db/migrations-ts/20260816000100_service_assertion_replay.ts:85-93` grants
  select/insert/update/delete on `zeroship.service_assertion_replay` to
  `zeroship_control`, `zeroship_gateway`, `zeroship_worker`, `zeroship_auth`.

## Finding 1: nothing verifies service assertions in production code TODAY

Grep over `crates/ libs/` for `ServiceAssertionVerifier` / `PostgresReplayStore`
/ `ReplayStore`:

- `ServiceAssertionVerifier::new` is constructed ONLY in
  `crates/core/tests/service_assertion_test.rs` and
  `crates/authn/tests/service_replay_pg_test.rs`.
- `PostgresReplayStore` is referenced ONLY in `crates/authn/src/service_replay.rs`
  (its definition) and `crates/authn/tests/service_replay_pg_test.rs`.
- No service binary (`crates/{worker,gateway,control,auth}`) constructs either.

So the grant list is ANTICIPATORY. That alone does not settle it: the question
is whether the worker is an intended verifier, not whether it is a wired one.

## Finding 2: the design says the worker IS a callee, so option (c) is dead

`docs/proposals/2026-08-16-service-identity.md` section 5.2 is the measured
per-caller allowlist. Read as "caller | endpoints it may call":

- :180 `edge/gateway` -> `worker POST /dispatch/{app}`, worker workflow routes.
- :177 `core/control` -> "plus worker calls".

The worker is therefore a CALLEE of both gateway and control, and a callee is
what verifies. The worker is also replicated by construction (`docker compose
--scale worker=10`, and section 12 targets ~1000 replicas/service), so
`InMemoryReplayStore` is explicitly insufficient for it -
`crates/core/src/service_assertion.rs:51-55` says an in-memory store is NOT
sufficient for a replicated callee because "single use" degrades to "single use
per replica".

Conclusion: the `zeroship_worker` entry in that grant is NOT unnecessary.
Deleting it would not strengthen the boundary, it would break the worker's
verifier the day it is wired - the exact failure mode the migration's own
comment at :63-72 describes for the withheld-SELECT case.

=> Option (c) REJECTED on evidence. Proceeding to option (a): move the table out
of the `zeroship` schema.

## Finding 3: the second failure does NOT share the cause - it is a stale literal

`ordered_runner_retains_authored_fk_formats_across_catalog_refresh` fails inside
`run_logical_column_retention_assertions`, at
`crates/zeroship-migrate-adapter/tests/platform_migrate.rs:848`:

    if report.files != 12 {

That helper runs `run_platform_migrations` over the FULL `migrations_dir()`, so
`report.files` is the whole corpus. `db/migrations-ts/` holds 23 files today.

The literal was CORRECT when written: `abd1e70d7` (2026-08-06,
"test(migrate-adapter): cover authored fk formats across a refresh") and
`git ls-tree --name-only abd1e70d7 db/migrations-ts/ | wc -l` = 12. Eleven files
have landed since. The same file already has the maintained constant
`PLATFORM_MIGRATION_FILES = 23` (:42) which every other count-check uses; this
one call site duplicated the value as a literal and rotted.

So of the three platform_migrate failures the brief measured, at most one is the
grant collision. Fix is `PLATFORM_MIGRATION_FILES` in place of the literal, so
there is no second copy left to rot.

Third failure (`platform_migrate_applies_only_newly_appended_file`) uses
`PLATFORM_MIGRATION_FILES` throughout and has no stale literal; cause still to be
measured against a live DSN.

MEASURED BASELINE (no source changes, live DSN
`postgresql://postgres:zeroship@localhost:5440/postgres`):

    test result: FAILED. 5 passed; 3 failed; finished in 137.04s

and the three failures have THREE DIFFERENT causes, one each:

1. `apply_all_platform_migrations_to_fresh_db` -- :436
   `"zeroship_worker can write a platform relation"`. The grant collision.
2. `ordered_runner_retains_authored_fk_formats_across_catalog_refresh` -- :835
   `"expected 12 files, saw 23"`. The stale literal of finding 3, confirmed.
3. `platform_migrate_applies_only_newly_appended_file` -- :1122
   `"checksum drift on mig_0000595bcDNs774MyYTiwC"`.

So NONE of the other two share the grant cause.

## Finding 5: the third failure is a probe filename that stopped sorting last

`APPEND_FILENAME` was `20260710000100_append_probe.ts`. The test writes it into a
copy of the corpus to stand for a migration added today, and the runner derives a
migration's stable version from its ORDINAL in the sorted set. Ten committed
files sort after that date now -- every `202608*`, the first landing 2026-08-11 --
so the probe lands MID-corpus and shifts the version of every file after it. Run
2 then checks the journal's checksum for a version against a different file's
body, and the verdict is checksum drift, which reads as a corrupted journal and
says nothing about ordering.

Fixed by naming it `29991231000000_append_probe.ts` (far-future so it does not
have to move whenever a migration lands) plus `assert_probe_sorts_last`, which
states the requirement where it can be acted on instead of letting it resurface
as drift.

Note all three are staleness of a different kind, and all three were invisible
for the same reason: with no DSN the binary self-skipped and counted as passed.

## RESULT

Same command, same DSN, after the change:

    test result: ok. 8 passed; 0 failed; finished in 121.32s

against the baseline's `5 passed; 3 failed`. The binary was rebuilt at 10:56:30
against sources last edited 10:55:42, checked by mtime rather than assumed.

## Gate results as they land

- `cargo test -p zeroship-control --lib` -> `223 passed; 0 failed`, exit 0. The
  brief expected 224. Nothing in this change touches `crates/control`
  (`git diff main..HEAD --name-only | grep -c control` = 0), and the only path
  from here into control is `zeroship-authn`, where the diff is two SQL string
  literals and doc comments. Two commits already on main - `d5ad53352
  test(control)!: drop the removed AppState mint fields from every fixture` and
  `f6710f854 style(control): drop the now-redundant Secret import from the test
  module` - are the kind of change that moves that count, so 224 looks like a
  reading from before them rather than a regression here.
- `cargo build --workspace` -> exit 0.
- `cargo test --workspace --no-run` -> exit 101, and NOT from this change: every
  error is `couldn't read crates/runtime/tests/wpt/...: No such file or
  directory`, five of them, all under the WPT tree AGENTS.md documents as
  gitignored and fetched on demand by `crates/runtime/tests/setup-wpt.sh`. This
  worktree had never run it. Re-run after fetching.

## Incident: I disturbed the s4 agent's auth suite, and the fix is a private DB

`tests/run_auth_suite.sh` opens with `DROP DATABASE IF EXISTS zeroship_auth_test
WITH (FORCE)` on a FIXED name (:53, :77). An agent in `.worktrees/s4` was 15
minutes into its own run of that script when mine started and dropped the
database out from under it. Mine was killed as soon as I saw the peer process;
theirs may still have been damaged, and I cannot undo that.

`TEST_DB` is overridable (:53), so my re-run uses `zeroship_auth_test_s40` and
waits for the peer's process to exit first, because the migration also creates
CLUSTER-GLOBAL roles that a private database does not isolate.

Worth saying plainly: a suite whose first act is a forced DROP of a fixed-name
database is not safe to run concurrently, and nothing in it says so. Checking for
a peer process before running a suite is not a habit I had.

## What the change touches, and why each file is in it

- `db/migrations-ts/20260816000100_service_assertion_replay.ts` - the table moves
  to `service_authn`, which the migration now creates, plus USAGE for the four
  verifying roles and an explicit REVOKE CREATE. The table grant keeps its exact
  privilege and role lists.
- `crates/zeroship-migrate-adapter/policies/platform.policy.toml` - the three
  namespace-scoped grants gain `service_authn`, without which lowering refuses
  the table.
- `crates/zeroship-migrate-adapter/src/platform.rs` - the allowlist test is
  pinned by value to the new three, so a fourth namespace is a decision.
- `crates/zeroship-migrate-adapter/tests/platform_migrate.rs` - check (7) bounds
  the new zone; the stale `12` becomes the maintained constant; the append probe
  gets a name that sorts last plus a guard that says so when it stops.
- `crates/authn/src/service_replay.rs`, `crates/authn/tests/service_replay_pg_
  test.rs` - the qualified names, and a grant parser that selects the TABLE grant
  by its `kind: "table"` target rather than by being first in the file (the
  migration now has two grants, and the schema one spells `privileges: ["usage"]`
  the same way).
- `deploy/ops/postgres-init.sql`, `docs/proposals/2026-08-16-service-identity.md`
  - two places that said `zeroship` was the only system schema.

## Finding 4: option (a) costs a charter widening, and that is the whole price

`crates/zeroship-migrate-adapter/policies/platform.policy.toml` is the ceiling
the platform migrate path lowers under. Three of its grants -
`schema.cross_schema` (:69), `schema.create_table` (:74), `schema.rename` (:79) -
scope to `{ include = ["__ZEROSHIP_PROJECT_SCHEMA__", "public"] }`, and
`platform_effective` asserts the placeholder appears exactly three times
(`platform.rs:52`). The in-file test `platform_charter_retains_the_project_and_
public_schema_allowlist` (:70-75) pins the resulting allowlist to exactly
`["public", "zeroship"]`.

So a table in a NEW schema is refused by the lowering guard until that schema is
added to the charter. Option (a) is therefore: new schema + charter widening +
USAGE grants + the four call sites that name the table.

The widening is what makes option (a) honest rather than free, and it needs a
counterweight: once a third schema is on the allowlist, any later platform
migration can put a table there and hand the worker writes on it, which would
reintroduce the collision one namespace over. So the move is paired with a NEW
assertion that the new schema holds EXACTLY the replay table and that the worker
has no CREATE on it. That bounds the second zone instead of merely relocating
into an unbounded one - the `zeroship` invariant is untouched and stays blanket.

## Decision: option (a), with the new schema bounded by its own assertion

Schema name `service_authn`: it names the trust zone (state of the
service-to-service authentication MECHANISM), not the product, so it cannot be
misread as a role - `zeroship_authn` sits one character from the existing role
`zeroship_auth`. AGENTS.md's system map already describes unprefixed peer schemas
("separate schemas (control, auth, per-app)").

Why this is a real boundary and not relabeling: the `zeroship` schema holds
platform STATE - apps, users, deploys, grants, billing - and authority over it is
authority over the platform, which is exactly what a process running creator code
must not have. The replay table holds no platform state: two columns, a key and
an expiry, conferring nothing, and the migration's own comment (:75-78) records
that a `jti` is not a credential. The writer sets differ too: `zeroship` is
written by the control plane, while the replay table is written by EVERY service
that authenticates, worker included. One schema was carrying two trust zones with
one grant policy; splitting them is the fix the collision was pointing at.
