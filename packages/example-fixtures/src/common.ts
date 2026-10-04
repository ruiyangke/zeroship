import assert from "node:assert/strict";
import { createServer } from "node:net";

/** A live fixture target an example suite points its client at. */
export interface Target {
  name: string;
  apiUrl: string;
  uiUrl: string;
  /** The log the target's server side writes, for a suite that reports from it. */
  log?: string;
}

/** What a fixture hands an example's settings to name the targets it serves. */
export interface TargetContext {
  /** The gateway a deployed target is reached through. */
  gateway: Port;
  /** The dev runtime, and the Vite dev server in front of it. */
  dev: Port;
  ui: Port;
  /** The log the service started under `name` writes: `dev` for Vite and its runtime, `worker` for the deployed app. */
  log(name: string): string;
}

/** The S3 endpoint a storage example suite reaches outside the worker. */
export interface S3Fixture {
  endpoint: string;
  bucket: string;
  prefix: string;
  appId: string;
  workerPid: number;
}

/** The worker a storage example suite drives directly. */
export interface WorkerFixture {
  url: string;
  appId: string;
  gatewayKey: string;
}

export interface Port {
  number: number;
  url: string;
  release(): Promise<void>;
}

/** Bind an ephemeral loopback port, released when the fixture closes it. */
export async function reservePort(): Promise<Port> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  assert(address && typeof address !== "string");
  return {
    number: address.port,
    url: `http://127.0.0.1:${address.port}`,
    release: () => new Promise<void>((resolve, reject) => {
      if (!server.listening) return resolve();
      server.close((error) => error ? reject(error) : resolve());
    }),
  };
}
