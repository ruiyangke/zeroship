import { test } from "node:test";
import assert from "node:assert/strict";
import {
  renderGeneratedEnvDb,
  type RuntimeDescriptor,
} from "../src/gen-types/render-env-db.js";

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
  const output = renderGeneratedEnvDb(descriptor);
  assert.match(output, /amount: t\.numeric\(\{ precision: 30, scale: 2 \}\)\.required\(\)/);
  assert.match(output, /secret: t\.encrypted\(\{ of: t\.numeric\(\{ precision: 20, scale: 4 \}\) \}\)/);
});
