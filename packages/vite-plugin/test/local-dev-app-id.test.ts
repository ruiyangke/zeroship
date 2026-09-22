import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { DEV_APP_ID } from "../src/gen-types/dev-apply.js";

test("local dev uses the shared canonical AppId", async () => {
  const contract = JSON.parse(
    await readFile(
      new URL("../../../crates/zeroship-id/local-dev-app-id.json", import.meta.url),
      "utf8",
    ),
  ) as { app_id: string };

  assert.equal(DEV_APP_ID, contract.app_id);
  assert.match(DEV_APP_ID, /^app_[0-9a-z]{25}$/);
});
