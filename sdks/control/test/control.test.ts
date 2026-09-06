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

test("apps archive and unarchive use the idempotent archive resource", async () => {
  const requests: Request[] = [];
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async (input, init) => {
      const request = new Request(input, init);
      requests.push(request);
      return json({
        id: "app_1",
        name: "demo",
        plan_id: "free",
        deploy_hash: null,
        archived_at:
          request.method === "PUT" ? "2026-08-31T12:00:00Z" : null,
        created_at: "2026-08-31T10:00:00Z",
        updated_at: "2026-08-31T12:00:00Z",
      });
    },
  });

  const archived = await client.apps.archive("app 1");
  const restored = await client.apps.unarchive("app 1");

  assert.equal(archived.archived_at, "2026-08-31T12:00:00Z");
  assert.equal(restored.archived_at, null);
  assert.deepEqual(
    requests.map(
      (request) => `${request.method} ${new URL(request.url).pathname}`,
    ),
    [
      "PUT /api/apps/app%201/archive",
      "DELETE /api/apps/app%201/archive",
    ],
  );
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

test("workflow helpers send app scope headers and JSON bodies", async () => {
  const requests: Request[] = [];
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async (input, init) => {
      requests.push(new Request(input, init));
      const path = new URL(String(input)).pathname;
      if (path.endsWith("/broadcast")) {
        return json({ id: "wbc_1", topic: "approvals" }, 202);
      }
      return json({ token: "wst_claim.sig", expiresAt: "2026-07-06T00:00:00Z" });
    },
  });

  await client.workflows.createSignalToken("run_1", {
    appId: "app_1",
    types: ["approved"],
    ttl: "PT5M",
  });
  await client.workflows.createTopicSignalToken("approvals", {
    appId: "app_1",
    types: ["approved"],
    ttl: "PT5M",
  });
  await client.workflows.publishTopic("approvals", {
    appId: "app_1",
    type: "approved",
    payload: { ok: true },
    idempotencyKey: "idem-1",
  });

  assert.deepEqual(
    requests.map((req) => `${req.method} ${new URL(req.url).pathname}`),
    [
      "POST /internal/workflows/runs/run_1/signal-token",
      "POST /internal/workflows/topics/approvals/signal-token",
      "POST /internal/workflows/topics/approvals/broadcast",
    ],
  );
  for (const req of requests) {
    assert.equal(req.headers.get("x-zeroship-app-id"), "app_1");
    assert.equal(req.headers.get("content-type"), "application/json");
  }
  assert.deepEqual(await requests[0]!.json(), {
    types: ["approved"],
    ttl: "PT5M",
  });
  assert.deepEqual(await requests[2]!.json(), {
    type: "approved",
    payload: { ok: true },
    idempotencyKey: "idem-1",
  });
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

// ---------------------------------------------------------------------
// trace_id -- the correlation id on an otherwise contentless body.
//
// `infrastructure_error_response` (crates/zeroship-control/src/api.rs) logs the
// real cause and returns `{"error":"internal error","trace_id":<uuid>}`.
// The cause is deliberately absent, so the id is the ONLY thing that
// makes the response reportable. The client used to drop it: it survived
// on `err.body` but had no field, so nothing surfaced it.
//
// WHAT THESE DO NOT CATCH:
//   - That the server sends it. That is `api.rs`'s own tests.
//   - That the id matches the one in the server's log line.
//   - Anything about the app-dispatch path. A creator app's 5xx comes
//     from the runtime rail, which emits a per-isolate counter and never
//     reaches this client.
// ---------------------------------------------------------------------

test("ControlError lifts trace_id off an infrastructure error body", async () => {
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () =>
      // Verbatim shape of `infrastructure_error_response`.
      json(
        {
          error: "internal error",
          trace_id: "3f2a1c88-9d4e-4f1b-8a02-6c5b7e9d0a11",
        },
        500,
      ),
  });

  await assert.rejects(client.apps.logs("app_1"), (error: unknown) => {
    assert.ok(error instanceof ControlError);
    assert.equal(error.status, 500);
    assert.equal(error.trace_id, "3f2a1c88-9d4e-4f1b-8a02-6c5b7e9d0a11");
    // Emitting the id is not permission to emit the cause.
    assert.equal(error.message, "internal error");
    assert.equal(error.code, undefined);
    return true;
  });
});

test("ControlError leaves trace_id undefined when the body omits it", async () => {
  // ONE-VARIABLE CONTROL for the test above: the SAME status and the SAME
  // body shape, minus the field. Without it, that test would also pass if
  // `trace_id` were defaulted to a constant.
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () => json({ error: "internal error" }, 500),
  });

  await assert.rejects(client.apps.logs("app_1"), (error: unknown) => {
    assert.ok(error instanceof ControlError);
    assert.equal(error.trace_id, undefined);
    return true;
  });
});

test("ControlError ignores a non-string trace_id", async () => {
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () => json({ error: "internal error", trace_id: 7 }, 500),
  });

  await assert.rejects(client.apps.logs("app_1"), (error: unknown) => {
    assert.ok(error instanceof ControlError);
    assert.equal(error.trace_id, undefined);
    return true;
  });
});

// ---------------------------------------------------------------------------
// organizations / projects
// ---------------------------------------------------------------------------

const ORG = "org_0123456789abcdefghijkl";
const PRJ = "prj_0123456789abcdefghijkl";
const IVT = "ivt_0123456789abcdefghijkl";
const USER = "11111111-2222-3333-4444-555555555555";

/**
 * The whole organization/project surface, stated as the request each method
 * makes. Method, path and body are the three things a mistake here sends
 * somewhere else, and every path segment carries a typed id or a UUID - the
 * server parses those before authorization runs, so a slug sent here is a 400
 * rather than a silent denial.
 *
 * Written as one table rather than one test per method so that the coverage
 * assertion below can rule on the SET: a method added to the client and
 * forgotten here fails, instead of being tested by nothing while every named
 * case still passes.
 */
const ORGANIZATION_CALLS: Array<{
  name: string;
  send: (client: ReturnType<typeof createControlClient>) => Promise<unknown>;
  method: string;
  path: string;
  body?: unknown;
}> = [
  {
    name: "organizations.list",
    send: (c) => c.organizations.list(),
    method: "GET",
    path: "/api/organizations",
  },
  {
    name: "organizations.create",
    send: (c) => c.organizations.create({ name: "Acme" }),
    method: "POST",
    path: "/api/organizations",
    body: { name: "Acme" },
  },
  {
    name: "organizations.get",
    send: (c) => c.organizations.get(ORG),
    method: "GET",
    path: `/api/organizations/${ORG}`,
  },
  {
    name: "organizations.update",
    send: (c) => c.organizations.update(ORG, { name: "Acme Inc" }),
    method: "PATCH",
    path: `/api/organizations/${ORG}`,
    body: { name: "Acme Inc" },
  },
  {
    name: "organizations.members",
    send: (c) => c.organizations.members(ORG),
    method: "GET",
    path: `/api/organizations/${ORG}/members`,
  },
  {
    name: "organizations.addMember",
    send: (c) => c.organizations.addMember(ORG, { user_id: USER, role: "developer" }),
    method: "POST",
    path: `/api/organizations/${ORG}/members`,
    body: { user_id: USER, role: "developer" },
  },
  {
    name: "organizations.changeMemberRole",
    send: (c) => c.organizations.changeMemberRole(ORG, USER, { role: "admin" }),
    method: "PATCH",
    path: `/api/organizations/${ORG}/members/${USER}`,
    body: { role: "admin" },
  },
  {
    name: "organizations.removeMember",
    send: (c) => c.organizations.removeMember(ORG, USER),
    method: "DELETE",
    path: `/api/organizations/${ORG}/members/${USER}`,
  },
  {
    name: "organizations.transferOwnership",
    send: (c) => c.organizations.transferOwnership(ORG, { user_id: USER }),
    method: "POST",
    path: `/api/organizations/${ORG}/transfer`,
    body: { user_id: USER },
  },
  {
    name: "organizations.invites",
    send: (c) => c.organizations.invites(ORG),
    method: "GET",
    path: `/api/organizations/${ORG}/invites`,
  },
  {
    name: "organizations.createInvite",
    send: (c) =>
      c.organizations.createInvite(ORG, { email: "a@b.test", role: "viewer" }),
    method: "POST",
    path: `/api/organizations/${ORG}/invites`,
    body: { email: "a@b.test", role: "viewer" },
  },
  {
    name: "organizations.revokeInvite",
    send: (c) => c.organizations.revokeInvite(ORG, IVT),
    method: "DELETE",
    path: `/api/organizations/${ORG}/invites/${IVT}`,
  },
  {
    name: "organizations.redeemInvite",
    send: (c) => c.organizations.redeemInvite({ token: "tok" }),
    method: "POST",
    path: "/api/organization-invites/redeem",
    body: { token: "tok" },
  },
  {
    name: "organizations.projects",
    send: (c) => c.organizations.projects(ORG),
    method: "GET",
    path: `/api/organizations/${ORG}/projects`,
  },
  {
    name: "organizations.createProject",
    send: (c) => c.organizations.createProject(ORG, { name: "Checkout" }),
    method: "POST",
    path: `/api/organizations/${ORG}/projects`,
    body: { name: "Checkout" },
  },
  {
    name: "projects.get",
    send: (c) => c.projects.get(PRJ),
    method: "GET",
    path: `/api/projects/${PRJ}`,
  },
  {
    name: "projects.members",
    send: (c) => c.projects.members(PRJ),
    method: "GET",
    path: `/api/projects/${PRJ}/members`,
  },
  {
    name: "projects.addMember",
    send: (c) => c.projects.addMember(PRJ, { user_id: USER, role: "developer" }),
    method: "POST",
    path: `/api/projects/${PRJ}/members`,
    body: { user_id: USER, role: "developer" },
  },
  {
    name: "projects.removeMember",
    send: (c) => c.projects.removeMember(PRJ, USER),
    method: "DELETE",
    path: `/api/projects/${PRJ}/members/${USER}`,
  },
];

test("every organization and project method targets its route", async () => {
  for (const call of ORGANIZATION_CALLS) {
    const seen: Request[] = [];
    const client = createControlClient({
      baseUrl: "http://control.local",
      fetch: async (input, init) => {
        seen.push(new Request(input, init));
        // 204 is what every void-returning route answers with, and JSON for
        // the rest; both parse through the same `request`.
        return call.method === "DELETE" || call.path.endsWith("/transfer")
          ? new Response(null, { status: 204 })
          : json({ id: ORG });
      },
    });

    await call.send(client);

    assert.equal(seen.length, 1, `${call.name} made ${seen.length} requests`);
    const request = seen[0]!;
    assert.equal(request.method, call.method, `${call.name} method`);
    assert.equal(new URL(request.url).pathname, call.path, `${call.name} path`);
    if (call.body === undefined) {
      assert.equal(request.body, null, `${call.name} sent a body`);
    } else {
      assert.deepEqual(await request.json(), call.body, `${call.name} body`);
    }
  }
});

test("the routing table covers every organization and project method", () => {
  // Without this, a method added to the client and forgotten above is exercised
  // by nothing while the table still passes - a check that examined the wrong
  // set and a clean result print the same thing.
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () => json({}),
  });
  const declared = [
    ...Object.keys(client.organizations).map((key) => `organizations.${key}`),
    ...Object.keys(client.projects).map((key) => `projects.${key}`),
  ].sort();
  const covered = ORGANIZATION_CALLS.map((call) => call.name).sort();
  assert.deepEqual(covered, declared);
});

test("a listed invite carries no token and the created one does", async () => {
  // The token exists exactly once, in the create response. A client that does
  // not surface it there has lost the invitation, so this pins both halves:
  // create returns it, and the list shape has no field to hold it.
  const invite = {
    id: IVT,
    organization_id: ORG,
    email: "a@b.test",
    role: "viewer",
    issued_at: "2026-09-06T00:00:00Z",
    expires_at: "2026-09-13T00:00:00Z",
    consumed_at: null,
  };
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async (input, init) => {
      const request = new Request(input, init);
      return request.method === "POST"
        ? json({ invite, token: "one-time-secret" }, 201)
        : json({ invites: [invite] });
    },
  });

  const created = await client.organizations.createInvite(ORG, {
    email: "a@b.test",
    role: "viewer",
  });
  assert.equal(created.token, "one-time-secret");

  const listed = await client.organizations.invites(ORG);
  assert.deepEqual(Object.keys(listed.invites[0]!).sort(), Object.keys(invite).sort());
  assert.ok(!("token" in listed.invites[0]!));
});

test("organization ids are percent-encoded into the path", async () => {
  // Nothing should ever send one of these - the server parses the typed id
  // first - which is exactly why the encoding has to hold: the day a caller
  // passes a slug or a path fragment, the URL must still be the URL they named
  // rather than a different route.
  const seen: string[] = [];
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async (input, init) => {
      seen.push(new Request(input, init).url);
      return new Response(null, { status: 204 });
    },
  });

  await client.organizations.removeMember("org/../apps", "user?x#y");

  assert.equal(
    seen[0],
    "http://control.local/api/organizations/org%2F..%2Fapps/members/user%3Fx%23y",
  );
});

test("a refused membership change surfaces the server's reason", async () => {
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () =>
      json(
        {
          error: "insufficient authority",
          detail:
            "granting \"owner\" needs a strictly higher rank and at least equal billing authority",
        },
        403,
      ),
  });

  await assert.rejects(
    client.organizations.addMember(ORG, { user_id: USER, role: "owner" }),
    (error: unknown) => {
      assert.ok(error instanceof ControlError);
      assert.equal(error.status, 403);
      assert.equal(error.message, "insufficient authority");
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
