# Database migrations

The platform's Postgres schema is applied by the `zero-migrate` Node CLI through
the sanctioned wrapper `deploy/ops/db-migrate.sh`. The source is the committed
TypeScript corpus in `db/migrations-ts/`, authored with the one
`@zeroship/migrate` package in `packages/zero-migrate/`. Each file records to
transient IR at apply time; committed SQL and Flyway files are not platform
sources. The engine journals completed migrations append-only, so a re-run is an
idempotent no-op.

Services **do not migrate themselves.** `zeroship-control`/`zeroship-auth`
connect to an already-migrated database — exactly as in production, where the
`migrate` step runs once before anything boots. There is no inline migration
code in the Rust crates.

## Why our own engine (not Liquibase/Flyway)

The platform schema used to run on Liquibase and later through a retired Rust
one-shot. It now runs through the general Node CLI under the **Platform** profile
from `db/migrations-ts/`. The trust separation
that mattered (the creator migration path must never reach
`control`/`auth`/`billing`) is preserved as a **call-site invariant**: the
Platform profile is constructible only at the operator call site, and the
creator submission ingress is hard-wired to the Confined profile with no API path
to Platform. The platform source is now the same structured JS DSL used for
creator migrations, with transient IR and per-migration checksum
tamper-evidence.

## Layout

Files use `db/migrations-ts/<YYYYMMDDHHMMSS>_<name>.ts`; the 14-digit prefix is
the corpus order key. Every file imports `@zeroship/migrate` and exports one of:

- `schema()` for DDL, with an engine-synthesized structural inverse;
- `data()` plus recorded `inverse()` for reversible DML; or
- `data()` plus a non-empty `irreversible` reason.

Schema and data operations cannot share one file. The CLI rejects the former
generic phase names rather than treating them as aliases. The platform directory
is `.ts` source only, not `.sql` and not committed `.ir.json`.

## Running migrations

**In the compose stack** — automatic. After Postgres is healthy, the one-shot
`migrate` service runs the Node CLI from the dedicated `migrate` image before
control and auth start; both services depend on its successful completion.

The DSN arrives as a PATH, never as a value: it is the cluster superuser DSN,
and an argument list is readable by `docker inspect`, `docker ps --no-trunc` and
anything sharing the PID namespace. The entrypoint turns the mounted secret into
an owner-only CLI config; the config reader refuses a literal database URL in a
file with any permission bit set in `0o077`.

```bash
docker compose up -d            # migrate runs, then control/auth boot
docker compose logs migrate     # see what was applied
```

**By hand** against a database you control — build the JavaScript packages and
native addon, then run the sanctioned wrapper. It invokes
`packages/zero-migrate-cli/dist/cli-bin.js` and writes the DSN to a temporary
0600 config rather than placing it on the CLI's argument list:

```bash
pnpm build
ZEROSHIP_MIGRATE_DSN="$DATABASE_URL" deploy/ops/db-migrate.sh
```

The wrapper also accepts `ZEROSHIP_MIGRATE_VERB` for another CLI verb and
`ZEROSHIP_MIGRATE_CLI` for an explicitly built CLI path. The normal build is the
expected preparation; to rebuild only the CLI while developing it:

```bash
pnpm --filter zero-migrate-cli build
```

The native corpus tests run the same CLI and platform policy against owned
PostgreSQL containers. The apply test reconciles recorded operations with the corpus ledger,
checks applied identities against durable history and strict status, and verifies
that applying again skips exactly those identities without changing history:

```bash
cargo xtask test migrations
```

The organization-authority tests apply the same corpus before exercising
membership and app ownership, invitation escalation, the control role's
privileges, and identifier collations. Refusals require PostgreSQL's expected
SQLSTATE and constraint, with accepted controls and checks of surviving rows.
Invitation ranks come from the migrated role ladder. These tests exercise
database constraints; authorization decisions remain covered in `zeroship-authz`.

## Adding a migration

1. Add a new `db/migrations-ts/<YYYYMMDDHHMMSS>_<name>.ts` file.
2. Express the change with the `@zeroship/migrate` JS DSL. Use the platform-only
   Postgres helpers only for platform schema objects that cannot be represented
   portably; they are capability-gated by the engine, not by the import path.
3. Keep DDL in `schema()`. Put DML in a separate `data()` migration and declare
   either its recorded `inverse()` or why it is `irreversible`.
4. Update `db/migrations-ts/op-counts.json` when the recorded operations change.
   Run the corpus test before relying on the change; this command builds the
   migration host and CLI and provisions its own fresh PostgreSQL database:

   ```bash
   cargo xtask test migrations
   ```

Docker and Node are required. After building the artifacts, the same tests run
in ordinary `cargo test -p zeroship-migrate-node`; missing prerequisites fail
instead of skipping database verification. Their private fixture is in
`crates/zeroship-migrate-node/tests/platform_corpus/fixture.rs`. Each database test
owns its container and waits for its Rust connection to close before teardown.
The tests cover the host CLI and database contract, including organization
authority and user-erasure foreign keys. The erasure checks inspect the migrated
catalog and compare rejection controls with actual PostgreSQL deletes.
The suite does not build the deployment image or invoke the operator's shell
wrapper.

The loader picks the file up by its timestamp order - no master file to edit.
While the platform remains pre-launch, the repository `AGENTS.md` policy is the
exception: add a new system field to its creating migration and recreate any
disposable development database whose checksum now differs. Do not add an
`ALTER` backfill for users or production data that do not exist. After launch,
or for creator migrations that have actually shipped, never edit an applied
migration; the engine validates checksums and aborts on drift, so add a new
timestamped file instead.

## Operator-approved creator go-live (online rename / destructive ops)

A creator app's `.zship` deploy applies its migrations on the
`POST /api/apps/{id}/deploy` path. A **routine** deploy is fail-closed: an online
`renameColumn` EXPAND or any destructive op (drop/truncate/lossy) is **refused
before go-live** — the AI/creator never auto-applies a gated change.

Completing such an op is an **operator** action, gated separately from the deploy
itself:

- The deploy carries `?approved_versions=<comma-joined version-ids>` — the
  individually-reviewed migration versions approved for this go-live (enumerate
  them with `deploy_migrate::plan_reviewed_versions`).
- A non-empty approval set is authorized by the **operator-only**
  `migrations:approve` action (`Action::AppsApproveMigration`). It is **not** in
  the creator OAuth scope vocabulary and **not** granted by the
  app-owner/editor/viewer policies — only the platform `admin` role. A caller
  holding merely `apps:deploy` (the bundle author, or an AI deploying on their
  behalf) is refused **403** the instant they pass an approval set, so the author
  cannot self-approve their own destructive go-live.
- The approval is **per-version scoped**: only the listed versions' destructive/
  online ops complete; any co-bundled op outside the set is refused
  (`ApprovalNotScoped`). The whole bundle is **pre-validated** before any file
  applies — if any scope-gated op is out-of-scope the deploy is refused
  wholesale, so an earlier approved EXPAND never commits ahead of a guaranteed
  later refusal (no half-renamed-table state).

> ⚠️ **No half-renamed-table state — the three cases it now covers (PR9d/PR9e).** PG
> DDL commits **per migration step** (`BEGIN; <up>; INSERT journal; COMMIT` per
> migration); there is **no whole-bundle transaction**. So in a multi-file bundle,
> an earlier in-scope online-rename EXPAND can durably commit (its dual-write
> trigger + shadow column + a journaled pending contract) **before** a later file
> is refused. The platform guarantees the creator never sees a half-renamed table
> across all three refusal/failure classes:
>
> 1. **Scope refusal** (a co-bundled op outside the approved set) — caught by the
>    whole-bundle pre-validation above, **before** any file applies; nothing
>    commits.
> 2. **Prior-deploy interlock refusal** (an op touching a table that owes an
>    *outstanding* online-rename contract from an earlier deploy) — also caught by
>    the same pre-validation read-back, before any file applies.
> 3. **Same-deploy later-file APPLY failure** (a runtime error the read-only
>    pre-validation *cannot* predict: a CHECK/unique violation, a backfill error, a
>    second rename mid-expand failure, any genuine DB error). Here the earlier
>    EXPAND *has already committed*. The deploy now drives a **deploy-scoped
>    recovery**: under the still-held project lock it **aborts the same-deploy
>    EXPAND** (drops the dual-write trigger + the shadow column with `IF EXISTS`,
>    leaving the pre-rename column intact, and discharges the just-opened
>    obligation `aborted`) **before** surfacing the creator's 4xx. The recovery is
>    journaled (a per-deploy recovery marker) and **crash-safe**: the marker is
>    written by the **engine in the same transaction as the obligation row** (PR9e),
>    so every outstanding obligation **always** has a marker. If the process dies
>    *after* that commit but before the abort, the **next** same-app deploy reconciles
>    the leftover marker first (the abort's `DROP … IF EXISTS` is idempotent on
>    resume). So a refused multi-file bundle leaves **no silently-half-open** renamed
>    table for any crash **at or after** the obligation+marker commit. (One narrow
>    pre-existing window remains — a crash *between* the E-phase DDL commit and the
>    obligation+marker commit; see the E-phase residue note below.)
>
> **The obligation + its recovery marker are now ATOMIC (PR9e — residue 2 CLOSED).**
> The journaled pending-contract obligation and its recovery marker are written in
> **one transaction** by the engine: the control-plane `deploy_id` is threaded into the
> migrate engine's apply path (a `DeployRecoveryScope`), so the `INSERT` of the
> obligation row and the `INSERT` of the `in_progress` recovery marker commit together
> or not at all. There is **no window** in which an obligation can be committed without
> its marker, so the auto crash-recovery leg's JOIN can never miss one — every crash
> window converges automatically. The pre-PR9e "obligation-recorded-but-marker-not-
> yet-written" residue is **eliminated**, not merely fenced.
>
> **The one remaining residue — abort-DDL failure.** It is **fail-closed** (the
> prior-deploy interlock fences the half-renamed table until cleared), never fail-open,
> and is **auto-recovered** by the next deploy: if the **abort DDL itself** fails (the
> DB went unreachable mid-recovery), the obligation stays outstanding and its recovery
> marker stays net-`in_progress`. The next same-app deploy's crash-recovery leg
> re-attempts the abort (idempotent `DROP … IF EXISTS`), and the prior-deploy interlock
> (case 2) refuses any new bundle touching the half-renamed table until it is cleared.
> The operator can also clear it manually with `resolve-pending --apply | --abort`.
>
> **A second narrow residue — the E-phase-commit-vs-obligation-commit window.**
> `run_online` commits the shadow column + dual-write trigger in its **own** per-step
> transaction; the obligation+marker transaction commits **after** it returns. A crash in
> that gap leaves a live shadow column + `zsdw_` trigger with **no** obligation row and
> **no** recovery marker — invisible to both the crash-recovery leg (its marker JOIN is
> empty) and the §2.0.3 interlock read-back (no pending contract). It is **pre-existing**
> (not introduced by PR9d/PR9e) and **narrow** (a crash in a sub-second window). It
> **self-heals** only if the *exact same* rename is re-deployed; a different next bundle
> leaves the orphaned shadow column + trigger unfenced. A future hardening (not yet
> shipped) is an **orphan-shadow sweep** that introspects live `zsdw_` triggers lacking a
> matching obligation and surfaces/aborts them.
> **A legit go-live is never mistaken for a crash, and a stamp failure can no longer
> silently revert one (PR9e — the inversion that CLOSES the MED).** A *successful*
> online-rename go-live legitimately leaves its EXPAND pending (the §2.0.2 cross-deploy
> partition) — that is **not** a half-state and must never be aborted by a later
> deploy's always-on recovery leg. The discriminator is the marker's **birth state**:
>
> - The marker is **born `in_progress`** (atomically with the obligation, above).
>   `in_progress` *is* the "this deploy has not durably reached a terminal outcome"
>   signal.
> - On success, the deploy **promotes** every marker `in_progress` → `committed` in
>   **one atomic batch**. The crash-recovery leg recovers **only net-`in_progress`**
>   markers, so a net-`committed` go-live is **excluded — never aborted**.
> - On a same-deploy / crash abort, the marker is closed `reconciled`.
>
> **Why a promotion failure can no longer false-abort a committed go-live.** If the
> `committed` promotion **itself** fails (the DB went unreachable the instant the
> go-live reached its success arm), the marker stays net-`in_progress` — the
> **recoverable (fail-safe) state**. The deploy surfaces a **hard error**, and the next
> same-app deploy's recovery leg **auto-aborts** the half-rename. This is **safe and
> loses no data**: a *pending* contract has **not** cut over reads/writes to the shadow
> column (the dual-write trigger keeps the old + shadow columns in sync, and the
> drop-old-column contract has not run), so rolling the rename back preserves the
> original column and all its data. The abort is idempotent, the obligation is
> discharged `aborted`, and the app can re-run the rename cleanly.
>
> This is the **inverse** of the pre-PR9e design, whose stamp failure left the marker
> in a *protected* (`open`/`reached_success`) state — so a later unrelated deploy would
> **silently revert** a live contract it could not distinguish from a crash, with no
> auto-recovery (manual `resolve-pending --apply` only). The PR9e direction degrades a
> stamp failure to **"safely re-runnable crash recovery"**, not "silent revert of a live
> contract." The key asymmetry: a pending contract has not cut over, so auto-aborting it
> is always data-safe; the harm the old MED could cause — dropping a shadow column **with
> data written only to it since cutover** — cannot occur, because cutover happens only
> when the contract (drop-old-column) runs, which by definition has not happened while
> the obligation is still pending. The promotion-failure hard error is logged at `error`
> level so the operator is alerted, but **no operator action is required** for safety —
> the next deploy auto-recovers.
- The approver's principal is stamped into the immutable journal
  (`applied_by = deploy-approved:<approver>` / `deploy-ir-approved:<approver>`),
  so an operator-approved go-live is forensically distinct from a routine deploy.
- **The approval can bind the exact reviewed bytes (H2).** Alongside the version
  set, the operator may pass the reviewed bundle's **combined integrity manifest
  hash** on the approval channel (`?expected_manifest=<hash>`). The reviewer
  computes it out-of-band with `deploy_migrate::plan_reviewed_manifest` (the SAME
  hash the apply recomputes over the reviewed migration documents: SQL migrations
  where that path is used, platform `.ts` migrations lowered to transient IR, and
  creator IR documents supplied in the apply request; `.zship` bundles do not carry
  migration documents). When present, the approved apply **refuses the bundle
  before any DDL** (`approved_manifest_mismatch`, 422) if the arrived set recomputes a
  different hash — a reorder / edit / insert / remove between review and apply.
  This closes the H2 TOCTOU: an approval that carries the manifest authorizes
  **exactly the reviewed bytes**, not merely a version-id list. The expected hash
  is a TRUSTED out-of-band stamp (operator-reviewed), never read from the `.zship`.

> ℹ️ **H2 binding is opt-in per approval (and recommended for destructive go-live).**
> If the operator approves with *only* `?approved_versions=` and **no**
> `?expected_manifest=`, the go-live is **integrity-traceable but not
> tamper-prevented** (the manifest is still computed + logged for forensics): a
> set reordered/edited between review and apply under a matching version-id
> approval is not refused. Pass `?expected_manifest=` (from
> `plan_reviewed_manifest`) to make the approval tamper-proof — review the
> migration set from its trusted source (platform `.ts` sources or creator
> in-request IR documents), not the `.zship`, and stamp its hash on the approval.

## Tests

The control/auth integration tests connect to a **pre-migrated** database
(the generated test overlay or `PG_TEST_URL`) — they no longer self-migrate. Bring the
schema up once before running them: the compose `migrate` service does this for
the compose DB, or set `ZEROSHIP_MIGRATE_DSN` and run
`deploy/ops/db-migrate.sh` against your test DB.
