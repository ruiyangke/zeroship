import assert from "node:assert/strict";
import { test } from "node:test";

import {
  cellsToParams,
  MYSQL_SESSION_SQL_MODE_PIN,
  openMysqlSession,
} from "../../src/driver-mysql2.js";
import type { JsReply, JsRequest } from "../../src/addon.js";
import { history } from "../../src/index.js";
import { noInjectPolicy } from "./policy.js";

// Gate the live-server test on the same env var the neighbouring MySQL host tests
// use, so a contributor without MySQL skips instead of failing.
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

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

// The DB-free half of the pin contract: the statement TEXT names both modes. This
// asserts nothing about any session -- see the live read-back test below for that.
test("MYSQL_SESSION_SQL_MODE_PIN statement text names both legacy modes", () => {
  assert.match(MYSQL_SESSION_SQL_MODE_PIN, /NO_AUTO_VALUE_ON_ZERO/);
  assert.match(MYSQL_SESSION_SQL_MODE_PIN, /NO_BACKSLASH_ESCAPES/);
});

/** Run one verb over the callback-shaped `hostDriver` seam as a promise. */
function driveVerb(
  hostDriver: (
    args: [request: JsRequest, done: (err: unknown, reply: JsReply | null) => void],
  ) => void,
  request: JsRequest,
): Promise<JsReply> {
  return new Promise((resolve, reject) => {
    hostDriver([
      request,
      (err, reply) => {
        if (err || !reply) reject(err ?? new Error("host driver returned no reply"));
        else resolve(reply);
      },
    ]);
  });
}

// The half that needs a server: read `@@SESSION.sql_mode` back out of a session the
// driver itself opened, over the same `hostDriver` seam the engine drives, and assert
// the SERVER reports both modes. This fails if the pin statement is never issued,
// which asserting on the constant alone cannot detect.
test("Live MySQL host session reports both pinned sql_mode modes back from the server", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset -- live MySQL session sql_mode read-back skipped");
    return;
  }

  const session = await openMysqlSession(MYSQL_URL);
  try {
    const reply = await driveVerb(session.hostDriver, {
      kind: "queryOne",
      sql: "SELECT @@SESSION.sql_mode",
      binds: [],
      textParams: [],
    });

    // The reply carries one row of one cell; the server's sql_mode crosses as text.
    assert.equal(reply.rows.length, 1, "the session answered with exactly one row");
    const reported = reply.rows[0].cells[0].text ?? "";

    assert.ok(
      reported.split(",").includes("NO_AUTO_VALUE_ON_ZERO"),
      `session sql_mode is missing NO_AUTO_VALUE_ON_ZERO; server reported ${JSON.stringify(reported)}`,
    );
    assert.ok(
      reported.split(",").includes("NO_BACKSLASH_ESCAPES"),
      `session sql_mode is missing NO_BACKSLASH_ESCAPES; server reported ${JSON.stringify(reported)}`,
    );
  } finally {
    await session.close();
  }
});

test("history rejects MySQL locally without opening a connection", async () => {
  const secretUrl = "mysql://private-user:secret-password@127.0.0.1:1/never_connect";

  await assert.rejects(
    history({
      ownerApp: "app_history_test",
      projectSchema: "history_test",
      driver: { kind: "mysql", url: secretUrl },
      policy: [noInjectPolicy("history_test")],
    }),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.match(error.message, /history supports only PostgreSQL/i);
      assert.doesNotMatch(error.message, /ECONNREFUSED|private-user|secret-password/);
      return true;
    },
  );
});
