/**
 * Build-time visibility for the fail-closed RPC auth default.
 *
 * A `rpc:` procedure whose entire inheritance chain declares no `auth`
 * resolves to `RequiredPrincipal::User` in the gateway
 * (`crates/zeroship-bundle/src/compiled.rs`, `resolve_effective_policy`, the
 * `if !auth_declared && key.starts_with("rpc:")` arm). That default is
 * deliberate and correct — a forgotten policy must be a loud 401, not a
 * silent public endpoint.
 *
 * The problem it creates is a VISIBILITY one: enforcement lives only in
 * the gateway, so `pnpm dev` (no gateway) runs such a procedure happily
 * and the creator does not learn about the 401 until after deploying.
 * The build already knows the discovered procedure list AND the resolved
 * resource tree, so it can name the affected procedures at build time.
 *
 * What these tests pin:
 *   - undeclared procedures produce a warning that NAMES them,
 *   - fully-declared apps produce NO warning at all (a warning that fires
 *     on correct apps trains people to ignore it),
 *   - the predicate follows the same inheritance chain the gateway walks:
 *     root `*`, dot-segment family ancestors, then the key itself.
 *
 * FIXTURE DISCRIMINATION (this pair must not collapse): the warn side
 * resolves to a merged resource whose `auth` field is literally
 * `undefined`; the silent side resolves to one whose `auth` field is a
 * string ("anonymous" / "user"). Several tests assert that resolved
 * value directly alongside the warning assertion, so a fixture that
 * accidentally gave both sides the same `auth` would fail loudly rather
 * than pass vacuously.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { computeManifestExtras } from "../src/manifest.js";
import type { DiscoveredProcedure } from "../src/manifest.js";

/**
 * Fixture root. `configSource === null` writes NO `src/server/config.ts`
 * at all — the shape 172 procedures across 11 examples actually ship.
 */
async function makeFixture(configSource: string | null): Promise<{
  root: string;
  cleanup: () => Promise<void>;
}> {
  const root = join(tmpdir(), `failclosed-${randomUUID()}`);
  await fs.mkdir(resolve(root, "src/server"), { recursive: true });
  if (configSource !== null) {
    await fs.writeFile(resolve(root, "src/server/config.ts"), configSource);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

/**
 * A discovered procedure with an explicit `id` (the production-mode gate
 * rejects bare-name defaults, and that gate is not what these tests are
 * about).
 */
function proc(
  id: string,
  extra: Partial<DiscoveredProcedure> = {},
): DiscoveredProcedure {
  return {
    filePath: `/app/src/index.ts`,
    exportName: id.replace(/\./g, "_"),
    moduleSlug: "src-index",
    kind: "query",
    isStream: false,
    ...extra,
    config: { id, ...(extra.config ?? {}) },
  };
}

async function run(
  configSource: string | null,
  procedures: DiscoveredProcedure[],
) {
  const fx = await makeFixture(configSource);
  try {
    const warnings: string[] = [];
    const extras = await computeManifestExtras({
      root: fx.root,
      procedures,
      mode: "production",
      onWarn: (m) => warnings.push(m),
    });
    return { warnings, extras };
  } finally {
    await fx.cleanup();
  }
}

/** The subset of warnings that are about the fail-closed auth default. */
function authWarnings(warnings: string[]): string[] {
  return warnings.filter((w) => /fail-closed/i.test(w));
}

describe("fail-closed auth warning", () => {
  test("warns and NAMES a procedure with no auth policy anywhere", async () => {
    const { warnings, extras } = await run(null, [proc("listTodos")]);

    // Fixture discrimination: the warn side's resolved `auth` is undefined.
    assert.equal(
      extras.resources["rpc:listTodos"].auth,
      undefined,
      "warn-side fixture must actually resolve to NO auth field",
    );

    const w = authWarnings(warnings);
    assert.equal(w.length, 1, `expected exactly one fail-closed warning, got ${w.length}`);
    assert.match(w[0], /listTodos/, "warning names the offending procedure");
  });

  test("says what to do: points at src/server/config.ts and both remedies", async () => {
    const { warnings } = await run(null, [proc("listTodos")]);
    const w = authWarnings(warnings)[0];
    assert.ok(w, "a warning was emitted");
    assert.match(w, /src\/server\/config\.ts/, "points at the file to create or edit");
    assert.match(w, /publiclyAccessible/, "offers the public remedy");
    assert.match(w, /auth/, "offers declaring a real auth level");
  });

  test("names EVERY undeclared procedure, not just a count", async () => {
    const { warnings } = await run(null, [
      proc("listTodos"),
      proc("createTodo"),
      proc("deleteTodo"),
    ]);
    const w = authWarnings(warnings).join("\n");
    for (const name of ["listTodos", "createTodo", "deleteTodo"]) {
      assert.match(w, new RegExp(name), `warning names ${name}`);
    }
  });

  // ── Silence on correct apps ─────────────────────────────────────────────

  test("SILENT when the procedure declares auth in src/server/config.ts", async () => {
    const { warnings, extras } = await run(
      `
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:listTodos": { auth: "anonymous", publiclyAccessible: true }
  }
});
`,
      [proc("listTodos")],
    );

    // Fixture discrimination: silent side resolves to a real auth string.
    assert.equal(
      extras.resources["rpc:listTodos"].auth,
      "anonymous",
      "silent-side fixture must actually resolve to a declared auth",
    );

    assert.deepEqual(
      authWarnings(warnings),
      [],
      "a declared policy must produce NO fail-closed warning",
    );
  });

  test("SILENT when auth comes from the per-procedure config", async () => {
    const { warnings, extras } = await run(null, [
      proc("listTodos", { config: { auth: "user" } }),
    ]);
    assert.equal(extras.resources["rpc:listTodos"].auth, "user");
    assert.deepEqual(authWarnings(warnings), []);
  });

  test("SILENT when auth comes from module-level $config", async () => {
    const { warnings, extras } = await run(null, [
      proc("listTodos", { moduleConfig: { auth: "user" } }),
    ]);
    assert.equal(extras.resources["rpc:listTodos"].auth, "user");
    assert.deepEqual(authWarnings(warnings), []);
  });

  test("SILENT when a dot-segment FAMILY ancestor declares auth", async () => {
    // The gateway's chain for `rpc:todos.list` includes `rpc:todos`
    // (build_inheritance_chain), so the family policy covers the child.
    const { warnings, extras } = await run(
      `
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:todos": { auth: "user" }
  }
});
`,
      [proc("todos.list"), proc("todos.create")],
    );
    // The child's OWN entry carries no auth - inheritance is what covers it.
    assert.equal(
      extras.resources["rpc:todos.list"].auth,
      undefined,
      "child entry itself declares no auth; only the ancestor does",
    );
    assert.equal(extras.resources["rpc:todos"].auth, "user");
    assert.deepEqual(
      authWarnings(warnings),
      [],
      "an inherited family policy must silence the warning",
    );
  });

  test("SILENT when the root `*` resource declares auth", async () => {
    const { warnings } = await run(
      `
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "*": { auth: "user" }
  }
});
`,
      [proc("listTodos")],
    );
    assert.deepEqual(
      authWarnings(warnings),
      [],
      "a root policy covers every rpc key, exactly as the gateway resolves it",
    );
  });

  // ── Discrimination: partial coverage ───────────────────────────────────

  test("names ONLY the undeclared procedure when a sibling is declared", async () => {
    const { warnings, extras } = await run(
      `
import { defineApp } from "@zeroship/server";
export default defineApp({
  resources: {
    "rpc:declared": { auth: "anonymous", publiclyAccessible: true }
  }
});
`,
      [proc("declared"), proc("undeclared")],
    );

    // The two sides of the pair take genuinely different resolved values.
    assert.equal(extras.resources["rpc:declared"].auth, "anonymous");
    assert.equal(extras.resources["rpc:undeclared"].auth, undefined);

    const w = authWarnings(warnings).join("\n");
    assert.match(w, /undeclared/, "names the procedure that will 401");
    assert.ok(
      !/\bdeclared\b(?!\s*`)/.test(w.replace(/undeclared/g, "")),
      `must not name the correctly-declared procedure; got:\n${w}`,
    );
  });

  test("no procedures at all => no warning", async () => {
    const { warnings } = await run(null, []);
    assert.deepEqual(authWarnings(warnings), []);
  });
});
