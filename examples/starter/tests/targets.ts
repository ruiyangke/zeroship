import { inject } from "vitest";

export interface Target { name: string; apiUrl: string; uiUrl: string; log?: string }
declare module "vitest" {
  export interface ProvidedContext { starterTargets: Target[]; starterArtifacts: string }
}

export function targets(): Target[] {
  const values = inject("starterTargets");
  if (!values?.length) throw new Error("The starter fixture provided no targets");
  return values;
}
