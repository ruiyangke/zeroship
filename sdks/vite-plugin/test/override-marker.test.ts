/**
 * Override marker validation.
 *
 * `docs/proposals/rpc.md` §7 says that when a child resource
 * shadows an inherited field, it MUST list that field in
 * `override: [...]`. Without the marker, the build refuses with a
 * clear error.
 *
 * This is the safety mechanism that prevents accidental policy
 * weakening (a child silently downgrading from `auth: user` to
 * `auth: anonymous`).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { computeManifestExtras } from "../src/manifest.js";

// A declared `rpc:` leaf must be backed by a discovered procedure — the
// manifest emitter refuses a policy that names nothing, which is what a
// procedure module missing its `"use server"` directive produces. These
// fixtures exist to exercise policy INHERITANCE, so they supply the
// matching procedure explicitly rather than leaning on the emitter
// tolerating an orphan key.
function procFor(root: string, id: string) {
  return {
    filePath: resolve(root, "src/server.ts"),
    exportName: id.split(".").pop() as string,
    moduleSlug: "src-server",
    kind: "mutation" as const,
    isStream: false,
    config: { id },
  };
}

async function makeConfigFixture(source: string): Promise<{
  root: string;
  cleanup: () => Promise<void>;
}> {
  const root = join(tmpdir(), `override-${randomUUID()}`);
  await fs.mkdir(resolve(root, "src/server"), { recursive: true });
  await fs.writeFile(resolve(root, "src/server/config.ts"), source);
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("override marker validation", () => {
  test("rejects child weakening parent auth without override", async () => {
    // /api requires `auth: user`. /api/public drops that to `auth: anonymous`
    // (a weakening) without `override: ["auth"]`. Build must refuse.
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api": { auth: "user" },
    "/api/public": { auth: "anonymous", publiclyAccessible: true }
  }
});
`);
    try {
      await assert.rejects(
        () =>
          computeManifestExtras({
            root: fx.root,
            procedures: [],
            mode: "production",
          }),
        (err: Error) => {
          assert.match(
            err.message,
            /override/i,
            "error message mentions override"
          );
          assert.match(
            err.message,
            /\/api\/public|auth/,
            "error message mentions which resource & field"
          );
          return true;
        }
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("accepts child weakening when override marker is present", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api": { auth: "user" },
    "/api/public": { auth: "anonymous", override: ["auth"], publiclyAccessible: true }
  }
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "production",
      });
      assert.equal(r.resources["/api/public"].auth, "anonymous");
    } finally {
      await fx.cleanup();
    }
  });

  test("does not require override when child is stricter than parent (anonymous to user)", async () => {
    // anonymous → user is a *strengthening* — no override marker needed.
    // With two principals the merge is a boolean OR, so "stricter" means
    // exactly one thing: the child demands a user where the parent did not.
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:todos": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.delete": { auth: "user" }
  }
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [procFor(fx.root, "todos.delete")],
        mode: "production",
      });
      assert.equal(r.resources["rpc:todos.delete"].auth, "user");
    } finally {
      await fx.cleanup();
    }
  });

  test("does not require override when child sets a non-shadowing field", async () => {
    // child adds `idempotent: true` — parent didn't set it, so no
    // shadow. Should pass.
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:todos": { auth: "user", override: ["auth"] },
    "rpc:todos.add": { idempotent: true }
  }
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [procFor(fx.root, "todos.add")],
        mode: "production",
      });
      assert.equal(r.resources["rpc:todos.add"].idempotent, true);
    } finally {
      await fx.cleanup();
    }
  });
});
