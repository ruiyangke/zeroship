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

  test("poll invalidates the current runner after a deps reoptimize swap", async () => {
    const firstRunner = createRunner({
      "/app/src/value.ts": [{ id: "/app/src/value.ts?v=1", importers: new Set<string>() }],
    });
    const secondRunner = createRunner({
      "/app/src/value.ts": [{ id: "/app/src/value.ts?v=2", importers: new Set(["/app/src/server.ts"]) }],
      "/app/src/server.ts": [{ id: "/app/src/server.ts", importers: new Set<string>() }],
    });

    let currentRunner: typeof firstRunner | typeof secondRunner | null = firstRunner;
    const logs: string[] = [];

    const invalidated = await pollHmrChanges({
      pollUrl: "http://vite.test/__zeroship_hmr_check",
      getCurrentRunner: () => currentRunner,
      fetchImpl: async () => ({
        async json() {
          currentRunner = secondRunner;
          return { changed: ["/app/src/value.ts"] };
        },
      } as Response),
      log: (message) => logs.push(message),
    });

    assert.equal(invalidated, 2);
    assert.deepEqual(firstRunner.invalidated, []);
    assert.deepEqual(
      new Set(secondRunner.invalidated),
      new Set([
        "/app/src/value.ts?v=2",
        "/app/src/server.ts",
      ]),
    );
    assert.deepEqual(logs, ["[zeroship:hmr] 1 module(s) updated"]);
  });

  test("poll prunes changed-module registrations before invalidation", async () => {
    const runner = createRunner({
      "/app/src/server.ts": [{ id: "/app/src/server.ts", importers: new Set<string>() }],
    });
    const pruned: string[][] = [];

    const invalidated = await pollHmrChanges({
      pollUrl: "http://vite.test/__zeroship_hmr_check",
      getCurrentRunner: () => runner,
      fetchImpl: async () => ({
        async json() {
          return { changed: ["/app/src/server.ts"] };
        },
      } as Response),
      onBeforeInvalidate: (changed) => pruned.push(changed),
    });

    assert.equal(invalidated, 1);
    assert.deepEqual(pruned, [["/app/src/server.ts"]]);
    assert.deepEqual(runner.invalidated, ["/app/src/server.ts"]);
  });

  test("poll applies runtime descriptor updates before returning", async () => {
    const updates: Array<string | null> = [];

    const invalidated = await pollHmrChanges({
      pollUrl: "http://vite.test/__zeroship_hmr_check",
      getCurrentRunner: () => null,
      fetchImpl: async () => ({
        async json() {
          return {
            changed: [],
            runtimeDescriptorJson: '{"version":1,"collections":{}}',
          };
        },
      } as Response),
      onRuntimeDescriptorJson: (json) => updates.push(json),
    });

    assert.equal(invalidated, 0);
    assert.deepEqual(updates, ['{"version":1,"collections":{}}']);
  });
});
