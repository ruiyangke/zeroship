import { describe, test } from "node:test";
import assert from "node:assert/strict";

import {
  buildDevEntrySnapshot,
  type DevServerBinding,
} from "../src/dev-bootstrap/entry.js";

function binding(
  sourceFile: string,
  exportName: string,
  wireId = exportName,
): DevServerBinding {
  return { sourceFile, exportName, wireId, kind: "query" };
}

describe("native dev entry snapshots", () => {
  test("returns handlers and explicit string-keyed procedure targets", async () => {
    const declared = Object.create({ inherited: () => "hidden" });
    Object.defineProperty(declared, "__proto__", {
      value: () => "declared",
      enumerable: true,
    });
    const tagged = Object.assign(() => "tagged", {
      config: { id: "todos.tagged", kind: "query" },
    });
    const defaultExport = {
      tag: "receiver",
      fetch() { return this.tag; },
      fetchFast() { return this.tag; },
      rpc: declared,
    };
    const modules = new Map<string, Record<string, unknown>>([
      ["/app/actions.ts", {
        first: Object.assign(() => "first", { config: { kind: "query" } }),
        second: Object.assign(() => "second", { config: { kind: "query" } }),
      }],
    ]);
    const imports: string[] = [];
    const runner = {
      async import(id: string) {
        imports.push(id);
        return modules.get(id);
      },
    };

    const snapshot = await buildDevEntrySnapshot(
      runner,
      { default: defaultExport, tagged, helper: () => "private" },
      [
        binding("/app/actions.ts", "first", "todos first"),
        binding("/app/actions.ts", "second", "constructor"),
      ],
    );

    assert.equal(Object.getPrototypeOf(snapshot.rpc), null);
    assert.equal(snapshot.rpc.__proto__(), "declared");
    assert.equal(snapshot.rpc["todos.tagged"](), "tagged");
    assert.equal(snapshot.rpc["todos first"](), "first");
    assert.equal(snapshot.rpc.constructor(), "second");
    assert.equal(Object.hasOwn(snapshot.rpc, "inherited"), false);
    assert.equal(Object.hasOwn(snapshot.rpc, "helper"), false);
    assert.deepEqual(imports, ["/app/actions.ts"]);
    assert.equal((snapshot.fetch as Function).call(snapshot.userDefault), "receiver");
    assert.equal((snapshot.fetchFast as Function).call(snapshot.userDefault), "receiver");
  });

  test("a replacement snapshot drops removed procedures", async () => {
    const modules = new Map<string, Record<string, unknown>>([
      ["/app/actions.ts", { current: () => "current" }],
    ]);
    const runner = { import: async (id: string) => modules.get(id) };
    const before = await buildDevEntrySnapshot(
      runner,
      { default: {} },
      [binding("/app/actions.ts", "current", "old")],
    );
    const after = await buildDevEntrySnapshot(runner, { default: {} }, []);

    assert.equal(before.rpc.old(), "current");
    assert.deepEqual(Object.keys(after.rpc), []);
  });

  test("rejects dispatch functions and missing binding exports", async () => {
    const runner = { import: async () => ({}) };
    await assert.rejects(
      buildDevEntrySnapshot(runner, { default: { rpc: () => "dispatch" } }, []),
      /default\.rpc must be a procedure dictionary/,
    );
    await assert.rejects(
      buildDevEntrySnapshot(
        runner,
        { default: {} },
        [binding("/app/actions.ts", "missing", "rpc name")],
      ),
      /procedure "rpc name".*must be a function/,
    );
  });
});
