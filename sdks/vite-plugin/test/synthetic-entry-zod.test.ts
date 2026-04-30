/**
 * Zod-direct validation in the synthetic SSR entry's `_zsRpc` dispatcher.
 *
 * Procedures opt into runtime input/output validation by setting
 * `fn.config.input` / `fn.config.output` to a Zod-shaped schema (any
 * object with a `.parse()` method). The synthetic entry's `default.rpc`
 * (and the wrapping `default.fetch` for /_zs/v1/<id>) calls
 * `cfg.input.parse(input)` before invoking the handler; failures throw
 * an `INVALID_ARGUMENT` error envelope (status 400, code
 * "INVALID_ARGUMENT", details.issues = ZodError.issues).
 *
 * These tests build a synthetic-entry source via `buildServerEntrySource`,
 * write it + a stub user module to disk, dynamically import the
 * synthetic entry, and exercise `default.rpc(name, input, ctx)` directly.
 * No vite build pipeline; we're testing the runtime contract.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { writeFile, mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";

import { buildServerEntrySource } from "../src/rpc-registry.js";

interface StubProcedure {
  name: string;
  // Avoid type-arg gymnastics — handlers are tested through the entry's
  // `default.rpc` shim, which type-erases the args anyway.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  fn: (...args: any[]) => any;
  config?: Record<string, unknown>;
}

/**
 * Materialize a synthetic SSR entry with a stub user module exposing
 * the given procedures. Returns the entry's `default.{fetch, rpc}`.
 */
async function loadEntry(procedures: StubProcedure[]): Promise<{
  rpc: (name: string, input?: unknown, ctx?: unknown) => Promise<unknown>;
  fetch: (req: Request) => Promise<Response>;
  cleanup: () => Promise<void>;
}> {
  const dir = await mkdtemp(join(tmpdir(), "zsrpc-"));

  // Stash impls + configs on globalThis so the user module can reach
  // them without having to inline closures into source.
  const userKey = `__zs_user_${dir.replace(/[^a-zA-Z0-9]/g, "_")}`;
  const stash: Record<string, { fn: (...args: unknown[]) => unknown; config?: Record<string, unknown> }> = {};
  for (const p of procedures) stash[p.name] = { fn: p.fn, config: p.config };
  (globalThis as unknown as Record<string, unknown>)[userKey] = stash;

  // User module — exports each procedure as a plain function with its
  // .config attached. Stash key looked up via globalThis at module
  // evaluation time.
  const userPath = join(dir, "user.mjs");
  const exportsSrc = procedures
    .map(
      (p) =>
        `export const ${p.name} = Object.assign(
           function (...args) { return globalThis[${JSON.stringify(userKey)}][${JSON.stringify(p.name)}].fn.apply(null, args); },
           ${p.config ? `{ config: globalThis[${JSON.stringify(userKey)}][${JSON.stringify(p.name)}].config }` : "{}"}
         );`,
    )
    .join("\n");
  await writeFile(userPath, exportsSrc, "utf8");

  // Synthetic entry generated for this stub.
  const entrySrc = buildServerEntrySource({
    userEntryRel: pathToFileURL(userPath).href,
    procedures: procedures.map((p) => ({
      filePath: pathToFileURL(userPath).href,
      exportName: p.name,
      wireId: (typeof p.config?.id === "string" && p.config.id) || p.name,
    })),
  });
  const entryPath = join(dir, "entry.mjs");
  await writeFile(entryPath, entrySrc, "utf8");

  const mod = (await import(pathToFileURL(entryPath).href)) as {
    default: {
      rpc: (name: string, input?: unknown, ctx?: unknown) => Promise<unknown>;
      fetch: (req: Request) => Promise<Response>;
    };
  };
  return {
    rpc: mod.default.rpc,
    fetch: mod.default.fetch,
    cleanup: async () => {
      delete (globalThis as unknown as Record<string, unknown>)[userKey];
      await rm(dir, { recursive: true, force: true });
    },
  };
}

/**
 * Tiny Zod-compatible schema: any object with `.parse()` qualifies. We
 * don't depend on Zod here because the synthetic entry treats schemas
 * structurally — anything with `.parse()` (Valibot, custom, real Zod)
 * works the same.
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

describe("synthetic-entry _zsRpc — Zod-direct validation", () => {
  test("valid input passes through; result returned unchanged", async () => {
    const inputSchema = fakeZod((i: unknown) => {
      if (typeof i !== "object" || i === null) throw new Error("not object");
      return i as { limit: number };
    });
    const { rpc, cleanup } = await loadEntry([
      {
        name: "listTodos",
        async fn(input: { limit: number }) {
          return [{ id: 1, limit: input.limit }];
        },
        config: { input: inputSchema },
      },
    ]);
    try {
      const result = await rpc("listTodos", { limit: 10 });
      assert.deepEqual(result, [{ id: 1, limit: 10 }]);
    } finally {
      await cleanup();
    }
  });

  test("invalid input throws INVALID_ARGUMENT with status 400 + Zod issues", async () => {
    const inputSchema = fakeZod((i: unknown) => {
      if (typeof (i as { limit: unknown })?.limit !== "number") {
        throw new Error("limit must be number");
      }
      return i;
    });
    const { rpc, cleanup } = await loadEntry([
      {
        name: "listTodos",
        async fn() {
          return [];
        },
        config: { input: inputSchema },
      },
    ]);
    try {
      await assert.rejects(
        rpc("listTodos", { limit: "not-a-number" }),
        (err: unknown) => {
          const e = err as {
            status?: number;
            code?: string;
            details?: { issues?: unknown[] };
          };
          assert.equal(e.status, 400);
          assert.equal(e.code, "INVALID_ARGUMENT");
          assert.ok(Array.isArray(e.details?.issues));
          assert.ok((e.details!.issues as unknown[]).length > 0);
          return true;
        },
      );
    } finally {
      await cleanup();
    }
  });

  test("parsed (transformed) input replaces input arg before handler runs", async () => {
    // Zod's `.transform()` lets schemas change the input. The dispatcher
    // must substitute the parsed value back so transforms reach the
    // handler.
    const inputSchema = fakeZod((i: unknown) => {
      const obj = i as { x: number };
      return { x: obj.x * 2 };
    });
    const { rpc, cleanup } = await loadEntry([
      {
        name: "double",
        async fn(input: { x: number }) {
          return input.x;
        },
        config: { input: inputSchema },
      },
    ]);
    try {
      const result = await rpc("double", { x: 5 });
      assert.equal(result, 10);
    } finally {
      await cleanup();
    }
  });

  test("missing procedure throws 404 NOT_FOUND", async () => {
    const { rpc, cleanup } = await loadEntry([]);
    try {
      await assert.rejects(rpc("doesNotExist", null), (err: unknown) => {
        const e = err as { status?: number; code?: string; message?: string };
        assert.equal(e.status, 404);
        assert.equal(e.code, "NOT_FOUND");
        assert.match(String(e.message), /Method not found/);
        return true;
      });
    } finally {
      await cleanup();
    }
  });
});
