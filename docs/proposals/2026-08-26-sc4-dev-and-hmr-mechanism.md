# SC-4: the dev and HMR mechanism

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`. **Decision 4 is
implemented** (`8c6caa465`); decisions 1, 2 and 3 are not.

**Gates:** merge 5c of that document, which lists "the SC-4 dev mechanism" and
"deletion of registration" as one co-landing step (`design.md:1406-1408`). Not
5b: 5b is the identity substrate and is blocked on Fork C's home
(`design.md:1394-1397`).

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## Why this exists

The parent proposal says "a fresh dev isolate is a simpler and testable
boundary". That names an **outcome**, not a mechanism, and four decisions hide
behind it - two of which are security-scope decisions that belong in a document
rather than in whatever the first implementer assumes.

## Decision 1 - a Postgres dev URL is REJECTED, with a typed error

Today it is neither implemented nor rejected: it is **silently misapplied**.

`migrate-dev.ts` resolves `databaseUrl` through `resolveDatabaseUrl`
(`sdks/vite-plugin/src/cli/migrate-dev.ts:118-122`), whose precedence is shell
`DATABASE_URL`, then project `.env`, then the SQLite default - so the value can
legitimately be a Postgres URL. It then calls, with no branch on the scheme:

```ts
const { appPath } = devSqlitePaths(root, DEV_APP_ID, databaseUrl);
const reply = await applyMigrationsToDevSqlite({ root, migrationsDir, collections, databaseUrl });
```

(`migrate-dev.ts:125-131`). The addon behind that call exposes only
`applyIrSqlite`, which its own comment calls "the dev tier's schema authority"
(`sdks/vite-plugin/src/gen-types/addon.ts:154`, declared at `:166`).

So a developer who exports a Postgres `DATABASE_URL` gets their migrations
applied to a **SQLite file**, and the command reports success. That is the worst
of the three possible behaviours.

It is worse still than one command misbehaving, because the same URL reaches a
second consumer that answers differently: the dev server passes
`DATABASE_URL: databaseUrl` straight into the runtime child's environment
(`sdks/vite-plugin/src/dev-server.ts:938`). So the migrations land in SQLite
while the runtime is told to use Postgres - the developer ends up with a schema
in one database and a runtime pointed at another, with no error from either
side. That divergence is the strongest argument for rejecting at the source
rather than fixing one call site.

**Decision: reject.** A non-SQLite dev database URL fails immediately with a
named, actionable error rather than being routed to the SQLite path. Rejection
rather than implementation because:

- the dev tier is deliberately a different tier, not a smaller production
  (`docs/reference/auth-dev-tier.md` documents the same split for auth);
- implementing a real Postgres dev apply path is substantial work serving no
  current requirement, and half-implementing it is how the present defect
  arose;
- rejection is honest and reversible - a later Postgres dev path replaces an
  error, whereas today's silent misapplication has to be discovered first.

**The shared resolution point this decision needs already exists.** Both
consumers go through one module, `sdks/vite-plugin/src/dev-database-url.ts`,
whose header states that sharing the resolution is the whole point: the dev
server imports `resolveDatabaseUrl` / `parseDotenvVars` / `logDatabaseUrlSource`
(`dev-server.ts:56-59`, called at `:648` and `:928`) and the migrate CLI imports
the same three (`migrate-dev.ts:32-34`, called at `:118`). **That module
contains no scheme check** - `resolveDatabaseUrl` returns the first of shell,
`.env`, default and inspects nothing (`dev-database-url.ts:47-59`). So what is
owed is one branch there, not a new plumbing path, and both consumers inherit it
by construction.

**OWED: three error names, none of which exists as a symbol.** This heading
promises a *typed* error and the decision says "named, actionable" - and no
code, type or symbol for it appears in this document or in the set. Nor can an
implementer copy the naming from the two names this document and the parent use
as examples: **`SCHEMA_NOT_APPLIED` and `SCHEMA_METADATA_MISMATCH` occur zero
times in `crates/` and `sdks/`** (measured 2026-08-28). They are specification
names - the parent introduces `SCHEMA_NOT_APPLIED` for a missing app file in
section 3.13 (`design.md:930-933`) - not existing symbols an implementer can
reach for. All three names have to be minted, in one place, as part of the
contract the dev server and the migrate command both inherit. An implementer
must not invent them silently.

## Decision 2 - descriptor HMR restarts the runtime under supervision

`serve.rs` builds one runtime under one accept loop (`run_single_worker`,
`crates/zeroship-runtime/src/core/serve.rs:1719-1795`). Two mechanisms could
give a fresh isolate on a descriptor change:

- **a runtime manager**: hold the runtime behind a swappable handle, build the
  new one, swap, drain the old;
- **supervised restart**: the dev supervisor rebuilds the runtime process.

**Decision: supervised restart.** It adds no new concurrency to the runtime for
a dev-only path, and the dev tier already tolerates restarts. A swappable handle
introduces exactly the shared-mutable-state class this proposal spends its
length removing, in the one vector where the cost of a restart is a few hundred
milliseconds of developer time.

The observable contract is what tests assert, not the mechanism: after a
descriptor-changing edit, the next request is served by an isolate whose
`env.db` matches the new descriptor, and a **removed collection is absent**. If
the matching migration has not been applied, DB operations fail with
`SCHEMA_NOT_APPLIED` or `SCHEMA_METADATA_MISMATCH` rather than serving stale
metadata.

This also deletes `resetSchemaInstalled` (`sdks/vite-plugin/src/dev-bootstrap/index.ts:121`,
`:185`), which exists only to make the old in-place mutation scheme work.

### Current position: the supervisor needs a specification, not an implementation

"Supervised restart" names no process, no restart trigger and no drain, and it
does not define the **exposed supervisor generation counter** that this
decision's own acceptance arm asserts against. That counter is not decoration:
the arm below argues it is the only thing that makes Decision 2 testable at all,
because the outcome half of the arm passes on the in-place mechanism this
decision rejects. **A decision whose test depends on an artifact the decision
does not specify is not implementable as written.** The next move on Decision 2
is to commission that specification - process, trigger, drain, counter - not to
start building against this section.

**A constraint on whoever writes it: the dev descriptor's only delivery path is
the HMR re-apply this decision deletes.** The dev descriptor is not frozen at
isolate construction; the dev vector re-applies it, and the channel is live end
to end (measured 2026-08-28). `HMR_POLL_PATH`
(`sdks/vite-plugin/src/constants.ts:5`) is served at `dev-server.ts:757` and
carries `runtimeDescriptorJson` at `:763-775`; that field is filled by the
watcher's regen in the `hotUpdate` branch (`dev-server.ts:1136-1137`), a
different process from the `pnpm migrate` CLI that applies the schema. On the
runtime side `dev-bootstrap/index.ts:173-174` polls it and `:183-185` applies
the descriptor and calls `resetSchemaInstalled()`. If the supervisor deletes
that re-apply before a working restart trigger exists, the dev tier has no
descriptor delivery at all. Whoever lands the supervisor owns replacing this
path in the same change.

## Decision 3 - the private module map does NOT apply in dev, and that is stated

This is a security-scope decision that must not be left implicit.

In dev the module graph is **Vite's**, not `ModuleRegistry`'s, and
`__zeroshipNodeBuiltin` remains installed because Vite's `fetchModule` is its
only consumer (`sdks/vite-plugin/src/environment.ts:64-68`; the bridge is
installed unconditionally at `crates/zeroship-runtime/src/core/init.rs:2261-2268`,
defined at `core/native_modules.rs:63-80`). The parent proposal deletes that
bridge from the **production** vector only (`design.md:442-443`).

So the dev vector does **not** carry the private module map, and the invariant
that replaces it is different in kind. That replacement invariant is Decision 4,
which is stated separately because it is a decision in its own right and
produces this document's strongest acceptance arm.

## Decision 4 - dev-ness is a typed input, never an ambient env read - IMPLEMENTED

**The invariant: dev-ness is a typed input derived from the runtime's identity,
never an ambient environment read.**

It is not `__zeroshipNodeBuiltin`'s absence from the production constructor.
That symbol carries **no capability a deployed isolate does not already have** -
it is an alias for the same `resolve_native` set production reaches by a plain
`import`. The dev vector that *does* carry a capability is the SSRF gate, and
until 2026-08-27 it resolved dev-ness from `declared_env!(dev, "ZEROSHIP_DEV",
..)` and returned `Ok(())` for every host above the host lookup. An env var is
not a construction boundary: `ZEROSHIP_DEV=1` exported into a production
`zeroship-worker` turned validation off for every `fetch` that worker made.

The tree already stated the opposite standard about the same process
(`crates/zeroship-worker/src/main.rs:140-147`):

> The authority is the worker's identity, not an env flag: SQLite is refused
> even if `ZEROSHIP_DEV=1` leaked into a prod worker.

**`8c6caa465` makes the SSRF gate meet it.** Two changes, both load-bearing:

- **The gate no longer reads the environment.** `dev_mode_enabled`
  (`crates/zeroship-runtime/src/transport/ssrf.rs:70`) returns only what
  `set_dev_mode` (`:119`) stored, and a process where nothing called it holds
  `false`. The surviving environment read is a separate function,
  `dev_mode_from_process_env` (`:89`), which no gate calls; its sole caller is
  `cmd_serve` (`crates/zeroship-cli/src/main.rs:95`) - the binary that is the
  dev tier by identity. `zeroship-worker` never calls it, which is what makes a
  leaked `ZEROSHIP_DEV=1` inert there. The module states the rule at `:19-22`.
- **The relaxation is loopback only.** The blanket early return is gone. All
  three consultation points - `validate_url` / `validate_url_under` (`:239`,
  `:249`), `SsrfResolver` (`:376`) and `transport::egress::filter_answer` - now
  share one predicate, `is_blocked_ip_under_dev` (`:222`), so "what dev opens"
  has one answer instead of three. Metadata, RFC1918, CGNAT and link-local stay
  refused in dev.

The arms are in the tree, each with an isolated-child-process twin because the
cell is process-wide: `zeroship_dev_in_the_environment_cannot_disable_the_guard`
(`ssrf.rs:507`), `dev_relaxation_is_loopback_only` (`:563`),
`absent_dev_relaxation_fails_closed` (`:617`).

**`__zeroshipNodeBuiltin`-absence may still be asserted, but as a hygiene arm,
never as the security arm** - it fences a symbol that carries no capability a
deployed isolate lacks. Stated here because the checklist below is what an
implementer is measured against, and shipping a rejected test in it is worse
than never having written it.

## No dev epoch, and no dev authority domain

**CONTESTED, FLAGGED 2026-09-04.** `docs/proposals/2026-08-28-app-database-decoupling.md`
says the opposite - "A dev-tier equivalent is owed and is not specified here" - and until
today neither document cited the other. Worse for the argument below: the review it leans on,
`docs/reviews/2026-08-28-sqlite-authority-row.md`, closed with "if the epoch's question ever
returns, the carrier already exists", and that is now FALSE. The carrier it named,
`__zeroship_migrations.schema_version`, was deleted along with `next_schema_version` and the
whole audit-table capability when the data plane's last DDL was removed. So "nothing to
specify" can no longer rest on "a carrier is already there if we need one".

The rest of this section is the original argument, which stands on its own terms - the
runtime descriptor really is the sole schema authority on this tier - but it is one side of
an open disagreement, not a settled conclusion.

There is nothing here to specify. SC-2 states that on this tier there is **no
`__zeroship_state` row, no epoch, and no `AuthorityRead` command**; the runtime
descriptor is the sole schema authority (SC-2, "No database-resident authority
row on this tier"; the column-by-column argument is
`docs/reviews/2026-08-28-sqlite-authority-row.md`). SC-2 also records that
`(system_identifier, timeline_id)` identifies a PostgreSQL cluster and its
recovery timeline, that a local file has neither, and that **the dev tier
therefore has no PITR-resurrection defence** - the developer owns the bytes, and
no scheme inside the file changes that.

Decision 1 still matters to this: it makes the SQLite apply path the *only* dev
writer, so there is no second writer to coordinate with.

**Open: where a dev binding's incarnation comes from.** SC-5 makes
`AppIncarnationId` part of every binding and compares it before any data SQL,
and Fork C's storage is unhomed - the identity state lived in a platform-schema
row that is deleted. SC-2 removes the file-resident option for this tier
outright. Neither document answers what a dev binding carries. This states the
question; it does not answer it.

## Open questions with no owner yet

- **The relationship to
  `docs/proposals/2026-08-09-dev-sqlite-migration-apply-ahead-of-runtime.md`.**
  That proposal owns the dev tier's apply-ahead-of-runtime path - the same path
  Decision 1 rejects a Postgres URL from. Whether this document supersedes it,
  depends on it, or assumes it already landed **is recorded nowhere**: the only
  cross-reference in `docs/proposals/` is this bullet (measured 2026-08-28), and
  that proposal's own header still reads "PROPOSED - not implemented" while the
  apply it describes exists as `pnpm migrate` (`migrate-dev.ts`) and the dev
  server only reports rather than applies (`reportDevSchemaState`,
  `dev-server.ts:392-444`). Someone owns reconciling those two statements; this
  document does not.
- **The dev and `zeroship serve` ceiling source.** `zeroship serve` and the Vite
  dev vector are separate composition points from the worker, and neither this
  document nor SC-5 covers them; a dev tier with no ceiling source plus SC-6's
  "failure is denial" denies every non-`auto` unmask in dev, permanently. SC-6
  owns the record (SC-6, "Owed: the dev and `zeroship serve` ceiling source is
  not specified anywhere"); it is repeated here only so a reader of SC-4 knows
  it exists.

## Acceptance shape

- A Postgres `DATABASE_URL` in dev fails with a named error naming the tier
  limitation - and, specifically, **no SQLite file is created or written** as a
  side effect of that run.
- **The runtime child is never started against a rejected URL.** This is the
  half that matches Decision 1's argument, which is a *divergence*: migrations
  land in SQLite while the dev server hands the runtime child
  `DATABASE_URL=<the Postgres URL>` (`dev-server.ts:938`). Without this arm the
  rejection can be implemented in one call site, which is precisely the failure
  the decision says it is avoiding.
- After a descriptor-changing edit, a removed collection is absent from
  `env.db` on the next request - **and that request runs under a new
  runtime/isolate generation**, asserted against an exposed supervisor
  generation counter.

  The generation half is what makes this arm test Decision 2 at all. Today's
  in-place path already produces the removed-collection outcome without any
  restart: the dev bootstrap reapplies the descriptor and resets the latch
  (`sdks/vite-plugin/src/dev-bootstrap/index.ts:103`) and `installSchema`
  deletes stale names and defines the replacements
  (`sdks/bootstrap/src/install-schema.ts:1225`). So an arm asserting only
  absence **passes on the very mechanism this document rejected** - and would
  keep passing if someone renamed the latch while preserving the mutation.
- After such an edit with no migration applied, DB operations fail closed rather
  than serving the previous descriptor's metadata.
- On a **fresh** dev database, a data operation before any apply fails closed
  under the tier's not-applied error. Distinct from the arm above: that one is a
  database whose schema has drifted, this one is a database that has never been
  migrated. Today the dev server only *reports* this state and keeps serving
  (`reportDevSchemaState`, `dev-server.ts:392-444`), so the runtime-side failure
  is the part that is owed - and it needs one of the three names Decision 1 owes.
- `resetSchemaInstalled` has no callers.
- **LANDED** - a deployed worker still enforces host/IP validation with
  `ZEROSHIP_DEV=1` present in its environment
  (`zeroship_dev_in_the_environment_cannot_disable_the_guard`,
  `crates/zeroship-runtime/src/transport/ssrf.rs:507`), the stated relaxation
  opens loopback and nothing else (`:563`), and a process that stated no mode
  runs the whole guard (`:617`). Each has an isolated-child-process twin at
  `:543`, `:598`, `:628`.
