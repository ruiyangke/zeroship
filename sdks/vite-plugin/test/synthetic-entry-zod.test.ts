/**
 * Zod-direct validation in the synthetic SSR entry.
 *
 * Procedures opt into runtime input/output validation by setting
 * `fn.config.input` / `fn.config.output` to a Zod-shaped schema (any
 * object with a `.parse()` method). The synthetic entry's `dispatch`
 * function calls `.parse(args[0])` before invoking the handler;
 * failures throw an `INVALID_ARGUMENT` error envelope (status 400,
 * code "INVALID_ARGUMENT", details.issues = ZodError.issues).
 *
 * These tests instantiate the closure-private registry source via a
 * dynamic import and exercise the dispatch path directly. No vite
 * build pipeline; we're testing the runtime contract.
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

/**
 * Materialize the registry source as a real ESM module so we can
 * dynamically import it. The source is closure-private; rolldown
 * normally inlines it into the SSR bundle, but for testing we just
 * import it directly.
 */
async function loadRegistry(): Promise<{
  mod: RegistryModule;
  cleanup: () => Promise<void>;
}> {
  const dir = await mkdtemp(join(tmpdir(), "zsrpc-"));
  const path = join(dir, "registry.mjs");
  await writeFile(path, RPC_REGISTRY_SOURCE, "utf8");
  const mod = (await import(pathToFileURL(path).href)) as RegistryModule;
  return { mod, cleanup: () => rm(dir, { recursive: true, force: true }) };
}

/**
 * Tiny Zod-compatible schema: any object with `.parse()` qualifies. We
 * don't depend on Zod here because the registry treats schemas
 * structurally — anything with `.parse()` (Valibot, custom, real Zod)
 * works the same.
 *
 * The error shape mirrors ZodError: `{ name: "ZodError", issues: [...] }`.
 */
function fakeZod<T>(check: (input: unknown) => T): { parse: (i: unknown) => T } {
  return {
    parse(input: unknown): T {
      try {
        return check(input);
      } catch (e) {
        const err = new Error("Validation failed") as Error & {
          name: string;
          issues: unknown[];
        };
        err.name = "ZodError";
        err.issues = [{ path: [], message: (e as Error).message }];
        throw err;
      }
    },
  };
}

describe("synthetic-entry — Zod-direct validation", () => {
  test("valid input passes through to handler; result returned unchanged", async () => {
    const { mod, cleanup } = await loadRegistry();
    try {
      const inputSchema = fakeZod((i: unknown) => {
        if (typeof i !== "object" || i === null) throw new Error("not object");
        return i as { limit: number };
      });
      async function listTodos(input: { limit: number }) {
        return [{ id: 1, limit: input.limit }];
      }
      (listTodos as { config?: unknown }).config = { input: inputSchema };
      mod._zsRegister("listTodos", listTodos as never);

      const result = await mod.dispatch("listTodos", [{ limit: 10 }]);
      assert.deepEqual(result, [{ id: 1, limit: 10 }]);
    } finally {
      await cleanup();
    }
  });

  test("invalid input throws INVALID_ARGUMENT with status 400 + Zod issues", async () => {
    const { mod, cleanup } = await loadRegistry();
    try {
      const inputSchema = fakeZod((i: unknown) => {
        if (typeof (i as { limit: unknown })?.limit !== "number") {
          throw new Error("limit must be number");
        }
        return i;
      });
      async function listTodos(_: unknown) {
        return [];
      }
      (listTodos as { config?: unknown }).config = { input: inputSchema };
      mod._zsRegister("listTodos", listTodos as never);

      await assert.rejects(
        mod.dispatch("listTodos", [{ limit: "not-a-number" }]),
        (err: unknown) => {
          const e = err as {
            status?: number;
            code?: string;
            details?: { issues?: unknown[] };
          };
          assert.equal(e.status, 400, "status 400");
          assert.equal(e.code, "INVALID_ARGUMENT", "code INVALID_ARGUMENT");
          assert.ok(
            Array.isArray(e.details?.issues),
            "details.issues is an array",
          );
          assert.ok(
            (e.details!.issues as unknown[]).length > 0,
            "at least one issue",
          );
          return true;
        },
      );
    } finally {
      await cleanup();
    }
  });

  test("parsed (transformed) input replaces argv[0] before handler runs", async () => {
    // Zod's `.transform()` lets schemas change the input. The dispatch
    // must substitute the parsed value back into argv[0] so transforms
    // reach the handler.
    const { mod, cleanup } = await loadRegistry();
    try {
      const inputSchema = fakeZod((i: unknown) => {
        const obj = i as { x: number };
        return { x: obj.x * 2 };
      });
      async function double(input: { x: number }) {
        return input.x;
      }
      (double as { config?: unknown }).config = { input: inputSchema };
      mod._zsRegister("double", double as never);

      const result = await mod.dispatch("double", [{ x: 5 }]);
      assert.equal(result, 10, "transformed input reaches handler");
    } finally {
      await cleanup();
    }
  });

  test("non-Zod errors thrown by parse propagate untouched", async () => {
    // If `.parse()` throws something that isn't ZodError-shaped, we
    // rethrow it — don't synthesize an INVALID_ARGUMENT envelope.
    const { mod, cleanup } = await loadRegistry();
    try {
      const inputSchema = {
        parse(_: unknown): never {
          throw new TypeError("unrelated bug");
        },
      };
      async function f(_: unknown) {
        return null;
      }
      (f as { config?: unknown }).config = { input: inputSchema };
      mod._zsRegister("f", f as never);

      await assert.rejects(mod.dispatch("f", [{}]), (err: unknown) => {
        const e = err as Error & { code?: string };
        assert.ok(e instanceof TypeError, "TypeError survives");
        assert.equal(e.code, undefined, "no INVALID_ARGUMENT envelope");
        return true;
      });
    } finally {
      await cleanup();
    }
  });
});
