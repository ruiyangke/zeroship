/**
 * Level 1 trust model: procedures without `fn.config.input` skip
 * validation. The synthetic entry's `_zsRpc` dispatcher passes input
 * directly to the handler — no Zod call, no envelope rewriting.
 *
 * Spec: `docs/proposals/rpc-v2.md` §4 — "Procedures without schemas
 * pass arguments through unchecked. Typed clients are expected to
 * send well-formed data; if you can't trust the caller, declare a
 * schema."
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
  const dir = await mkdtemp(join(tmpdir(), "zspass-"));
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

describe("synthetic-entry _zsRpc — Level 1 passthrough", () => {
  test("no fn.config: handler receives input verbatim", async () => {
    let received: unknown = null;
    const { rpc, cleanup } = await loadEntry([
      {
        name: "listTodos",
        async fn(input: unknown) {
          received = input;
          return ["ok"];
        },
      },
    ]);
    try {
      // Even a "wrong" shape: caller sent a string. Dispatch hands it
      // through; the handler is responsible.
      const result = await rpc("listTodos", "not-an-object");
      assert.equal(received, "not-an-object");
      assert.deepEqual(result, ["ok"]);
    } finally {
      await cleanup();
    }
  });

  test("fn.config without input: still passthrough", async () => {
    // Procedures may carry `fn.config = { id: "..." }` for wireId
    // pinning without declaring a schema. Validation must remain off.
    let received: unknown = null;
    const { rpc, cleanup } = await loadEntry([
      {
        name: "getUser",
        async fn(input: unknown) {
          received = input;
          return null;
        },
        config: { id: "users.get" },
      },
    ]);
    try {
      await rpc("users.get", { id: 42 });
      assert.deepEqual(received, { id: 42 });
    } finally {
      await cleanup();
    }
  });

  test("fn.config.input set to non-parseable value: passthrough", async () => {
    // Defensive: if a user mistakenly sets `input: 123` instead of a
    // schema, dispatch must NOT call `.parse()` (it would crash). Skip
    // validation gracefully.
    let received: unknown = null;
    const { rpc, cleanup } = await loadEntry([
      {
        name: "f",
        async fn(input: unknown) {
          received = input;
          return null;
        },
        config: { input: 123 },
      },
    ]);
    try {
      await rpc("f", { ok: true });
      assert.deepEqual(received, { ok: true });
    } finally {
      await cleanup();
    }
  });
});
