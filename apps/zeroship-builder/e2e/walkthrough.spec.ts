// ─── Walkthrough — the canonical "make a todo app" creator flow ──
//
// One test that walks the entire user journey:
//   home → type prompt → submit → wizard step 1 → blank → step 2 →
//   tweak slug → next → step 3 → Begin → /p/:id/preview →
//   chat auto-fires → agent deploys → RPC endpoints work.
//
// Captures a screenshot at each stage. Skipped unless E2E_AI_BUILD=1.

import { test, expect } from "@playwright/test";
import { deleteApp, listApps, rpc, visit, type AppRecord } from "./helpers";
import * as fs from "node:fs";

// Common export-name patterns the agent picks for a todo backend. We
// probe these instead of inspecting the deployed bundle source — the
// `/internal/bundles/<id>` endpoint that used to serve raw JS for
// introspection went away in the artifact-layout redesign (Phase 7).
const LIST_CANDIDATES = [
  "listTodos", "getTodos", "todos", "items", "listItems",
  "all", "getAll", "list", "fetch", "fetchTodos", "getList",
  "index",
];
const ADD_CANDIDATES = [
  "addTodo", "createTodo", "newTodo", "insertTodo", "saveTodo",
  "add", "create", "new", "post", "insert", "save",
];

const enabled = process.env.E2E_AI_BUILD === "1";
const SHOTS = "/tmp/zsb-walkthrough";
const GATEWAY_URL = process.env.E2E_GATEWAY_URL ?? "http://localhost:8001";
const BUILD_TIMEOUT_MS = 5 * 60 * 1000;

test.describe("walkthrough — minimal creator flow", () => {
  test.skip(!enabled, "set E2E_AI_BUILD=1 with OPENAI_API_KEY in dev runtime");

  test.setTimeout(BUILD_TIMEOUT_MS + 60_000);
  test.use({ actionTimeout: 30_000, navigationTimeout: 30_000 });

  test.beforeAll(() => {
    fs.mkdirSync(SHOTS, { recursive: true });
  });

  test("home → wizard → workspace → agent ships → RPC works", async ({ page, request }) => {
    page.on("pageerror", (e) => console.log("[browser:err]", e.message));
    page.on("console", (m) => {
      const t = m.type();
      if (t === "log" || t === "warning" || t === "error" || t === "info") {
        console.log(`[browser:${t}]`, m.text());
      }
    });
    let createdAppId: string | null = null;
    let createdAppName: string | null = null;
    let apiKey: string | null = null;

    try {
      // 1. Home
      await visit(page, "/");
      await page.screenshot({ path: `${SHOTS}/01-home.png`, fullPage: true });
      await expect(page.getByRole("heading", { name: /What will you/i })).toBeVisible();

      // 2. Type prompt + submit
      await page
        .getByTestId("home-prompt")
        .fill(
          "Build me a working todo list. I want to add items, mark them done, and delete them.",
        );
      await page.screenshot({ path: `${SHOTS}/02-typed.png`, fullPage: true });

      await page.getByTestId("home-submit").click();
      await expect(page).toHaveURL(/\/new\?prompt=/);
      await expect(page.getByTestId("wiz-step-1")).toBeVisible();
      await page.screenshot({ path: `${SHOTS}/03-wizard-step1.png`, fullPage: true });

      // 3. Skip the gallery
      await page.getByTestId("wiz-blank").click();
      await expect(page.getByTestId("wiz-step-2")).toBeVisible();
      await page.screenshot({ path: `${SHOTS}/04-wizard-step2.png`, fullPage: true });

      // The home prompt should have carried into the wizard textarea.
      await expect(page.getByTestId("wiz-prompt")).toHaveValue(/todo list/i);

      // 4. Set a deterministic slug + name, advance.
      const slug = `walk-${Math.random().toString(36).slice(2, 8)}`;
      await page.getByTestId("wiz-slug").fill(`${slug}.zeroship.app`);
      await page.getByTestId("wiz-name").fill("Walk Todo");
      await page.screenshot({ path: `${SHOTS}/05-wizard-filled.png`, fullPage: true });

      await page.getByTestId("wiz-next").click();
      await expect(page.getByTestId("wiz-step-3")).toBeVisible();
      await page.screenshot({ path: `${SHOTS}/06-wizard-step3.png`, fullPage: true });

      // 5. Begin → real createApp → /p/:id/preview
      await page.getByTestId("wiz-begin").click();
      await page.waitForURL(/\/p\/[a-f0-9-]+\/preview/, { timeout: 30_000 });
      const m = page.url().match(/\/p\/([^/]+)\/preview/);
      expect(m).not.toBeNull();
      createdAppId = m![1];

      const apps = await listApps(request);
      const row = apps.find((a) => a.id === createdAppId);
      expect(row, "app row should exist after Begin").toBeTruthy();
      createdAppName = row!.name;

      const fresh = await rpc<AppRecord & { api_key?: string }>(
        request,
        "src/server/apps/getApp",
        [createdAppId!],
      );
      apiKey = (fresh as any).api_key ?? null;
      await page.screenshot({ path: `${SHOTS}/07-workspace-fresh.png`, fullPage: true });

      // Diagnostic: what's in sessionStorage right after mount?
      const sessionState = await page.evaluate(() => ({
        pending: sessionStorage.getItem("zeroship_pending_prompt"),
        keys: Object.keys(sessionStorage),
      }));
      console.log("[walk] sessionStorage at mount:", sessionState);

      // 6. The pending prompt should auto-send into the chat.
      await expect(page.getByTestId("turn-user").last()).toContainText(/todo list/i, {
        timeout: 30_000,
      });
      await page.screenshot({ path: `${SHOTS}/08-chat-fired.png`, fullPage: true });

      // 7. Wait for the agent to deploy. We exit as soon as deploy_hash
      //    is set AND either (a) the chat returned to idle OR (b) we've
      //    waited 30s after the deploy for the agent to wrap up. The
      //    agent often keeps streaming explanatory text after a
      //    successful deploy — we don't need to wait for it.
      const startedAt = Date.now();
      let lastStatus: string | null = null;
      let deployHash: string | null = null;
      let deployAt = 0;
      while (Date.now() - startedAt < BUILD_TIMEOUT_MS) {
        const indicator = page.getByTestId("status-title");
        const isBusy = await indicator.isVisible().catch(() => false);
        const status = isBusy
          ? (await indicator.textContent())?.trim() ?? ""
          : "idle";
        if (status !== lastStatus) {
          console.log(`[walk] status=${status} t+${((Date.now() - startedAt) / 1000) | 0}s`);
          lastStatus = status;
        }

        const errBanner = page
          .locator(".bg-tomato\\/10")
          .filter({ hasText: /Error:|error/i });
        if (await errBanner.first().isVisible().catch(() => false)) {
          await page.screenshot({ path: `${SHOTS}/99-chat-error.png`, fullPage: true });
          throw new Error(
            `agent error banner: ${(await errBanner.first().textContent())?.trim()}`,
          );
        }

        const f = await rpc<AppRecord & { api_key?: string }>(
          request,
          "src/server/apps/getApp",
          [createdAppId!],
        );
        if (f.deploy_hash && f.deploy_hash !== deployHash) {
          deployHash = f.deploy_hash;
          deployAt = Date.now();
          if ((f as any).api_key && typeof (f as any).api_key === "string") {
            apiKey = (f as any).api_key;
          }
          console.log(
            `[walk] deploy_hash=${deployHash} api_key=${apiKey ? "set" : "none"} t+${((Date.now() - startedAt) / 1000) | 0}s`,
          );
        }

        if (deployHash && !isBusy) break;
        if (deployHash && Date.now() - deployAt > 30_000) {
          console.log("[walk] deploy_hash set, agent still busy; proceeding anyway");
          break;
        }
        await page.waitForTimeout(2_000);
      }
      expect(deployHash, "agent never produced a deploy_hash").toBeTruthy();
      expect(typeof apiKey, "api_key should be a string").toBe("string");
      await page.screenshot({ path: `${SHOTS}/09-deployed.png`, fullPage: true });

      // 8. Verify the deployed app is reachable + behaves as a todo backend.
      const base = `${GATEWAY_URL}/apps/${createdAppName}/_rpc`;
      const headers = { "x-api-key": apiKey!, "content-type": "application/json" };

      // Discover the agent's exports by probing common names. The
      // `/internal/bundles/<id>` source-fetch endpoint disappeared in
      // the artifact-layout redesign — bundle bytes are now content-
      // addressed in the BlobStore and aren't exposed individually.
      // Probe the candidates and use whichever the gateway accepts.
      const probeName = async (name: string): Promise<boolean> => {
        const r = await request.post(`${base}/${name}`, { data: [], headers });
        return r.ok();
      };

      // Wait for the gateway to be ready (route + worker isolate).
      // Any successful probe also confirms the route is live.
      let listExport: string | null = null;
      const ready = Date.now() + 15_000;
      while (Date.now() < ready && listExport == null) {
        for (const n of LIST_CANDIDATES) {
          if (await probeName(n)) {
            listExport = n;
            break;
          }
        }
        if (listExport == null) await page.waitForTimeout(500);
      }
      expect(listExport, "no list-like export reachable").toBeTruthy();

      const probed: { list: string; add: string | null } = {
        list: listExport!,
        add: null,
      };
      // Find an add-like export. Empty-args call may legitimately 4xx
      // (e.g., "title required") — treat any non-404 as "exists".
      for (const n of ADD_CANDIDATES) {
        const r = await request.post(`${base}/${n}`, { data: [], headers });
        if (r.status() !== 404) {
          probed.add = n;
          break;
        }
      }
      console.log(`[walk] mapped to:`, probed);
      expect(probed.add, "no add-like export found").toBeTruthy();

      // Drive the deployed app like a real user would.
      const post = async (mname: string, args: any[]) => {
        const r = await request.post(`${base}/${mname}`, { data: args, headers });
        if (!r.ok())
          throw new Error(`${mname} → ${r.status()} ${await r.text()}`);
        try {
          return await r.json();
        } catch {
          return null;
        }
      };

      const initial = await post(probed.list!, []);
      console.log(`[walk] initial list:`, JSON.stringify(initial).slice(0, 200));

      // The add function may take (string), ({title}), ({description}),
      // ({text}) — pass every common shape and accept whichever sticks.
      const ADD_VARIANTS = [
        ["buy milk"],
        [{ title: "buy milk" }],
        [{ description: "buy milk" }],
        [{ text: "buy milk" }],
        [{ name: "buy milk" }],
      ];
      let created: any = null;
      for (const args of ADD_VARIANTS) {
        try {
          created = await post(probed.add!, args);
          if (created && JSON.stringify(created).toLowerCase().includes("buy milk")) break;
        } catch {
          // try next shape
        }
      }
      console.log(`[walk] created:`, JSON.stringify(created).slice(0, 200));
      expect(
        created && JSON.stringify(created).toLowerCase().includes("buy milk"),
        `agent's add fn never accepted a "buy milk" payload`,
      ).toBeTruthy();

      const after = await post(probed.list!, []);
      console.log(`[walk] list after add:`, JSON.stringify(after).slice(0, 400));
      const persisted = JSON.stringify(after).toLowerCase().includes("buy milk");
      expect(persisted, "todo not persisted across list call").toBe(true);

      console.log(`[walk] OK — agent built a working todo backend`);
    } finally {
      if (createdAppId) await deleteApp(request, createdAppId);
    }
  });
});
