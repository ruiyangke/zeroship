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
