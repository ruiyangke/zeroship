import { test } from "node:test";
import assert from "node:assert/strict";
import {
  renderGeneratedEnvDb,
  type RuntimeDescriptor,
} from "../src/gen-types/render-env-db.js";

/** The database these renders belong to: a `main` labelled primary, which is
 *  what a single-database app's `zeroship.jsonc` declares. The renderer needs
 *  it because the emitted module keys `EnvDatabases` on the label. */
const MAIN = { label: "main", primary: true } as const;

test("generated database types preserve exact decimal facets", () => {
  const descriptor: RuntimeDescriptor = {
    collections: {
      ledger: {
        fields: {
          amount: { type: "number", precision: 30, scale: 2, required: true },
          secret: { type: "number", precision: 20, scale: 4, encrypted: true },
        },
      },
    },
  };
  const output = renderGeneratedEnvDb(descriptor, MAIN);
  assert.match(output, /amount: t\.numeric\(\{ precision: 30, scale: 2 \}\)\.required\(\)/);
  assert.match(output, /secret: t\.numeric\(\{ precision: 20, scale: 4 \}\)\.encrypted\(\)/);
});
