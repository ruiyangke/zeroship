import assert from "node:assert/strict";
import { generateKeyPairSync, randomBytes } from "node:crypto";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { parseTypedId } from "@zeroship/server/typed-id";
import { PlatformBase, readManifest, prepare, type FixtureSettings } from "./base";
import type { Backing } from "./services";
import type { Port, Target } from "../common";

/** The acceptance readiness chain a workflow example exposes to a suite. */
export interface WorkflowGate {
  startRequest(target: Target): { path: string; init: RequestInit };
  accepted(body: any): boolean;
  run(body: any): unknown;
  statusRequest(target: Target, run: unknown): { path: string; init?: RequestInit };
  state(body: any): string | undefined;
  expected: string;
}

export interface WorkflowSettings extends FixtureSettings {
  cargoPackages: string[];
  appName: string;
  manifest: (manifest: any) => void;
  backing?: (platform: WorkflowPlatform) => Promise<Backing>;
  sharedEnv?: (backing: Backing) => NodeJS.ProcessEnv;
  workerExtraArgs?: (backing: Backing) => string[];
  workerEnv?: (backing: Backing) => NodeJS.ProcessEnv;
  blobs: (backing: Backing, work: string) => Promise<string>;
  dev: { kind: "vite"; envVar: string } | { kind: "serve" };
  targets: (ctx: { dev: Port; ui: Port; gateway: Port }) => Target[];
  readiness: (target: Target) => { path: string; init?: RequestInit };
  gate: WorkflowGate;
}

/**
 * A workflow example: the platform plus the workflow manager, a deployed app
 * placed onto the worker, and a readiness gate that carries one run far enough
 * to prove the manager delivered it.
 */
export class WorkflowPlatform extends PlatformBase {
  protected constructor(readonly settings: WorkflowSettings, work: string, logs: string) {
    super(settings, work, logs);
  }

  static async create(settings: WorkflowSettings): Promise<WorkflowPlatform> {
    const { work, logs } = await prepare(settings);
    console.info(`${settings.label} fixture logs: ${logs}`);
    return new WorkflowPlatform(settings, work, logs);
  }

  async start(): Promise<Target[]> {
    const { work, processes, settings } = this;
    console.info(`${settings.label} fixture: build platform binaries and workflow`);
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
    const payloads = join(work, "objects");
    await mkdir(payloads, { recursive: true });
    const control = await this.port();
    const worker = await this.port();
    const gateway = await this.port();
    const relay = await this.port();
    const migrationServer = await this.port();
    const manager = await this.port();
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
      ZEROSHIP_CONTROL_WORKFLOW_COORDINATOR_URL: manager.url,
    });
    await this.waitFor("control", () => this.httpReady(`${control.url}/readyz`));

    // The workflow manager owns placement: a deployed app reaches env.workflows
    // only once the manager's placement lane has given it an owner, so the
    // deployed tier needs one. It holds no creator database - it reads the
    // platform metadata under its own login, which the migration creates
    // without a password.
    assert(this.postgres, "PostgreSQL must be owned by this fixture");
    const managerRole = await this.postgres.exec(["psql", "-U", "postgres", "-d", this.settings.database.name, "-v", "ON_ERROR_STOP=1", "-c",
      "ALTER ROLE zeroship_workflow WITH PASSWORD 'zeroship_workflow'"]);
    assert.equal(managerRole.exitCode, 0, `Give the manager role a fixture password: ${managerRole.output}`);
    await service("workflow", "zeroship-workflow-server", manager, ["--no-config", "--listen", `127.0.0.1:${manager.number}`], {
      ZEROSHIP_WORKFLOW_DATABASE_URL: `postgres://zeroship_workflow:zeroship_workflow@${authority}`,
      ZEROSHIP_WORKFLOW_CONTROL_URL: control.url,
      ZEROSHIP_WORKFLOW_SERVICE_KEY_FILE: keys.workflow, ZEROSHIP_WORKFLOW_SERVICE_PEERS_FILE: peers,
      // The store the worker names below: this service stages run inputs an
      // executing run reads back, so both processes name the one store.
      ZEROSHIP_WORKFLOW_STORAGE_URL: payloads,
    });
    await this.waitFor("workflow manager", () => this.httpReady(`${manager.url}/readyz`));
    await service("worker", "zeroship-worker", worker, [
      "--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1",
      ...(settings.workerExtraArgs?.(backing) ?? []),
    ], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_JOIN_TOKEN_FILE: joinToken, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
      // A workflow host reaches the journal through the manager and stages
      // payloads in the app object store, so the worker needs both.
      ZEROSHIP_WORKER_WORKFLOW_MANAGER_URL: manager.url,
      ZEROSHIP_WORKER_WORKFLOW_CAPACITY: "8", ZEROSHIP_WORKER_WORKFLOW_SLOTS: "2",
      ZEROSHIP_WORKER_STORAGE_URL: payloads,
      ...(settings.workerEnv?.(backing) ?? {}),
    });
    await this.waitFor("worker", () => this.httpReady(`${worker.url}/readyz`));
    await service("gateway", "zeroship-gate", gateway, ["--no-config", "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker], {
      ZEROSHIP_GATEWAY_DATABASE_URL: dsn, ZEROSHIP_GATEWAY_SIGNING_KEY_FILE: keys.gateway,
      ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: masterKey, ZEROSHIP_GATEWAY_SERVICE_KEY_FILE: keys.gateway, ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("gateway", () => this.httpReady(`${gateway.url}/readyz`));

    await service("migrate-server", "zeroship-migrate-server", migrationServer, [
      "--no-config", "--port", String(migrationServer.number), "--tmp-dir", join(work, "migration-service"),
    ], {
      ZEROSHIP_MIGRATE_SERVER_DATABASE_URL: dsn, ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL: dsn,
      ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY: randomBytes(32).toString("hex"),
    });
    await this.waitFor("migrate-server", () => this.httpReady(migrationServer.url + "/readyz"));

    console.info(`${settings.label} fixture: create and deploy the app`);
    const created = await fetch(`${control.url}/api/apps`, {
      method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" },
      body: JSON.stringify({ name: settings.appName }), signal: AbortSignal.any([processes.signal, AbortSignal.timeout(10_000)]),
    });
    assert(created.ok, `Create app: HTTP ${created.status}: ${created.ok ? "" : await created.text()}`);
    const { id } = await created.json();
    assert.equal(typeof id, "string", "Created app must have an id");
    parseTypedId(id, "app");
    // The app needs no journal of its own: the platform migrations applied
    // above install the journal into the workflow service's schema, where
    // every app's runs are kept.
    //
    // Workflow rollout and plan capabilities are operator-owned. A plan carries
    // the policy the manager grants an app's host under, and Control's startup
    // seeding leaves that column null, which refuses every policy lease. This is
    // the catalog default an operator publishes: the complete raw value of
    // zeroship_core::workflow_policy::AppPolicy::default(), which refuses
    // unknown and missing fields, so a change to that type fails here rather
    // than drifting.
    const policy = JSON.stringify({
      admission: true, dispatch: true, ingress: true,
      maxLiveRuns: 10000, maxChildDepth: 16, maxRunning: 16,
      maxInputBytes: 1048576, maxFrontier: 256, maxJournalBytes: 16777216,
      maxChildOutputBytes: 8388608,
      maxPayloadBytes: 67108864, maxPayloadObjects: 100000,
      maxPayloadStorageBytes: 1073741824, payloadStagingRetentionMs: 86400000,
      maxStepAttempts: 8, retryDelayMs: 1000, maxDeliveryAttempts: 8,
      maxStuckDispatches: 4,
      maxSchedules: 64, maxScheduleBackfill: 32, minScheduleIntervalMs: 1000,
      maxSignalTokenLifetimeSeconds: 86400, leaseMs: 60000,
    });
    const plan = await this.postgres.exec(["psql", "-U", "postgres", "-d", this.settings.database.name, "-v", "ON_ERROR_STOP=1", "-qtAc",
      "UPDATE zeroship.apps SET workflows_enabled = true, plan_id = (SELECT id FROM zeroship.plans WHERE name = 'unlimited' AND NOT archived) WHERE id = '" + id + "' RETURNING id"]);
    assert.equal(plan.exitCode, 0, plan.output);
    assert.equal(plan.output.trim(), id, "Operator must enable the workflow test plan");
    const rollout = await this.postgres.exec(["psql", "-U", "postgres", "-d", this.settings.database.name, "-v", "ON_ERROR_STOP=1", "-c",
      `UPDATE zeroship.plans SET workflows_allowed = true, workflow_policy_json = '${policy}'; INSERT INTO workflow_manager.workflow_rollout_config (id, dispatch_paused, ingress_disabled, source_validity_ms, updated_by) VALUES ('global', false, false, 30000, 'workflow-fixture') ON CONFLICT (id) DO UPDATE SET dispatch_paused = false, ingress_disabled = false, source_validity_ms = EXCLUDED.source_validity_ms`]);
    assert.equal(rollout.exitCode, 0, rollout.output);
    await processes.run("deploy", this.binary("zeroship"), ["deploy", bundle, `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`], work, { HOME: work });

    const dev = await this.port();
    let ui: Port = dev;
    if (settings.dev.kind === "vite") {
      console.info(`${settings.label} fixture: start local Vite and wait for app dispatch`);
      ui = await this.port();
      await dev.release();
      await ui.release();
      processes.start("dev", process.execPath, [vite, "--host", "127.0.0.1", "--port", `${ui.number}`, "--strictPort"], app, {
        [settings.dev.envVar]: `${dev.number}`, ZEROSHIP_BIN: this.binary("zeroship"),
      });
    } else {
      console.info(`${settings.label} fixture: serve the app bundle locally`);
      await dev.release();
      processes.start("dev", this.binary("zeroship"), ["serve", bundle, "--port", String(dev.number)], app, {});
    }
    const targets = settings.targets({ dev, ui, gateway });
    for (const target of targets) {
      const ready = settings.readiness(target);
      await this.waitFor(target.name, () => this.httpReady(target.apiUrl + ready.path, ready.init));
    }
    // Serving the app is not yet serving workflows. A deployed app reaches
    // env.workflows only once the manager has placed it and the worker host has
    // published its backend, which is later than the gateway route table.
    //
    // An accepted start proves only the ACCEPTANCE half of that chain: a
    // request isolate writing to the workflow journal. DELIVERY - the manager
    // handing the job back to a worker consumer - is a separate chain that
    // becomes ready later, so a gate that stopped at an accepted start let the
    // first timed assertion in the suite measure cold delivery. Carry one run
    // through, and keep the two waits separately named so an acceptance failure
    // and a delivery failure do not report as one thing.
    for (const target of targets) {
      let run: unknown = null;
      const start = settings.gate.startRequest(target);
      await this.waitFor(`${target.name} workflows accept a start`, async () => {
        const response = await fetch(`${target.apiUrl}${start.path}`, {
          ...start.init,
          signal: AbortSignal.any([this.processes.signal, AbortSignal.timeout(15_000)]),
        });
        const body = await response.json().catch(() => null);
        if (!response.ok || !settings.gate.accepted(body)) return false;
        run = settings.gate.run(body);
        return true;
      });
      const status = settings.gate.statusRequest(target, run);
      await this.waitFor(`${target.name} workflows deliver a run`, async () => {
        const response = await fetch(`${target.apiUrl}${status.path}`, {
          ...status.init,
          signal: AbortSignal.any([this.processes.signal, AbortSignal.timeout(15_000)]),
        });
        const body = await response.json().catch(() => null);
        return response.ok && settings.gate.state(body) === settings.gate.expected;
      });
    }
    return targets;
  }
}
