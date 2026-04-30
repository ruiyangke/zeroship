/**
 * Level 1 trust model: procedures without `fn.config.input` skip
 * validation. The dispatch function passes input directly to the
 * handler — no Zod call, no envelope rewriting, no surprises.
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
  const dir = await mkdtemp(join(tmpdir(), "zspass-"));
  const path = join(dir, "registry.mjs");
  await writeFile(path, RPC_REGISTRY_SOURCE, "utf8");
  const mod = (await import(pathToFileURL(path).href)) as RegistryModule;
  return { mod, cleanup: () => rm(dir, { recursive: true, force: true }) };
}

describe("synthetic-entry — Level 1 passthrough", () => {
  test("no fn.config: handler receives input verbatim", async () => {
    const { mod, cleanup } = await loadRegistry();
    try {
      let received: unknown = null;
      async function listTodos(input: unknown) {
        received = input;
        return ["ok"];
      }
      mod._zsRegister("listTodos", listTodos as never);

      // Even a "wrong" shape: caller sent a string. Dispatch hands it
      // through; the handler is responsible.
      const result = await mod.dispatch("listTodos", ["not-an-object"]);
      assert.equal(received, "not-an-object", "input passed through verbatim");
      assert.deepEqual(result, ["ok"]);
    } finally {
      await cleanup();
    }
  });

  test("fn.config without input: still passthrough", async () => {
    // Procedures may carry `fn.config = { id: "..." }` for wireId
    // pinning without declaring a schema. Validation must remain off.
    const { mod, cleanup } = await loadRegistry();
    try {
      let received: unknown = null;
      async function getUser(input: unknown) {
        received = input;
        return null;
      }
      (getUser as { config?: unknown }).config = { id: "users.get" };
      mod._zsRegister("getUser", getUser as never);

      await mod.dispatch("getUser", [{ id: 42 }]);
      assert.deepEqual(received, { id: 42 }, "input untouched");
    } finally {
      await cleanup();
    }
  });

  test("fn.config.input set to non-parseable value: passthrough", async () => {
    // Defensive: if a user mistakenly sets `input: 123` instead of a
    // schema, dispatch must NOT call `.parse()` (it would crash). Skip
    // validation gracefully.
    const { mod, cleanup } = await loadRegistry();
    try {
      let received: unknown = null;
      async function f(input: unknown) {
        received = input;
        return null;
      }
      (f as { config?: unknown }).config = { input: 123 };
      mod._zsRegister("f", f as never);

      await mod.dispatch("f", [{ ok: true }]);
      assert.deepEqual(received, { ok: true });
    } finally {
      await cleanup();
    }
  });
});
