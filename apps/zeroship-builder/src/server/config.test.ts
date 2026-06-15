"use server";

import { describe, expect, it } from "vitest";

import appConfig from "./config";
import * as projectProcedures from "./projects";

type DefinedAppLike = {
  definition?: {
    resources?: Record<string, { auth?: string; rateLimit?: unknown }>;
  };
};

function resources() {
  return ((appConfig as DefinedAppLike).definition?.resources ?? {});
}

function projectProcedureIds(): string[] {
  const ids: string[] = [];
  for (const value of Object.values(projectProcedures) as unknown[]) {
    if (typeof value !== "function") continue;
    const id = (value as { config?: { id?: unknown } }).config?.id;
    if (typeof id === "string" && id.startsWith("projects.")) {
      ids.push(id);
    }
  }
  return ids;
}

describe("builder resource policy", () => {
  it("declares the projects RPC namespace that project procedures inherit", () => {
    const r = resources();

    expect(r).toHaveProperty("rpc:projects");
    expect(r["rpc:projects"]).toMatchObject({
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
    });
    expect(r).not.toHaveProperty("rpc:apps");
  });

  it("keeps every projects.* procedure under the protected namespace", () => {
    const ids = projectProcedureIds();

    expect(ids.length).toBeGreaterThan(0);
    expect(ids.every((id) => id.startsWith("projects."))).toBe(true);
  });
});
