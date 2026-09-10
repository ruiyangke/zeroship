/**
 * Task #245 - the `isolationLevel` vocabulary was recorded in multiple
 * places and no two agreed. The runtime (`normalize_isolation_level`,
 * `crates/zeroship-data-v8/src/v8_classes/db.rs`) accepts three spellings per
 * level (camelCase, SQL-spaced, uppercase SQL) for all four levels, but
 * the only *published* TypeScript union is `ZeroshipIsolationLevel`
 * (`sdks/types/shared.d.ts`) - the SQL-spaced lowercase form. That is the
 * type `@zeroship/db`'s `Db<Schema>.transaction()` is checked against
 * (`sdks/db/src/db-types.ts` `TransactionOptions.isolationLevel:
 * IsolationLevel`, `IsolationLevel = ZeroshipIsolationLevel` in
 * `sdks/db/src/types.ts`).
 *
 * `sdks/types/db.d.ts`'s raw native `ZeroshipDb.transaction()` - the type
 * a creator gets from `import { env } from "zeroship"; env.db.transaction`
 * without going through `@zeroship/db` - used to declare its OWN
 * three-value camelCase-only literal (`"readCommitted" | "repeatableRead"
 * | "serializable"`), independently of `ZeroshipIsolationLevel`. It
 * disagreed on both spelling AND count: it dropped `readUncommitted`
 * entirely, camelCase or otherwise, even though the runtime accepts it
 * (measured 2026-08-10 by driving `db.transaction({isolationLevel:
 * "readUncommitted"})` against a live `pnpm dev` server - accepted, no
 * error). That is now fixed to reference `ZeroshipIsolationLevel`
 * directly, so the two surfaces cannot drift apart again silently.
 *
 * This file is a type-only regression test in the `@ts-expect-error`
 * idiom already used elsewhere in this package (see
 * `filter-encryption-types.test.ts`, `c2-union.test.ts`): each marked
 * line MUST fail to typecheck, or `tsc --noEmit` reports an "unused
 * @ts-expect-error directive" error. Verified directly (not only via
 * `node --test`, which runs these files through `tsx` and does not
 * typecheck them):
 *
 *   npx tsc --noEmit --strict --skipLibCheck \
 *     --moduleResolution Bundler --module ESNext --target ES2022 \
 *     -p <a tsconfig including this file, sdks/db/src, and
 *         sdks/types/*.d.ts>
 *
 * MUTATION PROOF (performed by hand against `sdks/types/db.d.ts`, not
 * left in the tree): reverting the fix back to the old three-value
 * camelCase literal made the `rawTransaction(..., { isolationLevel:
 * "read uncommitted" })` line below (the canonical spelling, no
 * @ts-expect-error) fail with:
 *
 *   TS2820: Type '"read uncommitted"' is not assignable to type
 *   '"serializable" | "repeatableRead" | "readCommitted" | undefined'.
 *   Did you mean '"readCommitted"'?
 *
 * Restoring the fix made it compile clean again. WHAT THIS DOES NOT
 * CATCH: a fifth level or spelling added to `ZeroshipIsolationLevel`
 * without a matching change to the runtime's `normalize_isolation_level`
 * match arms (or vice versa) - nothing here cross-checks the TypeScript
 * union against the Rust match arms at runtime; that pairing has to be
 * kept by hand on both sides.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { t, schema, type Db, type TransactionOptions } from "@zeroship/db";

const s = { widgets: schema({ name: t.string().required() }) } as const;
// Every check below lives inside a function that is declared but never
// called: these are TYPE assertions, checked by `tsc --noEmit`, and must
// not run as real code. `tsx` (the test runner `npm test` uses) strips
// types without checking them, so a top-level call against a `declare
// const` would compile away to a real `undefined.transaction(...)` call
// and crash at runtime with no bearing on the thing under test.
function typeOnly_neverCalledAtRuntime(db: Db<typeof s>, rawTransaction: ZeroshipDb["transaction"]) {
  // --- @zeroship/db's published surface (TransactionOptions.isolationLevel) ---

  const okSdkSpaced: TransactionOptions = { isolationLevel: "read uncommitted" };
  const okSdkSerializable: TransactionOptions = { isolationLevel: "serializable" };
  // @ts-expect-error "repeatableRead" (camelCase) is not in the published
  // ZeroshipIsolationLevel union - this is the exact TS2820
  // docs/reference/db.md now documents ("Did you mean '"repeatable read"'?").
  const badSdkCamel: TransactionOptions = { isolationLevel: "repeatableRead" };

  void okSdkSpaced;
  void okSdkSerializable;
  void badSdkCamel;

  db.transaction(async (tx) => {
    await tx.widgets.find({});
  }, { isolationLevel: "read uncommitted" });

  // --- the raw native surface (ZeroshipDb.transaction in sdks/types/db.d.ts) ---
  //
  // `ZeroshipDb` is an ambient global (no import) via the `@zeroship/types`
  // `types` array in this package's tsconfig - same mechanism creator code
  // gets it through; taken as a parameter type here only so this function
  // signature pins the same ambient name without an explicit import.

  // Canonical spelling - MUST compile. This is the line that was broken: the
  // old `ZeroshipDb` literal rejected every form of "read uncommitted",
  // camelCase or SQL-spaced, because the arm was missing outright.
  rawTransaction(async () => {}, { isolationLevel: "read uncommitted" });
  // @ts-expect-error camelCase was never published on ZeroshipIsolationLevel;
  // ZeroshipDb now matches that decision instead of carrying its own
  // independent (and, before the fix, incomplete) list.
  rawTransaction(async () => {}, { isolationLevel: "readUncommitted" });
  // @ts-expect-error not a real isolation level under any spelling.
  rawTransaction(async () => {}, { isolationLevel: "totallyBogus" });
}
void typeOnly_neverCalledAtRuntime;

test("isolation-level vocabulary: the type-only assertions above compile as annotated", () => {
  // This file's real assertions are the @ts-expect-error markers above,
  // checked by `tsc --noEmit`, not by this runtime test (tsx strips types
  // without checking them). This test exists so the file is still a valid
  // node:test module and shows up green in `npm test`.
  assert.ok(true);
});
