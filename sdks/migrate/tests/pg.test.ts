// `@zeroship/migrate/pg` package-boundary + op-shape tests.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table } from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";
import { pg } from "../src/pg.js";

function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

test("@zeroship/migrate/pg subpath resolves through package exports", async () => {
  const resolveImport = (import.meta as ImportMeta & { resolve(specifier: string): string }).resolve;
  const resolved = resolveImport("@zeroship/migrate/pg");
  assert.match(resolved, /\/dist\/pg\.js$/);
  const imported = await import("@zeroship/migrate/pg");
  assert.equal(typeof imported.pg.sql, "function");
});

test("SA-8: pg.grant/revoke reject empty privilege/role arrays and a non-object target", () => {
  assert.throws(
    () => record(() => pg.grant({ privileges: [], on: { kind: "table", names: ["u"] }, to: ["r"] } as any)),
    (e: any) => e.code === "OP_INVALID" && /privileges must be a non-empty array/.test(e.message),
  );
  assert.throws(
    () => record(() => pg.grant({ privileges: ["select"], on: "u" as any, to: ["r"] } as any)),
    (e: any) => e.code === "OP_INVALID" && /on must be a target object/.test(e.message),
  );
  assert.throws(
    () => record(() => pg.revoke({ privileges: ["select"], on: { kind: "table", names: ["u"] }, from: [] } as any)),
    (e: any) => e.code === "OP_INVALID" && /from must be a non-empty array/.test(e.message),
  );
});

test("SA-10/SA-11: pg.createPolicy requires using and rejects an explicit empty to[]", () => {
  assert.throws(
    () => record(() => pg.createPolicy({ name: "p", table: "u" } as any)),
    (e: any) => e.code === "OP_INVALID" && /using is required/.test(e.message),
  );
  assert.throws(
    () =>
      record(() =>
        pg.createPolicy({ name: "p", table: "u", to: [], using: (c: any) => c("x").isNotNull() } as any),
      ),
    (e: any) => e.code === "OP_INVALID" && /to must be a non-empty role array/.test(e.message),
  );
});

test("pg namespace records every standalone vendor op shape", () => {
  const ops = record(() => {
    pg.createSchema({ name: "zs", ifNotExists: true, authorization: "owner" });
    pg.dropSchema({ name: "zs", ifExists: true, cascade: true });
    pg.createExtension({ name: "citext", ifNotExists: true, schema: "public" });
    pg.dropExtension({ name: "citext", ifExists: true });
    pg.createRole({
      name: "app_role",
      login: true,
      password: "secret",
      bypassRls: true,
      createRole: true,
      createDb: true,
      superuser: false,
      inRole: ["parent_role"],
      setSearchPath: ["zs", "public"],
      ifNotExists: true,
    });
    pg.alterRole({ name: "app_role", setSearchPath: ["zs"], resetSearchPath: true });
    pg.dropRole({ name: "app_role", ifExists: true });
    pg.dropOwnedBy({ roles: ["app_role"] });
    pg.grant({
      privileges: ["select", "usage"],
      on: { kind: "schema", names: ["zs"] },
      to: ["app_role"],
      withGrantOption: true,
    });
    pg.revoke({
      privileges: ["update"],
      on: { kind: "table", names: ["users"], schema: "zs" },
      from: ["public"],
    });
    pg.createPolicy({
      name: "tenant_only",
      table: "users",
      schema: "zs",
      for: "select",
      to: ["app_role"],
      using: (c) => c("app_id").eq("app_demo"),
      withCheck: (c) => c("app_id").isNotNull(),
    });
    pg.dropPolicy({ name: "tenant_only", table: "users", schema: "zs", ifExists: true });
    pg.createFunction({
      name: "tenant_guard",
      schema: "zs",
      args: [{ name: "tenant_id", type: "text", mode: "in" }],
      returns: "boolean",
      language: "sql",
      replace: true,
      volatility: "stable",
      body: "SELECT true;",
    });
    pg.dropFunction({
      name: "tenant_guard",
      schema: "zs",
      argTypes: ["text"],
      ifExists: true,
    });
    pg.raw({ sql: "SELECT set_config('a', $1, false)", binds: ["x"] });
    pg.sql`SELECT ${"x"}, ${1}, ${true}`;
  });

  assert.deepEqual(ops, [
    { op: "createSchema", name: "zs", ifNotExists: true, authorization: "owner" },
    { op: "dropSchema", name: "zs", ifExists: true, cascade: true },
    { op: "createExtension", name: "citext", ifNotExists: true, schema: "public" },
    { op: "dropExtension", name: "citext", ifExists: true },
    {
      op: "createRole",
      name: "app_role",
      login: true,
      password: "secret",
      bypassRls: true,
      createRole: true,
      createDb: true,
      superuser: false,
      inRole: ["parent_role"],
      setSearchPath: ["zs", "public"],
      ifNotExists: true,
    },
    { op: "alterRole", name: "app_role", setSearchPath: ["zs"], resetSearchPath: true },
    { op: "dropRole", name: "app_role", ifExists: true },
    { op: "dropOwnedBy", roles: ["app_role"] },
    {
      op: "grant",
      privileges: ["select", "usage"],
      on: { kind: "schema", names: ["zs"] },
      to: ["app_role"],
      withGrantOption: true,
    },
    {
      op: "revoke",
      privileges: ["update"],
      on: { kind: "table", names: ["users"], schema: "zs" },
      from: ["public"],
    },
    {
      op: "createPolicy",
      name: "tenant_only",
      table: "users",
      schema: "zs",
      forCmd: "select",
      to: ["app_role"],
      using: {
        node: "binOp",
        op: "eq",
        lhs: { node: "colRef", name: "app_id" },
        rhs: { node: "literal", value: "app_demo" },
      },
      withCheck: {
        node: "unaryOp",
        op: "isNotNull",
        operand: { node: "colRef", name: "app_id" },
      },
    },
    { op: "dropPolicy", name: "tenant_only", table: "users", schema: "zs", ifExists: true },
    {
      op: "createFunction",
      name: "tenant_guard",
      schema: "zs",
      args: [{ name: "tenant_id", type: "text", mode: "in" }],
      returns: "boolean",
      language: "sql",
      replace: true,
      volatility: "stable",
      body: "SELECT true;",
    },
    {
      op: "dropFunction",
      name: "tenant_guard",
      schema: "zs",
      argTypes: ["text"],
      ifExists: true,
    },
    { op: "pgRaw", sql: "SELECT set_config('a', $1, false)", binds: ["x"] },
    { op: "pgRaw", sql: "SELECT $1, $2, $3", binds: ["x", 1, true] },
  ]);
});

test("table-scoped pg methods record RLS and policy op shapes", () => {
  const ops = record(() => {
    table("secrets", { schema: "zs" })
      .enableRowLevelSecurity()
      .forceRowLevelSecurity()
      .createPolicy({
        name: "tenant_only",
        using: (c) => c("tenant_id").eq(c.fn.currentSetting("tenant.id", true)),
      })
      .dropPolicy({ name: "tenant_only", ifExists: true })
      .disableRowLevelSecurity()
      .noForceRowLevelSecurity();
  });

  assert.deepEqual(ops, [
    { op: "enableRls", table: "secrets", schema: "zs" },
    { op: "forceRls", table: "secrets", schema: "zs" },
    {
      op: "createPolicy",
      name: "tenant_only",
      table: "secrets",
      schema: "zs",
      forCmd: "all",
      using: {
        node: "binOp",
        op: "eq",
        lhs: { node: "colRef", name: "tenant_id" },
        rhs: {
          node: "fnCall",
          fn: "currentSetting",
          args: [
            { node: "literal", value: "tenant.id" },
            { node: "literal", value: true },
          ],
        },
      },
    },
    { op: "dropPolicy", name: "tenant_only", table: "secrets", schema: "zs", ifExists: true },
    { op: "disableRls", table: "secrets", schema: "zs" },
    { op: "noForceRls", table: "secrets", schema: "zs" },
  ]);
});

test("pg.sql rejects non-integer number binds", () => {
  assert.throws(
    () => record(() => { pg.sql`SELECT ${1.25}`; }),
    (e: any) => e.code === "OP_INVALID" && /must be an integer scalar/.test(e.message),
  );
});
