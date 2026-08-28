# SC-4: the dev and HMR mechanism

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** merge 5b of that document. Deleting registration breaks the dev
vector unless this lands with it.

---

## Why this exists

The parent proposal says "a fresh dev isolate is a simpler and testable
boundary". That names an **outcome**, not a mechanism, and three decisions hide
behind it - one of which is a security-scope decision that belongs in a document
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
(`sdks/vite-plugin/src/gen-types/addon.ts:129,141`).

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

The check belongs where the URL is resolved, so **both** the dev server and the
migrate command inherit it from one place.

## Decision 2 - descriptor HMR restarts the runtime under supervision

`serve.rs` builds one runtime under one accept loop
(`crates/zeroship-runtime/src/core/serve.rs:1719-1795`). Two mechanisms could
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

## Decision 3 - the private module map does NOT apply in dev, and that is stated

This is the security-scope decision that must not be left implicit.

In dev the module graph is **Vite's**, not `ModuleRegistry`'s, and
`__zeroshipNodeBuiltin` remains installed because Vite's `fetchModule` is its
only consumer. The parent proposal deletes that bridge from the **production**
vector only.

So the dev vector does **not** carry the private module map, and the invariant
that replaces it is different in kind.

An earlier draft named the wrong invariant: "the dev vector is unreachable from
a deployed isolate, asserted by a gate arm on the production constructor",
pointing at `__zeroshipNodeBuiltin`. That is insufficient twice over.
`__zeroshipNodeBuiltin` carries **no capability a deployed isolate does not
already have** - it is an alias for the same `resolve_native` set production
reaches by a plain `import`. Meanwhile the dev vector that *does* carry a
capability is elsewhere and was unmentioned: SSRF validation is **skipped
entirely** in dev -

> `// In dev mode, skip host/IP validation (allows localhost fetch to Vite).`
> `if dev_mode_enabled() { return Ok(()); }`

- and `dev_mode_enabled()` resolves an **ambient environment read**,
`declared_env!(dev, "ZEROSHIP_DEV", ...)`. An env var is not a construction
boundary.

**The invariant is therefore: dev-ness is a typed input derived from the
runtime's identity, never an ambient environment read.**

The tree already states this principle, in the very place that would be hit by
a leak (`crates/zeroship-worker/src/main.rs:112-113`):

> The authority is the worker's identity, not an env flag: SQLite is refused
> even if `ZEROSHIP_DEV=1` leaked into a prod worker.

That is the standard to meet, and the SSRF gate does not meet it today. The
acceptance arm is correspondingly different: not "is a symbol absent from the
production constructor", but **"does a deployed worker still enforce host/IP
validation with `ZEROSHIP_DEV=1` present in its environment"** - a question the
old arm could not even ask.

## The SQLite dev epoch

Decision 1 makes this answerable, which is why the parent proposal deferred it
here: the dev writer is always the SQLite apply path, so **it** writes the
first stable epoch row, in the same transaction as its DDL, exactly as SC-2
specifies for `__zeroship_state`. There is no second dev writer to coordinate
with, because a Postgres dev URL no longer reaches this path at all.

## Acceptance shape

- A Postgres `DATABASE_URL` in dev fails with a named error naming the tier
  limitation - and, specifically, **no SQLite file is created or written** as a
  side effect of that run.
- After a descriptor-changing edit, a removed collection is absent from
  `env.db` on the next request - **and that request runs under a new
  runtime/isolate generation**, asserted against an exposed supervisor
  generation counter.

  The generation half is what makes this arm test Decision 2 at all. Today's
  in-place path already produces the removed-collection outcome without any
  restart: the dev bootstrap reapplies the descriptor and resets the latch
  (`sdks/vite-plugin/src/dev-bootstrap/index.ts:183-185`) and `installSchema`
  deletes stale names and defines the replacements
  (`sdks/bootstrap/src/install-schema.ts:1475-1508`). So an arm asserting only
  absence **passes on the very mechanism this document rejected** - and would
  keep passing if someone renamed the latch while preserving the mutation.
- After such an edit with no migration applied, DB operations fail closed rather
  than serving the previous descriptor's metadata.
- `resetSchemaInstalled` has no callers.
- **A deployed worker still enforces host/IP validation with `ZEROSHIP_DEV=1`
  present in its environment.** This is the arm Decision 3 argues for, and it
  replaces the `__zeroshipNodeBuiltin`-absence arm an earlier draft listed here.
  Leaving that arm in the acceptance list while the body explains why it is
  insufficient is worse than never having written it: the document would ship
  its own rejected test as the thing an implementer is measured against, and
  that implementer has no reason to read the body once the checklist looks
  complete. The bypass is real and one branch deep -
  `if dev_mode_enabled() { return Ok(()); }` in `validate_url`
  (`crates/zeroship-runtime/src/transport/ssrf.rs:206-207`), reached through a
  process-wide cell resolved from an ambient variable.

  `__zeroshipNodeBuiltin`-absence may still be asserted, but as a **hygiene**
  arm, never as the security arm - it fences a symbol that carries no capability
  a deployed isolate lacks.
- A fresh dev database gets its first stable epoch row from the SQLite apply
  path, and a data operation before any apply fails with `SCHEMA_NOT_APPLIED`.
