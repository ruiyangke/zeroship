import { test } from "node:test";
import assert from "node:assert/strict";

test("the published package exposes creator APIs without a framework entry", async () => {
  const sdk = await import("@zeroship/db");
  assert.equal(typeof sdk.subscribe, "function");

  const frameworkSubpath = ["@zeroship/db", "internal"].join("/");
  await assert.rejects(import(frameworkSubpath), (error: unknown) => {
    return (
      error instanceof Error &&
      "code" in error &&
      error.code === "ERR_PACKAGE_PATH_NOT_EXPORTED"
    );
  });
});
