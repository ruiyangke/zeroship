"use server";
// SEC-5 regression: every `projects.*` RPC procedure must resolve to an
// authenticated (auth: "user") policy, and the resource tree must carry
// a fail-closed root so an absent policy can never silently resolve to
// the gateway's Anon default.
//
// Background: `src/server/config.ts` once declared `rpc:apps` — a relic
// of the module being named `apps.ts`. The live procedures carry
// `projects.*` wireIds, so the vite-plugin's auto-derived per-procedure
// resources (`rpc:projects.listProjects`, …) had NO matching ancestor
// policy. The gateway's effective-policy compiler defaults to
// `AuthLevel::Anon` when nothing in a resource's inheritance chain
// declares auth (crates/gateway/src/compiled.rs `resolve_effective_policy`),
// so all ten procedures — including getEnv/setEnv/getLogs over sandbox
// secrets — shipped UNAUTHENTICATED and unthrottled.
//
// This test drives the REAL pipeline end to end:
//   1. imports the real `./projects` module and enumerates its RPC
//      exports exactly as the vite-plugin transform discovers them
//      (`__zsKind` marker + second-arg config literal);
//   2. feeds them through the REAL `computeManifestExtras` from
//      `@zeroship/vite-plugin` (production mode), which reads the REAL
//      `src/server/config.ts`;
//   3. resolves each procedure's EFFECTIVE auth/rate-limit by mirroring
//      the gateway's inheritance walk (see `effectiveAuth` below).

import { fileURLToPath } from "node:url";
import { describe, expect, it, vi } from "vitest";

// ─── seam mocks ──────────────────────────────────────────────────
// Needed only so the real `./projects` module can be IMPORTED; nothing
// in this test calls through them.
vi.mock("zeroship", () => ({ currentUser: vi.fn() }));
vi.mock("@zeroship/kv", () => ({
  kv: { get: vi.fn(), set: vi.fn(), delete: vi.fn() },
}));
vi.mock("./sandbox.js", () => ({
  readSandboxFileFor: vi.fn(),
  writeSandboxFileFor: vi.fn(),
}));

import * as projects from "./projects";
import {
  computeManifestExtras,
  type DiscoveredProcedure,
  type WireResource,
} from "../../../../sdks/vite-plugin/src/manifest";

const BUILDER_ROOT = fileURLToPath(new URL("../..", import.meta.url));
const PROJECTS_PATH = fileURLToPath(new URL("./projects.ts", import.meta.url));

type RpcKind = "query" | "mutation" | "action" | "stream" | "subscription";

/**
 * Enumerate the RPC procedures of a `"use server"` module the same way
 * the vite-plugin transform does: every export carrying a wrapper
 * marker (`__zsKind`) is published. The discovery record's `config` is
 * the wrapper's second-arg literal — the runtime `attach()` merges
 * `kind` into `.config`, which the AST literal does not have, so we
 * strip it back out and carry the kind on the record itself.
 */
function discoverProcedures(
  mod: Record<string, unknown>,
  filePath: string,
  moduleSlug: string,
): DiscoveredProcedure[] {
  const out: DiscoveredProcedure[] = [];
  for (const [exportName, value] of Object.entries(mod)) {
    if (typeof value !== "function") continue;
    const kind = (value as { __zsKind?: string }).__zsKind as
      | RpcKind
      | "procedure"
      | undefined;
    if (!kind || kind === "procedure") continue;
    const attached = (value as { config?: Record<string, unknown> }).config ?? {};
    const { kind: _attachedKind, ...config } = attached;
    out.push({
      filePath,
      exportName,
      moduleSlug,
      kind,
      isStream: false,
      config,
    });
  }
  return out;
}

// ─── effective-policy mirror ─────────────────────────────────────
// Faithful TS mirror of the gateway's policy compiler
// (crates/gateway/src/compiled.rs `build_inheritance_chain` +
// `resolve_effective_policy`): walk root `*` → declared dot-segment
// ancestors → self; stricter auth wins; a resource may only weaken via
// a self-declared `override: ["auth"]`; the default with no
// declarations anywhere in the chain is "anon".

const AUTH_RANK: Record<string, number> = { anon: 0, user: 1, admin: 2 };

function inheritanceChain(
  key: string,
  resources: Record<string, WireResource>,
): string[] {
  if (key === "*") return ["*"];
  const chain: string[] = [];
  if ("*" in resources) chain.push("*");
  if (key.startsWith("rpc:")) {
    const segs = key.slice("rpc:".length).split(".");
    for (let end = 1; end < segs.length; end++) {
      const ancestor = `rpc:${segs.slice(0, end).join(".")}`;
      if (ancestor in resources) chain.push(ancestor);
    }
  } else if (key.startsWith("/")) {
    const segs = key.replace(/^\/+/, "").split("/");
    for (let end = 1; end < segs.length; end++) {
      const ancestor = `/${segs.slice(0, end).join("/")}`;
      if (ancestor in resources) chain.push(ancestor);
    }
  }
  chain.push(key);
  return chain;
}

function effectiveAuth(
  key: string,
  resources: Record<string, WireResource>,
): "anon" | "user" | "admin" {
  let auth: "anon" | "user" | "admin" = "anon";
  for (const ancestorKey of inheritanceChain(key, resources)) {
    const node = resources[ancestorKey];
    if (!node) continue;
    const a = node.auth;
    if (typeof a !== "string" || !(a in AUTH_RANK)) continue;
    if (AUTH_RANK[a] > AUTH_RANK[auth]) {
      auth = a as "anon" | "user" | "admin";
    } else if (
      ancestorKey === key &&
      Array.isArray(node.override) &&
      node.override.includes("auth")
    ) {
      auth = a as "anon" | "user" | "admin";
    }
  }
  return auth;
}

function effectiveRateLimit(
  key: string,
  resources: Record<string, WireResource>,
): unknown | null {
  for (const ancestorKey of inheritanceChain(key, resources)) {
    const rl = resources[ancestorKey]?.rate_limit;
    if (rl != null) return rl;
  }
  return null;
}

async function builderManifestResources(): Promise<{
  resources: Record<string, WireResource>;
  wireIds: string[];
}> {
  const procedures = discoverProcedures(
    projects as Record<string, unknown>,
    PROJECTS_PATH,
    "src-server-projects",
  );
  expect(procedures.length).toBeGreaterThanOrEqual(10);
  const extras = await computeManifestExtras({
    root: BUILDER_ROOT,
    procedures,
    mode: "production",
  });
  const wireIds = procedures.map((p) => String(p.config?.id));
  return { resources: extras.resources, wireIds };
}

describe("SEC-5: projects.* RPC procedures are authenticated", () => {
  it("resolves effective auth 'user' + a rate limit for every projects.* procedure", async () => {
    const { resources, wireIds } = await builderManifestResources();

    // The real export set — guards against the test silently testing
    // nothing if the module shape changes.
    expect(wireIds).toEqual(
      expect.arrayContaining([
        "projects.listProjects",
        "projects.getProject",
        "projects.createProject",
        "projects.deleteProject",
        "projects.archiveProject",
        "projects.unarchiveProject",
        "projects.getEnv",
        "projects.setEnv",
        "projects.deleteEnv",
        "projects.getLogs",
      ]),
    );

    for (const wireId of wireIds) {
      expect
        .soft(effectiveAuth(`rpc:${wireId}`, resources), `auth for rpc:${wireId}`)
        .toBe("user");
      expect
        .soft(effectiveRateLimit(`rpc:${wireId}`, resources), `rate limit for rpc:${wireId}`)
        .not.toBeNull();
    }

    // The headline case from the finding: env writes over sandbox
    // secrets must never dispatch anonymously.
    expect(effectiveAuth("rpc:projects.setEnv", resources)).toBe("user");
  });

  it("declares a fail-closed root '*' policy and no orphan rpc:apps relic", async () => {
    const { resources } = await builderManifestResources();

    // Root default: an RPC procedure that matches NO declared policy
    // must inherit auth from `*` instead of falling through to the
    // gateway's Anon default.
    expect(resources["*"]).toBeDefined();
    expect(resources["*"]?.auth).toBe("user");
    expect(effectiveAuth("rpc:some.future.procedure", resources)).toBe("user");

    // The drifted key that caused the hole: `rpc:apps` matches no
    // procedure (the module is projects.ts) and must stay gone.
    expect(resources["rpc:apps"]).toBeUndefined();
  });

  it("keeps the resource tree deployable: shadowed fields carry override markers (crates/bundle validate mirror)", async () => {
    // Mirror of `Manifest::validate` step 2 + `shadowed_fields`
    // (crates/bundle/src/manifest.rs): a resource redeclaring a field
    // any ancestor (including root `*`) also declares must list it in
    // `override: [...]`, or the deploy is rejected. Guards the fix
    // itself — adding a root `*` without the markers would pass the
    // vite build but fail every deploy.
    const SHADOWABLE = [
      "auth",
      "rate_limit",
      "cors",
      "cache",
      "csrf_origins",
      "idempotent",
      "idempotency_ttl_hours",
      "max_input_bytes",
      "publicly_accessible",
    ] as const;
    const { resources } = await builderManifestResources();

    for (const [key, node] of Object.entries(resources)) {
      const chain = inheritanceChain(key, resources);
      const overrides = Array.isArray(node.override)
        ? (node.override as string[])
        : [];
      for (const ancestorKey of chain.slice(0, -1)) {
        const ancestor = resources[ancestorKey];
        if (!ancestor) continue;
        for (const field of SHADOWABLE) {
          if (node[field] === undefined || ancestor[field] === undefined) continue;
          expect(
            overrides,
            `resource ${key} redeclares "${field}" from ancestor ${ancestorKey} without override marker`,
          ).toContain(field);
        }
      }
    }
  });
});
