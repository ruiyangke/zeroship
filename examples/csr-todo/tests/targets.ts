import { inject } from "vitest";

export interface Target { name: string; apiUrl: string; uiUrl: string; log?: string }
declare module "vitest" {
  export interface ProvidedContext { csrTargets: Target[]; csrArtifacts: string }
}

export function targets(): Target[] {
  const values = inject("csrTargets");
  if (!values?.length) throw new Error("The csr-todo fixture provided no targets");
  return values;
}

export function target(name: string): Target {
  const found = targets().find((candidate) => candidate.name === name);
  if (!found) throw new Error(`The csr-todo fixture serves no ${name} target`);
  return found;
}
