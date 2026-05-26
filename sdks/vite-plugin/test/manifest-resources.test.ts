/**
 * Manifest emitter for the unified `resources` block.
 *
 * The new emitter (in `src/manifest.ts`) takes the transform state
 * (discovered procedures + their metadata) plus the `defineApp({ resources })`
 * tree from `src/server/config.ts`, and produces two companion blocks
 * the vite-plugin merges into the existing manifest:
 *
 *   - `manifest.resources`   — flat key map per
 *     `docs/proposals/rpc.md` §7
 *   - `manifest.transformer` — `"json"` (the current default)
 *
 * These tests feed the emitter a tmpdir fixture and inspect the output.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { computeManifestExtras, type DiscoveredProcedure } from "../src/manifest.js";

interface Fixture {
  root: string;
  cleanup: () => Promise<void>;
}

async function makeFixture(files: Record<string, string>): Promise<Fixture> {
  const root = join(tmpdir(), `manifest-resources-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  for (const [rel, content] of Object.entries(files)) {
    const abs = resolve(root, rel);
    await fs.mkdir(resolve(abs, ".."), { recursive: true });
    await fs.writeFile(abs, content);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("computeManifestExtras", () => {
  test("auto-derives rpc: entries for every discovered procedure", async () => {
    const fix = await makeFixture({});
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/server/todos.ts"),
          exportName: "list",
          moduleSlug: "src-server-todos",
          kind: "query",
          isStream: false,
        },
        {
          filePath: resolve(fix.root, "src/server/todos.ts"),
          exportName: "add",
          moduleSlug: "src-server-todos",
          kind: "mutation",
          isStream: false,
          config: { idempotent: true },
        },
      ];

      // Use development mode here — these procedures don't pin explicit
      // ids, so they fall to the bare-name default. Production mode
      // would refuse those (production-mode gate); the production-gate
      // test owns that scenario explicitly.
      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "development",
      });

      // Every discovered procedure gets an `rpc:<exportName>` resource.
      assert.ok(
        result.resources["rpc:list"],
        "rpc:list resource present"
      );
      assert.equal(
        result.resources["rpc:list"].kind,
        "query",
        "kind preserved"
      );
      assert.equal(
        result.resources["rpc:add"].idempotent,
        true,
        "idempotent passed through from config"
      );

      // Transformer default is json.
      assert.equal(result.transformer, "json");
    } finally {
      await fix.cleanup();
    }
  });

  test("zod schema markers on config.input/config.output never reach the wire", async () => {
    // The transform stashes a sentinel on `proc.config.input` /
    // `proc.config.output` when the user declared a Zod schema. The
    // manifest emitter must drop these keys — schemas are typed
    // runtime objects the synthetic SSR entry parses against; the
    // wire never sees a JSONSchema; a future codegen sidecar can
    // consume those schemas separately.
    const fix = await makeFixture({});
    try {
      const ZS_MARKER = Object.freeze({ __zsSchema: true });
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/a.ts"),
          exportName: "one",
          moduleSlug: "src-a",
          kind: "query",
          isStream: false,
          config: {
            id: "one",
            input: ZS_MARKER,
            output: ZS_MARKER,
            idempotent: false,
          },
        },
      ];

      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "production",
      });

      const r = result.resources["rpc:one"] as Record<string, unknown>;
      assert.ok(r, "resource emitted");
      // schemas field never appears on the result.
      assert.equal(
        (result as Record<string, unknown>).schemas,
        undefined,
        "no schemas field on manifest extras",
      );
      // input/output keys (declared by the user via Zod) must not
      // appear on the wire shape — they're runtime-only.
      assert.ok(!("input" in r), "input key dropped from wire shape");
      assert.ok(!("output" in r), "output key dropped from wire shape");
      assert.ok(!("input_schema" in r), "no input_schema on wire");
      assert.ok(!("output_schema" in r), "no output_schema on wire");
      // Other config fields still flow through.
      assert.equal(r.idempotent, false, "idempotent passes through");
    } finally {
      await fix.cleanup();
    }
  });

  test("flattens defineApp({ resources }) children: keys and merges with auto-derived", async () => {
    const fix = await makeFixture({
      "src/server/config.ts": `import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "*": { auth: "user", rateLimit: { rpm: 600, per: "user" } },
    "rpc:todos": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
      children: {
        "delete": { auth: "admin", override: ["auth"] },
      },
    },
  },
});
`,
    });
    try {
      // Pin explicit ids so the auto-derived rpc: keys land in the
      // `rpc:todos.<X>` namespace and merge with the user-defined tree.
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/server/todos.ts"),
          exportName: "list",
          moduleSlug: "todos",
          kind: "query",
          isStream: false,
          config: { id: "todos.list" },
        },
        {
          filePath: resolve(fix.root, "src/server/todos.ts"),
          exportName: "delete",
          moduleSlug: "todos",
          kind: "mutation",
          isStream: false,
          config: { id: "todos.delete" },
        },
      ];

      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "production",
      });

      // Root default
      assert.equal(result.resources["*"].auth, "user", "root * default present");

      // Parent rpc:todos retained
      assert.equal(
        result.resources["rpc:todos"].auth,
        "user",
        "rpc:todos parent retained"
      );

      // Flattened child — note the dot separator in the rpc: namespace.
      const del = result.resources["rpc:todos.delete"];
      assert.ok(del, "rpc:todos.delete (flattened) present");
      assert.equal(del.auth, "admin", "child auth wins (with override)");
      assert.deepEqual(del.override, ["auth"], "override marker preserved");
      assert.equal(del.kind, "mutation", "kind from auto-derived merged in");
    } finally {
      await fix.cleanup();
    }
  });

  test("ignores absent or empty config file", async () => {
    const fix = await makeFixture({});
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/server/x.ts"),
          exportName: "y",
          moduleSlug: "x",
          kind: "query",
          isStream: false,
        },
      ];
      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "development",
      });
      assert.ok(result.resources["rpc:y"], "auto-derived entry still present");
      // No `*` root if there's no defineApp.
      assert.ok(!("*" in result.resources), "no root * default when no config");
    } finally {
      await fix.cleanup();
    }
  });

  test("manifest schema versionHint is 1 (initial published shape)", async () => {
    const fix = await makeFixture({});
    try {
      const result = await computeManifestExtras({
        root: fix.root,
        procedures: [],
        mode: "production",
      });
      assert.equal(result.versionHint, 1, "schema version 1 hint emitted");
    } finally {
      await fix.cleanup();
    }
  });

  // ── Item #6: src/server/config.ts is the only canonical config path ──
  //
  // Pre-cleanup the emitter accepted four candidate paths
  // (src/server/config.{ts,js}, zeroship.config.{ts,js}). v2 collapses
  // to exactly one — `src/server/config.ts` — to avoid ambiguity about
  // where app-level RPC defaults live.

  test("src/server/config.ts IS the canonical path", async () => {
    // Sanity-check happy path: the file at src/server/config.ts is read.
    const fix = await makeFixture({
      "src/server/config.ts": `import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "*": { auth: "admin", rateLimit: { rpm: 30, per: "ip" } },
  },
});
`,
    });
    try {
      const result = await computeManifestExtras({
        root: fix.root,
        procedures: [],
        mode: "production",
      });
      assert.equal(result.resources["*"].auth, "admin", "config picked up");
    } finally {
      await fix.cleanup();
    }
  });

  test("zeroship.config.ts at project root is IGNORED", async () => {
    // Files at the project root were a legacy fallback. v2 only reads
    // `src/server/config.ts`. A `zeroship.config.ts` here is treated
    // as a stray file and produces no resources.
    const fix = await makeFixture({
      "zeroship.config.ts": `import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "*": { auth: "admin", rateLimit: { rpm: 30, per: "ip" } },
  },
});
`,
    });
    try {
      const result = await computeManifestExtras({
        root: fix.root,
        procedures: [],
        mode: "production",
      });
      // No `*` because zeroship.config.ts isn't read.
      assert.ok(
        !("*" in result.resources),
        "stray zeroship.config.ts at root is not consumed",
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("fn.config.idempotencyTtl: { hours: 48 } emits idempotency_ttl_hours: 48", async () => {
    // Authoring surface: `add.config = { idempotent: true, idempotencyTtl: { hours: 48 } }`
    // Wire shape: `idempotency_ttl_hours: 48`. The wrapper object exists
    // so future units (`days`, `minutes`) can land without breaking the
    // existing shape.
    const fix = await makeFixture({});
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/server/billing.ts"),
          exportName: "charge",
          moduleSlug: "src-server-billing",
          kind: "mutation",
          isStream: false,
          config: {
            id: "billing.charge",
            idempotent: true,
            idempotencyTtl: { hours: 48 },
          },
        },
      ];
      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "production",
      });
      const r = result.resources["rpc:billing.charge"] as Record<string, unknown>;
      assert.ok(r, "resource emitted");
      assert.equal(r.idempotent, true);
      assert.equal(r.idempotency_ttl_hours, 48, "ttl flowed to wire field");
      // Authoring-side key is dropped from the wire.
      assert.ok(
        !("idempotencyTtl" in r),
        "camelCase authoring key removed from wire shape",
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("idempotencyTtl absent → no idempotency_ttl_hours on the wire (gateway default applies)", async () => {
    // Default-TTL case: the wire field is undefined and the gateway
    // falls back to its 24h default. Spec §8 — keeping the wire small
    // means `skip_serializing_if = "Option::is_none"` on the Rust side.
    const fix = await makeFixture({});
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/server/todos.ts"),
          exportName: "add",
          moduleSlug: "src-server-todos",
          kind: "mutation",
          isStream: false,
          config: {
            id: "todos.add",
            idempotent: true,
          },
        },
      ];
      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "production",
      });
      const r = result.resources["rpc:todos.add"] as Record<string, unknown>;
      assert.ok(r, "resource emitted");
      assert.equal(r.idempotent, true);
      assert.equal(
        r.idempotency_ttl_hours,
        undefined,
        "no ttl field when not pinned",
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("idempotencyTtl out-of-band values clamp into [1, 168] hours", async () => {
    // Spec §8 / spec hygiene: 1h–168h band. Authored values outside
    // the band clamp at the boundary so the wire is always valid.
    const fix = await makeFixture({});
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/server/a.ts"),
          exportName: "tooLow",
          moduleSlug: "src-server-a",
          kind: "mutation",
          isStream: false,
          config: { id: "a.tooLow", idempotent: true, idempotencyTtl: { hours: 0 } },
        },
        {
          filePath: resolve(fix.root, "src/server/a.ts"),
          exportName: "tooHigh",
          moduleSlug: "src-server-a",
          kind: "mutation",
          isStream: false,
          config: { id: "a.tooHigh", idempotent: true, idempotencyTtl: { hours: 9999 } },
        },
        {
          filePath: resolve(fix.root, "src/server/a.ts"),
          exportName: "exactlyMin",
          moduleSlug: "src-server-a",
          kind: "mutation",
          isStream: false,
          config: { id: "a.exactlyMin", idempotent: true, idempotencyTtl: { hours: 1 } },
        },
        {
          filePath: resolve(fix.root, "src/server/a.ts"),
          exactly: undefined,
          exportName: "exactlyMax",
          moduleSlug: "src-server-a",
          kind: "mutation",
          isStream: false,
          config: { id: "a.exactlyMax", idempotent: true, idempotencyTtl: { hours: 168 } },
        } as DiscoveredProcedure,
      ];
      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "production",
      });
      assert.equal(
        (result.resources["rpc:a.tooLow"] as Record<string, unknown>).idempotency_ttl_hours,
        1,
        "0h clamps up to 1",
      );
      assert.equal(
        (result.resources["rpc:a.tooHigh"] as Record<string, unknown>).idempotency_ttl_hours,
        168,
        "9999h clamps down to 168",
      );
      assert.equal(
        (result.resources["rpc:a.exactlyMin"] as Record<string, unknown>).idempotency_ttl_hours,
        1,
        "1h passes through",
      );
      assert.equal(
        (result.resources["rpc:a.exactlyMax"] as Record<string, unknown>).idempotency_ttl_hours,
        168,
        "168h passes through",
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("idempotencyTtl with non-object value is silently dropped", async () => {
    // Mistakes like `idempotencyTtl: 48` (number not object) shouldn't
    // crash the build — drop the field and let the gateway fall back
    // to the default. Future shape evolution wins this gracefully.
    const fix = await makeFixture({});
    try {
      const procedures: DiscoveredProcedure[] = [
        {
          filePath: resolve(fix.root, "src/server/a.ts"),
          exportName: "wrongShape",
          moduleSlug: "src-server-a",
          kind: "mutation",
          isStream: false,
          config: {
            id: "a.wrongShape",
            idempotent: true,
            idempotencyTtl: 48 as unknown as { hours: number },
          },
        },
      ];
      const result = await computeManifestExtras({
        root: fix.root,
        procedures,
        mode: "production",
      });
      const r = result.resources["rpc:a.wrongShape"] as Record<string, unknown>;
      assert.equal(
        r.idempotency_ttl_hours,
        undefined,
        "non-object idempotencyTtl dropped",
      );
      assert.ok(!("idempotencyTtl" in r), "raw key never reaches wire");
    } finally {
      await fix.cleanup();
    }
  });
});
