import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { ServicesPlatform, prepare, type ServicesSettings } from "@zeroship/example-testkit";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: ServicesSettings = {
  exampleDir,
  label: "SSR",
  workDir: "zeroship-ssr-blog-",
  cleanupMessage: "Failed to clean up the ssr-blog fixture",
  cancelMessage: "ssr-blog fixture cancelled",
  database: { name: "ssr_fixture", password: "ssr-fixture-password" },
  issuer: { kid: "ssr-blog-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read" },
  owner: { seed: "ssr-blog-fixture-owner", emailPrefix: "ssr", name: "ssr-blog fixture owner" },
  signer: typedIdFromStableSeed("wjs", "ssr-blog-fixture-signer"),
  services: ["control", "gateway"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server"],
  appName: "ssr-blog",
  buildNoun: "ssr-blog",
  // Every document is rendered per request by the worker, and the browser
  // that asks for one, and then for the posts again once it hydrates, holds no
  // session.
  manifest: (manifest) => {
    assert(manifest.worker?.entry, "an SSR bundle must carry the worker that renders its documents");
    const documents = manifest.resources?.["/[...rest]"];
    assert(documents, "the bundle must route documents");
    assert.equal(documents.static, undefined, "documents must be rendered by the worker, not served from assets");
    assert.equal(documents.auth, "anonymous", "documents must admit the anonymous browser");
    assert.equal(manifest.resources?.["rpc:listPosts"]?.auth, "anonymous", "rpc:listPosts must admit the anonymous browser");
  },
  typedAppAssert: true,
  blobs: async (_backing, work) => {
    const blobs = join(work, "blobs");
    await mkdir(blobs);
    return blobs;
  },
  devEnvVar: "ZEROSHIP_DEV_PORT",
  targets: ({ dev, ui, gateway, log }) => [
    { name: "dev", apiUrl: dev.url, uiUrl: ui.url, log: log("dev") },
    { name: "deployed", apiUrl: `${gateway.url}/apps/ssr-blog`, uiUrl: `http://ssr-blog.localhost:${gateway.number}`, log: log("worker") },
  ],
  readiness: () => ({ path: "/" }),
};

export class Platform extends ServicesPlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`SSR fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
