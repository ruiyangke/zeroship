/**
 * Production-mode gate.
 *
 * `vite build --mode production` requires every procedure to have an
 * explicit `fn.config.id`. A procedure that lands at the
 * default-bare-name step ("step 2") in production is a build error —
 * production deploys must not depend on the implicit `<exportName>`
 * mapping (which is fragile across renames).
 *
 * Resolution order:
 *   1. fn.config.id                       → never errors
 *   2. <exportName> default                → ERROR in production
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { computeManifestExtras, type DiscoveredProcedure } from "../src/manifest.js";

async function tmproot(): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `wireid-prod-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("production-mode gate", () => {
  test("production build with no explicit id → build error", async () => {
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

      await assert.rejects(
        () =>
          computeManifestExtras({
            root,
            procedures,
            mode: "production",
          }),
        (err: Error) => {
          assert.match(err.message, /listTodos/, "names the offending procedure");
          assert.match(
            err.message,
            /explicit|config\.id|id/i,
            "explains the user must add an explicit id",
          );
          assert.match(
            err.message,
            /production|deploy/i,
            "mentions production / deploy context",
          );
          return true;
        },
      );
    } finally {
      await cleanup();
    }
  });

  test("development mode: bare-name default is fine, no error", async () => {
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

      const r = await computeManifestExtras({
        root,
        procedures,
        mode: "development",
      });
      assert.ok(r.resources["rpc:listTodos"], "dev build emits rpc:listTodos");
    } finally {
      await cleanup();
    }
  });

  test("production build OK when every procedure has explicit id", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/server.ts"),
          exportName: "listTodos",
          moduleSlug: "src-server",
          kind: "query",
          isStream: false,
          config: { id: "todos.list" },
        },
        {
          filePath: resolve(root, "src/server.ts"),
          exportName: "addTodo",
          moduleSlug: "src-server",
          kind: "mutation",
          isStream: false,
          config: { id: "todos.add" },
        },
      ];

      const r = await computeManifestExtras({
        root,
        procedures,
        mode: "production",
      });
      assert.ok(r.resources["rpc:todos.list"]);
      assert.ok(r.resources["rpc:todos.add"]);
    } finally {
      await cleanup();
    }
  });

  test("production build error names ALL offending procedures, not just the first", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/a.ts"),
          exportName: "alpha",
          moduleSlug: "src-a",
          kind: "query",
          isStream: false,
        },
        {
          filePath: resolve(root, "src/b.ts"),
          exportName: "beta",
          moduleSlug: "src-b",
          kind: "query",
          isStream: false,
        },
      ];
      await assert.rejects(
        () =>
          computeManifestExtras({
            root,
            procedures,
            mode: "production",
          }),
        (err: Error) => {
          assert.match(err.message, /alpha/, "names alpha");
          assert.match(err.message, /beta/, "names beta");
          return true;
        },
      );
    } finally {
      await cleanup();
    }
  });
});
