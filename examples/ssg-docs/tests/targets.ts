import { inject } from "vitest";

export interface Target { name: string; apiUrl: string; uiUrl: string; log?: string }
declare module "vitest" {
  export interface ProvidedContext { ssgTargets: Target[]; ssgArtifacts: string }
}

export function targets(): Target[] {
  const values = inject("ssgTargets");
  if (!values?.length) throw new Error("The ssg-docs fixture provided no targets");
  return values;
}

export function target(name: string): Target {
  const found = targets().find((candidate) => candidate.name === name);
  if (!found) throw new Error(`The ssg-docs fixture serves no ${name} target`);
  return found;
}
