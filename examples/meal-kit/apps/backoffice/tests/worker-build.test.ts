// The back office's worker, built the way `pnpm build` builds it: the `vite`
// CLI, in a process that inherits nothing from this one's environment.
//
// The server's sample loader translates seeded recipes with the zh catalog
// Lingui's Vite plugin compiles, the same compilation the browser imports. So
// the worker must bundle the PO source itself and no catalog compiler of its
// own, and its `loadSampleMenus` must write the translations the browser's
// compiled catalog gives.

import { spawnSync } from "node:child_process";
import { readFileSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { beforeAll, describe, expect, it } from "vitest";
import { setupI18n } from "@lingui/core";
import { env } from "zeroship";
import { messages as zh } from "@gather/meal-kit/locales/zh/messages.po";
import { marketOfferings, recipes, seedRecipeDraft } from "../src/seed-catalog";
import { memoryTx } from "./fixtures/tx";
import { workerModulesPath } from "./fixtures/worker-graph.vite.config";

const app = fileURLToPath(new URL("..", import.meta.url));
const zhCatalog = fileURLToPath(
  new URL("../../../packages/shared/locales/zh/messages.po", import.meta.url),
);
const vite = join(
  dirname(createRequire(import.meta.url).resolve("vite/package.json")),
  "bin/vite.js",
);

beforeAll(() => {
  rmSync(workerModulesPath, { force: true });
  const run = spawnSync(
    process.execPath,
    [vite, "build", "--config", "tests/fixtures/worker-graph.vite.config.ts"],
    { cwd: app, env: {}, encoding: "utf8" },
  );
  expect(run.status, run.stderr + run.stdout).toBe(0);
}, 300_000);

describe("the back office's built worker", () => {
  it("bundles the zh PO source, and no catalog compiler of its own", () => {
    const modules: string[] = JSON.parse(readFileSync(workerModulesPath, "utf8"));
    const packaged = (name: string) =>
      modules.some((id) => id.includes(`/node_modules/${name}/`));
    // Packages appear in the list by their installed path.
    expect(packaged("@lingui/core")).toBe(true);
    expect(modules.map((id) => id.split("?")[0])).toContain(zhCatalog);
    expect(packaged("@lingui/format-po")).toBe(false);
    expect(packaged("@lingui/message-utils")).toBe(false);
    expect(packaged("js-sha256")).toBe(false);
  });

  it("seeds the sample menus with the browser's zh translations", async () => {
    const db = memoryTx();
    const admin = { id: "pws_sampleadministrator1", email: "admin@gather.example", name: "Admin" };
    Object.assign(env as Record<string, unknown>, {
      GATHER_ADMIN_IDS: admin.id,
      auth: { getUser: () => admin },
      db: {
        transaction: async (fn: (tx: unknown) => Promise<unknown>) => ({ data: await fn(db.tx) }),
      },
    });
    const worker = await import(
      /* @vite-ignore */ pathToFileURL(join(app, "dist/server/index.js")).href
    );
    const binding = worker.default.rpc["gather.loadSampleMenus"];
    const loadSampleMenus = typeof binding === "function" ? binding : await binding.load();
    expect(await loadSampleMenus({ market: "us" })).toEqual({ ready: true });

    const browser = setupI18n({ locale: "zh", messages: { zh } });
    const seeded = db.table("meal_recipes").rows;
    const expected = recipes.filter((recipe) => Object.hasOwn(marketOfferings.us, recipe.id));
    expect(seeded.map((row) => row.slug)).toEqual(expected.map((recipe) => recipe.id));
    for (const recipe of expected) {
      const row = seeded.find((candidate) => candidate.slug === recipe.id)!;
      expect((row.draft as { translations: unknown }).translations).toEqual(
        seedRecipeDraft(recipe, (text) => browser._(text)).translations,
      );
    }
    const lemon = seeded.find((row) => row.slug === "lemon-chicken")!;
    expect((lemon.draft as { translations: { zh: { name: string } } }).translations.zh.name).toBe(
      "柠檬香草烤鸡",
    );
  });
});
