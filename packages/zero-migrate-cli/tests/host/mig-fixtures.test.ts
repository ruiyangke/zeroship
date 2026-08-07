// Every shared migration fixture in `tests/host/mig/` must be authorable for
// EVERY dialect this engine ships, not just the ones a suite happens to apply it
// against today.
//
// The fixtures are shared artifacts: `20260711000001_create_widgets.ts` is applied
// by both `authoring.test.ts` (PostgreSQL) and `mysql-authoring.test.ts` (MySQL),
// while `20260712000001_create_gadgets.ts` is applied by PostgreSQL suites only.
// That asymmetry is the hazard this file closes. A fixture can carry a shape one
// dialect refuses and stay green indefinitely, because nothing exercises the
// dialect that would refuse it; the defect surfaces only when someone later wires
// up a consumer for that dialect, far from the change that introduced it.
//
// So this suite runs the DB-FREE `validate` gate (the addon's sync `loadVerify`:
// structural + confinement + ownership + dialect validation, no connection) over
// the cross product of every fixture and every dialect. It is the same gate `lint`
// and `apply` run, so a verdict here is the verdict the real path would reach.
// No database, no Docker, no gating: it runs everywhere and fails immediately.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

import { validate } from "zero-migrate-cli";

const HERE = dirname(fileURLToPath(import.meta.url));

// Point the addon loader at the sibling crate's prebuilt `.node` unless the caller
// already set an explicit path (matches `authoring.test.ts`).
if (!process.env.ZERO_MIGRATE_ADDON_PATH) {
  const { platform, arch } = process;
  const abi = platform === "linux" ? "-gnu" : "";
  process.env.ZERO_MIGRATE_ADDON_PATH = join(
    HERE,
    `../../../../crates/zero-migrate-node/zero-migrate-node.${platform}-${arch}${abi}.node`,
  );
}

/** Every dialect the engine lowers for. A fixture must be valid for all of them. */
const DIALECTS = ["postgres", "mysql", "sqlite"] as const;

const MIG_DIR = join(HERE, "mig");

/** The fixture files, sorted so a failure report is stable across runs. */
const FIXTURES = readdirSync(MIG_DIR)
  .filter((file) => file.endsWith(".ts"))
  .sort();

/** `20260711000001_create_widgets.ts` -> `create_widgets`, the same name the
 *  applying suites pass as `nameFallback`. */
function fallbackName(file: string): string {
  return file.replace(/\.ts$/, "").replace(/^\d+_/, "");
}

test("every host migration fixture validates for every dialect", async () => {
  assert.ok(FIXTURES.length > 0, `no fixtures found in ${MIG_DIR}`);

  const failures: string[] = [];
  for (const file of FIXTURES) {
    const migration = await import(pathToFileURL(join(MIG_DIR, file)).href);
    for (const dialect of DIALECTS) {
      const verdict = validate({
        migration: migration as never,
        ownerApp: "app_fixture_dialects",
        dialect,
        nameFallback: fallbackName(file),
      });
      if (!verdict.ok) {
        failures.push(`${file} [${dialect}]: ${verdict.error ?? "(no error message)"}`);
      }
    }
  }

  assert.deepEqual(
    failures,
    [],
    `host migration fixtures must validate for all of ${DIALECTS.join(", ")}:\n  ${failures.join("\n  ")}`,
  );
});
