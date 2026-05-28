import { test, expect, type APIRequestContext } from "@playwright/test";

const CONTROL_URL = process.env.CONTROL_URL ?? "http://localhost:9090";

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

async function rpcPost<T>(
  request: APIRequestContext,
  id: string,
  input: unknown,
): Promise<T> {
  const res = await request.post(`/_zs/v1/${id}`, {
    data: { json: input },
    headers: { accept: "application/json" },
  });
  if (!res.ok()) {
    throw new Error(`${id} failed: ${res.status()} ${await res.text()}`);
  }
  const envelope = (await res.json()) as { json?: T };
  expect(envelope).toHaveProperty("json");
  return envelope.json as T;
}

test.describe("RPC wire capability wrappers", () => {
  test("query procedure round-trips over /_zs/v1/<id>", async ({ request }) => {
    // Pre-migration this returned 404 because plain `export async function`
    // declarations were no longer registered as RPC procedures.
    const result = await rpcPost<{ overall: string; dimensions: Array<{ key: string }> }>(
      request,
      "agents.quality.get",
      { appId: "rpc-wire-regression" },
    );

    expect(result.overall).toMatch(/^[A-F]/);
    expect(result.dimensions.map((d) => d.key)).toContain("correctness");
  });

  test("control-plane proxy action round-trips over /_zs/v1/<id>", async ({ request }) => {
    const controlUp = await probe(`${CONTROL_URL}/health`);
    test.skip(
      !controlUp,
      `control plane unreachable at ${CONTROL_URL}; start it to exercise apps.list`,
    );

    // This endpoint must be `action`: it calls fetch() to proxy to the
    // control plane. If it is migrated as query/mutation, the runtime
    // capability gate rejects the fetch before the control request leaves.
    const apps = await rpcPost<unknown[]>(request, "apps.list", null);
    expect(Array.isArray(apps)).toBe(true);
  });
});
