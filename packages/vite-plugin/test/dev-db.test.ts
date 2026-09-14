import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { resolveDevDatabase } from "../src/dev-db.js";

test("resolveDevDatabase returns the default sqlite url", () => {
  const root = mkdtempSync(join(tmpdir(), "zs-vite-dev-db-"));
  try {
    const devDb = resolveDevDatabase(root);
    assert.equal(devDb.databaseUrl, "sqlite:.zeroship/dev.sqlite");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
