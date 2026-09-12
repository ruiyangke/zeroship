import { inject } from "vitest";

export interface Target { name: string; apiUrl: string; uiUrl: string }
declare module "vitest" {
  export interface ProvidedContext { databaseTargets: Target[]; databaseArtifacts: string }
}
export function targets() {
  const values = inject("databaseTargets");
  if (!values?.length) throw new Error("Vitest must provision the database targets");
  return values;
}
