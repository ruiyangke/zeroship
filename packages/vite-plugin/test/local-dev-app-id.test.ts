import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { DEV_APP_ID, devSqlitePaths } from "../src/gen-types/dev-apply.js";

test("local dev uses the shared canonical AppId", async () => {
  const contract = JSON.parse(
    await readFile(
      new URL("../../../crates/zeroship-id/local-dev-app-id.json", import.meta.url),
      "utf8",
    ),
  ) as { app_id: string };

  assert.equal(DEV_APP_ID, contract.app_id);
  assert.match(DEV_APP_ID, /^app_[0-9a-z]{25}$/);
  assert.deepEqual(devSqlitePaths("/project"), {
    appPath: `/project/.zeroship/zs-${DEV_APP_ID}.sqlite`,
    journalPath: `/project/.zeroship/zs-${DEV_APP_ID}.migrations.sqlite`,
  });
});
