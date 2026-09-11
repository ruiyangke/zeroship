import { inject } from "vitest";
export interface Target { name: string; apiUrl: string; uiUrl: string }
declare module "vitest" {
  export interface ProvidedContext { workflowTargets: Target[]; workflowArtifacts: string }
}
export function targets(): Target[] {
  const values = inject("workflowTargets");
  if (!values?.length) throw new Error("Workflow fixture did not provide targets");
  return values;
}
