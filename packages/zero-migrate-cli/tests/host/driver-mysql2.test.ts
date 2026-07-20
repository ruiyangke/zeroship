import assert from "node:assert/strict";
import { test } from "node:test";

import {
  cellsToParams,
  MYSQL_SESSION_SQL_MODE_PIN,
} from "../../src/driver-mysql2.js";
import { history } from "../../src/index.js";
import { NO_INJECT_POLICY } from "./policy.js";

test("MySQL exact integers remain numeric parameters", () => {
  const values = cellsToParams([
    { kind: "int", intStr: "9007199254740993" },
    { kind: "int", int: 3 },
  ]);

  assert.deepEqual(values, [9007199254740993n, 3]);
  assert.equal(typeof values[0], "bigint");
});

test("MySQL decimal text never crosses JavaScript's number domain", () => {
  const decimal = "12345678901234567890.1234567890";
  const values = cellsToParams([{ kind: "text", text: decimal }]);

  assert.deepEqual(values, [decimal]);
  assert.equal(typeof values[0], "string");
});

test("MySQL host sessions pin explicit legacy zero preservation", () => {
  assert.match(MYSQL_SESSION_SQL_MODE_PIN, /NO_AUTO_VALUE_ON_ZERO/);
  assert.match(MYSQL_SESSION_SQL_MODE_PIN, /NO_BACKSLASH_ESCAPES/);
});

test("history rejects MySQL locally without opening a connection", async () => {
  const secretUrl = "mysql://private-user:secret-password@127.0.0.1:1/never_connect";

  await assert.rejects(
    history({
      ownerApp: "app_history_test",
      projectSchema: "history_test",
      driver: { kind: "mysql", url: secretUrl },
      policy: [NO_INJECT_POLICY],
    }),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.match(error.message, /history supports only PostgreSQL/i);
      assert.doesNotMatch(error.message, /ECONNREFUSED|private-user|secret-password/);
      return true;
    },
  );
});
