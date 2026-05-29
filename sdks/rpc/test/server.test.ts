import { test, describe } from "node:test";
import assert from "node:assert/strict";

import {
  procedure,
  query,
  mutation,
  action,
  stream,
  streamResponse,
  subscription,
} from "../src/server.js";

describe("@zeroship/rpc/server wrappers", () => {
  test("procedure() returns the handler unchanged", async () => {
    const impl = async (n: number) => n + 1;
    const wrapped = procedure(impl);
    assert.equal(wrapped, impl);
    assert.equal(await wrapped(5), 6);
  });

  test("procedure() with config attaches config but not an inferred kind", () => {
    const impl = async (n: number) => n;
    const wrapped = procedure(impl, { id: "myProc", idempotent: true }) as typeof impl & {
      config?: Record<string, unknown>;
      __zsKind?: string;
    };
    assert.equal(wrapped.config?.id, "myProc");
    assert.equal(wrapped.config?.idempotent, true);
    assert.equal(wrapped.config?.kind, undefined);
    assert.equal(wrapped.__zsKind, "procedure");
  });

  test("typed wrappers imply their kind", () => {
    const q = query(async () => []);
    const m = mutation(async () => true);
    const a = action(async () => "done");
    const s = stream(async function* () { yield 1; });
    const sub = subscription(async function* () { yield 1; });

    assert.equal(q.config?.kind, "query");
    assert.equal(m.config?.kind, "mutation");
    assert.equal(a.config?.kind, "action");
    assert.equal(s.config?.kind, "stream");
    assert.equal(sub.config?.kind, "subscription");
  });

  test("streamResponse() marks raw Response handlers as stream procedures", async () => {
    const wrapped = streamResponse(
      async (input: { ok: boolean }) => Response.json(input),
      { id: "raw.stream" },
    );

    const res = await wrapped({ ok: true });
    assert.equal(wrapped.config?.id, "raw.stream");
    assert.equal(wrapped.config?.kind, "stream");
    assert.equal(res.status, 200);
    assert.deepEqual(await res.json(), { ok: true });
  });

  test("explicit config.kind overrides wrapper default", () => {
    const wrapped = query(async () => null, { kind: "mutation" });
    assert.equal(wrapped.config?.kind, "mutation");
  });

  test("__zsKind is non-enumerable", () => {
    const wrapped = query(async () => null) as typeof query & { __zsKind?: string };
    assert.equal((wrapped as unknown as { __zsKind?: string }).__zsKind, "query");
    assert.equal(Object.keys(wrapped).includes("__zsKind"), false);
  });
});
