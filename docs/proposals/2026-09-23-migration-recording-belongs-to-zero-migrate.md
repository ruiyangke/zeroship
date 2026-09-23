# Migration recording belongs to zero-migrate

**Status.** PROPOSED, nothing built. The change is to stop the vite plugin discovering, loading and
recording migrations itself, and to have it hand a directory to `zero-migrate` instead. The payoff
is one implementation of the migration ORDER CONTRACT rather than two that already disagree. The
cost is that `zero-migrate` gains a directory-level export, and one of the two TypeScript-loading
strategies has to win.

---

## Two implementations of one job

Discovering migration files, loading them, and draining the authoring DSL into IR envelopes
happens twice in this repo.

`packages/zero-migrate-cli/src/cli.ts` does it for the CLI, in functions that are all PRIVATE to
that file:

- `discover` reads the directory and sorts by filename, commented as "the migration order
  contract".
- `assertUniqueMigrationTimestampPrefixes` refuses two migrations sharing a version prefix.
- `ensureTsLoader` lazily registers `tsx` so a `.ts` migration can be `import()`ed under plain
  Node, and fails with actionable guidance when it cannot.
- `importMigration` / `importMigrations` dynamic-import the ordered set.

`packages/vite-plugin/src/gen-types/recorder.ts` does it for the plugin, in `recordMigrationsDir`,
by esbuild-bundling each migration to a temp `.mjs` with `@zeroship/migrate` marked EXTERNAL and
then importing the bundle.

`zero-migrate`'s public surface (`packages/zero-migrate-cli/src/index.ts`) exports `apply`,
`rollback`, `plan`, `validate`, `status`, `history` and `baseline` - every one of which takes an
already-imported `MigrationModule`. The directory walk is never exported. That absence is why the
plugin grew its own: the capability exists, but not at a boundary anyone else can call.

## They already disagree, and the divergences are silent

Measured by reading both, not inferred from the shapes:

**Duplicate version prefixes.** The CLI refuses them. The plugin's discovery sorts by the 14-digit
prefix with "ties broken by stem", so two migrations stamped at the same instant are ordered by
name and applied. One path treats that as a corrupt set; the other picks an order.

**Which files are migrations.** The CLI accepts `.ts`, `.mts`, `.cts`, `.js`, `.mjs` and `.cjs`,
excluding `.d.ts`. The plugin's `MIGRATION_TS_RE` accepts only `.ts` with a
`<14-digit>_<desc>.ts` grammar. A `.js` migration is a migration to one and invisible to the other.

Neither divergence announces itself. A creator meets them as "it worked in dev", which is the
failure mode the dev tier exists to avoid.

## The change

`zero-migrate` exports a directory-level entry - discover, order, load, record, apply - and the
plugin calls it with the migrations dir and a driver. `recorder.ts` is deleted rather than kept
beside it, and with it the plugin's direct use of the addon: the plugin stops naming a verb, which
also settles the operator's objection to a dialect-specific one reaching that far up the stack.

The order contract, the uniqueness guard and the extension set then have ONE definition. Today they
have two, and the two differ.

## The hazard this must not reintroduce

`recorder.ts` bundles with the DSL marked external for a stated reason:

> its `@zeroship/migrate` DSL import must resolve to the SAME module instance the recorder drains
> from - a duplicated DSL module would drain an empty op list

A consolidated path that ends up with two instances of `@zeroship/migrate` records NOTHING and
reports success: `applied=0`, no error, no throw. That is a wrong answer wearing a success's
clothes, and it is invisible to any check that only asks whether the apply threw.

One guard already exists and must stay pointed at the consolidated path: `migrate-dev.ts` treats
`applied=0 skipped=0` as a FAILURE and exits non-zero, on the stated grounds that nothing applied
and nothing skipped on a fresh database means the schema is unchanged. That guard is the reason
this hazard is survivable, so it is a precondition of the change rather than a detail of it.

## Open

1. **Which loading strategy survives?** `tsx` registration and esbuild-bundle-with-external solve
   the same problem differently. The question that decides it is not which is faster: it is whether
   the surviving one GUARANTEES a single `@zeroship/migrate` instance, or merely happens to get one
   in the layouts tested so far. If neither guarantees it, the guarantee has to be asserted rather
   than assumed.

2. **Does the plugin still need esbuild after this?** It is a vite plugin, so esbuild is present
   regardless; the question is whether `recorder.ts` was its only migration-related use, which
   decides whether anything else moves with it.

3. **Does `zero-migrate` want a Node-API or a process boundary?** The plugin imports it today as a
   workspace package. A directory-level export is an import; the CLI binary is a subprocess. The
   existing recorder comment says "no CLI subprocess" deliberately, so this should be settled
   rather than inherited.

## Acceptance

Each arm names what makes it fail, because an arm that cannot fail measures nothing.

(a) **One order contract.** A directory whose filenames sort differently by prefix than by stem
applies in prefix order through both the plugin and the CLI. Fails if the two produce different
orders, which is the divergence being removed.

(b) **Duplicate prefixes are refused on both paths.** Two migrations sharing a version prefix are
rejected wherever they enter. Control: a set differing only in that one prefix is accepted, so the
arm is not passing by refusing everything.

(c) **A duplicated DSL instance is caught, not reported as success.** Force the consolidated path
to resolve two `@zeroship/migrate` instances and assert the run FAILS. Fails if it reports
`applied=0` and exits zero, which is the silent mode this whole section exists to prevent.

(d) **The dev tier still applies and serves.** After the change, a migrate followed by a dev boot
applies the example's migrations and answers a query against the file it wrote. Fails if the apply
targets a file the runtime does not open, which is the disagreement the dev tier was just repaired
for.
