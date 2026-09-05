/**
 * The build must REFUSE `auth: "admin"`.
 *
 * `admin` is a level for a principal that does not exist.
 * `docs/architecture/control-plane.md` states "There is no platform admin
 * surface", and the gateway enforces `Admin` byte-for-byte as `User` at all
 * three of its match sites:
 *
 *   crates/zeroship-gateway/src/router/auth.rs      `AuthLevel::User | AuthLevel::Admin` (x2)
 *   crates/zeroship-gateway/src/router/dispatch.rs  `(None, AuthLevel::User | AuthLevel::Admin)`
 *
 * No site anywhere tests for platform-admin identity, and
 * `crates/zeroship-bundle/src/rule.rs` says so in the variant's own doc
 * comment: `rank()` is real and load-bearing, but for the MERGE question,
 * not the ACCESS question.
 *
 * That would be an internal wart if the creator surface did not RECOMMEND
 * the value. It does, twice, in this very file's production sibling
 * (`src/manifest.ts`): the secure-by-default remedy says `or set auth:
 * "user" or "admin"`, and the fail-closed banner says `auth: "user"` /
 * `auth: "admin"` "to keep it gated". A creator who follows that advice to
 * lock a route down gets user-level protection - every signed-in end user
 * reaches it.
 *
 * The decision is to DELETE the variant, not implement it. The build is the
 * right place to say so: it is the first thing a creator runs, and it can
 * name the file and the key. These tests pin the refusal and the message.
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
  // The headline case. Today this call RESOLVES and emits
  // `resources["/api/reports"].auth === "admin"` into the manifest, which the
  // gateway then enforces as `user`.
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

  // Dev mode is not a softer tier here. `auth: "anon"` without
  // `publiclyAccessible` is a warning in dev because the shape is legal and
  // the creator may be mid-edit. `admin` is not legal in any mode: the
  // variant is being deleted, so a dev build that accepted it would emit a
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

  // The wildcard root is how a creator locks a whole app down in one line,
  // and `manifest-resources.test.ts` already has fixtures shaped exactly
  // like this using `admin`. Inheritance means one accepted `admin` here
  // silently under-protects every descendant.
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
    "/health": { auth: "anon", publiclyAccessible: true }
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
      assert.equal(result.resources["/health"].auth, "anon");
    } finally {
      await fx.cleanup();
    }
  });
});
