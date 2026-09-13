import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { parse } from "acorn";
import {
  rpcRegistryPlugin,
  buildServerEntrySource,
  pickEntryWireId,
  SERVER_ENTRY_VIRTUAL_ID,
  SERVER_ENTRY_RESOLVED_ID,
} from "../src/rpc-registry.js";
import { bindingMap, buildEntryFixture } from "./helpers/server-entry-fixture.js";

describe("rpcRegistryPlugin", () => {
  const plugin = rpcRegistryPlugin({ userEntryRel: "./user.mjs" });
  const resolveId = plugin.resolveId as (id: string) => unknown;
  const load = plugin.load as (id: string) => unknown;

  test("resolves and loads only its synthetic entry", async (t) => {
    assert.equal(resolveId(SERVER_ENTRY_VIRTUAL_ID), SERVER_ENTRY_RESOLVED_ID);
    assert.equal(resolveId("react"), null);
    assert.equal(resolveId("./user.mjs"), null);
    assert.equal(load("./user.mjs"), null);
    const source = load(SERVER_ENTRY_RESOLVED_ID);
    assert.equal(typeof source, "string");
    const fixture = await buildEntryFixture(t, {
      source: source as string,
      files: { "user.mjs": "export function ping() { return 'pong'; }" },
    });
    assert.equal((await fixture.load()).default.rpc.ping(), "pong");
  });

  test("reads the current binding map when the entry is loaded", async (t) => {
    let bindings = bindingMap([{ sourceFile: "./actions.mjs", exportName: "before" }]);
    const plugin = rpcRegistryPlugin({ userEntryRel: "./user.mjs", getBindings: () => bindings });
    const load = plugin.load as (id: string) => string;
    bindings = bindingMap([{ sourceFile: "./actions.mjs", exportName: "after" }]);
    const fixture = await buildEntryFixture(t, {
      source: load(SERVER_ENTRY_RESOLVED_ID),
      files: {
        "user.mjs": "export default {};",
        "actions.mjs": "export const before = () => 'old'; export const after = () => 'new';",
      },
    });
    const { rpc } = (await fixture.load()).default;
    assert.deepEqual(Object.keys(rpc), ["after"]);
    assert.equal(rpc.after(), "new");
  });
});

const variants = [
  { name: "namespace exports", bindings: undefined },
  { name: "empty bindings", bindings: new Map() },
  {
    name: "explicit bindings",
    bindings: bindingMap([{ sourceFile: "./bound.mjs", exportName: "bound" }]),
  },
];
const boundFile = { "bound.mjs": "export const bound = () => 'bound';" };

for (const { name, bindings } of variants) {
  describe(name, () => {
    test("preserves own string RPC names and procedure identity", async (t) => {
      const fixture = await buildEntryFixture(t, {
        bindings,
        files: {
          ...boundFile,
          "user.mjs": `
            const rpc = Object.create({ inherited: () => 'inherited' });
            const procedure = Object.assign(() => 'actual', { config: Object.freeze({ kind: 'query' }) });
            procedure.identity = procedure;
            Object.freeze(procedure);
            for (const name of ['__proto__', 'constructor', 'todos.list', 'with spaces', 'quoted"key']) {
              Object.defineProperty(rpc, name, { value: procedure });
            }
            rpc[Symbol('private')] = () => 'symbol';
            export default { rpc };
          `,
        },
      });
      const { rpc } = (await fixture.load()).default;
      assert.equal(Object.getPrototypeOf(rpc), null);
      assert.equal(Object.hasOwn(rpc, "inherited"), false);
      assert.deepEqual(Object.getOwnPropertySymbols(rpc), []);
      const procedure = rpc.__proto__;
      assert.equal(procedure, procedure.identity);
      assert.equal(procedure.config.kind, "query");
      assert.ok(Object.isFrozen(procedure));
      for (const key of ["__proto__", "constructor", "todos.list", "with spaces", 'quoted"key']) {
        assert.ok(Object.hasOwn(rpc, key), key);
        assert.equal(rpc[key], procedure);
      }
    });

    test("keeps the default fetch receiver without dispatching RPCs", async (t) => {
      const fixture = await buildEntryFixture(t, {
        bindings,
        files: {
          ...boundFile,
          "user.mjs": `
            let getterReads = 0;
            let calls = 0;
            export const fetch = () => new Response('top level');
            const def = {
              label: 'original receiver',
              rpc: { ping: () => { calls++; return 'RPC'; }, inspect: () => ({ calls, getterReads }) },
              get fetch() {
                getterReads++;
                return function (request, env, ctx) {
                  return new Response(JSON.stringify([this.label, request.url, env, ctx]));
                };
              },
              get schema() { throw Error('schema is supplied by the host'); },
            };
            export default def;
          `,
        },
      });
      const entry = (await fixture.load()).default;
      assert.deepEqual(Object.keys(entry).sort(), ["fetch", "rpc", "workflows"]);
      const response = await entry.fetch.call(
        { label: "wrong receiver" },
        new Request("http://app/__zeroship/v1/ping"),
        "environment",
        "context",
      );
      assert.deepEqual(await response.json(), [
        "original receiver", "http://app/__zeroship/v1/ping", "environment", "context",
      ]);
      assert.deepEqual(entry.rpc.inspect(), { calls: 0, getterReads: 1 });
      assert.equal(entry.rpc.ping(), "RPC");
    });

    test("uses top-level fetch when default fetch is absent", async (t) => {
      const fixture = await buildEntryFixture(t, {
        bindings,
        files: {
          ...boundFile,
          "user.mjs": "export function fetch() { return new Response('top level'); }",
        },
      });
      const entry = (await fixture.load()).default;
      assert.equal(await (await entry.fetch(new Request("http://app/"))).text(), "top level");
      assert.equal(Object.hasOwn(entry.rpc, "fetch"), false);
    });

    test("leaves absent fetch for the native runtime to handle", async (t) => {
      const fixture = await buildEntryFixture(t, {
        bindings,
        files: { ...boundFile, "user.mjs": "export default { rpc: { ping: () => 'pong' } };" },
      });
      const entry = (await fixture.load()).default;
      assert.equal(entry.fetch, undefined);
      assert.equal(entry.rpc.ping(), "pong");
    });

    for (const invalid of ["() => 'dispatcher'", "[]", "'procedure'", "false"]) {
      test("rejects invalid default.rpc: " + invalid, async (t) => {
        const fixture = await buildEntryFixture(t, {
          bindings,
          files: { ...boundFile, "user.mjs": "export default { rpc: " + invalid + " };" },
        });
        await assert.rejects(fixture.load(), /default.rpc must be a procedure dictionary/);
      });
    }

    for (const absent of ["null", "undefined"]) {
      test("accepts absent default.rpc: " + absent, async (t) => {
        const fixture = await buildEntryFixture(t, {
          bindings,
          files: { ...boundFile, "user.mjs": "export default { rpc: " + absent + " };" },
        });
        const entry = (await fixture.load()).default;
        assert.deepEqual(Object.keys(entry.rpc), bindings?.size ? ["bound"] : []);
      });
    }

    test("preserves the workflow owner's existing collection surface", async (t) => {
      const fixture = await buildEntryFixture(t, {
        bindings,
        files: {
          ...boundFile,
          "user.mjs": `
            export class Task { run() { throw Error('must not execute'); } }
            class Declared { run() { throw Error('must not execute'); } }
            export default { workflows: { declared: Declared } };
          `,
        },
      });
      const entry = (await fixture.load()).default;
      assert.deepEqual(Object.keys(entry.workflows).sort(), ["Task", "declared"]);
      assert.equal(entry.workflows.Task.name, "Task");
      assert.equal(typeof entry.workflows.declared, "function");
      assert.equal(Object.hasOwn(entry.rpc, "Task"), false);
    });
  });
}

test("named procedures retain identity and override declared RPC names", async (t) => {
  const fixture = await buildEntryFixture(t, {
    files: {
      "user.mjs": `
        export const ping = Object.assign(() => 'named', { config: { kind: 'query' } });
        export const proto = Object.assign(() => 'proto', { config: { id: '__proto__' } });
        export const aliased = Object.assign(() => 'alias', { config: { id: 'todos.list' } });
        export const empty = Object.assign(() => 'empty', { config: { id: '' } });
        export const notString = Object.assign(() => 'number', { config: { id: 42 } });
        export const constant = 'private';
        export default { rpc: { ping: () => 'declared', identity: () => ping } };
      `,
    },
  });
  const { rpc } = (await fixture.load()).default;
  assert.equal(rpc.ping, rpc.identity());
  assert.equal(rpc.ping(), "named");
  assert.equal(rpc.__proto__(), "proto");
  assert.equal(rpc["todos.list"](), "alias");
  assert.equal(rpc.empty(), "empty");
  assert.equal(rpc.notString(), "number");
  assert.equal(Object.hasOwn(rpc, "constant"), false);
});

function assertEntryImports(code: string, allowed: Set<string>): void {
  const artifact = parse(code, { ecmaVersion: "latest", sourceType: "module" });
  const imports = artifact.body.filter(node => node.type === "ImportDeclaration");
  assert.ok(imports.length > 0, "entry must import application targets");
  for (const node of imports) {
    assert.ok(allowed.has(node.source.value as string), "unexpected entry import: " + node.source.value);
  }
}

test("generated imports contain only normalization and application targets", () => {
  for (const { bindings } of variants) {
    const code = buildServerEntrySource({ userEntryRel: "./user.mjs", bindings });
    const allowed = new Set(["./user.mjs", "./bound.mjs", "@zeroship/bootstrap/normalize"]);
    assertEntryImports(code, allowed);
    for (const forbidden of ["@zeroship/db/internal", "@zeroship/bootstrap/fetch-handler"]) {
      assert.throws(
        () => assertEntryImports(code + "\nimport " + JSON.stringify(forbidden) + ";", allowed),
        /unexpected entry import/,
      );
    }
  }
  assert.throws(() => assertEntryImports("export default {};", new Set()), /application targets/);
});

describe("pickEntryWireId", () => {
  test("preserves explicit string names and otherwise uses the export name", () => {
    for (const id of ["todos.add", "__proto__", "with spaces"]) {
      assert.equal(pickEntryWireId({ exportName: "add", config: { id } }), id);
    }
    for (const id of ["", undefined, null, 42, false]) {
      assert.equal(pickEntryWireId({ exportName: "add", config: { id } }), "add");
    }
    assert.equal(pickEntryWireId({ exportName: "add" }), "add");
  });
});
