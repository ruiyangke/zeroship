/**
 * wireId default is just `<exportName>`.
 *
 * The earlier default of `<moduleSlug>.<exportName>` leaked file
 * structure to the wire. The corrected default uses just the export
 * name. Path-derived slugs are gone from wireId derivation entirely
 * (they remain only for diagnostic messages).
 *
 * `docs/proposals/rpc-v2.md` §2:
 *   1. fn.config.id (explicit)            wins
 *   2. <exportName> alone                  — default
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { computeManifestExtras, type DiscoveredProcedure } from "../src/manifest.js";

async function tmproot(): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `wireid-default-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("wireId default — bare export name", () => {
  test("single export `listTodos` produces `rpc:listTodos` (no path slug)", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/server.ts"),
          exportName: "listTodos",
          moduleSlug: "src-server",
          kind: "query",
          isStream: false,
        },
      ];

      const result = await computeManifestExtras({
        root,
        procedures,
        mode: "development",
      });

      // Resource keyed by bare export name — no `src-server.` prefix.
      assert.ok(
        result.resources["rpc:listTodos"],
        `expected rpc:listTodos in resources, got: ${JSON.stringify(Object.keys(result.resources))}`,
      );
      assert.ok(
        !result.resources["rpc:src-server.listTodos"],
        "the path-slug-prefixed wireId is gone",
      );
    } finally {
      await cleanup();
    }
  });

  test("multiple non-colliding exports each get their own bare-name wireId", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/server.ts"),
          exportName: "listTodos",
          moduleSlug: "src-server",
          kind: "query",
          isStream: false,
        },
        {
          filePath: resolve(root, "src/server.ts"),
          exportName: "addTodo",
          moduleSlug: "src-server",
          kind: "mutation",
          isStream: false,
        },
        {
          filePath: resolve(root, "src/api.ts"),
          exportName: "search",
          moduleSlug: "src-api",
          kind: "query",
          isStream: false,
        },
      ];

      const result = await computeManifestExtras({
        root,
        procedures,
        mode: "development",
      });

      assert.ok(result.resources["rpc:listTodos"]);
      assert.ok(result.resources["rpc:addTodo"]);
      assert.ok(result.resources["rpc:search"]);
    } finally {
      await cleanup();
    }
  });

  test("explicit fn.config.id still wins over the bare-name default", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/server.ts"),
          exportName: "list",
          moduleSlug: "src-server",
          kind: "query",
          isStream: false,
          config: { id: "todos.list" },
        },
      ];

      const result = await computeManifestExtras({
        root,
        procedures,
        mode: "production",
      });

      assert.ok(result.resources["rpc:todos.list"], "explicit id wins");
      assert.ok(
        !result.resources["rpc:list"],
        "default doesn't apply when explicit id pinned",
      );
    } finally {
      await cleanup();
    }
  });
});
