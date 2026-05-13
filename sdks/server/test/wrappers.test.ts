// Wrapper helpers — `procedure` / `query` / `mutation` / `stream` /
// `subscription`. These are the explicit opt-in markers the transform
// looks for when deciding which exports become RPC procedures.
//
// At runtime the wrappers are identity functions: the value returned IS
// the handler, with `.config` (when supplied) and a non-enumerable
// `__zsKind` tag attached. The vite-plugin transform reads kind
// statically from the wrapper name; the runtime tag is a belt-and-
// suspenders fallback for any code path that needs to dispatch on kind
// without re-reading the AST.

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import {
  procedure,
  query,
  mutation,
  action,
  stream,
  subscription,
} from "../src/wrappers.js";

describe("wrappers — identity + kind tagging", () => {
  test("procedure() returns the handler unchanged (callable, same return value)", async () => {
    const impl = async (n: number) => n + 1;
    const wrapped = procedure(impl);
    // Wrapped IS the handler — no proxy, no `.bind`, no `.call` indirection.
    assert.equal(typeof wrapped, "function");
    assert.equal(await wrapped(5), 6);
  });

  test("procedure() with config attaches config but NOT a kind (let transform infer)", () => {
    const impl = async (n: number) => n;
    const wrapped = procedure(impl, { id: "myProc", idempotent: true }) as typeof impl & {
      config?: Record<string, unknown>;
      __zsKind?: string;
    };
    assert.equal(wrapped.config?.id, "myProc");
    assert.equal(wrapped.config?.idempotent, true);
    // Generic procedure() leaves kind unset; the transform's name-based
    // heuristic picks query/mutation/stream.
    assert.equal(wrapped.config?.kind, undefined);
    // Runtime fallback tag is "procedure" for the generic marker.
    assert.equal(wrapped.__zsKind, "procedure");
  });

  test("query() implies kind: 'query' on config", () => {
    const impl = async () => [];
    const wrapped = query(impl) as typeof impl & {
      config?: { kind?: string };
      __zsKind?: string;
    };
    assert.equal(wrapped.config?.kind, "query");
    assert.equal(wrapped.__zsKind, "query");
  });

  test("mutation() implies kind: 'mutation' on config", () => {
    const impl = async () => true;
    const wrapped = mutation(impl) as typeof impl & {
      config?: { kind?: string };
    };
    assert.equal(wrapped.config?.kind, "mutation");
  });

  test("stream() implies kind: 'stream' on config", () => {
    const impl = async function* () { yield 1; };
    const wrapped = stream(impl) as typeof impl & {
      config?: { kind?: string };
    };
    assert.equal(wrapped.config?.kind, "stream");
  });

  test("subscription() implies kind: 'subscription' on config", () => {
    const impl = async function* () { yield 1; };
    const wrapped = subscription(impl) as typeof impl & {
      config?: { kind?: string };
    };
    assert.equal(wrapped.config?.kind, "subscription");
  });

  test("action() implies kind: 'action' on config (B3)", async () => {
    // Untyped handler shape to bypass capability ctx-typing here; the
    // ctx-shape enforcement is verified by the typecheck suite. This
    // test just verifies runtime kind tagging.
    const impl = async (_args: unknown, _ctx: unknown) => "done";
    const wrapped = action(impl) as typeof impl & {
      config?: { kind?: string };
      __zsKind?: string;
    };
    assert.equal(wrapped.config?.kind, "action");
    assert.equal(wrapped.__zsKind, "action");
    // Identity: wrapping doesn't change the return value.
    assert.equal(await wrapped("x", {}), "done");
  });

  test("action() with config — id propagates, kind defaults to 'action'", () => {
    const wrapped = action(
      async (_args: unknown, _ctx: unknown) => null,
      { id: "send-email" },
    ) as ((args: unknown, ctx: unknown) => Promise<null>) & {
      config?: { id?: string; kind?: string };
    };
    assert.equal(wrapped.config?.id, "send-email");
    assert.equal(wrapped.config?.kind, "action");
  });

  test("explicit config.kind overrides wrapper default", () => {
    // procedure(fn, { kind: "stream" }) → kind: "stream" wins.
    const impl = async function* () { yield 1; };
    const wrapped = procedure(impl, { kind: "stream", id: "x" }) as typeof impl & {
      config?: { kind?: string };
    };
    assert.equal(wrapped.config?.kind, "stream");
  });

  test("query(fn, { kind: 'mutation' }) — explicit kind beats the wrapper", () => {
    const impl = async () => null;
    const wrapped = query(impl, { kind: "mutation" }) as typeof impl & {
      config?: { kind?: string };
    };
    assert.equal(wrapped.config?.kind, "mutation");
  });

  test("__zsKind is non-enumerable (no leak via Object.keys)", () => {
    const wrapped = query(async () => null);
    const keys = Object.keys(wrapped);
    assert.ok(!keys.includes("__zsKind"), "__zsKind not enumerable");
  });

  test("legacy `<fn>.config = { ... }` shape still works alongside wrappers", () => {
    // The transform recognizes both shapes; the wrapper is the canonical
    // form for new code, but the legacy assignment must keep parsing.
    async function legacy() { return 1; }
    (legacy as unknown as { config: Record<string, unknown> }).config = {
      id: "legacy",
      kind: "query",
    };
    assert.equal((legacy as unknown as { config: { id: string } }).config.id, "legacy");
  });
});
