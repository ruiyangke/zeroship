/**
 * The build must REFUSE `auth: "admin"`.
 *
 * `admin` named a principal that does not exist. There is no platform admin
 * surface: the gateway enforces `Admin` byte-for-byte as `User` at all of its
 * match sites - two arms in `crates/zeroship-gateway/src/router/auth.rs` and
 * one in `crates/zeroship-gateway/src/router/dispatch.rs`, each spelled
 * `User | Admin` - and no site tests for platform-admin identity. A `rank()`
 * function makes `Admin` win a MERGE against `User`, so the level reads as
 * implemented while the access question is never asked.
 *
 * The variant is DELETED, not implemented (`crates/zeroship-bundle/src/rule.rs`,
 * `RequiredPrincipal` with two variants). The build is the right place to
 * enforce that: it is the first thing a creator runs, and it can name the file
 * and the key. These tests pin the refusal and the message.
 *
 * `computeManifestExtras` is the same entry point `src/build.ts` calls, so a
 * refusal here is a refusal of `zeroship build`.
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
  const root = join(tmpdir(), `reject-admin-${randomUUID()}`);
  await fs.mkdir(resolve(root, "src/server"), { recursive: true });
  await fs.writeFile(resolve(root, "src/server/config.ts"), source);
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("auth: admin is refused by the build", () => {
  // The headline case. Before the deletion this call RESOLVED and emitted
  // `resources["/api/reports"].auth === "admin"` into the manifest, which the
  // gateway then enforced as `user`.
  test("refuses auth: admin on a URL-namespace resource, in production", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api/reports": { auth: "admin" }
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
            /"\/api\/reports"/,
            "the error names the offending resource key",
          );
          assert.match(
            err.message,
            /admin/,
            "the error names the value it refused",
          );
          // A creator can act on it: the message must say what to write
          // instead. `user` is the only gated level that exists.
          assert.match(
            err.message,
            /auth: "user"/,
            "the error tells the creator what to write instead",
          );
          // And it must NOT recommend the thing it just refused.
          assert.doesNotMatch(
            err.message,
            /or "admin"|or set auth: "user" or "admin"/,
            "the remedy must not re-recommend admin",
          );
          return true;
        },
      );
    } finally {
      await fx.cleanup();
    }
  });

  // Dev mode is not a softer tier here. `auth: "anonymous"` without
  // `publiclyAccessible` is a warning in dev because the shape is legal and
  // the creator may be mid-edit. `admin` is not legal in any mode: the
  // variant is gone, so a dev build that accepted it would emit a
  // manifest the runtime cannot honour.
  test("refuses auth: admin in development mode too", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api/reports": { auth: "admin" }
  }
});
`);
    try {
      await assert.rejects(
        () =>
          computeManifestExtras({
            root: fx.root,
            procedures: [],
            mode: "development",
            onWarn: () => {},
          }),
        (err: Error) => {
          assert.match(err.message, /admin/);
          return true;
        },
      );
    } finally {
      await fx.cleanup();
    }
  });

  // The wildcard root is how a creator locks a whole app down in one line.
  // Inheritance means one accepted `admin` here silently under-protects
  // every descendant.
  test("refuses auth: admin on the wildcard root", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "*": { auth: "admin", rateLimit: { rpm: 30, per: "ip" } }
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
          assert.match(err.message, /"\*"/);
          assert.match(err.message, /admin/);
          return true;
        },
      );
    } finally {
      await fx.cleanup();
    }
  });

  // A nested child is flattened before validation, so the refusal has to
  // survive the flatten. Without this arm the guard could be written on the
  // authored tree and miss every `children:` node.
  test("refuses auth: admin declared on a nested child", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api": {
      auth: "user",
      children: {
        "/admin-panel": { auth: "admin" }
      }
    }
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
          assert.match(err.message, /admin/);
          return true;
        },
      );
    } finally {
      await fx.cleanup();
    }
  });

  // The two levels that survive must keep working, or the refusal above is
  // indistinguishable from a build that rejects every `auth` value. This is
  // the control: same shape, one variable changed.
  test("still accepts the two levels that exist", async () => {
    const fx = await makeConfigFixture(`
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "/api/reports": { auth: "user" },
    "/health": { auth: "anonymous", publiclyAccessible: true }
  }
});
`);
    try {
      const result = await computeManifestExtras({
        root: fx.root,
        procedures: [],
        mode: "production",
      });
      assert.equal(result.resources["/api/reports"].auth, "user");
      assert.equal(result.resources["/health"].auth, "anonymous");
    } finally {
      await fx.cleanup();
    }
  });
});
