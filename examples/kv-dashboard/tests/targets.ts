import { inject } from "vitest";

export interface Target {
  name: string;
  apiUrl: string;
  uiUrl: string;
}

declare module "vitest" {
  export interface ProvidedContext {
    dashboardTargets: Target[];
    dashboardArtifacts: string;
    dashboardExisting: boolean;
  }
}

export function targets(): Target[] {
  const provisioned = inject("dashboardTargets");
  if (provisioned) return provisioned;
  if (!inject("dashboardExisting")) throw new Error("Dashboard fixture did not provide test targets");
  const configured = process.env.KV_DASHBOARD_TARGETS;
  const values: unknown = configured ? JSON.parse(configured) : [{
    name: "existing server",
    apiUrl: process.env.ZEROSHIP_URL ?? "http://localhost:3011",
    uiUrl: process.env.KV_DASHBOARD_UI_URL ?? "http://localhost:5173",
  }];
  if (!Array.isArray(values) || values.length === 0) throw new Error("No dashboard test targets");
  return values.map((value) => {
    for (const field of ["name", "apiUrl", "uiUrl"]) {
      if (typeof value?.[field] !== "string" || !value[field]) throw new Error(`Missing target ${field}`);
    }
    for (const field of ["apiUrl", "uiUrl"]) {
      const url = new URL(value[field]);
      if (!["http:", "https:"].includes(url.protocol)) throw new Error(`Invalid target ${field}`);
    }
    return value as Target;
  });
}
