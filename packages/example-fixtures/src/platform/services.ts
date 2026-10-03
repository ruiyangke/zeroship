import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { isTypedId } from "@zeroship/server/typed-id";
import { PlatformBase, readManifest, prepare, type Backing, type FixtureSettings } from "./base";
import type { ManagedProcess } from "../processes";
import type { Port, S3Fixture, Target, WorkerFixture } from "../common";

export type { Backing } from "./base";

export interface PostDeployContext {
  id: string;
  gateway: Port;
  worker: Port;
  workerProcess: ManagedProcess;
  backing: Backing;
  keys: Record<string, string>;
}

export interface ServicesSettings extends FixtureSettings {
  cargoPackages: string[];
  appName: string;
  /** The word after "build platform binaries and" in the bring-up log. */
  buildNoun: string;
  manifest: (manifest: any) => void;
  /** Whether the created app id must parse as a typed app id. */
  typedAppAssert?: boolean;
  /** The operator-assigned plan the app needs, named for the assertion. */
  plan?: "streaming";
  backing?: (platform: ServicesPlatform) => Promise<Backing>;
  sharedEnv?: (backing: Backing) => NodeJS.ProcessEnv;
  workerExtraArgs?: (backing: Backing) => string[];
  workerEnv?: (backing: Backing) => NodeJS.ProcessEnv;
  blobs: (backing: Backing, work: string) => Promise<string>;
  devEnvVar: string;
  targets: (ctx: { dev: Port; ui: Port; gateway: Port }) => Target[];
  readiness: (target: Target) => { path: string; body: unknown };
  postDeploy?: (ctx: PostDeployContext) => Promise<{ s3?: S3Fixture; worker?: WorkerFixture } | void>;
}

/**
 * A primitive-first example: the platform services come up, one app is created
 * and deployed through the control plane, and the fixture hands the suite a
 * local and a deployed target to drive.
 */
export class ServicesPlatform extends PlatformBase {
  s3?: S3Fixture;
  worker?: WorkerFixture;

  protected constructor(readonly settings: ServicesSettings, work: string, logs: string) {
    super(settings, work, logs);
  }

  static async create(settings: ServicesSettings): Promise<ServicesPlatform> {
    const { work, logs } = await prepare(settings);
    console.info(`${settings.label} fixture logs: ${logs}`);
    return new ServicesPlatform(settings, work, logs);
  }

  async start(): Promise<Target[]> {
    const { work, processes, settings } = this;
    console.info(`${settings.label} fixture: build platform binaries and ${settings.buildNoun}`);
    await this.buildBinaries(settings.cargoPackages);
    const { app, vite } = await this.copyApp(["node_modules", "dist", ".zeroship", "tests"]);
    const bundle = await this.buildBundle(app, vite);
    settings.manifest(await readManifest(bundle));

    console.info(`${settings.label} fixture: start backing containers and apply platform migrations`);
    const { authority, dsn } = await this.startPostgres("postgres:18");
    await this.applyMigrations(dsn);

    let backing: Backing = {};
    if (settings.backing) backing = await settings.backing(this);

    const { issuerUrl, bearer } = await this.startIdentity();
    const { keys, peers, joinToken, joinSigner, joinSigners, masterKey, broker } = await this.writeServiceIdentity();
    const shared = {
      ...(settings.sharedEnv?.(backing) ?? {}),
      ZEROSHIP_CONTROL_KEY: randomBytes(16).toString("hex"),
      ZEROSHIP_PAIRWISE_SALT: randomBytes(16).toString("hex"),
      ZEROSHIP_ORIGIN_SCHEME: "http", ZEROSHIP_AUTH_PLATFORM_ISSUER: issuerUrl,
      ZEROSHIP_OBSERVABILITY_LOG_FORMAT: "json",
    };
    const { cert, key } = await this.generateRelayCert();
    const blobs = await settings.blobs(backing, work);
    const control = await this.port();
    const worker = await this.port();
    const gateway = await this.port();
    const relay = await this.port();
    const service = this.service(shared);
    console.info(`${settings.label} fixture: start platform services`);
    await this.startRelay(shared, authority, cert, key, relay);
    await service("control", "zeroship-control", control, ["--no-config", "--port", `${control.number}`, "--blob-store", blobs, "--worker-urls", worker.url], {
      ZEROSHIP_CONTROL_DATABASE_URL: dsn, ZEROSHIP_CONTROL_MASTER_KEY: masterKey,
      ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING: "true", ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS: "127.0.0.1/32",
      ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS: `${worker.number}`,
      ZEROSHIP_CONTROL_SERVICE_KEY_FILE: keys.control, ZEROSHIP_CONTROL_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_CONTROL_JOIN_SIGNERS_FILE: joinSigners,
      ZEROSHIP_CONTROL_JOIN_TOKEN_SIGNER_FILE: joinSigner,
      ZEROSHIP_CONTROL_JOIN_TOKEN_FILE: joinToken,
      ZEROSHIP_CONTROL_JOIN_TOKEN_ZONE: "default",
    });
    await this.waitFor("control", () => this.httpReady(`${control.url}/readyz`));
    const workerProcess = await service("worker", "zeroship-worker", worker, [
      "--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1",
      ...(settings.workerExtraArgs?.(backing) ?? []),
    ], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_JOIN_TOKEN_FILE: joinToken, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
      ...(settings.workerEnv?.(backing) ?? {}),
    });
    await this.waitFor("worker", () => this.httpReady(`${worker.url}/readyz`));
    await service("gateway", "zeroship-gate", gateway, ["--no-config", "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker], {
      ZEROSHIP_GATEWAY_DATABASE_URL: dsn, ZEROSHIP_GATEWAY_SIGNING_KEY_FILE: keys.gateway,
      ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: masterKey, ZEROSHIP_GATEWAY_SERVICE_KEY_FILE: keys.gateway, ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("gateway", () => this.httpReady(`${gateway.url}/readyz`));

    console.info(`${settings.label} fixture: create and deploy the app`);
    const created = await fetch(`${control.url}/api/apps`, {
      method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" },
      body: JSON.stringify({ name: settings.appName }), signal: AbortSignal.any([processes.signal, AbortSignal.timeout(10_000)]),
    });
    assert(created.ok, `Create app: HTTP ${created.status}: ${created.ok ? "" : await created.text()}`);
    const { id } = await created.json();
    assert.equal(typeof id, "string", "Created app must have an id");
    if (settings.typedAppAssert) assert(isTypedId(id, "app"), `Created app must have a typed app id: ${id}`);
    if (settings.plan) {
      // The fixture is the operator. Creator requests cannot self-assign this tier.
      assert(this.postgres, "PostgreSQL must be owned by this fixture");
      const plan = await this.postgres.exec(["psql", "-U", "postgres", "-d", this.settings.database.name, "-v", "ON_ERROR_STOP=1", "-qtAc",
        "UPDATE zeroship.apps SET plan_id = (SELECT id FROM zeroship.plans WHERE name = 'unlimited' AND NOT archived) WHERE id = '" + id + "' RETURNING id"]);
      assert.equal(plan.exitCode, 0, plan.output);
      assert.equal(plan.output.trim(), id, "Operator must assign the streaming test plan");
    }
    await processes.run("deploy", this.binary("zeroship"), ["deploy", bundle, `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`], work, { HOME: work });

    if (settings.postDeploy) {
      const exposed = await settings.postDeploy({ id, gateway, worker, workerProcess, backing, keys });
      if (exposed?.s3) this.s3 = exposed.s3;
      if (exposed?.worker) this.worker = exposed.worker;
    }

    console.info(`${settings.label} fixture: start local Vite and wait for app dispatch`);
    const dev = await this.port();
    const ui = await this.port();
    await dev.release();
    await ui.release();
    processes.start("dev", process.execPath, [vite, "--host", "127.0.0.1", "--port", `${ui.number}`, "--strictPort"], app, {
      [settings.devEnvVar]: `${dev.number}`, ZEROSHIP_BIN: this.binary("zeroship"),
    });
    const targets = settings.targets({ dev, ui, gateway });
    for (const target of targets) {
      const ready = settings.readiness(target);
      await this.waitFor(target.name, () => this.httpReady(`${target.apiUrl}${ready.path}`, {
        method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: ready.body }),
      }));
    }
    return targets;
  }
}
