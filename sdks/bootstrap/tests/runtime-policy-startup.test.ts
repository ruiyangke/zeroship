import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { afterEach, test } from "node:test";
import ts from "typescript";
import { defineMaskPolicy } from "@zeroship/db";
import { _flushPendingMaskPolicy } from "@zeroship/db/internal";
import { devEntry } from "../src/dev-entry.js";

const global = globalThis as unknown as Record<PropertyKey, unknown>;
const state = Symbol.for("@zeroship/db/MaskPolicyState");
const source = ts.transpileModule(readFileSync(new URL("../src/runtime-entry.ts", import.meta.url), "utf8"), {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
}).outputText.replace(/^export\s*\{\s*\};?$/m, "").replaceAll("import(", "load(");
const AsyncFunction = Object.getPrototypeOf(async () => {}).constructor;

afterEach(() => {
  for (const key of [state, "__zsRuntimeDescriptor", "__zsDbPlatform", "__zs_env", "__zsSchemaReady", "__zsDeferSchemaInstall"]) delete global[key];
});

function fixture() {
  global.__zsRuntimeDescriptor = { version: 2, collections: {} };
  const installed: unknown[] = [];
  const db = {};
  global.__zs_env = () => ({ db });
  global.__zsDbPlatform = () => ({ setMaskPolicy: async (value: unknown) => { installed.push(value); } });
  const installSchema = () => ({ collections: {} });
  const load = async (name: string) => name === "@zeroship/db/internal"
    ? { _flushPendingMaskPolicy } : { installSchema };
  return { installed, db, installSchema, wrapper: (devMode = false) =>
    new AsyncFunction("load", "__zsAllowDeferredSchemaInstall", source)(load, devMode) };
}

test("the runtime wrapper leaves lazy app policy initialization to devEntry", async () => {
  const f = fixture();
  let loaded = false;
  const entry = devEntry({
    loadUserModule: async () => {
      if (!loaded) { defineMaskPolicy({ support: ["pii"] }); loaded = true; }
      return { default: { rpc: { ping: () => "pong" } } };
    },
    getEnvDb: () => f.db,
    getInstallSchema: async () => f.installSchema as Awaited<ReturnType<NonNullable<Parameters<typeof devEntry>[0]["getInstallSchema"]>>>,
    getDbInternal: async () => ({ _flushPendingMaskPolicy }),
    getDevAuthEnv: () => undefined,
    logger: { log: () => {}, error: () => {} },
  });
  await f.wrapper(true);
  assert.deepEqual(f.installed, []);
  assert.equal(global.__zsDbPlatform, undefined);
  assert.equal(global.__zsDeferSchemaInstall, undefined);
  assert.equal(await entry.rpc("ping", {}, {}), "pong");
  const expected = [Object.assign(Object.create(null), { support: ["pii"] })];
  assert.deepEqual(f.installed, expected);
  assert.throws(() => defineMaskPolicy({ support: [] }), { code: "MASK_POLICY_IMMUTABLE" });
  assert.equal(await entry.rpc("ping", {}, {}), "pong");
  assert.deepEqual(f.installed, expected);
});

test("ordinary runtime startup seals even an undeclared policy", async () => {
  const f = fixture();
  await f.wrapper();
  await global.__zsSchemaReady;
  assert.deepEqual(f.installed, [{}]);
  assert.throws(() => defineMaskPolicy({ support: ["pii"] }), { code: "MASK_POLICY_IMMUTABLE" });
  assert.equal(global.__zsDbPlatform, undefined);
});

test("a creator global cannot defer production policy sealing", async () => {
  const f = fixture();
  global.__zsDeferSchemaInstall = true;
  await f.wrapper();
  await global.__zsSchemaReady;
  assert.deepEqual(f.installed, [{}]);
  assert.throws(() => defineMaskPolicy({ support: ["pii"] }), { code: "MASK_POLICY_IMMUTABLE" });
  assert.equal(global.__zsDeferSchemaInstall, undefined);
  assert.equal(global.__zsDbPlatform, undefined);
});
