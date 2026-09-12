import { inject } from "vitest";

declare module "vitest" {
  export interface ProvidedContext {
    dbExample: { apiUrl: string; uiUrl: string; logs: string };
  }
}

export function target() {
  const target = inject("dbExample");
  if (!target) throw new Error("Vitest must provision the database example");
  return target;
}
