import { inject } from "vitest";
export interface Target { name: string; apiUrl: string; uiUrl: string }
export interface S3Fixture { endpoint: string; bucket: string; prefix: string; appId: string; workerPid: number }
declare module "vitest" {
  export interface ProvidedContext {
    storageTargets: Target[];
    storageArtifacts: string;
    storageS3: S3Fixture;
  }
}
export function targets(): Target[] {
  const values = inject("storageTargets");
  if (!values?.length) throw new Error("Storage fixture did not provide targets");
  return values;
}
