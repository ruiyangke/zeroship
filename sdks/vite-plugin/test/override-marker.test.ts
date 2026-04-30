/**
 * Phase 1 — override marker validation.
 *
 * Per spec §7: when a child resource shadows an inherited field, it
 * MUST list that field in `override: [...]`. Without the marker, the
 * build refuses with a clear error.
 *
 * This is the safety mechanism that prevents accidental policy
 * weakening (a child silently downgrading from `auth: admin` to
 * `auth: user`).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { computeManifestExtras } from "../src/manifest.js";

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
    // /api requires `auth: admin`. /api/public sets `auth: user` (a
    // weakening) without `override: ["auth"]`. Build must refuse.
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api": { auth: "admin", override: ["auth"] },
    "/api/public": { auth: "user", publiclyAccessible: true }
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
    "/api": { auth: "admin", override: ["auth"] },
    "/api/public": { auth: "anon", override: ["auth"], publiclyAccessible: true }
  }
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "production",
      });
      assert.equal(r.resources["/api/public"].auth, "anon");
    } finally {
      await fx.cleanup();
    }
  });

  test("does not require override when child is stricter than parent (admin > user)", async () => {
    // user → admin is a *strengthening* — no override marker needed.
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:todos": { auth: "user", override: ["auth"] },
    "rpc:todos.delete": { auth: "admin", override: ["auth"] }
  }
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "production",
      });
      assert.equal(r.resources["rpc:todos.delete"].auth, "admin");
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
        procedures: [],
        mode: "production",
      });
      assert.equal(r.resources["rpc:todos.add"].idempotent, true);
    } finally {
      await fx.cleanup();
    }
  });
});
