// Executes every `.mig.js` in the op fixture corpus through the production
// `buildEnvelope` seam and compares the drained envelope to the committed
// raw-author envelope in `op_fixtures/recorded.json`.
//
// This is one half of a two-part check. `recorded.json` is the join: the Rust half
// (`crates/zero-migrate/tests/op_fixture_goldens.rs`) reads the same file, resolves
// those recorded ops through the real `resolve_create_table_policy`, and compares
// the result to `<stem>.golden.json`. Composed, the halves check `.mig.js` ->
// golden for all 27 stems. Neither half alone does, and each runs in the job that
// already has its toolchain.
//
// Before this, nothing executed a `.mig.js` at all. The Rust op matrix enumerated
// `*.golden.json` and skipped everything else, and this file re-authored six
// migration bodies INLINE in TypeScript and compared those against the goldens. A
// fixture and the golden it claimed to produce could therefore disagree forever in
// silence. The inline re-authorings and their `authorProjection` helper are gone:
// that helper derived the expected column set FROM the recorder output and then
// filtered the golden down to it, so a recorder that silently dropped a column
// still passed.
//
// There is deliberately NO re-bless environment variable here or in the Rust half.
// Regenerating `recorded.json` is a hand edit, because an easy update affordance is
// precisely what converts a corpus into a mirror of whatever the code emits today.

import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { test } from "node:test";

import { buildEnvelope, type MigrationModule } from "../src/internal/recorder.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixturesDir = resolve(here, "../../../crates/zero-migrate/tests/op_fixtures");

const MIG_SUFFIX = ".mig.js";
const GOLDEN_SUFFIX = ".golden.json";
const RECORDED_FILE = "recorded.json";

/** The corpus, committed rather than globbed. A directory listing cannot notice a
 *  fixture that went missing, so the list is the authority and the directory is
 *  checked against it in both directions. */
const EXPECTED_STEMS = [
  "alter_primary_key",
  "comments_indexes",
  "constraint_not_valid",
  "ddl_addcol_constraints",
  "ddl_alter",
  "ddl_create",
  "ddl_drop",
  "ddl_rename_table",
  "dialectal_ops",
  "dml",
  "dml_upsert",
  "edge_scalars",
  "enums_domains",
  "fluent_ddl",
  "fluent_dml",
  "fluent_scalars",
  "fluent_scalars_dml",
  "grouped_views",
  "in_list_scalars",
  "p2a_facets",
  "partition",
  "pg_aggregates",
  "pg_vendor",
  "runtime_options",
  "sequences_exclusion",
  "synchronize_identity",
  "views",
] as const;

/** `buildEnvelope` requires an `irVersion` because a real host passes the addon's
 *  `irVersion()`. The corpus join carries only `{ name, ops }` -- the surface a
 *  `.mig.js` actually determines -- so the stamped version is compared by neither
 *  half; the Rust half supplies the authoritative `CURRENT_IR_VERSION` when it
 *  rebuilds the envelope. */
const UNCOMPARED_IR_VERSION = 1;

/** One recorded fixture: the migration name and the drained op list. */
interface RecordedEnvelope {
  name: string;
  ops: unknown[];
}

async function readRecorded(): Promise<Record<string, RecordedEnvelope>> {
  return JSON.parse(await readFile(resolve(fixturesDir, RECORDED_FILE), "utf8"));
}

/**
 * Import one fixture and drain its migration phase through the production recorder
 * seam.
 *
 * A `.mig.js` under `crates/` imports the BARE specifier `zero-migrate`. Node
 * resolves a bare specifier by walking up from the IMPORTING file, and nothing
 * under `crates/` can see this package, so importing the file where it lives fails
 * with ERR_MODULE_NOT_FOUND (a working directory does not affect ESM resolution).
 * The source is read and its single bare import rewritten to an absolute URL, the
 * same in-memory rewrite `ops.test.ts` already uses -- chosen over a `node --import`
 * loader hook because it needs no change to the test command, and because pointing
 * the rewrite at a URL this file also imports is what keeps the recorder single.
 *
 * The rewrite targets `src/`, not `dist/`: letting the package export map resolve
 * the bare specifier would test the built output, and the authoring source is the
 * surface these fixtures exercise. Both this module's `internal/recorder.js` import
 * and the rewritten URL resolve to the same `src/ops.ts` instance, which is
 * load-bearing -- `__begin`/`__drain` and the fixture's `table()` must share one
 * ambient recorder or the drain returns an empty list.
 */
async function recordFixture(stem: string): Promise<RecordedEnvelope> {
  const source = await readFile(resolve(fixturesDir, `${stem}${MIG_SUFFIX}`), "utf8");
  const indexUrl = pathToFileURL(resolve(here, "../src/index.js")).href;
  const parts = source.split(`from "zero-migrate"`);
  assert.equal(
    parts.length,
    2,
    `${stem}${MIG_SUFFIX} must carry exactly one bare "zero-migrate" import to rewrite`,
  );
  const rewritten = parts.join(`from "${indexUrl}"`);
  const dataUrl = `data:text/javascript;base64,${Buffer.from(rewritten).toString("base64")}`;
  const mod = (await import(dataUrl)) as MigrationModule;
  // The name fallback is passed explicitly because `deriveNameFromPath` strips ONE
  // extension, which would turn `views.mig.js` into `views.mig`. The double
  // extension is an artifact of this corpus alone -- real migrations are
  // `<timestamp>_<slug>.ts` -- so the caller supplies the stem instead of the shared
  // helper being widened for a fixture-only shape.
  const envelope = buildEnvelope(mod, {
    irVersion: UNCOMPARED_IR_VERSION,
    nameFallback: stem,
  });
  return { name: envelope.name, ops: envelope.ops };
}

test("the op fixture corpus is exactly the committed stem list", async () => {
  assert.equal(new Set(EXPECTED_STEMS).size, EXPECTED_STEMS.length, "the stem list has no duplicates");
  assert.equal(EXPECTED_STEMS.length, 27, "the corpus is 27 stems");

  const migStems: string[] = [];
  const goldenStems: string[] = [];
  const unrecognized: string[] = [];
  for (const entry of await readdir(fixturesDir)) {
    // No skip branch: an entry that matches nothing is a failure, not a pass. A
    // loop that quietly continues past unmatched entries is how a corpus shrinks
    // without any test noticing.
    if (entry.endsWith(MIG_SUFFIX)) migStems.push(entry.slice(0, -MIG_SUFFIX.length));
    else if (entry.endsWith(GOLDEN_SUFFIX)) goldenStems.push(entry.slice(0, -GOLDEN_SUFFIX.length));
    else if (entry !== RECORDED_FILE) unrecognized.push(entry);
  }
  assert.deepEqual(
    unrecognized,
    [],
    `every op_fixtures entry is a ${MIG_SUFFIX}, a ${GOLDEN_SUFFIX}, or ${RECORDED_FILE}`,
  );

  const expected = [...EXPECTED_STEMS].sort();
  assert.deepEqual(migStems.sort(), expected, `the ${MIG_SUFFIX} set equals the committed stem list`);
  assert.deepEqual(goldenStems.sort(), expected, `the ${GOLDEN_SUFFIX} set equals the committed stem list`);
  assert.deepEqual(
    Object.keys(await readRecorded()).sort(),
    expected,
    `the ${RECORDED_FILE} key set equals the committed stem list`,
  );
});

test("every .mig.js records the committed raw-author envelope", async () => {
  const recorded = await readRecorded();
  let compared = 0;
  // Enumerated from the AUTHORING INPUTS. Keying this loop on the committed output
  // is the defect being closed: it lets an input drift away from the artifact it is
  // supposed to produce without any assertion ever running.
  for (const stem of EXPECTED_STEMS) {
    const actual = await recordFixture(stem);
    const expected = recorded[stem];
    assert.ok(expected, `${RECORDED_FILE} carries an entry for ${stem}`);
    assert.ok(
      actual.ops.length > 0,
      `${stem} records at least one op; an empty drain means the fixture and the recorder ` +
        "resolved to two different module instances",
    );
    assert.ok(expected.ops.length > 0, `${stem} has a non-empty committed op list`);
    // Counts first, so an empty list can never match an empty list by accident.
    assert.equal(actual.ops.length, expected.ops.length, `${stem} records the committed op count`);
    assert.equal(actual.name, expected.name, `${stem} records the committed migration name`);
    // deepEqual, never JSON.stringify: an update `set` map is recorded in author
    // order here and held in a sorted BTreeMap on the Rust side, so key order is not
    // part of the contract even though every key and value is.
    assert.deepEqual(actual.ops, expected.ops, `${stem} records the committed op list`);
    compared += 1;
  }
  // The pin that catches the failure the other pins cannot: a loop that enumerated
  // every stem and asserted on none of them.
  assert.equal(compared, EXPECTED_STEMS.length, "every stem in the corpus was compared");
});
