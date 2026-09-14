import { describe, test } from "node:test";
import assert from "node:assert/strict";

import {
  invalidateChangedFiles,
  pollHmrChanges,
} from "../src/dev-bootstrap/hmr.js";

interface FakeModuleNode {
  id: string;
  importers: Set<string>;
}

function createRunner(files: Record<string, FakeModuleNode[]>) {
  const byId = new Map<string, FakeModuleNode>();
  const byFile = new Map<string, Set<FakeModuleNode>>();
  const invalidated: string[] = [];

  for (const [file, mods] of Object.entries(files)) {
    byFile.set(file, new Set(mods));
    for (const mod of mods) {
      byId.set(mod.id, mod);
    }
  }

  return {
    invalidated,
    evaluatedModules: {
      getModuleById(id: string) {
        return byId.get(id);
      },
      getModulesByFile(file: string) {
        return byFile.get(file);
      },
      invalidateModule(mod: FakeModuleNode) {
        invalidated.push(mod.id);
      },
    },
  };
}

describe("dev-bootstrap HMR", () => {
  test("invalidates a changed module and every importer up to the entrypoint", () => {
    const leaf = { id: "/app/src/value.ts", importers: new Set(["/app/src/routes.ts"]) };
    const routes = { id: "/app/src/routes.ts", importers: new Set(["/app/src/server.ts"]) };
    const entry = { id: "/app/src/server.ts", importers: new Set<string>() };

    const runner = createRunner({
      "/app/src/value.ts": [leaf],
      "/app/src/routes.ts": [routes],
      "/app/src/server.ts": [entry],
    });

    const invalidated = invalidateChangedFiles(runner, ["/app/src/value.ts"]);

    assert.equal(invalidated, 3);
    assert.deepEqual(
      new Set(runner.invalidated),
      new Set([
        "/app/src/value.ts",
        "/app/src/routes.ts",
        "/app/src/server.ts",
      ]),
    );
  });

  test("poll reports changes before the serialized loader invalidates modules", async () => {
    const firstRunner = createRunner({
      "/app/src/value.ts": [{ id: "/app/src/value.ts?v=1", importers: new Set<string>() }],
    });
    const secondRunner = createRunner({
      "/app/src/value.ts": [{ id: "/app/src/value.ts?v=2", importers: new Set(["/app/src/server.ts"]) }],
      "/app/src/server.ts": [{ id: "/app/src/server.ts", importers: new Set<string>() }],
    });

    const logs: string[] = [];
    const updates: unknown[] = [];

    const update = await pollHmrChanges({
      pollUrl: "http://vite.test/__zeroship_hmr_check",
      fetchImpl: async () => new Response(JSON.stringify({
        changed: ["/app/src/value.ts"],
        bindingsVersion: "next",
      })),
      log: (message) => logs.push(message),
      onChange: (value) => updates.push(value),
    });

    assert.deepEqual(update, {
      changed: ["/app/src/value.ts"],
      bindingsVersion: "next",
    });
    assert.deepEqual(updates, [update]);
    assert.deepEqual(firstRunner.invalidated, []);
    assert.deepEqual(secondRunner.invalidated, []);

    const invalidated = invalidateChangedFiles(secondRunner, update.changed);
    assert.equal(invalidated, 2);
    assert.deepEqual(
      new Set(secondRunner.invalidated),
      new Set([
        "/app/src/value.ts?v=2",
        "/app/src/server.ts",
      ]),
    );
    assert.deepEqual(logs, ["[zeroship:hmr] 1 module(s) updated"]);
  });

  test("poll filters malformed changes and survives a failed host response", async () => {
    const update = await pollHmrChanges({
      pollUrl: "http://vite.test/__zeroship_hmr_check",
      fetchImpl: async () => new Response(JSON.stringify({
        changed: [null, "/app/src/server.ts", 42],
        bindingsVersion: 42,
      })),
    });
    assert.deepEqual(update, { changed: ["/app/src/server.ts"] });

    const failed = await pollHmrChanges({
      pollUrl: "http://vite.test/__zeroship_hmr_check",
      fetchImpl: async () => new Response("unavailable", { status: 503 }),
    });
    assert.deepEqual(failed, { changed: [] });
  });
});
