# Migration crate survey

Date: 2026-08-27

> **THIS SURVEY'S ANSWER HAS BEEN OVERTAKEN (checked 2026-08-29).**
> `zeroship-migrate-adapter` was DELETED in `d3a35a2cc`. It is no longer a
> workspace member and nothing depends on it; the only trace left is a comment in
> `crates/zeroship-config-contract/Cargo.toml` recording that its entry used to be
> there. Every `crates/zeroship-migrate-adapter/...` citation below therefore
> resolves against nothing, and the conclusion in the next paragraph - that both
> crates are needed - is false as written. The survey is kept as the record of what
> was true on its own date; do not read it as current.

## Answer

`zeroship-migrate-server` and `zeroship-migrate-adapter` are both still needed today.
They do different jobs:

- `zeroship-migrate-server` is the deployed, long-running creator migration service. It
  authenticates an app migration request, composes the operator ceiling with the
  creator's policy draft, provisions the app's Postgres role/schema, and applies
  the app's frozen IR. Its HTTP routes are registered at
  `crates/zeroship-migrate-server/src/api.rs:20-47`, the apply route authenticates and
  enters the apply path at `crates/zeroship-migrate-server/src/api.rs:87-128`, and the
  binary starts that service at `crates/zeroship-migrate-server/src/main.rs:263-279`.
- `zeroship-migrate-adapter` is the native host integration between the
  driver-neutral migration engine and `compio-postgres`. It owns the
  `CompioPgSession` newtype and its `SqlSession` implementation
  (`crates/zeroship-migrate-adapter/src/lib.rs:135-145`,
  `crates/zeroship-migrate-adapter/src/lib.rs:454-507`). It also owns a second,
  independent responsibility: the platform-schema migration configuration,
  V8 authoring/orchestration, one-shot binary, and platform migration tests
  (`crates/zeroship-migrate-adapter/src/lib.rs:120-133`,
  `crates/zeroship-migrate-adapter/Cargo.toml:30-39`,
  `crates/zeroship-migrate-adapter/Cargo.toml:80-98`).

Recommendation: **KEEP the adapter and port the 22 stale test errors.** The same
API port must also cover the `platform-cli` production module and its feature-gated
test suite; deleting only the two default test targets would leave the deployed
platform runner on the same superseded API. In-sourcing removed a repository
boundary. It did not remove the Rust driver boundary or either shipped consumer.

## Survey method and scope

The consumer inventory came from these tree-wide searches:

```text
rg -n --glob Cargo.toml 'zeroship-migrate-adapter|zeroship-migrate-server' .
rg -n --glob '*.rs' '^\s*use zeroship_migrate_adapter' .
rg -n --glob '*.rs' '^\s*use zeroship_migrate_server' .
rg -n --glob '*.rs' 'zeroship_migrate_adapter::|zeroship_migrate_server::' crates
```

The platform-binary and guard searches were:

```text
rg -n 'zeroship-platform-migrate' deploy/scripts .github docs/runbooks tests
rg -n --glob package.json 'zeroship-platform-migrate' .
rg -n 'released_migrations\.tsv|unreleased_migrations_sort_after_every_released_one|released_platform_migrations_keep_their_released_bytes' crates/zeroship-migrate* tests deploy/scripts
```

The first command set is why the inventories below distinguish the root
`[workspace.dependencies]` catalog from actual dependency declarations. The root
only registers package paths at `Cargo.toml:224-245` and `Cargo.toml:265`; it is not
a consumer.

The Rust guard search found its implementation only in the adapter. The one
non-test mention in the engine-shaped paths is an adapter comment referring back
to the test (`crates/zeroship-migrate-adapter/src/platform.rs:1037-1044`). There is
no Rust released-ledger guard in `crates/zeroship-migrate/tests`,
`crates/zeroship-migrate-core`, `crates/zeroship-migrate-backend`, or
`crates/zeroship-migrate-postgres`.

The broader search also found an independent shell implementation and gate.
`deploy/scripts/deploy-remote.sh` implements released-byte/deletion and ordering
checks at `deploy/scripts/deploy-remote.sh:139-195`, and
`tests/deploy_scripts_gate.sh` feeds it a fake host journal derived from the real
snapshot at `tests/deploy_scripts_gate.sh:1439-1457`. That gate requires the
unchanged corpus to reach the roll at `tests/deploy_scripts_gate.sh:1460-1479`,
mutation-proves byte drift at `tests/deploy_scripts_gate.sh:1522-1554`, and
mutation-proves a mid-corpus insertion at
`tests/deploy_scripts_gate.sh:1556-1584`. CI runs it at
`.github/workflows/ci.yml:500-513`.

A targeted `platform-cli` check could not reach adapter code because the
gitignored SDK `dist` files embedded by `zeroship-runtime` are absent in this
worktree. CI documents and builds that prerequisite at
`.github/workflows/ci.yml:1067-1086`. I did not generate those files because the
task permits changing only this review. The platform API-drift finding below is
therefore based on direct source-to-current-API comparison, not a new compiler
error count.

## The deletion constraint: what survives and what does not

### The Rust guard is adapter-owned; a shell guard also exists

The checked-in deployed-journal snapshot is loaded by the Rust suite at
`crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2315-2361`. That suite
contains four distinct DB-free protections:

- released bytes may not change:
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2379-2422`;
- every released file must still exist:
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2424-2463`;
- every unreleased file must sort after the released prefix:
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2465-2523`; and
- the snapshot must be nonempty and every checksum must be 64 hexadecimal
  characters:
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2525-2582`.

The ordering function is exactly
`unreleased_migrations_sort_after_every_released_one` at
`crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2500-2523`. The test
target requires `platform-cli` (`crates/zeroship-migrate-adapter/Cargo.toml:84-98`),
and CI explicitly runs it with that feature
(`.github/workflows/ci.yml:728-738`).

The runtime half is also adapter-owned. `run_platform_migrations` reads the
corpus, opens `CompioPgSession`, and constructs the Postgres backend at
`crates/zeroship-migrate-adapter/src/platform.rs:873-905`. Before applying a new
file it detects journal-version ownership collisions at
`crates/zeroship-migrate-adapter/src/platform.rs:1019-1060`; its named
`VersionCollision` error and remediation are defined at
`crates/zeroship-migrate-adapter/src/platform.rs:145-168` and rendered at
`crates/zeroship-migrate-adapter/src/platform.rs:199-212`.

`deploy/scripts/deploy-remote.sh` says that the adapter test is the CI half at
`deploy/scripts/deploy-remote.sh:107-131`; its own functions compare the real
target database's journal at `deploy/scripts/deploy-remote.sh:142-195`, and it
runs those checks before the roll at `deploy/scripts/deploy-remote.sh:953-989`.
That comment understates current CI: `tests/deploy_scripts_gate.sh` also makes
the shell implementation DB-free by stubbing the host journal from the checked-in
snapshot (`tests/deploy_scripts_gate.sh:1439-1457`) and exercising the real deploy
script. It additionally tests post-roll snapshot refresh at
`tests/deploy_scripts_gate.sh:1700-1736`.

Therefore adapter deletion would remove this Rust copy of the guard and CI's
explicit Rust-suite invocation, but it would not leave the repository with no
DB-free released-ledger enforcement. The deploy-script gate would survive as
long as `deploy/scripts/deploy-remote.sh`, `tests/deploy_scripts_gate.sh`, and its
CI step remain.

### The binary is adapter-owned and deployed

The adapter declares `zeroship-platform-migrate` behind `platform-cli` at
`crates/zeroship-migrate-adapter/Cargo.toml:35-39` and
`crates/zeroship-migrate-adapter/Cargo.toml:80-82`. Its `main` imports the adapter
configuration and platform runner at
`crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:60-65`,
builds `PlatformMigrateConfig` at
`crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:122-128`,
and executes `run_platform_migrations` on a Compio runtime at
`crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:130-154`.

The production image builds the adapter with that feature and copies the binary
at `deploy/Dockerfile:184-192` and `deploy/Dockerfile:206-209`. Compose invokes it
as the `migrate` service command at
`deploy/compose/docker-compose.yml:157-174` and
`deploy/compose/docker-compose.yml:197-210`.

### What deletion would do

There is no existing destination for the adapter's native session, platform
runner, or binary. The in-sourced
`zeroship-migrate` crate explicitly defines itself as a featureless,
runtime-free composition root at `crates/zeroship-migrate/Cargo.toml:8-27`; its
Compio dependency is test-only at `crates/zeroship-migrate/Cargo.toml:81-90`, and
it declares no binary. The Postgres vendor crate says it implements the backend
contract without depending on the engine at
`crates/zeroship-migrate-postgres/Cargo.toml:1-6` and
`crates/zeroship-migrate-postgres/Cargo.toml:30-37`; its Compio dependency is
also test-only at `crates/zeroship-migrate-postgres/Cargo.toml:70-74`.

Therefore deleting the adapter today would:

1. break the shipped `zeroship-migrate-server` app-migration apply path;
2. remove the platform migration binary used by the production image and
   compose startup;
3. remove the Rust released-byte, released-file-presence, unreleased-order, and
   snapshot-shape tests, while leaving the independent deploy-script gate in
   place; and
4. remove the platform runner's database-local named collision check.

The shell gate is a real surviving counterpart to item 3. No current file or
crate takes over the native session, platform runner/binary, or database-local
collision check in items 1, 2, and 4. Those shipped responsibilities are enough
to require keeping and repairing the adapter, independent of duplicated guard
coverage or how much generic engine test behavior moved elsewhere.

## 1. Consumers of `zeroship-migrate-adapter`

### Cargo dependents

| Dependent | Kind | Evidence and actual use |
| --- | --- | --- |
| `zeroship-migrate-server` | Normal, shipped library dependency | `crates/zeroship-migrate-server/Cargo.toml:16-64`; production code imports `CompioPgSession` at `crates/zeroship-migrate-server/src/apply.rs:23`. |
| `zeroship-config-contract` | Normal dependency of a non-shipped tool | The tool declares itself non-shipped at `crates/zeroship-config-contract/Cargo.toml:1-15`, declares the adapter at `crates/zeroship-config-contract/Cargo.toml:17-33`, and reads `PlatformMigrateSettings::SPECS` at `crates/zeroship-config-contract/src/registry.rs:43-53`. |
| `zeroship-plugin-db` | Dev-dependency only | The entry is under `[dev-dependencies]` at `crates/zeroship-plugin-db/Cargo.toml:91-134`. There is no current `zeroship_migrate_adapter` reference in that crate's Rust source, so this is presently an unused test dependency, not a library consumer. |

The adapter's own binary and integration tests consume its library target but do
not create separate Cargo dependency edges.

### Every literal `use zeroship_migrate_adapter` statement

The anchored search returned exactly eight statements:

- shipped/production: the platform binary has two at
  `crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:62-65`,
  and `zeroship-migrate-server` has one at
  `crates/zeroship-migrate-server/src/apply.rs:23`;
- test-only: the platform suite has three at
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:52-58`, the V8 author
  test has one at
  `crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:41`, and the
  Compio smoke test has one at
  `crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:28`.

The config checker uses a fully qualified path rather than a `use` statement at
`crates/zeroship-config-contract/src/registry.rs:51`. The adapter platform module
uses its own crate-relative session type at
`crates/zeroship-migrate-adapter/src/platform.rs:86`.

## 2. Consumers of `zeroship-migrate-server` and division of labor

### Cargo dependents

| Dependent | Kind | Evidence and actual use |
| --- | --- | --- |
| `zeroship-config-contract` | Normal dependency of a non-shipped tool | `crates/zeroship-config-contract/Cargo.toml:17-27`; it reads `MigratedSettings::SPECS` at `crates/zeroship-config-contract/src/registry.rs:50`. |
| `zeroship-control` | Dev-dependency only | `crates/zeroship-control/Cargo.toml:111-141`; its test helper calls the migrated provisioning function at `crates/zeroship-control/tests/common/mod.rs:278-283`. Production control instead forwards over HTTP, as documented and implemented at `crates/zeroship-control/src/migrations_api.rs:1-5` and `crates/zeroship-control/src/migrations_api.rs:170-204`. |
| `zeroship-plugin-db` | Dev-dependency only | `crates/zeroship-plugin-db/Cargo.toml:91-143`; its test calls migrated's workflow-schema provisioning and constants at `crates/zeroship-plugin-db/tests/integration.rs:6295-6315` and `crates/zeroship-plugin-db/tests/integration.rs:6351-6359`. |

The package's own binary is a shipped consumer of its library: the binary target
is declared at `crates/zeroship-migrate-server/Cargo.toml:7-14`, and the image builds and
copies it at `deploy/Dockerfile:184-192` and `deploy/Dockerfile:210-223`.

### Every literal `use zeroship_migrate_server` statement

The anchored search returned exactly twelve statements, all inside the package:

- four in the shipped binary at `crates/zeroship-migrate-server/src/main.rs:13-16`;
- three in `health_endpoints_test` at
  `crates/zeroship-migrate-server/tests/health_endpoints_test.rs:18-20`;
- three top-level and two nested imports in `apply_api_test` at
  `crates/zeroship-migrate-server/tests/apply_api_test.rs:18-22` and
  `crates/zeroship-migrate-server/tests/apply_api_test.rs:1304-1305`.

The external test/tool consumers use fully qualified paths at the config-contract,
control, and plugin-db locations cited in the preceding table.

### Current division of labor

The AGENTS description of migrated as the managed-policy creator migration
service is accurate. The service owns:

- the creator-facing apply/approve/policy HTTP boundary
  (`crates/zeroship-migrate-server/src/api.rs:20-47`);
- operator-ceiling plus creator-draft composition, including escalation rejection
  (`crates/zeroship-migrate-server/src/policy.rs:1-25`,
  `crates/zeroship-migrate-server/src/policy.rs:119-142`);
- app role/schema provisioning over the raw Compio client
  (`crates/zeroship-migrate-server/src/provisioning.rs:1-15`,
  `crates/zeroship-migrate-server/src/provisioning.rs:91-105`); and
- current-API guarded lower and engine apply
  (`crates/zeroship-migrate-server/src/apply.rs:1085-1152`).

The adapter description is accurate about the architectural bridge and stale
about provenance. Its own documentation explains that the newtype maps the
engine's neutral `Bind`, `Value`, `Row`, and `DbError` types onto
`compio-postgres`, with no Node or Tokio in the loop
(`crates/zeroship-migrate-adapter/src/lib.rs:4-20`). The engine is no longer a
published/vendored submodule: the root now points all engine crates at workspace
paths at `Cargo.toml:225-245`. The adapter still depends on the engine through the
workspace at `crates/zeroship-migrate-adapter/Cargo.toml:41-52`.

Thus `AGENTS.md:123-124` should eventually be updated from "vendored" and
`third_party/zero-migrate` to the in-sourced paths, and from the old directory
name `migrated` to `zeroship-migrate-server`. The division of labor in those lines did
not outlive the code.

## 3. `CompioPgSession` reaches shipped binaries

`CompioPgSession` is not test-only. It is reachable from two shipped binaries.

### Creator migrations

The deployed `zeroship-migrate-server` binary has a normal adapter dependency
(`crates/zeroship-migrate-server/Cargo.toml:12-20`,
`crates/zeroship-migrate-server/Cargo.toml:64`). Its live HTTP apply path is:

```text
POST route
  -> apply_ir_documents
  -> apply_ir_documents_with_policy
  -> CompioPgSession::connect
  -> PostgresBackend::new_generic(&session)
  -> guarded lower and engine apply
```

The corresponding evidence is `crates/zeroship-migrate-server/src/api.rs:87-116`,
`crates/zeroship-migrate-server/src/apply.rs:235-260`,
`crates/zeroship-migrate-server/src/apply.rs:409-461`, and
`crates/zeroship-migrate-server/src/apply.rs:737-749`. Compose starts the service at
`deploy/compose/docker-compose.yml:382-405`.

### Platform migrations

The one-shot binary calls `run_platform_migrations` at
`crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:122-132`.
That runner constructs `CompioPgSession` and the generic Postgres backend at
`crates/zeroship-migrate-adapter/src/platform.rs:873-905`. The production image
and compose invocation are `deploy/Dockerfile:206-209` and
`deploy/compose/docker-compose.yml:197-210`.

The in-sourced engine tests do not exercise this concrete driver. Their
`PgDevSession` is explicitly a test-only implementation backed by the blocking
`postgres` crate (`crates/zeroship-migrate/tests/support/mod.rs:1-15`,
`crates/zeroship-migrate/tests/support/mod.rs:381-405`,
`crates/zeroship-migrate/tests/support/mod.rs:863-880`). The generic PG apply
helper uses that `SqlSession` boundary at
`crates/zeroship-migrate/tests/support/mod.rs:945-971`.

## 4. `zeroship-platform-migrate` invocation inventory

The binary is actively referenced by production, deployment tooling, CI, tests,
and runbooks.

| Area | Finding | Evidence |
| --- | --- | --- |
| Compose production | Direct invocation as the one-shot `migrate` service. | `deploy/compose/docker-compose.yml:157-174`, `deploy/compose/docker-compose.yml:197-210` |
| `deploy/scripts/*` | `deploy-remote.sh` has only one literal binary-name hit, a comment at lines 463-467. Operationally it copies the tracked compose file and runs the whole compose stack, which invokes the command above. | `deploy/scripts/deploy-remote.sh:1034-1043`, `deploy/scripts/deploy-remote.sh:1218-1225` |
| Manual deploy wrapper | Direct Cargo/prebuilt runner selection and execution. | `deploy/ops/db-migrate.sh:30-47`, `deploy/ops/db-migrate.sh:59-66` |
| CI platform test | Directly compiles/runs the adapter platform suite. | `.github/workflows/ci.yml:728-738` |
| CI binary builds | The golden-path, dev-vs-deployed, and billing jobs explicitly build the binary. | `.github/workflows/ci.yml:1470-1475`, `.github/workflows/ci.yml:1729-1734`, `.github/workflows/ci.yml:2203-2208` |
| CI execution | CI runs `golden_path.sh`, which calls the binary through `zs_platform_migrate`; that helper executes the supplied binary. Billing and auth suites also run the deploy wrapper. | `.github/workflows/ci.yml:1519-1522`, `tests/golden_path.sh:502-512`, `tests/lib/runtime_secrets.sh:337-354`, `.github/workflows/ci.yml:1016-1019`, `tests/run_billing_suite.sh:212-221`, `.github/workflows/ci.yml:1089-1092`, `tests/run_auth_suite.sh:235-237` |
| Runbook | Documents compose, wrapper, build, and direct binary invocation. | `docs/runbooks/db-migrations.md:52-90`, `docs/runbooks/db-migrations.md:98-113` |
| Any `package.json` | No invocation or reference in any of the 64 package manifests found by `rg --glob package.json`. The complete root scripts table contains only SDK/build/test/check/type-generation commands. | `package.json:20-27` |

The tree has many additional E2E callers. One shared execution site is
`tests/lib/e2e_stack.sh:253-258`, and representative direct call sites are
`tests/e2e_db_app_end_to_end.sh:207-210`,
`tests/e2e_dev_vs_deployed_db.sh:812`, and
`tests/e2e_metering_billing.sh:289-294`. These are uses of the actual binary, not
mentions in prose.

## 5. What the stale tests still test

The 22 reported default-target errors come from
`smoke_apply_pg.rs` and `author_and_apply_pg.rs`. Their source carries the exact
superseded shapes described in the request:

- root imports of `PostgresBackend` and `SqlDialect` at
  `crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:22-28` and
  `crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:35-41`;
- `ExecutorConfig.pg` at
  `crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:96-105` and
  `crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:219-228`;
- the old four-argument `IrAuthor::new`, zero-argument `MigrationEngine::new`,
  and removed guard constructor at
  `crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:206-221` and
  `crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:341-356`; and
- the removed root `applied` function at
  `crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:253-279` and
  `crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:387-409`.

The current API deliberately moved these responsibilities. The neutral facade
does not re-export a vendor backend
(`crates/zeroship-migrate-core/src/lib.rs:196-204`); Postgres owns
`PostgresBackend` and `DIALECT` at
`crates/zeroship-migrate-postgres/src/lib.rs:78-84` and
`crates/zeroship-migrate-postgres/src/lib.rs:98-125`. `IrAuthor::new` now takes a
`VendorSet` plus four other arguments at
`crates/zeroship-migrate-core/src/render/lower.rs:2417-2428`, and
`MigrationEngine::new` takes that set at
`crates/zeroship-migrate-core/src/engine.rs:487-494`. The journal reader now
lives at
`crates/zeroship-migrate-postgres/src/backend/journal_sql.rs:503-510`.

### What is superseded in `crates/zeroship-migrate/tests`

The generic engine behavior has stronger current replacements:

- guarded IR lower/apply and a create-table/add-column lifecycle are exercised
  by the current `apply_ir` helper and `add_and_alter_columns` test at
  `crates/zeroship-migrate/tests/fold_live/fold_roundtrip_pg.rs:156-204` and
  `crates/zeroship-migrate/tests/fold_live/fold_roundtrip_pg.rs:559-598`;
- the generic host-shaped session surface is exercised by
  `full_surface_runs_generically_with_in_flight_guard_never_tripping` at
  `crates/zeroship-migrate/tests/pg_engine/pg_recorded_engine_paths.rs:242-341`;
- live Postgres table creation, completion journaling, idempotent reapply, and
  no duplicate journal row are covered by
  `transactional_apply_creates_table_and_journals_completed` at
  `crates/zeroship-migrate/tests/pg_engine/pg_scenarios.rs:2653-2717`; and
- journal bootstrap/read idempotency is covered by
  `journal_ensure_is_idempotent_and_records_read_back` at
  `crates/zeroship-migrate/tests/pg_engine/pg_scenarios.rs:3508-3555`.

Those replacements establish engine semantics, not adapter semantics. They use
`RecordingSession` or the test-only blocking `PgDevSession`, not
`CompioPgSession` (`crates/zeroship-migrate/tests/support/mod.rs:1-15`).

### What is not superseded there

`smoke_apply_pg` explicitly tests the concrete native Compio seam end to end
(`crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:1-14`,
`crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:173-282`).
`author_and_apply_pg` adds the V8-authored TypeScript-to-IR leg and says its
downstream path is the same Compio apply path
(`crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:1-29`,
`crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:292-318`). No test
under `crates/zeroship-migrate/tests` embeds the platform V8 host or uses
`CompioPgSession`.

There is broader host-specific coverage outside that engine suite. The migrated
test is current; the platform portion is present but blocked by the API drift
documented in the next subsection:

- the production V8 host calls itself the port of the Stage 2 mechanism and
  authors an envelope at
  `crates/zeroship-migrate-adapter/src/platform/author.rs:1-5` and
  `crates/zeroship-migrate-adapter/src/platform/author.rs:58-120`;
- the platform suite sends every platform file through that host at
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:211-279`, then tests
  the real runner on fresh Postgres at
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:469-557` and an
  idempotent rerun at
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:1069-1174`; and
- migrated's live API test drives current IR through the production Compio path
  and asserts the app table, confinement, and journal at
  `crates/zeroship-migrate-server/tests/apply_api_test.rs:1018-1057`.

That means the old tests are partially redundant, but not genuinely superseded
by `crates/zeroship-migrate/tests`. In particular, migrated's live test does not
repeat the same migration to prove no-op/no-duplicate behavior at the concrete
Compio boundary, while the two stale adapter tests do
(`crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs:253-279`,
`crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs:387-409`).

### The feature-gated platform path is stale too

The 22-error count does not include `platform_migrate`, because Cargo skips that
test unless `platform-cli` is enabled
(`crates/zeroship-migrate-adapter/Cargo.toml:84-98`). The production module has
the same API drift: it imports removed root vendor types at
`crates/zeroship-migrate-adapter/src/platform.rs:35-41`, calls the old dialect and
four-argument author shapes at
`crates/zeroship-migrate-adapter/src/platform.rs:395-447`, constructs the engine
without the required vendor set at
`crates/zeroship-migrate-adapter/src/platform.rs:897-905`, and still calls the
removed root journal reader and `exec_cfg.pg` at
`crates/zeroship-migrate-adapter/src/platform.rs:919-929` and
`crates/zeroship-migrate-adapter/src/platform.rs:1151-1181`.

This is source-level proof that deleting only the 22-error tests does not make
the feature or binary current. The working migrated implementation shows the
intended current shape: vendor imports at
`crates/zeroship-migrate-server/src/apply.rs:15-23`, five-argument author construction
at `crates/zeroship-migrate-server/src/apply.rs:1120-1135`, and vendor-set engine apply
at `crates/zeroship-migrate-server/src/apply.rs:1137-1152`.

## History: wired path, not unused future scaffolding

Git history shows both long-lived shipped paths and one removed test consumer:

- `38acd2685` (2026-07-13) introduced `CompioPgSession` and its smoke test.
- `170be8f16` (2026-07-13) rewired migrated onto the adapter. Current blame still
  traces the normal dependency and import to that commit at
  `crates/zeroship-migrate-server/Cargo.toml:64` and
  `crates/zeroship-migrate-server/src/apply.rs:23`.
- `8c3c24324` (2026-07-13) added the platform binary/runner; current runner
  construction remains at `crates/zeroship-migrate-adapter/src/platform.rs:873-905`.
- `a6349f240` (2026-08-11) put migrated in the production image/compose; the
  current locations are `deploy/Dockerfile:210-223` and
  `deploy/compose/docker-compose.yml:382-405`.
- `825d36ccb` (2026-08-20) added plugin-db's Compio adapter test consumer. The
  August 26 `c6910bac6`/`0ef16dbb9` change removed that source use when plugin-db
  tests began spelling their own table DDL. The dev-dependency entry remains at
  `crates/zeroship-plugin-db/Cargo.toml:127-134`, but the current tree has no
  adapter symbol use in plugin-db.
- The August 26 in-sourcing sequence (`ccb5a7edc`, `b3a5c545c`, `e9dd97a8f`,
  `fe7aa0e45`, and `b044546c2`) changed repository/package paths. It retained the
  migrated-to-adapter edge; the current root explicitly says migrated consumes
  the engine through the adapter at `Cargo.toml:225-229`.

`git log -S'zeroship-migrate-adapter = { workspace = true }' -- '**/Cargo.toml'`
finds the migrated, plugin-db, and config-contract additions and no removal of a
manifest dependency. `git log -S'use zeroship_migrate_adapter::CompioPgSession'`
does show the plugin-db test removal above, but not removal of either production
consumer. This is "used before in-sourcing and still wired now," not "built for a
future path that never acquired a caller."

## Recommendation and strongest contrary case

### Recommendation: KEEP it and port the 22 errors

The adapter earns its crate boundary because it isolates a concrete network
driver from a driver-neutral, multi-vendor engine. The orphan-rule reason for the
newtype still exists: both `compio_postgres::Client` and `SqlSession` are foreign
to the adapter, so the adapter needs a local carrier
(`crates/zeroship-migrate-adapter/src/lib.rs:17-20`). It is linked into two shipped
binaries, and it owns platform-only V8/config/ledger behavior that is not part of
the portable engine.

The port should preserve, not redesign, the boundary:

1. Port `smoke_apply_pg.rs` and `author_and_apply_pg.rs` to the current vendor
   imports, dialect value, `ExecutorConfig.confinement`, five-argument
   `IrAuthor::new`, vendor-set `MigrationEngine::new`, current guard constructor,
   and Postgres journal location.
2. Port the same shapes in `src/platform.rs`; otherwise
   `zeroship-platform-migrate` and the released-ledger suite remain stale even if
   the default 22 errors disappear.
3. Keep the four DB-free released-ledger tests at
   `crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2379-2582` and keep
   CI's explicit feature invocation at `.github/workflows/ci.yml:728-738`.
4. Separately remove plugin-db's unused adapter dev-dependency only if a cleanup
   task is authorized; it is unrelated to whether the adapter crate is needed.

### Strongest case for DELETE

The best opposing argument is consolidation, not dead-code removal. Generic
lower/apply/journal behavior now has deeper coverage inside
`crates/zeroship-migrate/tests`, the platform suite is broader than the two
small adapter tests, and migrated itself supplies a current Compio integration
test. Under that argument, the exact deletion plan would first move
`CompioPgSession` into the existing `crates/zeroship-migrate-postgres` package,
then move the adapter's `config`, `platform` module and submodules, V8 glue and
policy assets, binary target, and complete `platform_migrate` suite into the
existing `crates/zeroship-migrate` composition package. The existing
`tests/deploy_scripts_gate.sh:1439-1584` could remain the DB-free byte,
file-presence, and ordering guard. The one Rust assertion it does not spell out,
the snapshot's nonempty/64-hex shape at
`crates/zeroship-migrate-adapter/tests/platform_migrate.rs:2562-2582`, would have
to move beside the snapshot load at `tests/deploy_scripts_gate.sh:1439-1446` (or
move with the platform suite). Only after updating migrated, config-contract,
Docker, compose, CI, test harnesses, and runbooks could the adapter package be
deleted without losing a responsibility.

That case loses today for four evidence-backed reasons:

1. It is a multi-responsibility refactor, not removal of an unused bridge.
2. Putting Compio into the Postgres vendor crate would turn its current
   driver-neutral boundary (`crates/zeroship-migrate-postgres/Cargo.toml:30-37`)
   into a platform-host dependency.
3. Putting the platform runner into the composition root would add V8, Compio,
   config, a feature, and a binary to a crate explicitly kept featureless and
   runtime-free (`crates/zeroship-migrate/Cargo.toml:6-27`).
4. Until every move above exists, deletion loses the shipped binary and the
   adapter's Rust guard plus database-local collision check. The independent
   shell guard would remain, but the destination search documented at the start
   proves no move of the binary, runner, or native session has happened.

The narrower opposing recommendation, "keep the crate but delete the stale
tests," also loses now. It is defensible only after the platform production path
and its broader suite are ported and an explicit concrete-Compio idempotency
assertion is retained there. Today that supposed replacement is itself on the
stale API, while the in-sourced engine replacements use a different driver.
