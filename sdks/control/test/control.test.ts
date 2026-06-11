import assert from "node:assert/strict";
import test from "node:test";

import {
  ControlError,
  createControlClient,
  type ControlClientOptions,
} from "../src/index.ts";

test("apps.create sends bearer auth and JSON", async () => {
  const requests: Request[] = [];
  const client = createControlClient({
    baseUrl: "http://control.local",
    auth: "master-key",
    fetch: async (input, init) => {
      requests.push(new Request(input, init));
      return json({ id: "app_1", name: "demo" }, 201);
    },
  });

  await client.apps.create({ name: "demo" });

  assert.equal(requests.length, 1);
  const req = requests[0]!;
  assert.equal(req.method, "POST");
  assert.equal(req.url, "http://control.local/api/apps");
  assert.equal(req.headers.get("authorization"), "Bearer master-key");
  assert.equal(req.headers.get("content-type"), "application/json");
  assert.deepEqual(await req.json(), { name: "demo", plan_id: "free" });
});

test("request forwards cookies and mirrors set-cookie", async () => {
  const mirrored: string[] = [];
  const client = createControlClient({
    baseUrl: "http://control.local/",
    cookie: () => "a=b",
    onSetCookie: (cookie) => {
      mirrored.push(cookie);
    },
    fetch: async (input, init) => {
      const req = new Request(input, init);
      assert.equal(req.headers.get("cookie"), "a=b");
      return json({ id: "app_1", name: "demo" }, 200, {
        "set-cookie": "session=token; HttpOnly",
      });
    },
  });

  const result = await client.apps.get("app_1");

  assert.equal(result.id, "app_1");
  assert.deepEqual(mirrored, ["session=token; HttpOnly"]);
});

test("env mutations treat 204 as void", async () => {
  const seen: string[] = [];
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async (input, init) => {
      const req = new Request(input, init);
      seen.push(`${req.method} ${new URL(req.url).pathname}`);
      return new Response(null, { status: 204 });
    },
  });

  assert.equal(
    await client.env.setVar("app 1", { key: "FOO", value: "bar" }),
    undefined,
  );
  assert.equal(await client.env.deleteSecret("app 1", "SECRET/1"), undefined);
  assert.deepEqual(seen, [
    "POST /api/apps/app%201/vars",
    "DELETE /api/apps/app%201/secrets/SECRET%2F1",
  ]);
});

test("deploy sends binary artifact as application/x-zship", async () => {
  const artifact = new Uint8Array([1, 2, 3]);
  let request: Request | undefined;
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async (input, init) => {
      request = new Request(input, init);
      return json({ deploy_hash: "sha256:abc", blobs_uploaded: 1 }, 200);
    },
  });

  const result = await client.apps.deploy("app_1", artifact);

  assert.equal(result.deploy_hash, "sha256:abc");
  assert.equal(request?.method, "POST");
  assert.equal(request?.headers.get("content-type"), "application/x-zship");
  assert.deepEqual(new Uint8Array(await request!.arrayBuffer()), artifact);
});

test("request builds query strings and supports raw text", async () => {
  const client = createControlClient({
    baseUrl: "http://control.local/root",
    fetch: async (input) => {
      const url = new URL(String(input));
      assert.equal(url.pathname, "/audit");
      assert.equal(url.searchParams.get("limit"), "25");
      assert.equal(url.searchParams.has("empty"), false);
      return new Response("ok", {
        headers: { "content-type": "text/plain" },
      });
    },
  });

  const text = await client.request<string>("/audit", {
    query: { limit: 25, empty: undefined },
    parseAs: "text",
  });

  assert.equal(text, "ok");
});

test("non-2xx responses throw ControlError with parsed body", async () => {
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () =>
      json({ error: "unauthorized", code: "auth_failed" }, 401),
  });

  await assert.rejects(
    client.apps.logs("app_1"),
    (error: unknown) => {
      assert.ok(error instanceof ControlError);
      assert.equal(error.status, 401);
      assert.equal(error.code, "auth_failed");
      assert.equal(error.message, "unauthorized");
      assert.deepEqual(error.body, {
        error: "unauthorized",
        code: "auth_failed",
      });
      return true;
    },
  );
});

function json(
  body: unknown,
  status = 200,
  headers: HeadersInit = {},
): Response {
  const merged = new Headers(headers);
  merged.set("content-type", "application/json");
  return new Response(JSON.stringify(body), { status, headers: merged });
}

void ({} satisfies Partial<ControlClientOptions>);
