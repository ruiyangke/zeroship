# Migration subsystem — security + correctness review (2026-07-27)

Scope: `crates/migrated/*`, `crates/zeroship-migrate-adapter/*`, `sdks/migrate/src/host/*` +
`host-recorder.ts`, `db/migrations-ts/*`. Vendored engine `third_party/zero-migrate`
reviewed only at the SEAM (how the adapter invokes + trusts it). Pre-launch, no back-compat.

Reviewer stance: harsh, evidence-based, traced the real path. Overall the subsystem is
better-than-average: the least-privilege role model is genuine, escalation-reject is real
and tested, the approve/apply TOCTOU is closed with a content checksum, and — importantly —
**the migration service never executes creator JS** (it ingests already-authored `.ir.json`
over HTTP; V8 authoring runs only on trusted `db/migrations-ts/*.ts`). Findings below are
mostly correctness/robustness, with two genuinely high-severity items.

---

## CRITICAL

None found in the reviewed monorepo seam. (The role model, escalation-reject, ownership
gate, and operator-only approve are all sound — see "What holds up" at the end.)

---

## HIGH

### H1. Preflight → approval-classification → store-write runs OUTSIDE the app advisory lock; concurrent applies of the same app race the classification
`crates/migrated/src/apply.rs:417-637` (preflight + `insert_pending`/`insert_auto_approved`)
vs `:677` (`apply_sealed`, where the lock is first acquired at `:945-949`).

The project advisory lock (`pg_advisory_lock(hashtext(project_id))`, engine
`apply/baseline.rs:155`) is acquired only inside `apply_bundle_ir_postgres`, on the FIRST
`.ir.json` file. Everything before it — `CREATE SCHEMA`, `provision_migrator`,
`preflight_ir_documents` (which `snapshot_schema`s the live catalog), the
gated-versions/destructive classification, and the `insert_*`/audit store writes — runs
with NO app-scoped lock.

Attack/failure: two `POST …/migrations/apply` for the same app arriving concurrently both
snapshot the same live schema, both classify approval independently, and both create
distinct `migration_id` rows (`Uuid::now_v7()` per request, `apply.rs:203`). The engine's
journal serializes the actual DDL, but the *approval decision* was computed against a stale
snapshot: migration B classified as "non-destructive / auto-approved" against the
pre-A schema may in fact be destructive once A lands. B then applies auto-approved. The
security-relevant classification (does this need operator sign-off?) is not covered by the
lock that protects the DDL.

Fix: acquire the per-app advisory lock (or a `pg_advisory_xact_lock(hashtextextended(app_id…))`,
as `policy_store.rs:55` already does for policy writes) at the TOP of
`apply_ir_documents_with_policy`, before preflight, and hold it through the store write +
engine apply.

### H2. Static, hardcoded database-role passwords committed in the platform migration
`db/migrations-ts/20260702000100_schema_roles_extensions.ts:15-23`

Every login role is created with a password equal to its own name:
`role("zeroship_control").create({ login: true, password: "zeroship_control", … })`, likewise
`zeroship_auth`, `zeroship_gateway`, `zeroship_worker`, `zeroship_app`, `sandbox_app`,
`sandbox_audit`, `sandbox_gdpr`. These are the actual login credentials for the roles that
front the whole platform DB, checked into git.

Attack: anyone with the source (or who guesses the trivial pattern) who can reach the
Postgres port authenticates as `zeroship_control` / `zeroship_auth` and reads/writes every
control + auth table (users, oauth tokens, billing, secrets). `bypassRls: true` on
`zeroship_auth`/`zeroship_control` makes it total.

Pre-launch caveat: acceptable for a throwaway local dev DB, but this file is the SOLE
platform migration source and these literals will provision production unless overridden.
Fix: create roles with `PASSWORD NULL` / no password in the migration and set real secrets
out-of-band (or via `ALTER ROLE … PASSWORD` from a secret store) at provisioning time; never
commit the credential equal to the rolename.

---

## MEDIUM

### M1. `provision_runtime_app_role` issues role/grant DDL without the `tuple concurrently updated` retry the rest of provisioning uses
`crates/migrated/src/apply.rs:1470-1509` (raw `conn.batch_execute`) vs
`crates/migrated/src/provisioning.rs:61-81` (`exec_retry`, 8 attempts).

`provision_runtime_app_role` runs as admin OUTSIDE the app advisory lock (called at
`apply.rs:674` before the lock is taken). Concurrent applies for two DIFFERENT apps both
touch the shared `__zeroship_app_role_template` role and issue `CREATE ROLE` / `ALTER
DEFAULT PRIVILEGES` — exactly the catalog contention `exec_retry` exists to absorb. A
transient `tuple concurrently updated` here fails the whole apply with a 503
(`ProvisionRuntimeRole`), where the migrator provisioning would have retried.

Fix: route these two `batch_execute`s through `exec_retry` (or the same helper).

### M2. Approval-required migrations accumulate as unbounded `pending_approval` rows with no submit-side dedup or rate limit
`crates/migrated/src/apply.rs:203` (`Uuid::now_v7()` per request) + `:425 insert_pending`.

Every apply of a gated (destructive) migration inserts a fresh `pending_approval` row keyed
by a new `migration_id`. An owner (or a prompt-injected AI acting as them) can POST the same
destructive migration N times and mint N pending rows, each requiring an operator to
individually reject/ignore. There is no content-hash dedup (the `approved_checksum` /
`content_checksum()` machinery exists but is only used post-approval, not to collapse
duplicate submissions) and no per-app pending cap.

Fix: dedup pending submissions on `(app_id, content_checksum)` — return the existing pending
`migration_id` instead of inserting a duplicate — and/or cap open pending rows per app.

### M3. `revert_to_pending` (TOCTOU drift arm) is unguarded on current status — can resurrect a terminal row
`crates/migrated/src/migration_store.rs:138-156`.

`revert_to_pending` runs `UPDATE … SET status='pending_approval' … WHERE app_id=$1 AND
migration_id=$2` with NO `AND status = 'approved'` guard, unlike `mark_approved` (`:123`,
guarded on `pending_approval`). It is only *reached* from the drift branch
(`apply.rs:612`) where the row was `approved`, so today it is safe by call-site. But as a
store primitive it will flip an `applied` or `rejected` (terminal) row back to
`pending_approval` if ever called out of that context — a latent state-machine hole.

Fix: add `AND status = 'approved'` to the `WHERE` and treat 0-rows-updated as a no-op/error,
matching the guarded style of `mark_approved`.

### M4. `mark_applied` / `mark_rejected` are unguarded on status and can clobber each other on the error path
`crates/migrated/src/migration_store.rs:158-195`.

Both update purely on `(app_id, migration_id)` with no status precondition. On the apply
error path (`apply.rs:748 mark_migration_failed → mark_rejected`) a migration that the
engine actually half-applied-then-errored is marked `rejected` even though DDL may have
landed (the engine journal is the real source of truth). The store status can therefore
disagree with the journal. Because there is no compensating check, an operator reading
`migrated_migrations.status = 'rejected'` may believe nothing changed when in fact the
journal recorded applied steps.

Fix: guard these transitions on the expected prior status, and/or reconcile terminal status
from the engine journal outcome rather than the Rust-side error alone.

### M5. `MigrationStore` / `AppPolicyStore` open a fresh un-pooled connection per operation
`crates/migrated/src/migration_store.rs:232-243`, `policy_store.rs:159-170`.

Every `insert_*`, `record_audit`, `mark_*`, `get_*` does a full
`compio_postgres::connect` + spawn/detach run-loop. A single apply performs ~5-8 store
calls, so ~5-8 TCP+auth handshakes per apply. Under load this is a connect storm against the
control PG and a DoS-amplification vector (each cheap HTTP apply fans out to many DB
connects). Also: `record_audit` failures are only logged best-effort (`apply.rs:1345`), so
audit gaps are silent.

Fix: hold one pooled/shared client on `MigrationServiceState` (the service already keeps a
long-lived `control_pg` for authz — reuse that pattern for the stores).

### M6. Auth infrastructure errors on the ownership lookup fail-OPEN into a 500 but the classification leaks intent; token verification uses a random per-call request-id defeating replay correlation
`crates/migrated/src/auth.rs:122-124`, `:149-159`.

Two smaller issues:
(a) `verify_bearer(token, None, Uuid::new_v4().to_string())` mints a throwaway request-id per
call (`auth.rs:123`), so the bearer verifier's replay/correlation window can never key on a
caller-supplied request id — every call looks fresh. If the underlying verifier relies on
request-id for any replay dedup, it is defeated here. (Confirm against `zeroship_authn`
semantics; if request-id is purely for logging this is LOW.)
(b) `caller_owns_app` returns `AuthError::Infrastructure` (→ 500) on a DB error
(`:157`). That is fail-closed for authorization (no `Allow` is returned), which is correct —
noted only to confirm it does NOT fail open.

Fix: thread the real inbound request id (if the platform propagates one) into
`verify_bearer` instead of a fresh UUID.

---

## LOW

### L1. Seal HMAC is in-process-only tamper detection, not a cross-trust boundary — the doc comment is honest but the audit trail implies more
`crates/migrated/src/apply.rs:644-663`, `policy.rs:147-173`.

The sealed policy is minted and verified within the SAME process/request (`seal_effective_for_app`
→ `verify` a few lines later). It provides zero protection against an attacker who controls
the process, and the `mac_key` defaults to a hardcoded dev string under `--dev-insecure`
(`main.rs:247`). This is fine and the comments say so, but the `sealed_profile` audit JSON
(`migration_store.rs:403`) records a "tamper-evidence identity boundary" that could mislead
an auditor into thinking the seal is an attestation. Keep, but ensure ops docs don't
overstate the seal's authority.

### L2. `quote_lit` only escapes single quotes; role/schema names flow into `DO $$ … EXECUTE format-string $$` blocks
`crates/migrated/src/apply.rs:1460-1509`, `provisioning.rs:54-56`.

`quote_lit` does `value.replace('\'', "''")` and is used to build `rolname = '{…}'`
comparisons and `EXECUTE 'CREATE ROLE {ident}…'` strings inside `DO` blocks. The inputs are
`APP_ROLE_TEMPLATE` (a constant), the migrator role name (derived by the engine's
`migrator_role_name` from the schema), and the schema (`app_id.to_string()`, a UUID). All
current inputs are UUID/constant-derived so not attacker-controlled, and identifiers use
`quote_ident`. But building executable DDL by string interpolation of a "literal" that is
then re-parsed as an identifier inside `EXECUTE` is fragile: if `runtime_app_role_name`'s
input ever became non-UUID, `'{role_lit}'` could break out. Defense-in-depth: use
`format('%I', …)` / `quote_ident()` inside the `DO` block rather than pre-interpolating.

### L3. `mac_key`/seal key accepts any-length key incl. empty-in-prod-if-misconfigured only guarded at CLI layer
`crates/migrated/src/policy.rs:69-76`, `main.rs:244-257`.

`ManagedPolicyConfig::new` takes `impl Into<Vec<u8>>` with no minimum-length check; the only
32-byte-ish enforcement is the CLI refusing an empty key unless `--dev-insecure`. A caller
constructing the config programmatically with a short key gets a weak HMAC silently. LOW
because H2/L1 note the seal is in-process anyway. Fix: enforce a minimum key length in
`ManagedPolicyConfig::new`.

### L4. `discover_ir_files` / `discover_ts_files` sort by full `PathBuf` including the tempdir prefix
`crates/migrated/src/apply.rs:812-835`, `platform.rs:164-187`.

Ordering is `ir_files.sort()` on the full path. Because all files share the same tempdir
parent (`write_ir_documents`, `apply.rs:1304`), the sort reduces to filename order, which is
the intent. Correct today, but the determinism relies on a shared parent; sorting on
`file_name()` would state the invariant directly and be robust if the layout ever changes.

### L5. `to_holder` maps unknown future `Bind` variants to a text NULL silently
`crates/zeroship-migrate-adapter/src/lib.rs:135-147`.

`Bind` is `#[non_exhaustive]`; the `_ => ToSqlHolder::Null` arm turns any future variant
into a NULL bind rather than erroring. A future engine version that adds a `Bind::Bytes`
would silently write NULLs into the platform DB through this adapter with no signal. LOW
(engine + adapter are version-pinned via submodule) but a `debug_assert!`/error arm would
fail loudly on a seam drift.

### L6. `driver-mysql2.ts` sets `multipleStatements: true` on the host connection
`sdks/migrate/src/host/driver-mysql2.ts:91`.

`multipleStatements: true` widens the classic SQL-injection blast radius (a single injected
`;` becomes a second statement). The engine renders parameterized DDL and the comment
justifies it ("the engine issues multi-statement DDL batches"), and this driver is not on
the creator apply path (it is the MySQL authoring/live-driver seam). Still, pairing
`multipleStatements` with any string-built SQL is a smell; ensure no user string ever reaches
the `batch` verb un-parameterized on this path.

---

## What holds up (verified, not findings)

- **Least-privilege role model is real.** `provision_migrator` creates the migrator
  `NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS` (`provisioning.rs:106-117`),
  REVOKEs meta-schema access (journal unforgeable by deny-by-absence, `:156-168`), pins
  `search_path` to project-schema-first (`:177-184`), and confines extension schemas to
  USAGE-only (`:186-194`). The engine drives DDL via `SET ROLE`/`RESET ROLE` to this
  `NOLOGIN` role (engine `apply/role.rs`), so a superuser admin connection still runs the
  migration under the confined role's privilege checks. The runtime app role gets USAGE +
  DML only, never CREATE (`apply.rs:1495-1507`) — app code cannot author schema.
- **Escalation-reject is genuine and tested.** `compose_effective_for_app` composes via
  `admit(app_base, draft)` (`policy.rs:104-124`); a draft looser than the ceiling is
  rejected, not clamped — proven by `draft_permission_escalation_is_rejected_not_clamped`
  and `draft_cannot_escape_the_app_schema_boundary` (`policy.rs:738-784`). The confined
  ceiling is bound to the EXACT app schema before admission (`:111`,
  `bind_confined_charter_to_schema`), so a `schema.cross_schema` grant in a draft cannot
  reach another tenant's schema.
- **Operator-only approval is enforced at the authz layer, not just intended.**
  `requires_app_owner` returns FALSE for `AppsApproveMigration` (`auth.rs:161-163`), and the
  platform authz policy DENIES the app owner `migrations:approve` on their own app while
  granting it to platform admins (`crates/authz/tests/platform_policies_test.rs:135-209`).
  So an owner (or an AI deploying as them) cannot self-approve a destructive/gated migration.
- **Approve/apply TOCTOU is closed.** `approve()` stamps `approved_checksum = X`
  (`migration_store.rs:110-132`, guarded on `pending_approval`); the apply gate re-resolves
  to `X'` and refuses + reverts-to-pending if `X' != X` (`apply.rs:582-624`). Ceiling-version
  staleness and preflight-classification drift are both separately re-checked
  (`apply.rs:243-276`, `:547-581`).
- **The migration service never runs creator JS.** The creator apply surface ingests
  `.ir.json` documents (`ApplyMigrationsRequest.documents: Vec<IrDocument>` with
  `body: Value`, `apply.rs:44-48`) — already-lowered IR, validated by the fail-closed load
  gate + guarded lower. V8 authoring (`platform/author.rs`) executes `.ts` source ONLY for
  the trusted, committed `db/migrations-ts/*.ts` platform set, not for creator input. The
  "untrusted migration JS runs with too much authority" risk does not apply to the creator
  path.
- **Filename traversal is blocked.** `validate_filename` (`apply.rs:1365-1378`) rejects
  non-bare names, `/`, `\`, `.`, `..`, and non-`.ir.json` — tested at `:1517-1523`. Documents
  are written into a fresh `tempfile::TempDir` per request (`:1298-1323`).
- **Identifier quoting on the direct-DDL paths is correct.** `quote_ident` doubles embedded
  quotes (`apply.rs:1456`, `provisioning.rs:50`) and is used for every schema/role identifier
  interpolated into `CREATE SCHEMA` / `GRANT` / `ALTER ROLE`. Store queries are all
  `zeroship.`-qualified and parameterized.

---

## Ranked summary

| # | Sev | Location | Issue |
|---|-----|----------|-------|
| H1 | HIGH | apply.rs:417-677 | Preflight + approval classification + store write run outside the app advisory lock; concurrent same-app applies race the "needs approval?" decision on a stale snapshot |
| H2 | HIGH | schema_roles_extensions.ts:15-23 | Static DB-role passwords equal to rolenames committed to git (incl. `bypassRls` control/auth roles) |
| M1 | MED | apply.rs:1470-1509 | `provision_runtime_app_role` skips the `tuple concurrently updated` retry; concurrent cross-app applies can 503 |
| M2 | MED | apply.rs:203,425 | No submit-side dedup/cap on gated migrations → unbounded `pending_approval` rows an operator must triage |
| M3 | MED | migration_store.rs:138-156 | `revert_to_pending` unguarded on status — latent resurrection of terminal rows |
| M4 | MED | migration_store.rs:158-195 | `mark_applied`/`mark_rejected` unguarded; store status can disagree with the engine journal on the half-applied error path |
| M5 | MED | migration_store.rs:232, policy_store.rs:159 | Fresh un-pooled PG connection per store op → connect storm / DoS amplification; silent best-effort audit |
| M6 | MED | auth.rs:123 | Per-call random request-id passed to bearer verifier defeats any request-id replay correlation |
| L1 | LOW | apply.rs:644-663 | Seal is in-process tamper-detection only (dev key hardcoded); audit wording overstates it |
| L2 | LOW | apply.rs:1460-1509 | `quote_lit`-then-`EXECUTE` DDL string-building is fragile; use `%I`/`quote_ident` inside `DO` blocks |
| L3 | LOW | policy.rs:69 | No minimum seal-key length enforced at construction |
| L4 | LOW | apply.rs:812 | File ordering relies on shared tempdir parent; sort on `file_name()` to state the invariant |
| L5 | LOW | adapter lib.rs:145 | Unknown future `Bind` variant silently → NULL bind (seam-drift hazard) |
| L6 | LOW | driver-mysql2.ts:91 | `multipleStatements: true` widens injection blast radius on the MySQL driver seam |
