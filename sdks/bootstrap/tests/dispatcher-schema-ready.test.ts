/**
 * ISS-66 regression — `__zsDispatch` MUST await `globalThis.__zsSchemaReady`
 * before running a procedure.
 *
 * The production `runtime-entry` installs the Collection wrappers
 * synchronously but defers the async DDL chain to `__zsSchemaReady`
 * (awaiting it at module top-level leaves the bootstrap module's
 * evaluation pending and 404s the dispatch). The dispatcher is the gate
 * that ensures a handler doesn't race `registerModel`. This test pins
 * that gate: the procedure body must not execute until `__zsSchemaReady`
 * settles, and a rejected chain must surface through the dispatch.
 */

import { test, describe, afterEach } from "node:test";
import assert from "node:assert/strict";

// Importing the dispatcher installs `globalThis.__zsDispatch` (idempotent
// IIFE). The post-build strips the `export {}`; the source form is a
// module, so importing it runs the install side-effect.
import "../src/dispatcher.js";

type Dispatch = (
  rpc: Record<string, unknown>,
  name: string,
  input: unknown,
  ctx: unknown,
) => Promise<unknown>;

const g = globalThis as unknown as {
  __zsDispatch: Dispatch;
  __zsSchemaReady?: Promise<unknown>;
};

afterEach(() => {
  delete g.__zsSchemaReady;
});

describe("ISS-66 — __zsDispatch awaits __zsSchemaReady", () => {
  test("procedure body does not run until __zsSchemaReady settles", async () => {
    let resolveReady!: () => void;
    g.__zsSchemaReady = new Promise<void>((r) => { resolveReady = r; });

    let ran = false;
    const rpc = {
      ping: () => { ran = true; return "pong"; },
    };

    const p = g.__zsDispatch(rpc, "ping", {}, {});
    // Yield a few microtasks: the gate must still be holding (ready not settled).
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(ran, false, "procedure ran before schema-ready settled");

    resolveReady();
    const out = await p;
    assert.equal(ran, true, "procedure did not run after schema-ready settled");
    assert.equal(out, "pong");
  });

  test("a rejected __zsSchemaReady surfaces through the dispatch", async () => {
    g.__zsSchemaReady = Promise.reject(new Error("DDL boom"));

    let ran = false;
    const rpc = { ping: () => { ran = true; return "pong"; } };

    await assert.rejects(
      () => g.__zsDispatch(rpc, "ping", {}, {}),
      /DDL boom/,
    );
    assert.equal(ran, false, "procedure must not run when schema-ready rejected");
  });

  test("schema-less apps (no __zsSchemaReady) dispatch normally", async () => {
    const rpc = { ping: () => "pong" };
    const out = await g.__zsDispatch(rpc, "ping", {}, {});
    assert.equal(out, "pong");
  });
});
