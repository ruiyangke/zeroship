import { fileURLToPath } from "node:url";

/** The render modes the specs cover, each demonstrated by one example app. */
export const EXAMPLES = { csr: "csr-todo", ssr: "ssr-blog", ssg: "ssg-docs" } as const;

export type AppKind = keyof typeof EXAMPLES;

/**
 * What global setup hands the specs: where the gateway listens and the name
 * each example was deployed under. Every kind has a name - setup either
 * deploys all of them or fails.
 */
export interface StackDescriptor {
  gatePort: number;
  apps: Record<AppKind, string>;
}

/** Written by global setup, removed by its teardown, gitignored. */
export const DESCRIPTOR = fileURLToPath(new URL("../.stack.json", import.meta.url));
