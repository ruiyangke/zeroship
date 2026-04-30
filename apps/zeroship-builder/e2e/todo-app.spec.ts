// ─── todo app — agent builds it from scratch, we exercise it ─────
//
// The most meaningful e2e: the agent receives a single natural-language
// prompt and, with no hand-coded scaffolding, produces a working todo
// CRUD backend reachable through the gateway. The test then drives
// real HTTP requests against the deployed app and asserts the
// behaviour we'd expect from a competent implementation:
//
//   GET  /todos           → []           (initially empty)
//   POST /todos {title}   → {id, title, done:false}
//   GET  /todos           → [the row]
//   PATCH /todos/<id>     {done:true}    → {…, done:true}
//   DELETE /todos/<id>    → {deleted:true} (or 200/204)
//   GET  /todos           → []           (gone)
//   GET  /                → HTML page
//
// Skipped by default — needs an LLM key in the dev runtime. Enable
// with E2E_AI_BUILD=1.

import { test, expect } from "@playwright/test";
import {
  createApp,
  deleteApp,
  rpc,
  uniqueAppName,
  visit,
  type AppRecord,
} from "./helpers";

const enabled = process.env.E2E_AI_BUILD === "1";

const BUILD_TIMEOUT_MS = 10 * 60 * 1000;
const POLL_INTERVAL_MS = 2_500;
const GATEWAY_URL = process.env.E2E_GATEWAY_URL ?? "http://localhost:8001";

const PROMPT = `Build a working todo-list app for THIS workspace.

Use ONLY ONE TOOL: \`deploy_app\`. The workspace's app_id is provided in
the workspace context — pass that as the \`app_id\` argument.

The module you deploy must export EXACTLY this shape (the runtime only
invokes \`default.fetch\`):

    export default { fetch(req, env, ctx) { /* code */ } }

Routes the deployed app must serve:

    GET  /todos             → 200 JSON array of {id, title, done}
    POST /todos {title}     → 200 JSON {id, title, done:false}
    PATCH /todos/<id>{done} → 200 JSON updated todo
    DELETE /todos/<id>      → 200 or 204
    GET /                   → 200 HTML page (must contain word "todo")

Persist todos in a module-scoped in-memory array.

CRITICAL RULE: do NOT nest backtick template literals. The runtime parses
the entire module as one JS file, so an inner backtick inside an outer
template literal terminates the outer one and breaks compilation. For
embedded HTML/JS, prefer single-quoted strings + \`+\` concatenation. Do
NOT use any backticks inside an HTML string that's itself a backtick
template literal.

Make ONE deploy_app call with the complete module. After it succeeds,
reply with just "done" — do not try other tools.`;

test.describe.serial("todo app — built by the agent, driven by us", () => {
  test.skip(!enabled, "set E2E_AI_BUILD=1 to enable real-LLM build tests");

  test.setTimeout(BUILD_TIMEOUT_MS + 60_000);
  test.use({ actionTimeout: 30_000, navigationTimeout: 30_000 });

  let app: AppRecord;
  let deployedHash: string | null = null;
  let baseUrl = "";

  test.beforeAll(async ({ request }) => {
    app = await createApp(request, uniqueAppName("e2e-todo"));
    baseUrl = `${GATEWAY_URL}/apps/${encodeURIComponent(app.name)}`;
  });

  test.afterAll(async ({ request }) => {
    if (app) await deleteApp(request, app.id);
  });

  test("agent receives the prompt → deploys a real todo backend", async ({ page, request }) => {
    await visit(page, `/p/${app.id}/preview`);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    await page.getByTestId("chat-input").fill(PROMPT);
    await page.getByTestId("chat-send").click();
    await expect(page.getByTestId("turn-user").last()).toContainText(/todo[- ]list app/i);

    const startedAt = Date.now();
    let lastStatus: string | null = null;
    while (Date.now() - startedAt < BUILD_TIMEOUT_MS) {
      const indicator = page.getByTestId("status-title");
      const isBusy = await indicator.isVisible().catch(() => false);
      const statusLabel = isBusy ? (await indicator.textContent())?.trim() ?? "" : "idle";
      if (statusLabel !== lastStatus) {
        // eslint-disable-next-line no-console
        console.log(`[todo-app] status=${statusLabel} t+${((Date.now() - startedAt) / 1000) | 0}s`);
        lastStatus = statusLabel;
      }

      const errBanner = page.locator(".bg-tomato\\/10").filter({ hasText: /Error:|error/i });
      if (await errBanner.first().isVisible().catch(() => false)) {
        const txt = await errBanner.first().textContent();
        throw new Error(`agent error: ${txt?.trim()}`);
      }

      const fresh = await rpc<AppRecord>(request, "src/server/apps/getApp", [app.id]);
      if (fresh.deploy_hash && fresh.deploy_hash !== deployedHash) {
        deployedHash = fresh.deploy_hash;
        // eslint-disable-next-line no-console
        console.log(`[todo-app] deploy_hash=${deployedHash} t+${((Date.now() - startedAt) / 1000) | 0}s`);
      }

      if (deployedHash && !isBusy) break;
      await page.waitForTimeout(POLL_INTERVAL_MS);
    }
    expect(deployedHash, "agent never produced a deploy_hash").toBeTruthy();

    // Wait for the gateway to pick up the route — same poll the
    // preview spec uses. On failure, surface the latest probe so we
    // know whether the gateway routes the name OR the worker can load
    // the bundle.
    const deadline = Date.now() + 30_000;
    let probeStatus = 0;
    let probeBody = "";
    while (Date.now() < deadline) {
      const probe = await request.get(`${baseUrl}/`);
      probeStatus = probe.status();
      probeBody = await probe.text().catch(() => "");
      if (probe.ok()) {
        // eslint-disable-next-line no-console
        console.log(`[todo-app] gateway 200 t+${((Date.now() - startedAt) / 1000) | 0}s`);
        return;
      }
      await new Promise((r) => setTimeout(r, 500));
    }

    // The `/internal/bundles/<id>` source-dump endpoint went away in
    // the artifact-layout redesign — bundles are now BlobStore blobs
    // keyed by sha256, not addressable by app id. We surface the last
    // gateway probe + the deploy_hash from the API instead; that's
    // enough to triage "did the route reach the worker at all?".
    const fresh = await rpc<AppRecord>(request, "src/server/apps/getApp", [app.id]);
    throw new Error(
      `deployed app never reachable: status=${probeStatus} body=${probeBody.slice(0, 200)} ` +
      `(deploy_hash=${fresh.deploy_hash ?? "<none>"})`,
    );
  });

  test("GET /todos starts empty (or returns an array)", async ({ request }) => {
    expect(deployedHash, "build step must run first").toBeTruthy();
    const r = await request.get(`${baseUrl}/todos`);
    expect(r.ok(), `GET /todos returned ${r.status()}`).toBe(true);
    const body = await r.json();
    expect(Array.isArray(body), `GET /todos body should be array, got ${JSON.stringify(body).slice(0, 200)}`).toBe(true);
    // eslint-disable-next-line no-console
    console.log(`[todo-app] initial /todos: ${JSON.stringify(body).slice(0, 200)}`);
  });

  let firstId: string | number | null = null;
  test("POST /todos {title:'buy milk'} adds a todo", async ({ request }) => {
    const r = await request.post(`${baseUrl}/todos`, {
      data: { title: "buy milk" },
      headers: { "content-type": "application/json" },
    });
    expect(r.ok(), `POST returned ${r.status()}: ${await r.text().catch(() => "")}`).toBe(true);
    const created = await r.json();
    // eslint-disable-next-line no-console
    console.log(`[todo-app] POST /todos → ${JSON.stringify(created)}`);
    expect(created, "POST response should be a todo object").toMatchObject({
      title: "buy milk",
      done: false,
    });
    expect(created.id != null, `POST response missing id: ${JSON.stringify(created)}`).toBe(true);
    firstId = created.id;
  });

  test("GET /todos contains the new row", async ({ request }) => {
    const r = await request.get(`${baseUrl}/todos`);
    const body = await r.json();
    expect(Array.isArray(body)).toBe(true);
    const found = body.find((t: any) => t.id === firstId);
    expect(found, `expected to find id=${firstId} in ${JSON.stringify(body)}`).toBeTruthy();
    expect(found).toMatchObject({ title: "buy milk", done: false });
  });

  test("POST a second todo, then GET returns both", async ({ request }) => {
    const r = await request.post(`${baseUrl}/todos`, {
      data: { title: "write tests" },
      headers: { "content-type": "application/json" },
    });
    expect(r.ok()).toBe(true);
    const list = await (await request.get(`${baseUrl}/todos`)).json();
    expect(list.length).toBeGreaterThanOrEqual(2);
    const titles = list.map((t: any) => t.title);
    expect(titles).toContain("buy milk");
    expect(titles).toContain("write tests");
  });

  test("PATCH /todos/<id> {done:true} marks it done", async ({ request }) => {
    const r = await request.patch(`${baseUrl}/todos/${encodeURIComponent(String(firstId))}`, {
      data: { done: true },
      headers: { "content-type": "application/json" },
    });
    expect(r.ok(), `PATCH returned ${r.status()}: ${await r.text().catch(() => "")}`).toBe(true);

    const after = await (await request.get(`${baseUrl}/todos`)).json();
    const found = after.find((t: any) => t.id === firstId);
    expect(found, "todo missing after PATCH").toBeTruthy();
    expect(found.done, `expected done:true after PATCH, got ${JSON.stringify(found)}`).toBe(true);
  });

  test("DELETE /todos/<id> removes it", async ({ request }) => {
    const r = await request.delete(`${baseUrl}/todos/${encodeURIComponent(String(firstId))}`);
    expect([200, 204].includes(r.status()), `DELETE returned ${r.status()}`).toBe(true);

    const after = await (await request.get(`${baseUrl}/todos`)).json();
    const stillThere = after.find((t: any) => t.id === firstId);
    expect(stillThere, `todo with id=${firstId} should be gone, list=${JSON.stringify(after)}`).toBeFalsy();
    // The second todo we added earlier should still be there.
    expect(after.find((t: any) => t.title === "write tests"), "second todo should survive").toBeTruthy();
  });

  test("GET / returns an HTML page that mentions 'todo'", async ({ request }) => {
    const r = await request.get(`${baseUrl}/`);
    expect(r.ok()).toBe(true);
    const ct = r.headers()["content-type"] ?? "";
    expect(ct.toLowerCase()).toContain("text/html");
    const body = (await r.text()).toLowerCase();
    expect(body).toMatch(/<!doctype html|<html|<body/);
    expect(body).toContain("todo");
  });

  test("preview iframe in the workspace shows the deployed UI", async ({ page }) => {
    await visit(page, `/p/${app.id}/preview`);
    const iframe = page.locator('iframe[title*="Preview"]');
    await expect(iframe).toBeVisible();
    const frame = page.frameLocator('iframe[title*="Preview"]');
    // Just probe for the word "todo" anywhere in the rendered DOM.
    await expect(frame.locator("body")).toContainText(/todo/i);
  });
});
