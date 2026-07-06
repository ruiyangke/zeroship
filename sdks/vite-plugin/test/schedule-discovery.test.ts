import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { parse as acornParse } from "acorn";

import { computeManifestExtras } from "../src/manifest.js";
import { transformPlugin, type TransformState } from "../src/transform.js";

function makeState(): TransformState {
  return {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
    discoveredSchedules: [],
  };
}

function makeCtx(envName: string) {
  return {
    environment: { name: envName },
    parse(code: string, _opts: { lang?: string }) {
      return acornParse(code, {
        ecmaVersion: 2024,
        sourceType: "module",
        allowImportExportEverywhere: true,
      });
    },
    warnings: [] as string[],
    warn(msg: string) {
      this.warnings.push(msg);
    },
  };
}

function getHandler(plugin: ReturnType<typeof transformPlugin>): any {
  return typeof (plugin.transform as any) === "function"
    ? (plugin.transform as any)
    : (plugin.transform as any).handler;
}

async function withRoot<T>(fn: (root: string) => Promise<T>): Promise<T> {
  const root = join(tmpdir(), `schedule-discovery-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  try {
    return await fn(root);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

describe("schedule discovery", () => {
  test("discovers schedule registrations and emits manifest schedules", async () => {
    await withRoot(async (root) => {
      const state = makeState();
      const plugin = transformPlugin("/_rpc", state);
      (plugin.configResolved as (c: unknown) => void).call(plugin, { root });

      const code = `
import { Workflow } from "@zeroship/workflows";
import { schedule, every } from "@zeroship/workflows/schedule";

class NightlyReport extends Workflow {}

schedule({
  name: "nightly-report-us",
  schedule: every.day.at("02:30", "America/New_York"),
  workflow: NightlyReport,
  input: { region: "us" },
  overlap: "skipIfRunning",
  catchUp: { mode: "backfill", max: 3 },
});
`;
      getHandler(plugin).call(makeCtx("ssr"), code, `${root}/src/schedules.ts`);

      assert.equal(state.discoveredSchedules.length, 1);
      const extras = await computeManifestExtras({
        root,
        procedures: [],
        schedules: state.discoveredSchedules,
        mode: "development",
      });

      assert.deepEqual(extras.schedules, [
        {
          name: "nightly-report-us",
          workflowName: "NightlyReport",
          input: { region: "us" },
          overlap: "skipIfRunning",
          catchUp: { mode: "backfill", max: 3 },
          schedule: {
            kind: "cron",
            cron_expr: "30 2 * * *",
            tz: "America/New_York",
            overlap: "skipIfRunning",
            catchUp: { mode: "backfill", max: 3 },
          },
        },
      ]);
    });
  });

  test("bad schedule fails manifest compilation with a clear error", async () => {
    await withRoot(async (root) => {
      const state = makeState();
      const plugin = transformPlugin("/_rpc", state);
      (plugin.configResolved as (c: unknown) => void).call(plugin, { root });

      const code = `
import { Workflow } from "@zeroship/workflows";
import { schedule, cronExpr } from "@zeroship/workflows/schedule";

class HeartBeat extends Workflow {}

schedule({
  name: "bad-cron",
  schedule: cronExpr("* * * * * *"),
  workflow: HeartBeat,
});
`;
      getHandler(plugin).call(makeCtx("ssr"), code, `${root}/src/schedules.ts`);

      await assert.rejects(
        () =>
          computeManifestExtras({
            root,
            procedures: [],
            schedules: state.discoveredSchedules,
            mode: "development",
          }),
        /invalid workflow schedule "bad-cron".*sub-minute cron is unsupported/s,
      );
    });
  });
});
