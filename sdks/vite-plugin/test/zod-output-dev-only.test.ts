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

import { buildServerEntrySource } from "../src/rpc-registry.js";

interface StubProcedure {
  name: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  fn: (...args: any[]) => any;
  config?: Record<string, unknown>;
}

async function loadEntry(procedures: StubProcedure[]): Promise<{
  rpc: (name: string, input?: unknown, ctx?: unknown) => Promise<unknown>;
  cleanup: () => Promise<void>;
}> {
  const dir = await mkdtemp(join(tmpdir(), "zsout-"));
  const userKey = `__zs_user_${dir.replace(/[^a-zA-Z0-9]/g, "_")}`;
  const stash: Record<string, { fn: (...args: unknown[]) => unknown; config?: Record<string, unknown> }> = {};
  for (const p of procedures) stash[p.name] = { fn: p.fn, config: p.config };
  (globalThis as unknown as Record<string, unknown>)[userKey] = stash;

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
    default: { rpc: (name: string, input?: unknown, ctx?: unknown) => Promise<unknown> };
  };
  return {
    rpc: mod.default.rpc,
    cleanup: async () => {
      delete (globalThis as unknown as Record<string, unknown>)[userKey];
      await rm(dir, { recursive: true, force: true });
    },
  };
}

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

describe("synthetic-entry _zsRpc — output validation gating", () => {
  test("dev mode: output schema runs and surfaces INTERNAL on mismatch", async () => {
    const { rpc, cleanup } = await loadEntry([
      {
        name: "f",
        async fn() {
          return { wrong: "shape" };
        },
        config: { output: FAILING_OUTPUT_SCHEMA },
      },
    ]);
    try {
      await withNodeEnv("development", async () => {
        // `async () =>` form — _zsRpc is sync, so output validation throws
        // synchronously when the handler is sync. (For async handlers,
        // the throw happens inside .then and rejects.)
        await assert.rejects(async () => rpc("f", null), (err: unknown) => {
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

  test("production: output schema is skipped; handler result returned", async () => {
    const { rpc, cleanup } = await loadEntry([
      {
        name: "f",
        async fn() {
          return { wrong: "shape" };
        },
        config: { output: FAILING_OUTPUT_SCHEMA },
      },
    ]);
    try {
      await withNodeEnv("production", async () => {
        const result = await rpc("f", null);
        assert.deepEqual(result, { wrong: "shape" });
      });
    } finally {
      await cleanup();
    }
  });

  test("undefined NODE_ENV: behaves as production (validation skipped)", async () => {
    // Secure-by-default: only NODE_ENV=development opts INTO validation.
    // Unset / anything else is treated as production. Aligns with the
    // V8 worker runtime where process.env is per-app and rarely carries
    // NODE_ENV at all.
    const { rpc, cleanup } = await loadEntry([
      {
        name: "f",
        async fn() {
          return null;
        },
        config: { output: FAILING_OUTPUT_SCHEMA },
      },
    ]);
    try {
      await withNodeEnv(undefined, async () => {
        const result = await rpc("f", null);
        assert.equal(result, null);
      });
    } finally {
      await cleanup();
    }
  });

  test("input validation ALWAYS runs (regardless of NODE_ENV)", async () => {
    // Output is dev-only; input is always-on.
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
    const { rpc, cleanup } = await loadEntry([
      {
        name: "f",
        async fn() {
          return null;
        },
        config: { input: FAILING_INPUT },
      },
    ]);
    try {
      await withNodeEnv("production", async () => {
        // `async () =>` form — input validation throws sync from _zsRpc.
        await assert.rejects(async () => rpc("f", {}), (err: unknown) => {
          const e = err as { status?: number; code?: string };
          assert.equal(e.status, 400);
          assert.equal(e.code, "INVALID_ARGUMENT");
          return true;
        });
      });
    } finally {
      await cleanup();
    }
  });
});
