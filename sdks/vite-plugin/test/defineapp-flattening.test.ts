/**
 * Phase 1 — defineApp({ resources }) tree flattening.
 *
 * `children: { ... }` is authoring sugar. The build flattens to fully
 * qualified keys, prepending the parent's namespace separator:
 *   - `rpc:` namespace uses dots:    "rpc:todos" + "add"   → "rpc:todos.add"
 *   - URL  namespace uses slashes:   "/api"      + "admin" → "/api/admin"
 *
 * The bare `*` root holds default-policy fallback; never has children.
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
  const root = join(tmpdir(), `defineapp-${randomUUID()}`);
  await fs.mkdir(resolve(root, "src/server"), { recursive: true });
  await fs.writeFile(resolve(root, "src/server/config.ts"), source);
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("defineApp tree flattening", () => {
  test("flattens nested rpc: tree with dot separator", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:todos": {
      auth: "user", override: ["auth"],
      children: {
        "list":   { kind: "query" },
        "add":    { kind: "mutation", idempotent: true },
        "delete": { auth: "admin", override: ["auth"] },
      },
    },
  },
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "production",
      });
      assert.ok(r.resources["rpc:todos"], "parent retained");
      assert.ok(r.resources["rpc:todos.list"], "rpc:todos.list flattened");
      assert.ok(r.resources["rpc:todos.add"], "rpc:todos.add flattened");
      assert.ok(r.resources["rpc:todos.delete"], "rpc:todos.delete flattened");
      // children: key is stripped from the parent.
      assert.ok(
        !("children" in r.resources["rpc:todos"]),
        "children: key stripped from parent"
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("flattens nested URL tree with slash separator", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api": {
      auth: "user", override: ["auth"],
      children: {
        "admin": {
          auth: "admin", override: ["auth"],
          children: {
            "users": { rateLimit: { rpm: 100, per: "user" } },
          },
        },
        "public": { auth: "anon", override: ["auth"], publiclyAccessible: true },
      },
    },
  },
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "production",
      });
      assert.ok(r.resources["/api"], "parent retained");
      assert.ok(r.resources["/api/admin"], "/api/admin flattened");
      assert.ok(
        r.resources["/api/admin/users"],
        "/api/admin/users (deeply nested) flattened"
      );
      assert.ok(r.resources["/api/public"], "/api/public flattened");
      assert.equal(r.resources["/api/admin"].auth, "admin");
    } finally {
      await fx.cleanup();
    }
  });

  test("camelCase fields snake-case-rename to wire shape", async () => {
    // The manifest wire shape uses snake_case (rate_limit, max_input_bytes,
    // publicly_accessible, csrf_origins). The authoring API uses camelCase.
    // The flattener does the rename so the manifest matches spec §7.
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api/public": {
      auth: "anon",
      override: ["auth"],
      publiclyAccessible: true,
      maxInputBytes: 4096,
      rateLimit: { rpm: 60, per: "ip" },
      csrfOrigins: ["https://example.com"],
    },
  },
});
`);
    try {
      const r = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "production",
      });
      const e = r.resources["/api/public"] as Record<string, unknown>;
      assert.equal(e.publicly_accessible, true);
      assert.equal(e.max_input_bytes, 4096);
      assert.deepEqual(e.rate_limit, { rpm: 60, per: "ip" });
      assert.deepEqual(e.csrf_origins, ["https://example.com"]);
      // camelCase keys must NOT be on the wire.
      assert.ok(!("publiclyAccessible" in e));
      assert.ok(!("maxInputBytes" in e));
      assert.ok(!("rateLimit" in e));
      assert.ok(!("csrfOrigins" in e));
    } finally {
      await fx.cleanup();
    }
  });
});
