/**
 * wireId collision detection.
 *
 * With the default wireId being just `<exportName>`, two server
 * modules that both export `add` (and neither pins `fn.config.id`)
 * would both want `rpc:add`. That's an unrecoverable collision —
 * the build must refuse, citing both file paths, and instruct the
 * user to add an explicit id.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { computeManifestExtras, type DiscoveredProcedure } from "../src/manifest.js";

async function tmproot(): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `wireid-collision-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("wireId collision detection", () => {
  test("two `add` exports without explicit ids → build error citing both paths", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/todos.ts"),
          exportName: "add",
          moduleSlug: "src-todos",
          kind: "mutation",
          isStream: false,
        },
        {
          filePath: resolve(root, "src/users.ts"),
          exportName: "add",
          moduleSlug: "src-users",
          kind: "mutation",
          isStream: false,
        },
      ];

      await assert.rejects(
        () =>
          computeManifestExtras({
            root,
            procedures,
            mode: "development",
          }),
        (err: Error) => {
          assert.match(err.message, /collision|collid|duplicate/i, "mentions collision");
          assert.match(err.message, /add/, "mentions the colliding export name");
          // Both file paths must be cited so the user knows where to fix.
          assert.match(err.message, /src\/todos\.ts/, "cites first file path");
          assert.match(err.message, /src\/users\.ts/, "cites second file path");
          // Action item: add explicit id.
          assert.match(err.message, /explicit|config\.id|pin/i, "instructs to add explicit id");
          return true;
        },
      );
    } finally {
      await cleanup();
    }
  });

  test("collision is resolved when one side pins an explicit id", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/todos.ts"),
          exportName: "add",
          moduleSlug: "src-todos",
          kind: "mutation",
          isStream: false,
          config: { id: "todos.add" },
        },
        {
          filePath: resolve(root, "src/users.ts"),
          exportName: "add",
          moduleSlug: "src-users",
          kind: "mutation",
          isStream: false,
          // No id — gets default `add`.
        },
      ];

      const result = await computeManifestExtras({
        root,
        procedures,
        mode: "development",
      });

      assert.ok(result.resources["rpc:todos.add"], "explicit id resource");
      assert.ok(result.resources["rpc:add"], "default id resource");
    } finally {
      await cleanup();
    }
  });
});
