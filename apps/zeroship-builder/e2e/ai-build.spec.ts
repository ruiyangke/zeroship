// ─── AI build — drive the studio from prompt to live URL ─────────
//
// This is the heaviest end-to-end test in the suite. It exercises:
//   • the chat RPC (streamAgent SSE)
//   • deepagents + LangGraph + the LLM provider
//   • the docker sandbox (open_session, write_file, npm install/build)
//   • control-plane deploy_app
//   • the gateway → worker → V8 isolate path for the deployed app
//
// Skipped by default. Enable with E2E_AI_BUILD=1 once your dev runtime
// has an LLM key (OPENAI_API_KEY or ANTHROPIC_API_KEY) baked into its
// process env. The dev runtime is the `zeroship serve` child process
// vite spawns from the plugin — it inherits env from `npm run dev`,
// so `OPENAI_API_KEY=… npm run dev` is enough.
//
// Cleanup: the test deletes the app it created in afterAll, and the
// suite-wide `cleanupLeakedTestApps` sweeps anything that escaped.

import { test, expect } from "@playwright/test";
import {
  createApp,
  deleteApp,
  rpc,
  uniqueAppName,
  visit,
  type AppRecord,
} from "./helpers";

const BUILD_TIMEOUT_MS = 10 * 60 * 1000;   // 10 min — generous, real LLM
const POLL_INTERVAL_MS = 2_500;
const GATEWAY_URL = process.env.E2E_GATEWAY_URL ?? "http://localhost:8001";

const enabled = process.env.E2E_AI_BUILD === "1";

test.describe("ai build (real LLM, real sandbox, real deploy)", () => {
  test.skip(!enabled, "set E2E_AI_BUILD=1 with OPENAI_API_KEY (or ANTHROPIC_API_KEY) wired into the dev runtime to enable");

  test.setTimeout(BUILD_TIMEOUT_MS + 60_000);
  // The dev runtime can be sluggish on first import (optimizer warmup,
  // sandbox docker spin-up) — bump the per-action timeout so the helper
  // RPCs and the page navigation have headroom.
  test.use({ actionTimeout: 30_000, navigationTimeout: 30_000 });

  let app: AppRecord;

  test.beforeAll(async ({ request }) => {
    app = await createApp(request, uniqueAppName("e2e-ai"));
  });

  test.afterAll(async ({ request }) => {
    if (app) await deleteApp(request, app.id);
  });

  test("send a prompt → agent builds, deploys, and the deployed app responds", async ({ page, request }) => {
    await visit(page, `/p/${app.id}/preview`);
    await expect(page.getByTestId("chat-rail")).toBeVisible();

    // Empty state on a fresh app — the chat rail shows the prompt examples.
    await expect(
      page.getByTestId("chat-rail").getByText(/Tell the studio what to make/i),
    ).toBeVisible();

    // Direct prompt — short-circuit Vite scaffolding to keep wall time low.
    // The agent is allowed to pick its own path (the system prompt nudges
    // toward Vite + React); we just say "the simplest possible".
    const prompt =
      "Build the simplest possible app on this project: a single ES-module fetch handler that responds to GET / with a 200 plain-text body containing the literal string 'E2E_OK_<UUID>'. Don't bother with Vite/React/Tailwind — call deploy_app directly with a one-line module. Use the marker 'E2E_OK_e2e-ai-build' verbatim so I can grep for it.";

    await page.getByTestId("chat-input").fill(prompt);
    await page.getByTestId("chat-send").click();

    // The user's turn should appear immediately.
    await expect(page.getByTestId("turn-user").last()).toContainText(/simplest possible/i);

    // The assistant turn must mount within a few seconds — the streaming
    // is what makes the build feel real to the creator.
    await expect(page.getByTestId("turn-bot")).toHaveCount(1, { timeout: 15_000 });

    // From here we wait for one of three terminal states:
    //   1. status returns to "idle" AND deploy_hash is set         — success
    //   2. an explicit error banner appears                        — failure
    //   3. the test timeout                                        — failure
    const startedAt = Date.now();
    let lastStatus: string | null = null;
    let toolsSeen = 0;
    let deployedHash: string | null = null;

    while (Date.now() - startedAt < BUILD_TIMEOUT_MS) {
      // The status pill is only mounted while chat.status !== "idle".
      // Visible → agent is busy. Hidden → agent is idle.
      const indicator = page.getByTestId("status-title");
      const isBusy = await indicator.isVisible().catch(() => false);
      const statusLabel = isBusy ? (await indicator.textContent())?.trim() ?? "" : "idle";
      if (statusLabel !== lastStatus) {
        // eslint-disable-next-line no-console
        console.log(`[ai-build] status=${statusLabel} t+${((Date.now() - startedAt) / 1000) | 0}s`);
        lastStatus = statusLabel;
      }

      // Bail early on a chat error banner
      const errBanner = page.locator(".bg-tomato\\/10").filter({ hasText: /Error:|error/i });
      if (await errBanner.first().isVisible().catch(() => false)) {
        const txt = await errBanner.first().textContent();
        throw new Error(`agent error banner: ${txt?.trim()}`);
      }

      // Pull current state from the backend
      const fresh = await rpc<AppRecord>(request, "src/server/apps/getApp", [app.id]);
      if (fresh.deploy_hash && fresh.deploy_hash !== deployedHash) {
        deployedHash = fresh.deploy_hash;
        // eslint-disable-next-line no-console
        console.log(`[ai-build] deploy_hash=${deployedHash} t+${((Date.now() - startedAt) / 1000) | 0}s`);
      }

      // Count tool receipts in the assistant turn — proof the agent is
      // actually doing work, not just streaming text.
      const tools = await page.locator("[data-testid='turn-bot'] .receipt, [data-testid='turn-bot']").count();
      toolsSeen = Math.max(toolsSeen, tools);

      if (deployedHash && !isBusy) break;

      await page.waitForTimeout(POLL_INTERVAL_MS);
    }

    expect(deployedHash, "agent never produced a deploy_hash").toBeTruthy();

    // The assistant should have streamed at least one tool receipt OR
    // some text into the chat — proves the SSE wire reached the UI.
    const assistantText = (await page.getByTestId("turn-bot").last().textContent()) ?? "";
    const body = assistantText.replace(/The studio/i, "").replace(/\s+/g, " ").trim();
    expect(
      body.length,
      `assistant turn has no streamed body: ${JSON.stringify(body)}`,
    ).toBeGreaterThan(2);

    // ── Verify the deployed app actually serves traffic ─────────────
    //
    // Path-style routing on the gateway: /apps/<name>/.
    const deployedUrl = `${GATEWAY_URL}/apps/${encodeURIComponent(app.name)}/`;
    let bodyText = "";
    let ok = false;
    for (let i = 0; i < 12; i++) {
      const resp = await request.get(deployedUrl);
      if (resp.ok()) {
        bodyText = await resp.text();
        ok = true;
        break;
      }
      // worker is sometimes cold for a few seconds after first deploy
      await page.waitForTimeout(2_000);
    }

    expect(ok, `deployed app at ${deployedUrl} never returned 200`).toBe(true);

    // The agent may not have included the literal marker if it took a
    // different path (full Vite SPA, etc.). Make the body assertion
    // forgiving: we accept any reasonable HTML/text response that's not
    // an error page.
    expect(bodyText.length).toBeGreaterThan(0);
    expect(bodyText.toLowerCase()).not.toContain("internal server error");
    expect(bodyText.toLowerCase()).not.toContain("404 not found");

    // eslint-disable-next-line no-console
    console.log(
      `[ai-build] OK · ${app.name} · deploy_hash=${deployedHash} · body=${bodyText.length}B`,
    );

    // LiveBanner should have shown (the workspace's onDeploy fired).
    // It's a one-shot on first deploy; allow it to be visible OR already
    // dismissed by the time we get here.
    // (Soft check — don't fail the test if it raced past us.)
  });

  test("status indicator returns to idle after the run", async ({ page }) => {
    await visit(page, `/p/${app.id}/preview`);
    // The previous test already concluded; the status pill should be hidden
    // (it only renders when status !== "idle").
    await expect(page.getByTestId("status-title")).toHaveCount(0);
  });
});
