# Database migrations (zeroship-migrate)

The platform's Postgres schema — the single `zeroship` schema that holds every
platform/system table, plus the dedicated `oauth_hydra` schema — is managed by
**`zeroship-migrate`**, zeroship's own versioned migration engine, run under its
**Platform** trust profile. Migrations are hand-authored Flyway-style SQL files
in `db/migrations/`, version-controlled, reviewable, and rollback-able. The
engine tracks what's applied in an append-only journal (in a meta schema,
default `zeroship_migrations`), so `migrate` only ever runs pending files and is
safe to re-run (idempotent no-op when everything is applied).

> **Hydra owns its own schema** via `hydra migrate` (the `hydra-migrate` compose
> service). Do not put hydra tables in `db/migrations/`. The `oauth_hydra`
> *schema + role + grants + uuid-ossp pre-create* live in `V0027`; Hydra's own
> `hydra_*` tables are migrated by Hydra.

Services **do not migrate themselves.** `zeroship-control`/`zeroship-auth`
connect to an already-migrated database — exactly as in production, where the
`migrate` step runs once before anything boots. There is no inline migration
code in the Rust crates.

## Why our own engine (not Liquibase)

The platform schema used to run on Liquibase; it now runs on `zeroship-migrate`
under the **Platform** profile. The trust separation that mattered (the creator
migration path must never reach `control`/`auth`/`billing`) is preserved as a
**call-site invariant**: the Platform profile is constructible only at the
operator call site (gated by a crate-private `PlatformCapability` token), and
the creator submission ingress is hard-wired to the Confined profile with no API
path to Platform. The win over Liquibase is a **parse-time deny-list backstop**
(every statement — including DO-block / EXECUTE-literal bodies — is parsed with
the real `pg_query` parser and the RCE / host-escape / file / network surface is
hard-denied even under Platform) plus per-migration checksum tamper-evidence.
See `docs/proposals/2026-06-17-platform-migrations-flyway-mode-design.md`.

## Layout

```
db/migrations/
  V0001__extensions_schemas.sql        # citext + CREATE SCHEMA zeroship (the one platform schema)
  V0001__extensions_schemas.down.sql   # OPTIONAL reverse for the same version
  …                                    # 56 versioned files today (V0001–V0057, 0045 is a gap)
  V0004__control.sql                   # the control-plane app/usage/env tables
  V0027__oauth_hydra_schema.sql        # the separate oauth_hydra schema + least-priv role for Hydra
```

The filename encodes everything — there is **no** `--changeset`/`--rollback`
header parsing. The grammar is `V<NNNN>__<description>.sql` (versioned "up"),
`V<NNNN>__<description>.down.sql` (optional reverse for that version), and
`R__<description>.sql` (repeatable, re-applies on checksum change). Files apply
in numeric `V<NNNN>` order. **One file = one migration**, multi-statement,
whole-file transaction-atomic. Any `--rollback` / `--liquibase formatted sql`
comment surviving from the port is just a comment — the reverse lives in the
sibling `.down.sql`. Every object reference is fully schema-qualified; the
Platform guard permits the `zeroship`/`oauth_hydra`/`public` schema allowlist.

## Running migrations

**In the compose stack** — automatic. The one-shot `migrate` service runs
`zeroship-migrate migrate --dir /db/migrations --profile platform` after
Postgres is healthy and before control/auth start (they `depends_on` it with
`service_completed_successfully`):

```bash
docker compose up -d            # migrate runs, then control/auth boot
docker compose logs migrate     # see what was applied
```

**By hand** against a running dev DB — `ops/db-migrate.sh` shells into the
`zeroship-migrate` bin (via `cargo run`, or set `ZEROSHIP_MIGRATE_BIN` to a
prebuilt binary). It targets the compose Postgres on `localhost:5440` by default
(override with `ZEROSHIP_MIGRATE_DSN`):

```bash
ops/db-migrate.sh status            # pending vs applied
ops/db-migrate.sh migrate           # apply pending migrations
ops/db-migrate.sh validate          # dry-run on a shadow DB + guard-check + drift + destructive advisories
ops/db-migrate.sh rollback --steps 1  # roll back the last applied migration (requires --yes for the bin)
ops/db-migrate.sh rollback --to 0024  # roll back everything strictly after V0024
```

(There is no `changelog-sync` / adoption verb — pre-launch zeroship has no
deployed DB whose history must be honoured; fresh DBs re-migrate from scratch.)

## Adding a migration

1. Create the next-numbered file `db/migrations/V<NNNN>__<name>.sql` (the whole
   file is the "up", multi-statement). Schema-qualify everything.
2. If the change is reversible, add a sibling `V<NNNN>__<name>.down.sql` with the
   reverse SQL. (A multi-statement file's `.down.sql` should undo its statements
   in reverse order.)
3. `ops/db-migrate.sh validate` to dry-run + guard-check, then `migrate` to apply.

The loader picks the file up by its numeric `V<NNNN>` version order — no master
file to edit. **Never edit an already-applied migration** (the engine validates
per-migration checksums and aborts on drift); add a new versioned file instead.

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

> ⚠️ **No half-renamed-table state — the three cases it now covers (PR9d).** PG
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
>    journaled (a per-deploy recovery marker) and **crash-safe**: if the process
>    dies *after* the `open` marker is written but before the abort, the **next**
>    same-app deploy reconciles the leftover marker first (the abort's
>    `DROP … IF EXISTS` is idempotent on resume). The one window the auto leg does
>    NOT cover — a crash *between* the obligation row and the marker write, leaving
>    an outstanding obligation with no marker — is **residue 2** below: still
>    fail-closed (the table stays fenced), cleared by `resolve-pending`. So a refused
>    multi-file bundle leaves **no silently-half-open** renamed table regardless of
>    the failure cause.
>
> **The irreducible residues — there are TWO.** Both are **fail-closed** (the
> prior-deploy interlock fences the half-renamed table until cleared), never
> fail-open, and both are cleared by the operator with the
> `resolve-pending --apply | --abort` CLI command (`--apply` completes the rename;
> `--abort` rolls it back, dropping the shadow column):
>
> 1. **Abort-DDL failure.** If the **abort DDL itself** fails (the DB went
>    unreachable mid-recovery), the obligation stays outstanding and its recovery
>    marker stays `open`. The next same-app deploy's crash-recovery leg re-attempts
>    the abort (the abort's `DROP … IF EXISTS` is idempotent), and the prior-deploy
>    interlock (case 2) refuses any new bundle touching the half-renamed table until
>    it is cleared.
> 2. **Obligation-recorded-but-marker-not-yet-written crash.** The journaled
>    pending-contract obligation and its `open` recovery marker are written by
>    **two separate statements** (the engine commits the EXPAND DDL + the obligation
>    row; the control loop then writes the `open` marker — they are NOT in one
>    transaction, because the per-deploy `deploy_id` the marker keys on is a
>    control-plane value the migrate engine does not carry). A process death in that
>    narrow window leaves the obligation **outstanding but with NO recovery marker**.
>    Because the auto crash-recovery leg JOINs `outstanding_deploy_recoveries` on the
>    marker table, it finds nothing to abort — so this residue is **NOT
>    auto-recovered** by the next deploy (unlike residue 1). It is still
>    **fail-closed**: the obligation is outstanding, so the prior-deploy interlock
>    (case 2) fences the table against any new bundle, and the **only** clearance is
>    the operator running `resolve-pending --abort` (roll back the half-rename) or
>    `--apply` (complete it). The posture is identical to residue 1 (fenced + manual
>    resolve), it just does not self-heal via the auto leg. Closing this window
>    entirely would require folding the marker write into the same transaction as the
>    obligation row — i.e. threading the control-plane `deploy_id` into the migrate
>    engine's apply path (a future hardening, not in PR9d).
>
> **A legit go-live is never mistaken for a crash (PR9d HIGH).** A *successful*
> online-rename go-live legitimately leaves its EXPAND pending (the §2.0.2
> cross-deploy partition) — that is **not** a half-state and must never be aborted by
> a later deploy's always-on recovery leg. To make the success arm distinguishable
> from a genuine crash, it stamps the recovery marker **`reached_success`** *before*
> it appends the `reconciled` marker. The recovery leg only treats **net-`open`**
> markers as recoverable, so a legitimately-pending go-live is excluded.
>
> The success arm has **two append phases**, and they fail very differently:
>
> 1. **Phase 1 — `reached_success`** (the discriminator). All of this deploy's
>    obligations are stamped in **one atomic transaction** (PR9d-crit HIGH), so a
>    multi-EXPAND go-live flips **all or none** — there is no partial-stamp window
>    where one obligation is protected and a sibling stays a bare `open` over a live
>    contract.
> 2. **Phase 2 — `reconciled`** (cleanup). Best-effort. A `reconciled`-append failure
>    (a DB hiccup right after phase 1 committed) is **non-fatal**: the marker is
>    already net-`reached_success` (the live contract is protected), the deploy still
>    **succeeds**, and the next deploy's success path is a harmless no-op on the
>    already-protected marker. (Pre-PR9d this window left a bare `open` marker that the
>    next *unrelated* deploy's recovery leg would mistake for a crash and silently roll
>    back a column the creator's app was already using.)
>
> **The irreducible success-path residual (be precise — a re-run does NOT fix it).**
> If **phase 1 itself fails** (the DB went unreachable the instant the go-live reached
> its success arm), the deploy surfaces a **hard error** and *all* of its recovery
> markers stay net-`open` over a **legitimately-pending live contract** (the dual-write
> trigger + shadow column are committed, the obligation is pending). This marker is
> **byte-for-byte indistinguishable from a genuine crash half-state** — the go-live's
> physical schema state is identical to a deploy that crashed before its in-process
> abort (compare the `crash_before_abort_is_recovered` and
> `legit_pending_survives_…` recovery tests: both leave the trigger + shadow column
> live and the obligation outstanding). No durable signal can tell them apart, so:
>
> - **Re-running the deploy does NOT clear it.** The idempotent re-run finds the EXPAND
>   `already_outstanding`, so it never re-opens the obligation, `opened_this_deploy` is
>   empty, and the success arm never re-stamps the stale `open` marker. The phase-1
>   failure is *not* self-healing.
> - **Until it is cleared, an unrelated next deploy's always-on crash-recovery leg
>   WILL false-abort this live contract** (drop the dual-write trigger + shadow column,
>   discharge the obligation `aborted`) — the deploy must be treated as not-yet-safe.
> - **The only safe clearance is the operator running `resolve-pending --apply`**
>   (complete the rename, discharging the obligation so the recovery leg's
>   outstanding-join finds nothing to abort). `--abort` is the alternative if the
>   rename should be rolled back instead. Run this **before** any further deploy of the
>   same app.
>
> The phase-1 hard error is logged at `error` level with this exact remedy so the
> operator is alerted. This residual is the narrow, honestly-documented cost of having
> *no* durable crash-vs-go-live signal beyond the success-arm commit record; closing
> it entirely would require a per-deploy outcome journal written atomically with the
> EXPAND commit (a future hardening, not in PR9d).
- The approver's principal is stamped into the immutable journal
  (`applied_by = deploy-approved:<approver>` / `deploy-ir-approved:<approver>`),
  so an operator-approved go-live is forensically distinct from a routine deploy.
- **The approval can bind the exact reviewed bytes (H2).** Alongside the version
  set, the operator may pass the reviewed bundle's **combined integrity manifest
  hash** on the approval channel (`?expected_manifest=<hash>`). The reviewer
  computes it out-of-band with `deploy_migrate::plan_reviewed_manifest` (the SAME
  hash the apply recomputes — `.sql` flat set ++ every `.ir.json` file's lowered
  migrations). When present, the approved apply **refuses the bundle before any
  DDL** (`approved_manifest_mismatch`, 422) if the arrived set recomputes a
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
> migration set from a trusted source of truth, not the `.zship` alone, and stamp
> its hash on the approval.

## Tests

The control/auth integration tests connect to a **pre-migrated** database
(`AUTH_DB_URL` / `CONTROL_TEST_DB`) — they no longer self-migrate. Bring the
schema up once before running them: the compose `migrate` service does this for
the compose DB, or run `ops/db-migrate.sh migrate` against your test DB.
