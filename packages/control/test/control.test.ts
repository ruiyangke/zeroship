import assert from "node:assert/strict";
import test from "node:test";

import {
  ControlError,
  createControlClient,
  createDeployCommand,
  DeployOutcomeUnknownError,
  isAppId,
  isDeployCommandId,
  mintDeployCommandId,
  type AppId,
  type ControlClientOptions,
  type DeployCommand,
  type DeployCommandId,
  type UserId,
} from "../src/index.ts";

const APP_ID: AppId = "app_0000000002e4nenowz3qmamtd";

test("AppId validation matches the canonical wire shape", () => {
  assert.equal(isAppId(APP_ID), true);
  assert.equal(isAppId("00000000-0000-7000-8000-000000000001"), false);
  assert.equal(isAppId("app_0000000002E4NENOWZ3QMAMTD"), false);
  assert.equal(isAppId("app_zzzzzzzzzzzzzzzzzzzzzzzzz"), false);
});

test("apps.create sends bearer auth and JSON", async () => {
  const requests: Request[] = [];
  const client = createControlClient({
    baseUrl: "http://control.local",
    auth: "master-key",
    fetch: async (input, init) => {
      requests.push(new Request(input, init));
      return json({ id: APP_ID, name: "demo" }, 201);
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
      return json({ id: APP_ID, name: "demo" }, 200, {
        "set-cookie": "session=token; HttpOnly",
      });
    },
  });

  const result = await client.apps.get(APP_ID);

  assert.equal(result.id, APP_ID);
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
        id: APP_ID,
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

  const archived = await client.apps.archive(APP_ID);
  const restored = await client.apps.unarchive(APP_ID);

  assert.equal(archived.archived_at, "2026-08-31T12:00:00Z");
  assert.equal(restored.archived_at, null);
  assert.deepEqual(
    requests.map(
      (request) => `${request.method} ${new URL(request.url).pathname}`,
    ),
    [
      `PUT /api/apps/${APP_ID}/archive`,
      `DELETE /api/apps/${APP_ID}/archive`,
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
    await client.env.setVar(APP_ID, { key: "FOO", value: "bar" }),
    undefined,
  );
  assert.equal(await client.env.deleteSecret(APP_ID, "SECRET/1"), undefined);
  assert.deepEqual(seen, [
    `POST /api/apps/${APP_ID}/vars`,
    `DELETE /api/apps/${APP_ID}/secrets/SECRET%2F1`,
  ]);
});

const DEPLOYMENT = "dep_0000000002e4nenowz3qmamtd";

/** Control's acceptance of `command`, as the deploy route answers it. */
function acceptance(command: string, replayed = false): Response {
  return json(
    {
      command_id: command,
      deploy_id: DEPLOYMENT,
      deploy_hash: "sha256:abc",
      blobs_uploaded: 1,
      blobs_deduped: 2,
      lifecycle_revision: 3,
    },
    200,
    replayed ? { "idempotent-replayed": "true" } : {},
  );
}

test("deploy command ids are canonical, fresh and UUIDv7-shaped", async () => {
  const first = mintDeployCommandId();
  const second = mintDeployCommandId();
  assert.notEqual(first, second);
  for (const id of [first, second]) {
    assert.equal(isDeployCommandId(id), true, id);
    const value = [...id.slice(4)].reduce(
      (acc, digit) => acc * 36n + BigInt(Number.parseInt(digit, 36)),
      0n,
    );
    assert.equal((value >> 76n) & 0xfn, 7n, "version nibble");
    assert.equal((value >> 62n) & 0x3n, 2n, "variant bits");
  }
  assert.equal(isDeployCommandId(APP_ID), false);
  assert.equal(isDeployCommandId(first.toUpperCase()), false);
  assert.equal(isDeployCommandId("dcm_zzzzzzzzzzzzzzzzzzzzzzzzz"), false);
  assert.equal(isAppId(first), false);

  await assert.rejects(
    createDeployCommand(new Uint8Array([1]), APP_ID as unknown as DeployCommandId),
    TypeError,
  );
  const resumed = await createDeployCommand(new Uint8Array([1]), first);
  assert.equal(resumed.id, first);
  assert.equal(Object.isFrozen(resumed), true);
});

test("deploy sends the command's snapshot as application/x-zship under its id", async () => {
  const artifact = new Uint8Array([1, 2, 3, 4]);
  const command = await createDeployCommand(artifact.subarray(1, 3));
  // Later writes to the caller's buffer do not reach the command.
  artifact.fill(9);
  let request: Request | undefined;
  const client = createControlClient({
    baseUrl: "http://control.local",
    headers: { "content-type": "application/json" },
    fetch: async (input, init) => {
      request = new Request(input, init);
      return acceptance(command.id);
    },
  });

  const result = await client.apps.deploy(APP_ID, command);

  assert.deepEqual(result, {
    command_id: command.id,
    deploy_id: DEPLOYMENT,
    deploy_hash: "sha256:abc",
    blobs_uploaded: 1,
    blobs_deduped: 2,
    lifecycle_revision: 3,
    replayed: false,
  });
  assert.equal(request?.method, "POST");
  assert.equal(new URL(request!.url).pathname, `/api/apps/${APP_ID}/deploy`);
  assert.equal(request?.headers.get("content-type"), "application/x-zship");
  assert.equal(request?.headers.get("idempotency-key"), command.id);
  assert.deepEqual(new Uint8Array(await request!.arrayBuffer()), new Uint8Array([2, 3]));
});

test("an unanswered deploy is resent with the same id and bytes", async () => {
  const command = await createDeployCommand(new Blob([new Uint8Array([7, 8])]));
  const sent: Array<{ key: string | null; bytes: number[] }> = [];
  const answers: Array<() => Response> = [
    () => {
      throw new TypeError("fetch failed");
    },
    () => json({ error: "internal error" }, 503),
    () => acceptance(command.id, true),
  ];
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async (input, init) => {
      const request = new Request(input, init);
      sent.push({
        key: request.headers.get("idempotency-key"),
        bytes: [...new Uint8Array(await request.arrayBuffer())],
      });
      return answers.shift()!();
    },
  });

  for (const cause of [TypeError, ControlError]) {
    await assert.rejects(client.apps.deploy(APP_ID, command), (error: unknown) => {
      assert.ok(error instanceof DeployOutcomeUnknownError);
      assert.equal(error.commandId, command.id);
      assert.ok(error.cause instanceof cause);
      assert.match(error.message, new RegExp(command.id));
      return true;
    });
  }
  const result = await client.apps.deploy(APP_ID, command);

  assert.equal(result.replayed, true);
  assert.equal(result.command_id, command.id);
  assert.deepEqual(sent, Array(3).fill({ key: command.id, bytes: [7, 8] }));
});

test("a deploy refusal is a ControlError and a foreign acceptance is not a success", async () => {
  const command = await createDeployCommand(new Uint8Array([1]));
  const other = mintDeployCommandId();
  const answers = [
    () => json({ error: "idempotency_key_conflict" }, 409),
    () => acceptance(other),
  ];
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () => answers.shift()!(),
  });

  await assert.rejects(client.apps.deploy(APP_ID, command), (error: unknown) => {
    assert.ok(error instanceof ControlError);
    assert.ok(!(error instanceof DeployOutcomeUnknownError));
    assert.equal(error.status, 409);
    assert.equal(error.message, "idempotency_key_conflict");
    return true;
  });
  await assert.rejects(client.apps.deploy(APP_ID, command), (error: unknown) => {
    assert.ok(error instanceof DeployOutcomeUnknownError);
    assert.equal(error.commandId, command.id);
    return true;
  });
});

test("deploy refuses a raw artifact without sending it", async () => {
  let calls = 0;
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () => {
      calls += 1;
      return acceptance(mintDeployCommandId());
    },
  });

  await assert.rejects(
    client.apps.deploy(APP_ID, new Uint8Array([1]) as unknown as DeployCommand),
    TypeError,
  );
  await assert.rejects(
    client.apps.deploy(APP_ID, { id: "dcm_bad", archive: new Blob([]) } as unknown as DeployCommand),
    TypeError,
  );
  assert.equal(calls, 0);
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
    client.apps.logs(APP_ID),
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

  await assert.rejects(client.apps.logs(APP_ID), (error: unknown) => {
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

  await assert.rejects(client.apps.logs(APP_ID), (error: unknown) => {
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

  await assert.rejects(client.apps.logs(APP_ID), (error: unknown) => {
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
const USER = "usr_0000000002e4nenowz3qmamtd";

/**
 * The whole organization/project surface, stated as the request each method
 * makes. Method, path and body are the three things a mistake here sends
 * somewhere else, and every path segment carries a typed id - the
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
  /**
   * What the stub answers. Defaulted from the method below, because almost
   * every DELETE here is a `204` - but `organizations.dissolve` is a DELETE
   * that returns the closed record, so the shape cannot be derived from the
   * verb alone.
   */
  returns?: "json" | "void";
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
    name: "organizations.leave",
    send: (c) => c.organizations.leave(ORG),
    method: "DELETE",
    path: `/api/organizations/${ORG}/membership`,
  },
  {
    name: "organizations.dissolve",
    send: (c) => c.organizations.dissolve(ORG),
    method: "DELETE",
    path: `/api/organizations/${ORG}`,
    returns: "json",
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
    name: "projects.update",
    send: (c) => c.projects.update(PRJ, { name: "Checkout" }),
    method: "PATCH",
    path: `/api/projects/${PRJ}`,
    body: { name: "Checkout" },
  },
  {
    name: "projects.delete",
    send: (c) => c.projects.delete(PRJ),
    method: "DELETE",
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
    name: "projects.changeMemberRole",
    send: (c) => c.projects.changeMemberRole(PRJ, USER, { role: "viewer" }),
    method: "PATCH",
    path: `/api/projects/${PRJ}/members/${USER}`,
    body: { role: "viewer" },
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
        // the rest; both parse through the same `request`. The verb alone does
        // not decide it - `organizations.dissolve` is a DELETE that returns the
        // closed record - so a call may say.
        const returns =
          call.returns ??
          (call.method === "DELETE" || call.path.endsWith("/transfer")
            ? "void"
            : "json");
        return returns === "void"
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
        ? json({ invite, token: "one-time-secret", delivery: "sent" }, 201)
        : json({ invites: [invite] });
    },
  });

  const created = await client.organizations.createInvite(ORG, {
    email: "a@b.test",
    role: "viewer",
  });
  assert.equal(created.token, "one-time-secret");
  assert.equal(created.delivery, "sent");

  const listed = await client.organizations.invites(ORG);
  assert.deepEqual(Object.keys(listed.invites[0]!).sort(), Object.keys(invite).sort());
  assert.ok(!("token" in listed.invites[0]!));
});

test("every delivery outcome reaches the caller unchanged", async () => {
  // The three outcomes drive different advice - `sent` means the recipient has
  // it, the other two mean nobody does but this caller - so a client that
  // dropped or coerced the field would tell a person the wrong thing at the
  // one moment the token still exists.
  const invite = {
    id: IVT,
    organization_id: ORG,
    email: "a@b.test",
    role: "viewer",
    issued_at: "2026-09-06T00:00:00Z",
    expires_at: "2026-09-13T00:00:00Z",
    consumed_at: null,
  };
  for (const delivery of ["sent", "suppressed", "failed"] as const) {
    const client = createControlClient({
      baseUrl: "http://control.local",
      fetch: async () => json({ invite, token: "one-time-secret", delivery }, 201),
    });
    const created = await client.organizations.createInvite(ORG, {
      email: "a@b.test",
      role: "viewer",
    });
    assert.equal(created.delivery, delivery);
    // The token is present in EVERY outcome. It is the only copy when the mail
    // did not carry it, so a client must never have to guess.
    assert.equal(created.token, "one-time-secret");
  }
});

test("a closed organization reports when it was closed", async () => {
  // `dissolve` is a DELETE that answers with a body, which is unusual enough
  // that a client parsing it as void would silently discard the one fact the
  // call exists to report.
  const closed = {
    id: ORG,
    slug: "acme",
    name: "Acme",
    billing_email: "pay@acme.test",
    personal_owner_id: null,
    created_at: "2026-09-06T00:00:00Z",
    updated_at: "2026-09-07T00:00:00Z",
    dissolved_at: "2026-09-07T00:00:00Z",
  };
  const client = createControlClient({
    baseUrl: "http://control.local",
    fetch: async () => json(closed),
  });
  const record = await client.organizations.dissolve(ORG);
  assert.equal(record.dissolved_at, "2026-09-07T00:00:00Z");
  assert.equal(record.slug, "acme", "closing does not rewrite the record");
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

  await client.organizations.removeMember(
    "org/../apps",
    "user?x#y" as UserId,
  );

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
