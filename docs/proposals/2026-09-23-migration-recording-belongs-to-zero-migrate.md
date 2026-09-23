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

Narrower than it looks, because `zero-migrate` needs no new CAPABILITY. Measured: its CLI already
drives SQLite - `DriverConfig` carries `{ kind: "sqlite", appPath, journalPath }`, and the driver
resolver accepts `sqlite:<path>` or a bare `.sqlite` / `.db` path with `--journal` as an override -
and it already accepts all three zeroship-specific inputs the dev path supplies: `--owner-app`,
`--registry` (the table-ownership registry) and `--policy` (ordered charter layers). A creator can
apply the dev database by hand today with nothing but that CLI.

What is missing is an EXPORT, not a feature. The library should expose the directory-level entry
its own CLI already uses internally - discover, order, load, record, apply - and
`zeroship-dev-migrate` should shrink to the three things only it can do:

- read `zeroship.jsonc` to select the database, its label, whether it is the app's primary, and its
  migrations and out dirs;
- run gen-types, because `env.db.ts` and `schema.runtime.json` are zeroship concepts the engine has
  never heard of, and because the apply's ownership registry is keyed on the descriptor's
  collections, so one command has to produce both or they disagree;
- derive the arguments - registry, policy, owner app, SQLite path - from that config, rather than
  making a creator hand-type four values the file already declares.

`recorder.ts` is deleted rather than kept beside it, and with it the plugin's direct use of the
addon: the plugin stops naming a verb, which also settles the operator's objection to a
dialect-specific one reaching that far up the stack.

The order contract, the uniqueness guard and the extension set then have ONE definition. Today they
have two, and the two differ.

## The duplicated-DSL hazard is real but LOUD, which lowers the cost of this change

`recorder.ts` bundles with the DSL marked external for a stated reason:

> its `@zeroship/migrate` DSL import must resolve to the SAME module instance the recorder drains
> from - a duplicated DSL module would drain an empty op list

That describes the shape correctly but not the current failure. MEASURED in
`packages/zero-migrate/src/ops.ts`: the recording buffer is a module-level `let active`, the host
calls `__begin` on the instance IT imported, and every authoring call goes through `recorder()`,
which refuses a null buffer with a structured error:

```
OP_OUTSIDE_RECORDER: migration operations may only be authored synchronously
inside schema(), data(), or inverse()
```

So with two instances the migration's first `table()` call reaches an instance whose buffer was
never begun, and it THROWS with a code and a suggested fix. It does not record nothing and pass.
`__drain` does return an empty list when its buffer is null, but that is the host's own instance,
which it began a moment earlier.

Two consequences for this change. First, consolidating the loaders is safer than the comment
implies: the invariant is enforced at the authoring call, not merely assumed by whoever remembers
to pass `external`. Second, the guard worth naming is that structured refusal rather than the
aggregate one downstream - though `migrate-dev.ts` treating `applied=0 skipped=0` as a failing
exit remains the backstop for a genuinely empty set, and should stay pointed at the consolidated
path.

What stays unguarded is the legitimately empty migration: a `schema()` whose body records nothing
drains empty and raises no error, because `enforceRecordedPhase` only refuses ops recorded in the
WRONG phase and finds nothing to object to in an empty list. That is the case the downstream
`applied=0` check exists for.

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

(c) **A duplicated DSL instance is refused by CODE, not by convention.** Force the consolidated
path to resolve two `@zeroship/migrate` instances and assert the run fails with
`OP_OUTSIDE_RECORDER` specifically, not merely with some error. Pinning the code is the point: any
error would pass a weaker assertion, including one thrown for an unrelated reason, and the claim
being protected is that the authoring call itself refuses. Control: the same set through a single
instance records its ops, so the arm is not passing by failing everything.

(d) **The dev tier still applies and serves.** After the change, a migrate followed by a dev boot
applies the example's migrations and answers a query against the file it wrote. Fails if the apply
targets a file the runtime does not open, which is the disagreement the dev tier was just repaired
for.
