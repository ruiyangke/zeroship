/**
 * Workspace + Home UI — Playwright e2e tests.
 *
 * Targets the new unified DX:
 *   - / (Home: gallery + new-prompt)
 *   - /p/:appId/{chat,files,logs,env,settings} (project workspace)
 *   - /account
 *
 * Most tests stub the agent SSE response via Playwright's route
 * interception so they run fast + deterministically without an
 * OpenAI key.
 *
 * One test at the bottom (`live-stack`) runs against the actual
 * agent + sandbox + control plane — gated by the AGENT_LIVE env
 * var so CI doesn't accidentally need a running stack.
 *
 * Servers required for ALL tests:
 *   - vite dev server on :5173
 *
 * Live test additionally requires:
 *   - agent on :4444 (so /agent/chat proxies somewhere real)
 *   - sandbox + control + worker + gateway running
 *   - OPENAI_API_KEY set on the agent process
 *
 * Run:
 *   npx playwright test e2e/builder.spec.ts
 *   AGENT_LIVE=1 npx playwright test e2e/builder.spec.ts
 */
import { test, expect, type Page, type Route } from "@playwright/test";

const LIVE = process.env.AGENT_LIVE === "1";

/** Helper: serialize a sequence of SSE events as a wire-format string. */
function makeSSE(events: object[]): string {
  return events.map((ev) => `data: ${JSON.stringify(ev)}\n\n`).join("");
}

/** Stub the agent SSE response so tests don't need OpenAI. */
async function stubAgent(page: Page, events: object[]): Promise<void> {
  await page.route("**/agent/chat", async (route: Route) => {
    await route.fulfill({
      status: 200,
      contentType: "text/event-stream; charset=utf-8",
      body: makeSSE(events),
    });
  });
}

// =========================================================================
// Home page (/) — gallery + prompt
// =========================================================================

test.describe("Home page", () => {
  test.beforeEach(async ({ page }) => {
    // Empty app list so the gallery shows its empty state.
    await page.route("**/api/apps", async (route) => {
      await route.fulfill({ status: 200, contentType: "application/json", body: "[]" });
    });
  });

  test("renders the prompt + empty gallery", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("home")).toBeVisible();
    await expect(page.getByTestId("home-prompt")).toBeVisible();
    await expect(page.getByText(/no projects yet/i)).toBeVisible();
  });

  test("submit button is disabled until prompt has content", async ({ page }) => {
    await page.goto("/");
    const submit = page.getByTestId("home-submit");
    await expect(submit).toBeDisabled();
    await page.getByTestId("home-prompt").fill("a recipe app");
    await expect(submit).toBeEnabled();
  });

  test("typing then submitting creates a project + navigates", async ({ page }) => {
    const NEW_ID = "11111111-2222-3333-4444-555555555555";
    await page.route("**/api/apps", async (route) => {
      if (route.request().method() === "POST") {
        await route.fulfill({
          status: 201,
          contentType: "application/json",
          body: JSON.stringify({
            id: NEW_ID, name: "recipe-app", plan_id: "free",
            deploy_hash: null, api_key: "k",
            created_at: "2026-01-01", updated_at: "2026-01-01",
          }),
        });
      } else {
        await route.fulfill({ status: 200, contentType: "application/json", body: "[]" });
      }
    });
    await page.route(`**/api/apps/${NEW_ID}`, async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: NEW_ID, name: "recipe-app", plan_id: "free",
          deploy_hash: null, api_key: "k",
          created_at: "x", updated_at: "x",
        }),
      });
    });

    await page.goto("/");
    await page.getByTestId("home-prompt").fill("recipe app");
    await page.getByTestId("home-submit").click();
    await page.waitForURL(`**/p/${NEW_ID}/chat`, { timeout: 5000 });
  });
});

// =========================================================================
// Workspace shell (/p/:appId/*)
// =========================================================================

const APP_ID = "abcdef00-1234-5678-9abc-def012345678";
async function stubApp(page: Page, overrides: Partial<{ deploy_hash: string | null; server_js: string | null }> = {}) {
  await page.route(`**/api/apps/${APP_ID}`, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        id: APP_ID,
        name: "demo",
        plan_id: "free",
        deploy_hash: overrides.deploy_hash ?? "abc123",
        api_key: "k",
        created_at: "2026-01-01",
        updated_at: "2026-01-01",
        server_js: overrides.server_js ?? "export default { fetch() { return new Response('hi'); } };\n",
      }),
    });
  });
}

test.describe("Workspace layout", () => {
  test.beforeEach(async ({ page }) => { await stubApp(page); });

  test("loads /p/:id/chat without login", async ({ page }) => {
    await page.goto(`/p/${APP_ID}/chat`);
    await expect(page.getByTestId("topbar")).toBeVisible();
    await expect(page.getByTestId("topbar-project")).toContainText("demo");
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await expect(page.getByTestId("chat-tab")).toBeVisible();
  });

  test("/p/:id redirects to /chat tab", async ({ page }) => {
    await page.goto(`/p/${APP_ID}`);
    await page.waitForURL(`**/p/${APP_ID}/chat`, { timeout: 3000 });
  });

  test("tab nav links work", async ({ page }) => {
    await page.goto(`/p/${APP_ID}/chat`);
    await page.getByTestId("tab-settings").click();
    await page.waitForURL(`**/p/${APP_ID}/settings`);
    await expect(page.getByTestId("settings-tab")).toBeVisible();
    await page.getByTestId("tab-chat").click();
    await page.waitForURL(`**/p/${APP_ID}/chat`);
  });

  test("chat-toggle hides + shows the chat rail", async ({ page }) => {
    await page.goto(`/p/${APP_ID}/chat`);
    const rail = page.getByTestId("chat-rail");
    await expect(rail.locator("textarea")).toBeVisible();
    await page.getByTestId("topbar-toggle-chat").click();
    await expect(rail.locator("textarea")).not.toBeVisible();
    await page.getByTestId("topbar-toggle-chat").click();
    await expect(rail.locator("textarea")).toBeVisible();
  });

  test("composer + send-button gating", async ({ page }) => {
    await page.goto(`/p/${APP_ID}/chat`);
    const send = page.locator('button[title="Send (Enter)"]');
    const textarea = page.getByTestId("chat-rail").locator("textarea");
    await expect(send).toBeDisabled();
    await textarea.fill("hi");
    await expect(send).toBeEnabled();
  });
});

// =========================================================================
// Chat behavior — agent stubbed
// =========================================================================

test.describe("Chat with stubbed agent", () => {
  test.beforeEach(async ({ page }) => { await stubApp(page); });

  test("plain text response renders + appears in the message thread", async ({ page }) => {
    await stubAgent(page, [
      { type: "text", content: "Hello! " },
      { type: "text", content: "How can I help?" },
      { type: "done" },
    ]);
    await page.goto(`/p/${APP_ID}/chat`);
    const textarea = page.getByTestId("chat-rail").locator("textarea");
    await textarea.fill("hi");
    await page.locator('button[title="Send (Enter)"]').click();
    await expect(page.getByText("hi", { exact: true })).toBeVisible();
    await expect(page.getByText("Hello! How can I help?")).toBeVisible({ timeout: 5000 });
  });

  test("Enter submits, Shift+Enter inserts newline", async ({ page }) => {
    await stubAgent(page, [{ type: "text", content: "ack" }, { type: "done" }]);
    await page.goto(`/p/${APP_ID}/chat`);
    const textarea = page.getByTestId("chat-rail").locator("textarea");
    await textarea.fill("first line");
    await textarea.press("Shift+Enter");
    await textarea.type("second line");
    await expect(textarea).toHaveValue("first line\nsecond line");
    await textarea.press("Enter");
    await expect(page.getByText("ack")).toBeVisible({ timeout: 5000 });
    await expect(textarea).toHaveValue("");
  });

  test("tool_start / tool_end render a collapsible tool card", async ({ page }) => {
    await stubAgent(page, [
      { type: "tool_start", name: "open_session", input: { project_id: APP_ID } },
      { type: "tool_end", name: "open_session", output: JSON.stringify({ ok: true, session_id: "abc" }) },
      { type: "text", content: "Done." },
      { type: "done" },
    ]);
    await page.goto(`/p/${APP_ID}/chat`);
    const textarea = page.getByTestId("chat-rail").locator("textarea");
    await textarea.fill("go");
    await page.locator('button[title="Send (Enter)"]').click();
    const card = page.locator('button:has-text("open_session")').first();
    await expect(card).toBeVisible({ timeout: 5000 });
    // Tool output is JSON with "session_id":"abc" — only visible
    // when the card is expanded.
    await expect(page.getByText(/"session_id"/)).not.toBeVisible();
    await card.click();
    await expect(page.getByText(/"session_id"/)).toBeVisible();
  });

  test("error event shows error banner above composer", async ({ page }) => {
    await stubAgent(page, [
      { type: "error", content: "agent init failed: missing API key" },
      { type: "done" },
    ]);
    await page.goto(`/p/${APP_ID}/chat`);
    await page.getByTestId("chat-rail").locator("textarea").fill("trigger");
    await page.locator('button[title="Send (Enter)"]').click();
    await expect(page.getByText(/agent init failed/i).first()).toBeVisible({ timeout: 5000 });
  });

  test("status pill flips from idle → busy → idle", async ({ page }) => {
    await page.route("**/agent/chat", async (route) => {
      await new Promise((r) => setTimeout(r, 800));
      await route.fulfill({
        status: 200, contentType: "text/event-stream",
        body: makeSSE([{ type: "text", content: "ok" }, { type: "done" }]),
      });
    });
    await page.goto(`/p/${APP_ID}/chat`);
    const topbar = page.getByTestId("topbar");
    await page.getByTestId("chat-rail").locator("textarea").fill("go");
    await page.locator('button[title="Send (Enter)"]').click();
    // While busy, the topbar shows the live status word.
    await expect(topbar).toContainText(/thinking|tool|deploying/i, { timeout: 2000 });
    // After: status pill returns to idle (so the busy word goes away).
    await expect(topbar).not.toContainText(/thinking|deploying/i, { timeout: 5000 });
  });

  test("create_app result navigates to the new project's chat", async ({ page }) => {
    const NEW_ID = "11111111-2222-3333-4444-555555555555";
    await stubAgent(page, [
      { type: "tool_start", name: "create_app", input: {} },
      { type: "tool_end", name: "create_app", output: JSON.stringify({ ok: true, app_id: NEW_ID, app_name: "newproj" }) },
      { type: "done" },
    ]);
    await page.route("**/api/apps/" + NEW_ID, async (route) => {
      await route.fulfill({
        status: 200, contentType: "application/json",
        body: JSON.stringify({
          id: NEW_ID, name: "newproj", plan_id: "free",
          deploy_hash: null, api_key: "k",
          created_at: "2026-01-01", updated_at: "2026-01-01",
        }),
      });
    });
    await page.goto(`/p/${APP_ID}/chat`);
    await page.getByTestId("chat-rail").locator("textarea").fill("make me an app");
    await page.locator('button[title="Send (Enter)"]').click();
    await page.waitForURL(`**/p/${NEW_ID}/chat`, { timeout: 5000 });
  });
});

// =========================================================================
// Conversation persistence (localStorage) — survives across tabs
// =========================================================================

test.describe("Conversation persistence", () => {
  test("history survives a tab switch + reload", async ({ page }) => {
    await stubApp(page);
    await stubAgent(page, [{ type: "text", content: "first reply" }, { type: "done" }]);

    await page.goto(`/p/${APP_ID}/chat`);
    await page.getByTestId("chat-rail").locator("textarea").fill("ping");
    await page.locator('button[title="Send (Enter)"]').click();
    await expect(page.getByText("first reply")).toBeVisible();

    // Tab switch — chat rail is shared, history persists across tabs.
    await page.getByTestId("tab-settings").click();
    await page.waitForURL(`**/p/${APP_ID}/settings`);
    await expect(page.getByText("ping").first()).toBeVisible();

    // Hard reload — history lives in localStorage.
    await page.reload();
    await expect(page.getByText("ping").first()).toBeVisible();
    await expect(page.getByText("first reply")).toBeVisible();
  });

  test("clearing chat (trash icon) wipes the thread", async ({ page }) => {
    await stubApp(page);
    await stubAgent(page, [{ type: "text", content: "reply" }, { type: "done" }]);
    await page.goto(`/p/${APP_ID}/chat`);
    await page.getByTestId("chat-rail").locator("textarea").fill("hi");
    await page.locator('button[title="Send (Enter)"]').click();
    await expect(page.getByText("reply")).toBeVisible();

    await page.getByTestId("chat-clear").click();
    await expect(page.getByText("hi", { exact: true })).not.toBeVisible();
  });
});

// =========================================================================
// Files tab — file tree + CodeMirror
// =========================================================================

test.describe("Files tab", () => {
  test.beforeEach(async ({ page }) => {
    await stubApp(page);
    // Stub the agent's /projects/:id/files proxy.
    await page.route(`**/agent/projects/${APP_ID}/files`, async (route) => {
      await route.fulfill({
        status: 200, contentType: "application/json",
        body: JSON.stringify({
          entries: [
            { kind: "file", path: "package.json", size: 100 },
            { kind: "dir",  path: "src",          size: 0 },
            { kind: "file", path: "src/App.tsx",  size: 500 },
          ],
        }),
      });
    });
    await page.route(`**/agent/projects/${APP_ID}/files/src/App.tsx`, async (route) => {
      if (route.request().method() === "GET") {
        await route.fulfill({ status: 200, contentType: "text/plain", body: "export const x = 42;\n" });
      } else if (route.request().method() === "PUT") {
        await route.fulfill({
          status: 200, contentType: "application/json",
          body: JSON.stringify({ written: "src/App.tsx", size: 32 }),
        });
      }
    });
  });

  test("renders file tree + opens a file in CodeMirror", async ({ page }) => {
    await page.goto(`/p/${APP_ID}/files`);
    await expect(page.getByTestId("files-tab")).toBeVisible();
    await expect(page.getByTestId("file-tree")).toBeVisible();

    // Click the file in the tree.
    await page.getByTestId("file-tree-file:src/App.tsx").click();
    await expect(page.locator(".cm-editor")).toBeVisible({ timeout: 5000 });
    await expect(page.locator(".cm-content")).toContainText("export const x");
  });
});

// =========================================================================
// Settings tab — delete + plan
// =========================================================================

test.describe("Settings tab", () => {
  test.beforeEach(async ({ page }) => { await stubApp(page); });

  test("delete button stays disabled until name confirmed", async ({ page }) => {
    await page.goto(`/p/${APP_ID}/settings`);
    const btn = page.getByTestId("settings-delete-button");
    await expect(btn).toBeDisabled();
    await page.getByTestId("settings-delete-confirm").fill("nope");
    await expect(btn).toBeDisabled();
    await page.getByTestId("settings-delete-confirm").fill("demo");
    await expect(btn).toBeEnabled();
  });
});

// =========================================================================
// Dev auth bypass — login should never gate the dashboard in dev mode.
// =========================================================================

test.describe("Dev auth bypass", () => {
  test.beforeEach(async ({ page }) => {
    await page.route("**/api/apps", async (route) => {
      await route.fulfill({ status: 200, contentType: "application/json", body: "[]" });
    });
    await page.route("**/_health", async (route) => {
      await route.fulfill({ status: 200, contentType: "application/json", body: '{"status":"ok"}' });
    });
    await page.route("**/_stats", async (route) => {
      await route.fulfill({
        status: 200, contentType: "application/json",
        body: JSON.stringify({ active_isolates: 0, max_isolates: 100, apps: [] }),
      });
    });
  });

  test("/admin/apps renders without a stored key in dev", async ({ page }) => {
    // Clear localStorage explicitly — even more strict than default.
    await page.addInitScript(() => localStorage.clear());
    await page.goto("/admin/apps");
    // Sidebar is the legacy admin Layout; if dev bypass works the
    // page mounts. The presence of /admin/apps text in the URL +
    // the Layout sidebar's "apps" link is sufficient.
    await expect(page).toHaveURL(/\/admin\/apps$/);
    await expect(page.locator("text=/// apps/").first()).toBeVisible({ timeout: 5000 });
  });

  test("/login renders in dev (auth UI is reachable for iteration)", async ({ page }) => {
    await page.addInitScript(() => localStorage.clear());
    await page.goto("/login");
    // The auto-bypass on /admin/* still works (covered above), but
    // /login and /signup explicitly stay accessible so devs can
    // poke at the form while building features.
    await expect(page.getByTestId("login-page")).toBeVisible({ timeout: 5000 });
    await expect(page.getByTestId("login-google")).toBeVisible();
  });

  test("api requests carry a Bearer header even when no key is stored", async ({ page }) => {
    await page.addInitScript(() => localStorage.clear());
    let captured = "";
    await page.route("**/api/apps", async (route) => {
      captured = route.request().headers()["authorization"] ?? "";
      await route.fulfill({ status: 200, contentType: "application/json", body: "[]" });
    });
    await page.goto("/");
    await page.waitForResponse("**/api/apps", { timeout: 5000 });
    expect(captured).toMatch(/^Bearer .+/);
  });
});

// =========================================================================
// Auth UI — Login + Signup + protected-route gating
// =========================================================================
//
// In dev mode the dashboard short-circuits authentication via
// `import.meta.env.DEV` (see src/auth/AuthContext.tsx) — every
// route renders without ever calling /auth/userinfo. These tests
// exercise the surfaces that DO appear (Login + Signup pages,
// /account) and the dev-bypass behavior we want to preserve.
//
// Production-mode auth (real cookie session round-trip) is covered
// by the backend curl smoke-tests committed alongside the auth
// service; testing it here would require shelling out to the
// control plane during the test run, which is out of scope for the
// dashboard e2e suite.

test.describe("Auth UI", () => {
  test.beforeEach(async ({ page }) => {
    await page.addInitScript(() => localStorage.clear());
    await page.route("**/api/apps", async (r) =>
      r.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
    );
    await page.route("**/_health", async (r) =>
      r.fulfill({ status: 200, contentType: "application/json", body: '{"status":"ok"}' }),
    );
  });

  test("/login renders email + password form + Google button", async ({ page }) => {
    await page.goto("/login");
    await expect(page.getByTestId("login-page")).toBeVisible();
    await expect(page.getByTestId("login-email")).toBeVisible();
    await expect(page.getByTestId("login-password")).toBeVisible();
    await expect(page.getByTestId("login-google")).toBeVisible();
  });

  test("/signup renders all fields", async ({ page }) => {
    await page.goto("/signup");
    await expect(page.getByTestId("signup-page")).toBeVisible();
    await expect(page.getByTestId("signup-name")).toBeVisible();
    await expect(page.getByTestId("signup-email")).toBeVisible();
    await expect(page.getByTestId("signup-password")).toBeVisible();
    await expect(page.getByTestId("signup-google")).toBeVisible();
  });

  test("/signup submit is disabled until all fields valid", async ({ page }) => {
    await page.goto("/signup");
    const submit = page.getByTestId("signup-submit");
    await expect(submit).toBeDisabled();
    await page.getByTestId("signup-name").fill("Alice");
    await page.getByTestId("signup-email").fill("a@b.dev");
    await page.getByTestId("signup-password").fill("short");
    await expect(submit).toBeDisabled();
    await page.getByTestId("signup-password").fill("longerpassword");
    await expect(submit).toBeEnabled();
  });

  test("/signup → /login link works", async ({ page }) => {
    await page.goto("/signup");
    await page.getByTestId("signup-link-login").click();
    await page.waitForURL(/\/login/, { timeout: 3000 });
    await expect(page.getByTestId("login-page")).toBeVisible();
  });

  test("/account shows the (synthetic) dev user", async ({ page }) => {
    await page.goto("/account");
    await expect(page.getByTestId("account")).toBeVisible();
    await expect(page.getByTestId("account-name")).toContainText("Dev");
    await expect(page.getByTestId("account-email")).toContainText("dev@localhost");
  });

  test("Google start link points at /auth/google/start", async ({ page }) => {
    await page.goto("/signup");
    const href = await page.getByTestId("signup-google").getAttribute("href");
    expect(href ?? "").toContain("/auth/google/start");
    expect(href ?? "").toContain("return=");
  });
});

// =========================================================================
// Live e2e — gated on AGENT_LIVE=1 because it needs all the platform
// services running plus an OpenAI key on the agent process.
// =========================================================================

test.describe("Live stack", () => {
  test.skip(!LIVE, "set AGENT_LIVE=1 to run against the running agent + sandbox");

  test("agent responds to a real message via the vite proxy", async ({ page }) => {
    test.setTimeout(60_000);
    // We need a real app the workspace can render; use a fresh
    // project_id so the live agent's open_session creates a clean
    // sandbox container.
    const id = "550e8400-e29b-41d4-a716-44665544aaaa";
    // Stub the control-plane app fetch (agent doesn't auto-create
    // until build_and_publish runs, which we don't trigger here).
    await page.route(`**/api/apps/${id}`, async (route) => {
      await route.fulfill({
        status: 200, contentType: "application/json",
        body: JSON.stringify({
          id, name: "live", plan_id: "free", deploy_hash: null,
          api_key: "k", created_at: "x", updated_at: "x",
        }),
      });
    });
    await page.goto(`/p/${id}/chat`);
    await page.getByTestId("chat-rail").locator("textarea").fill(
      "Reply with the literal text PONG and nothing else.",
    );
    await page.locator('button[title="Send (Enter)"]').click();
    await expect(page.getByText(/PONG/)).toBeVisible({ timeout: 50_000 });
  });
});
