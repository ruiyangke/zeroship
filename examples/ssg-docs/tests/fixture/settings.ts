import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { ServicesPlatform, prepare, type ServicesSettings } from "@zeroship/example-fixtures";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

/** The documents the suite loads. */
const PAGES = ["/index.html", "/about.html"];

const settings: ServicesSettings = {
  exampleDir,
  label: "SSG",
  workDir: "zeroship-ssg-docs-",
  cleanupMessage: "Failed to clean up the ssg-docs fixture",
  cancelMessage: "ssg-docs fixture cancelled",
  database: { name: "ssg_fixture", password: "ssg-fixture-password" },
  issuer: { kid: "ssg-docs-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read" },
  owner: { seed: "ssg-docs-fixture-owner", emailPrefix: "ssg", name: "ssg-docs fixture owner" },
  signer: typedIdFromStableSeed("wjs", "ssg-docs-fixture-signer"),
  services: ["control", "gateway"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server"],
  appName: "ssg-docs",
  buildNoun: "ssg-docs",
  // A static deploy carries no worker and serves every route from its assets.
  // A browser cannot tell that apart from a worker returning the same HTML, so
  // the suite's claim that the pages are static rests on this. Requiring the
  // pages it loads keeps the check from passing over an empty manifest.
  manifest: (manifest) => {
    assert.equal(manifest.worker ?? null, null, "a static bundle must declare no worker");
    for (const page of PAGES) assert(page in manifest.assets, `the bundle ships no ${page}`);
    const routes = Object.entries(manifest.resources as Record<string, { static?: unknown }>);
    assert(routes.length > 0, "the bundle declares no routes");
    for (const [path, resource] of routes) assert(resource.static, `route ${path} is not served from assets`);
  },
  typedAppAssert: true,
  blobs: async (_backing, work) => {
    const blobs = join(work, "blobs");
    await mkdir(blobs);
    return blobs;
  },
  // `vite dev` serves no prerendered page: the build copies them out of
  // content/. So this suite has the deployed target alone, and the gateway
  // serves it without the worker.
  targets: ({ gateway, log }) => [
    { name: "deployed", apiUrl: `${gateway.url}/apps/ssg-docs`, uiUrl: `http://ssg-docs.localhost:${gateway.number}`, log: log("gateway") },
  ],
  readiness: () => ({ path: "/about" }),
};

export class Platform extends ServicesPlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`SSG fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
