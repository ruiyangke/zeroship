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
