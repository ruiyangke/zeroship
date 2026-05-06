/**
 * Secure-by-default validation.
 *
 * `docs/proposals/rpc-v2.md` §7 ("Validation") requires
 * `auth: "anon"` to be paired with `publiclyAccessible: true` on the
 * same resource. Build error in production; warning in dev.
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
  const root = join(tmpdir(), `secure-default-${randomUUID()}`);
  await fs.mkdir(resolve(root, "src/server"), { recursive: true });
  await fs.writeFile(resolve(root, "src/server/config.ts"), source);
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("secure-by-default", () => {
  test("rejects auth: anon without publiclyAccessible: true (production)", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api/public": { auth: "anon", override: ["auth"] }
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
            /publicly_accessible|publiclyAccessible/,
            "error message mentions the missing field"
          );
          assert.match(
            err.message,
            /\/api\/public|anon/,
            "error message mentions which resource"
          );
          return true;
        }
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("accepts auth: anon when publiclyAccessible: true is present", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
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
      assert.equal(
        (r.resources["/api/public"] as Record<string, unknown>).publicly_accessible,
        true
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("dev mode warns instead of throwing", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api/public": { auth: "anon", override: ["auth"] }
  }
});
`);
    try {
      const warnings: string[] = [];
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "development",
        onWarn: (msg) => warnings.push(msg),
      });
      // Build still produces output in dev.
      assert.ok(r.resources["/api/public"], "resource still emitted in dev");
      assert.ok(
        warnings.some((w) => /publicly_accessible|publiclyAccessible/.test(w)),
        "warning was emitted in dev mode"
      );
    } finally {
      await fx.cleanup();
    }
  });
});
