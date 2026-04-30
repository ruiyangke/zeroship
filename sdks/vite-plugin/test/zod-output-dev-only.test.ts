/**
 * Output validation runs in dev only.
 *
 * `fn.config.output` is a cheap dev-time correctness check — does the
 * handler return what its schema says it returns? In production we
 * skip validation for hot-path performance. Toggled by
 * `process.env.NODE_ENV`.
 *
 * Spec: `docs/proposals/rpc-v2.md` §4 — "Output validation runs in
 * dev mode only (cheap dev-time correctness check); skipped in
 * production for hot-path performance."
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { writeFile, mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";

import { RPC_REGISTRY_SOURCE } from "../src/rpc-registry.js";

interface RegistryModule {
  _zsRegister: (name: string, fn: (...a: unknown[]) => unknown) => void;
  dispatch: (
    methodName: string,
    args: unknown[],
  ) => Promise<unknown>;
}

async function loadRegistry(): Promise<{
  mod: RegistryModule;
  cleanup: () => Promise<void>;
}> {
  const dir = await mkdtemp(join(tmpdir(), "zsout-"));
  const path = join(dir, "registry.mjs");
  await writeFile(path, RPC_REGISTRY_SOURCE, "utf8");
  const mod = (await import(pathToFileURL(path).href)) as RegistryModule;
  return { mod, cleanup: () => rm(dir, { recursive: true, force: true }) };
}

/**
 * Always-failing output schema. Used to assert validation ran (the
 * dev path) or didn't (the prod path).
 */
const FAILING_OUTPUT_SCHEMA = {
  parse(_: unknown): never {
    const err = new Error("output mismatch") as Error & {
      name: string;
      issues: unknown[];
    };
    err.name = "ZodError";
    err.issues = [{ path: ["root"], message: "type mismatch" }];
    throw err;
  },
};

async function withNodeEnv<T>(
  value: string | undefined,
  fn: () => Promise<T>,
): Promise<T> {
  const prev = process.env.NODE_ENV;
  if (value === undefined) delete process.env.NODE_ENV;
  else process.env.NODE_ENV = value;
  try {
    return await fn();
  } finally {
    if (prev === undefined) delete process.env.NODE_ENV;
    else process.env.NODE_ENV = prev;
  }
}

describe("synthetic-entry — output validation gating", () => {
  test("dev mode: output schema runs and surfaces INTERNAL on mismatch", async () => {
    const { mod, cleanup } = await loadRegistry();
    try {
      async function f() {
        return { wrong: "shape" };
      }
      (f as { config?: unknown }).config = { output: FAILING_OUTPUT_SCHEMA };
      mod._zsRegister("f", f as never);

      await withNodeEnv("development", async () => {
        await assert.rejects(mod.dispatch("f", []), (err: unknown) => {
          const e = err as { status?: number; code?: string };
          assert.equal(e.status, 500, "status 500 (INTERNAL)");
          assert.equal(e.code, "INTERNAL", "code INTERNAL");
          return true;
        });
      });
    } finally {
      await cleanup();
    }
  });

  test("production: output schema is skipped; handler result returned", async () => {
    const { mod, cleanup } = await loadRegistry();
    try {
      async function f() {
        return { wrong: "shape" };
      }
      (f as { config?: unknown }).config = { output: FAILING_OUTPUT_SCHEMA };
      mod._zsRegister("f", f as never);

      await withNodeEnv("production", async () => {
        const result = await mod.dispatch("f", []);
        assert.deepEqual(result, { wrong: "shape" });
      });
    } finally {
      await cleanup();
    }
  });

  test("undefined NODE_ENV: behaves as dev (validation runs)", async () => {
    // Per spec, only NODE_ENV=production opts out. Anything else
    // (including unset) keeps the dev-time correctness check on.
    const { mod, cleanup } = await loadRegistry();
    try {
      async function f() {
        return null;
      }
      (f as { config?: unknown }).config = { output: FAILING_OUTPUT_SCHEMA };
      mod._zsRegister("f", f as never);

      await withNodeEnv(undefined, async () => {
        await assert.rejects(mod.dispatch("f", []), (err: unknown) => {
          const e = err as { status?: number; code?: string };
          assert.equal(e.status, 500);
          assert.equal(e.code, "INTERNAL");
          return true;
        });
      });
    } finally {
      await cleanup();
    }
  });

  test("input validation ALWAYS runs (regardless of NODE_ENV)", async () => {
    // Output is dev-only; input is always-on. The trust model says
    // "if you declare a schema, the wire enforces it for callers".
    const { mod, cleanup } = await loadRegistry();
    try {
      const FAILING_INPUT = {
        parse(_: unknown): never {
          const err = new Error("bad input") as Error & {
            name: string;
            issues: unknown[];
          };
          err.name = "ZodError";
          err.issues = [{ path: [], message: "nope" }];
          throw err;
        },
      };
      async function f() {
        return null;
      }
      (f as { config?: unknown }).config = { input: FAILING_INPUT };
      mod._zsRegister("f", f as never);

      await withNodeEnv("production", async () => {
        await assert.rejects(mod.dispatch("f", [{}]), (err: unknown) => {
          const e = err as { status?: number; code?: string };
          assert.equal(e.status, 400, "input still validates in prod");
          assert.equal(e.code, "INVALID_ARGUMENT");
          return true;
        });
      });
    } finally {
      await cleanup();
    }
  });
});
