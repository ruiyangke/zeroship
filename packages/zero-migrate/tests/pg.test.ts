// Rooted Postgres vendor op-shape tests.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  createFunction,
  dropFunction,
  dropOwnedBy,
  extension,
  grant,
  table,
  raw,
  revoke,
  role,
  schema,
  currentSetting,
} from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

test("zero-migrate root exports vendor names and /pg subpath is retired", async () => {
  const resolveImport = (import.meta as ImportMeta & { resolve(specifier: string): string }).resolve;
  const resolved = resolveImport("zero-migrate");
  assert.match(resolved, /\/dist\/index\.js$/);
  assert.throws(() => resolveImport("zero-migrate/pg"), /Package subpath|ERR_PACKAGE_PATH_NOT_EXPORTED/);
  const imported = await import("zero-migrate");
  assert.equal((imported as any).pg, undefined);
  assert.equal(typeof imported.schema, "function");
  assert.equal(typeof imported.raw, "function");
  assert.equal(typeof imported.createFunction, "function");
  assert.equal(typeof imported.domain, "function");
  assert.equal(typeof imported.table, "function");
  assert.equal(typeof imported.sequence, "function");
  assert.equal(imported.dropSchema, undefined);
  assert.equal(imported.dropExtension, undefined);
  assert.equal(imported.alterRole, undefined);
  assert.equal(imported.dropRole, undefined);
  assert.equal(imported.createPolicy, undefined);
  assert.equal(imported.dropPolicy, undefined);
  assert.equal(imported.sql, undefined);
});

test("SA-8: grant/revoke reject empty privilege/role arrays and a non-object target", () => {
  assert.throws(
    () => record(() => grant({ privileges: [], on: { kind: "table", names: ["u"] }, to: ["r"] } as any)),
    (e: any) => e.code === "OP_INVALID" && /privileges must be a non-empty array/.test(e.message),
  );
  assert.throws(
    () => record(() => grant({ privileges: ["select"], on: "u" as any, to: ["r"] } as any)),
    (e: any) => e.code === "OP_INVALID" && /on must be a target object/.test(e.message),
  );
  assert.throws(
    () => record(() => revoke({ privileges: ["select"], on: { kind: "table", names: ["u"] }, from: [] } as any)),
    (e: any) => e.code === "OP_INVALID" && /from must be a non-empty array/.test(e.message),
  );
});

test("table().policy().create requires using and rejects an explicit empty to[]", () => {
  assert.throws(
    () => record(() => table("u").policy("p").create({} as any)),
    (e: any) => e.code === "OP_INVALID" && /using is required/.test(e.message),
  );
  assert.throws(
    () =>
      record(() =>
        table("u").policy("p").create({ to: [], using: (col: any) => col("x").isNotNull() } as any),
      ),
    (e: any) => e.code === "OP_INVALID" && /to must be a non-empty role array/.test(e.message),
  );
});

test("vendor exports and policy selectors record every vendor op shape", () => {
  const ops = record(() => {
    schema("zs").create({ ifNotExists: true, authorization: "owner" });
    schema("zs").drop({ ifExists: true, cascade: true });
    extension("citext").create({ ifNotExists: true, schema: "public" });
    extension("citext").drop({ ifExists: true });
    role("app_role").create({
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
    role("app_role").setOptions({ setSearchPath: ["zs"], resetSearchPath: true });
    role("app_role").drop({ ifExists: true });
    dropOwnedBy({ roles: ["app_role"] });
    grant({
      privileges: ["select", "usage"],
      on: { kind: "schema", names: ["zs"] },
      to: ["app_role"],
      withGrantOption: true,
    });
    revoke({
      privileges: ["update"],
      on: { kind: "table", names: ["users"], schema: "zs" },
      from: ["public"],
    });
    table("users", { schema: "zs" }).policy("tenant_only").create({
      for: "select",
      to: ["app_role"],
      using: (col) => col("app_id").eq("app_demo"),
      withCheck: (col) => col("app_id").isNotNull(),
    });
    table("users", { schema: "zs" }).policy("tenant_only").drop({ ifExists: true });
    createFunction({
      name: "tenant_guard",
      schema: "zs",
      args: [{ name: "tenant_id", type: "text", mode: "in" }],
      returns: "boolean",
      language: "sql",
      replace: true,
      volatility: "stable",
      body: "SELECT true;",
    });
    dropFunction({
      name: "tenant_guard",
      schema: "zs",
      argTypes: ["text"],
      ifExists: true,
    });
    raw({
      sql: "SELECT set_config('a', 'x', false)",
      reason: "set a test GUC in raw SQL",
    });
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
    {
      op: "pgRaw",
      sql: "SELECT set_config('a', 'x', false)",
      reason: "set a test GUC in raw SQL",
    },
  ]);
});

test("table-scoped pg methods record setRls and legacy policy op payloads", () => {
  const ops = record(() => {
    table("secrets", { schema: "zs" })
      .setRls({ enabled: true, forced: true })
      .policy("tenant_only").create({
        using: (col) => col("tenant_id").eq(currentSetting("tenant.id", { missingOk: true })),
      })
      .policy("tenant_only").drop({ ifExists: true })
      .setRls({ enabled: false, forced: false });
  });

  assert.deepEqual(ops, [
    { op: "setRls", table: "secrets", schema: "zs", enabled: true, forced: true },
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
    { op: "setRls", table: "secrets", schema: "zs", enabled: false, forced: false },
  ]);
});

test("setRls omits absent fields and rejects empty patches", () => {
  assert.deepEqual(
    record(() => table("secrets", { schema: "zs" }).setRls({ enabled: true })),
    [{ op: "setRls", table: "secrets", schema: "zs", enabled: true }],
  );
  assert.throws(
    () => record(() => table("secrets", { schema: "zs" }).setRls({})),
    (e: any) => e.code === "OP_INVALID" && /\.setRls needs at least one/.test(e.message),
  );
});

test("raw requires reason and never records binds", () => {
  assert.throws(
    () => record(() => { raw({ sql: "SELECT 1" } as any); }),
    (e: any) => e.code === "OP_INVALID" && /raw\(\{ reason \}\) must be a string/.test(e.message),
  );

  const [op] = record(() => {
    raw({ sql: "SELECT 1", reason: "raw smoke test", binds: ["x"] } as any);
  });
  assert.deepEqual(op, { op: "pgRaw", sql: "SELECT 1", reason: "raw smoke test" });
  assert.equal("binds" in op, false);
});

test("sql is not exposed", async () => {
  const imported = await import("../src/index.js");
  assert.equal((imported as any).sql, undefined);
  assert.equal((imported as any).pg, undefined);
});
