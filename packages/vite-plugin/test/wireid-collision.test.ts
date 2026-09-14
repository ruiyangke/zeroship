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

  test("both sides pinning distinct explicit ids collide on neither", async () => {
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
          config: { id: "users.add" },
        },
      ];

      const result = await computeManifestExtras({
        root,
        procedures,
        mode: "development",
      });

      assert.ok(result.resources["rpc:todos.add"], "first explicit id resource");
      assert.ok(result.resources["rpc:users.add"], "second explicit id resource");
      assert.equal(
        result.resources["rpc:add"],
        undefined,
        "no bare-name resource is emitted when both sides pin",
      );
    } finally {
      await cleanup();
    }
  });

  // The dedup arm inside the collision loop. The transform can fire on
  // more than one Vite environment, recording the SAME
  // (filePath, exportName) twice. Two records that name one procedure
  // are not two procedures — flagging them would refuse a build that has
  // no ambiguity at all, since both entries dispatch to the same handler.
  test("the same (filePath, exportName) recorded twice is not a collision", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const dup: DiscoveredProcedure = {
        filePath: resolve(root, "src/todos.ts"),
        exportName: "add",
        moduleSlug: "src-todos",
        kind: "mutation",
        isStream: false,
      };

      const result = await computeManifestExtras({
        root,
        // Same procedure, recorded twice — e.g. client + ssr environments.
        procedures: [dup, { ...dup }],
        mode: "development",
      });

      assert.ok(result.resources["rpc:add"], "the one procedure still lands");
    } finally {
      await cleanup();
    }
  });

  // Guards the OTHER direction of the dedup arm: dedup keys on
  // (filePath, exportName), so two records that agree on the file but
  // differ on the export name are still two distinct procedures and must
  // still collide when they resolve to one wireId.
  test("same file, different exports, one wireId → still a collision", async () => {
    const { root, cleanup } = await tmproot();
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(root, "src/todos.ts"),
          exportName: "add",
          moduleSlug: "src-todos",
          kind: "mutation",
          isStream: false,
          config: { id: "todos.write" },
        },
        {
          filePath: resolve(root, "src/todos.ts"),
          exportName: "insert",
          moduleSlug: "src-todos",
          kind: "mutation",
          isStream: false,
          config: { id: "todos.write" },
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
          assert.match(err.message, /collision/i, "mentions collision");
          assert.match(err.message, /todos\.write/, "cites the colliding wireId");
          assert.match(err.message, /"add"/, "cites the first export name");
          assert.match(err.message, /"insert"/, "cites the second export name");
          return true;
        },
      );
    } finally {
      await cleanup();
    }
  });
});
