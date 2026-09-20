/**
 * `env.databases` as a COMPILE contract, not as rendered text.
 *
 * The whole reason `EnvDatabases` is a separate interface is that two
 * generated modules have to coexist in one program. A `databases` property
 * declared inline on `Env` by each of them would be a declaration-merging
 * CONFLICT, not a union, and the only way to see that is to put both modules
 * in front of tsc. So these tests render two databases, compile them together
 * with a consumer, and read the diagnostics.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { resolve } from "node:path";
import ts from "typescript";

import {
  renderGeneratedEnvDb,
  type RuntimeDescriptor,
} from "../src/gen-types/render-env-db.js";

/** The app's primary: `env.db` and `env.databases.main`. */
const MAIN_SCHEMA: RuntimeDescriptor = {
  version: 2,
  collections: {
    users: {
      fields: {
        id: { type: "string", required: true, primaryKey: true },
        email: { type: "string", required: true },
      },
    },
  },
};

/** A second database the app also uses, with a DIFFERENT collection set, so a
 *  handle typed from the wrong one is a type error rather than a coincidence. */
const ANALYTICS_SCHEMA: RuntimeDescriptor = {
  version: 2,
  collections: {
    events: {
      fields: {
        id: { type: "string", required: true, primaryKey: true },
        weight: { type: "int", required: true },
      },
    },
  },
};

const MAIN = { label: "main", primary: true } as const;
const ANALYTICS = { label: "analytics", primary: false } as const;

/**
 * Compile the two generated modules plus `consumer` and return every
 * diagnostic message, in order.
 *
 * The generated modules are real `.ts` files in this directory's namespace so
 * their `declare module "zeroship"` blocks are AUGMENTATIONS of the same
 * module the consumer imports - which is the thing under test.
 */
function diagnosticsFor(
  consumer: string,
  main: { label: string; primary: boolean } = MAIN,
  analytics: { label: string; primary: boolean } = ANALYTICS,
): string[] {
  const dir = import.meta.dirname;
  const sources = new Map<string, string>([
    [resolve(dir, "__env-db-main.ts"), renderGeneratedEnvDb(MAIN_SCHEMA, main)],
    [resolve(dir, "__env-db-analytics.ts"), renderGeneratedEnvDb(ANALYTICS_SCHEMA, analytics)],
    [
      resolve(dir, "__env-databases-consumer.ts"),
      `import "./__env-db-main.js";\nimport "./__env-db-analytics.js";\n${consumer}`,
    ],
  ]);
  const options: ts.CompilerOptions = {
    strict: true,
    noEmit: true,
    skipLibCheck: true,
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    types: [],
  };
  const host = ts.createCompilerHost(options);
  const getSourceFile = host.getSourceFile.bind(host);
  host.getSourceFile = (name, version, ...args) => {
    const text = sources.get(name);
    return text === undefined
      ? getSourceFile(name, version, ...args)
      : ts.createSourceFile(name, text, version, true);
  };
  const program = ts.createProgram(
    [...sources.keys(), resolve(dir, "../../types/globals.d.ts")],
    options,
    host,
  );
  return ts
    .getPreEmitDiagnostics(program)
    .map((d) => ts.flattenDiagnosticMessageText(d.messageText, "\n"));
}

test("two databases augment one env without a declaration-merging conflict", () => {
  // Reaching a collection through BOTH handles is what proves the two modules
  // merged rather than collided: a conflict makes `databases` an error type and
  // every access under it fails, and an overwrite makes one of the two wrong.
  assert.deepEqual(
    diagnosticsFor(`
import { env } from "zeroship";
async function use() {
  const users = await env.databases.main.users.find();
  const events = await env.databases.analytics.events.find();
  void [users, events];
}
void use;
`),
    [],
  );
});

test("env.db is the primary's handle and the secondary is reachable only by label", () => {
  // `env.db === env.databases[primary]` at runtime, by object identity. The
  // generated module states that as `Env.db = EnvDatabases["main"]`, so the
  // primary's collections are on `env.db` and the secondary's are not.
  assert.deepEqual(
    diagnosticsFor(`
import { env } from "zeroship";
async function use() {
  const viaDb = await env.db.users.find();
  const viaLabel = await env.databases.main.users.find();
  void [viaDb, viaLabel];
}
void use;
`),
    [],
  );
  const wrongHandle = diagnosticsFor(`
import { env } from "zeroship";
async function use() {
  await env.db.events.find();
}
void use;
`);
  assert.equal(wrongHandle.length, 1, `expected exactly one diagnostic, got ${wrongHandle}`);
  assert.match(
    wrongHandle[0]!,
    /'events' does not exist/,
    "the secondary's collections must not be reachable through env.db",
  );
});

test("a label no database declares is a type error rather than unknown", () => {
  // `EnvDatabases` carries no index signature ON PURPOSE: the runtime
  // publishes an entry per declared database and nothing else, so a label the
  // app never declared is absent there too. Typed with an index signature this
  // would compile and fail at runtime.
  const missing = diagnosticsFor(`
import { env } from "zeroship";
async function use() {
  await env.databases.reporting.events.find();
}
void use;
`);
  assert.equal(missing.length, 1, `expected exactly one diagnostic, got ${missing}`);
  assert.match(missing[0]!, /'reporting' does not exist/);
});

test("the primary flag is what moves Env.db, and only one database may carry it", () => {
  // Flip which database is primary: `env.db` follows it. This is the flag
  // doing the work, observed through the compiler rather than through the
  // emitted text.
  const analyticsPrimary = { label: "analytics", primary: true };
  const mainSecondary = { label: "main", primary: false };
  assert.deepEqual(
    diagnosticsFor(
      `
import { env } from "zeroship";
async function use() {
  const events = await env.db.events.find();
  const users = await env.databases.main.users.find();
  void [events, users];
}
void use;
`,
      mainSecondary,
      analyticsPrimary,
    ),
    [],
  );

  // Both primary is the state the config validator refuses
  // (`checkPrimacyIsUniform`), and this is what it costs when it is not
  // refused: two modules declare `Env.db` with different handles, which
  // TypeScript rejects outright rather than merging.
  const bothPrimary = diagnosticsFor("", { label: "main", primary: true }, {
    label: "analytics",
    primary: true,
  });
  assert.ok(
    bothPrimary.some((message) => /Subsequent property declarations/.test(message)),
    `two primaries must collide on Env.db, got ${JSON.stringify(bothPrimary)}`,
  );
});
