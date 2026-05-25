import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { resolveDevDatabase } from "../src/dev-db.js";

test("resolveDevDatabase creates .zeroship and returns sqlite url", () => {
  const root = mkdtempSync(join(tmpdir(), "zs-vite-dev-db-"));
  try {
    const devDb = resolveDevDatabase(root);
    assert.equal(devDb.databaseUrl, "sqlite:.zeroship/dev.sqlite");
    assert.equal(existsSync(join(root, ".zeroship")), true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
