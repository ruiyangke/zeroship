import assert from "node:assert/strict";
import { generateKeyPairSync, randomBytes } from "node:crypto";
import { cp, mkdir, mkdtemp, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { connect, createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { zstdDecompressSync } from "node:zlib";
import { generate } from "selfsigned";
import { stringify } from "smol-toml";
import { Parser } from "tar";
import { GenericContainer, Wait, type StartedTestContainer } from "testcontainers";
import { parseTypedId, typedIdFromStableSeed } from "@zeroship/server/typed-id";
import type { Target } from "../targets";
import { issuer } from "./issuer";
import { Processes } from "./processes";

const example = fileURLToPath(new URL("../../", import.meta.url));
const root = resolve(example, "../..");

async function reservePort() {
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

async function checkManifest(bundle: string) {
  let first: string | undefined;
  const chunks: Buffer[] = [];
  const parser = new Parser({ onReadEntry(entry) {
    first ??= entry.path;
    if (entry.path === "manifest.json") entry.on("data", (chunk: Buffer) => chunks.push(chunk));
    else entry.resume();
  } });
  await pipeline(Readable.from([zstdDecompressSync(await readFile(bundle))]), parser);
  assert.equal(first, "manifest.json");
  const manifest = JSON.parse(Buffer.concat(chunks).toString());
  const resources = Object.entries(manifest.resources as Record<string, { auth?: unknown }>)
    .filter(([id]) => id.startsWith("rpc:"));
  assert(resources.length > 0, "Fixture must expose RPC resources");
  assert.deepEqual([...manifest.workflows].sort(), ["BasicCase", "SleepCase", "SignalCase", "ChildCase", "CompensateCase", "DoubleChild"].sort(), "Build must preserve workflow class names");
  assert(resources.every(([, resource]) => resource.auth === "anonymous"), "Workflow example RPC resources must declare anonymous access");
}

export class Platform {
  readonly processes: Processes;
  private readonly containers: StartedTestContainer[] = [];
  private readonly ports: Awaited<ReturnType<typeof reservePort>>[] = [];
  private closing?: Promise<void>;

  private constructor(readonly work: string, readonly logs: string) {
    this.processes = new Processes(logs);
  }

  static async create(): Promise<Platform> {
    const artifacts = join(example, "tests/.artifacts");
    await mkdir(artifacts, { recursive: true });
    const logs = await mkdtemp(join(artifacts, "run-"));
    const work = await mkdtemp(join(tmpdir(), "zeroship-workflow-probe-"));
    console.info(`Workflow fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  private async secret(name: string, contents: string): Promise<string> {
    const path = join(this.work, name);
    await writeFile(path, contents, { mode: 0o600 });
    return path;
  }

  private async port() {
    const port = await reservePort();
    this.ports.push(port);
    return port;
  }

  private async container(image: GenericContainer): Promise<StartedTestContainer> {
    this.processes.signal.throwIfAborted();
    const container = await image.withStartupTimeout(120_000).start();
    this.containers.push(container);
    this.processes.signal.throwIfAborted();
    return container;
  }

  private async waitFor(label: string, ready: () => Promise<boolean>): Promise<void> {
    const deadline = Date.now() + 90_000;
    let lastError: unknown;
    do {
      this.processes.assertAlive();
      try { if (await ready()) return; } catch (error) { lastError = error; }
      await sleep(100, undefined, { signal: this.processes.signal });
    } while (Date.now() < deadline);
    throw new Error(`Not ready: ${label}; logs: ${this.logs}`, { cause: lastError });
  }

  private async httpReady(url: string, init?: RequestInit) {
    const response = await fetch(url, { ...init, signal: AbortSignal.any([this.processes.signal, AbortSignal.timeout(5_000)]) });
    const body = await response.text();
    if (!response.ok) throw new Error(`${url}: HTTP ${response.status}: ${body}`);
    return true;
  }

  async start(): Promise<Target[]> {
    const { work, processes } = this;
    console.info("Workflow fixture: build platform binaries and workflow");
    const artifacts = await processes.run("cargo", process.env.CARGO ?? "cargo", [
      "build", "--message-format=json", "--locked", "--bins",
      ...["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server", "zeroship-migrate-server", "zeroship-workflow-server"].flatMap((name) => ["-p", name]),
    ], root, process.env);
    const binaries = new Map<string, string>();
    for (const line of artifacts.split("\n")) {
      if (!line.startsWith("{")) continue;
      const value = JSON.parse(line);
      if (value.reason === "compiler-artifact" && value.executable) binaries.set(value.target.name, value.executable);
    }
    const binary = (name: string) => { const path = binaries.get(name); assert(path, `Missing built binary: ${name}`); return path; };
    const app = join(work, "app");
    await mkdir(app);
    for (const name of await readdir(example)) {
      if (!["node_modules", "dist", ".zeroship", "tests"].includes(name)) await cp(join(example, name), join(app, name), { recursive: true });
    }
    await symlink(join(example, "node_modules"), join(app, "node_modules"), "dir");
    const vite = join(app, "node_modules/vite/bin/vite.js");
    await processes.run("app-build", process.execPath, [vite, "build"], app);
    const bundle = join(app, "dist/app.zship");
    await checkManifest(bundle);

    console.info("Workflow fixture: start backing containers and apply platform migrations");
    const postgres = await this.container(new GenericContainer("postgres:18")
      .withExposedPorts(5432)
      .withEnvironment({ POSTGRES_PASSWORD: "workflow-fixture-password", POSTGRES_DB: "workflow_fixture" })
      .withCommand(["postgres", "-c", "wal_level=logical", "-c", "max_slot_wal_keep_size=128MB", "-c", "fsync=off"])
      .withWaitStrategy(Wait.forAll([Wait.forLogMessage("database system is ready to accept connections", 2), Wait.forListeningPorts()])));
    const authority = `${postgres.getHost()}:${postgres.getMappedPort(5432)}/workflow_fixture`;
    const dsn = `postgres://postgres:workflow-fixture-password@${authority}`;
    const migration = await this.secret("migrate.toml", stringify({ env: { platform: {
      url: dsn, dir: join(root, "db/migrations-ts"), schema: "zeroship", owner_app: "zeroship_platform",
      registry: join(root, "policies/platform-table-owners.json"), policy: [join(root, "policies/platform.policy.toml")],
    } } }));
    await processes.run("migrate", process.execPath, [join(root, "packages/zero-migrate-cli/dist/cli-bin.js"), "apply", "--config", migration, "--env", "platform", "--approve"], root);

    const redis = await this.container(new GenericContainer("redis:7").withExposedPorts(6379)
      .withWaitStrategy(Wait.forLogMessage("Ready to accept connections")));
    const kv = await this.secret("kv.toml", stringify({ backend: "redis", redis: { topology: {
      mode: "standalone", endpoint: redis.getHost() + ":" + redis.getMappedPort(6379),
    } } }));
    const identity = issuer();
    const jwks = await this.container(identity.container);
    const issuerUrl = `http://${jwks.getHost()}:${jwks.getMappedPort(80)}`;
    const owner = typedIdFromStableSeed("usr", "workflow-probe-fixture-owner");
    const seeded = await postgres.exec(["psql", "-U", "postgres", "-d", "workflow_fixture", "-v", "ON_ERROR_STOP=1", "-c",
      `INSERT INTO zeroship.users (id, email, name, email_verified_at) VALUES ('${owner}', 'probe-${owner}@zeroship.test', 'Workflow fixture owner', NOW())`]);
    assert.equal(seeded.exitCode, 0, `Seed authenticated fixture owner: ${seeded.output}`);
    const bearer = identity.bearer(issuerUrl, owner);

    const keys: Record<string, string> = {};
    const peerKeys = [];
    for (const service of ["control", "gateway", "workflow"]) {
      const { publicKey, privateKey } = generateKeyPairSync("ed25519");
      peerKeys.push({ ...publicKey.export({ format: "jwk" }), iss: `spiffe://zeroship.ai/svc/${service}` });
      keys[service] = await this.secret(`${service}.pem`, privateKey.export({ format: "pem", type: "pkcs8" }).toString());
    }
    const peers = await this.secret("peers.json", JSON.stringify({ keys: peerKeys }));
    // The worker holds no service key: it enrols with its deployment unit's
    // enroller. The operator tool mints the credential the worker mounts and
    // the import file Control reads at startup.
    const enroller = join(work, "worker-enroller.json");
    const enrollers = join(work, "worker-enrollers.json");
    await processes.run("enroller", binary("zeroship"), ["dev", "enroller", `--credential=${enroller}`, `--import-file=${enrollers}`], work);
    const masterKey = randomBytes(32).toString("hex");
    const broker = await this.secret("broker", masterKey);
    const shared = {
      ZEROSHIP_CONTROL_KEY: randomBytes(16).toString("hex"),
      ZEROSHIP_PAIRWISE_SALT: randomBytes(16).toString("hex"),
      ZEROSHIP_ORIGIN_SCHEME: "http", ZEROSHIP_AUTH_PLATFORM_ISSUER: issuerUrl,
      ZEROSHIP_OBSERVABILITY_LOG_FORMAT: "json",
    };
    const certificate = await generate([{ name: "commonName", value: "localhost" }], {
      keyType: "ec", algorithm: "sha256", notBeforeDate: new Date(Date.now() - 60_000),
    });
    const cert = await this.secret("relay-cert.pem", certificate.cert);
    const key = await this.secret("relay-key.pem", certificate.private);
    const blobs = join(work, "blobs");
    const payloads = join(work, "objects");
    await mkdir(payloads, { recursive: true });
    const control = await this.port();
    const worker = await this.port();
    const gateway = await this.port();
    const relay = await this.port();
    const migrationServer = await this.port();
    const manager = await this.port();
    const service = async (name: string, executable: string, port: typeof relay, args: string[], env: NodeJS.ProcessEnv) => {
      await port.release();
      return processes.start(name, binary(executable), args, work, { ...shared, ...env });
    };
    console.info("Workflow fixture: start platform services");
    await service("relay", "zeroship-data-cdc-server", relay, ["--no-config", "--listen", `127.0.0.1:${relay.number}`, "--tls-cert-file", cert, "--tls-key-file", key], {
      ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL: `postgres://zeroship_cdc:zeroship_cdc@${authority}`,
    });
    await this.waitFor("CDC relay", () => new Promise((resolve) => {
      const socket = connect(relay.number, "127.0.0.1");
      const done = (ready: boolean) => { socket.destroy(); resolve(ready); };
      socket.once("connect", () => done(true));
      socket.once("error", () => done(false));
      socket.setTimeout(1000, () => done(false));
    }));
    await service("control", "zeroship-control", control, ["--no-config", "--port", `${control.number}`, "--blob-store", blobs, "--gateway-url", gateway.url, "--worker-urls", worker.url, "--disable-workflow-engine"], {
      ZEROSHIP_CONTROL_DATABASE_URL: dsn, ZEROSHIP_CONTROL_MASTER_KEY: masterKey,
      ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING: "true", ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS: "127.0.0.1/32",
      ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS: `${worker.number}`,
      ZEROSHIP_CONTROL_SERVICE_KEY_FILE: keys.control, ZEROSHIP_CONTROL_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_CONTROL_WORKER_ENROLLERS_FILE: enrollers,
      ZEROSHIP_CONTROL_WORKFLOW_COORDINATOR_URL: manager.url,
    });
    await this.waitFor("control", () => this.httpReady(`${control.url}/readyz`));

    // The workflow manager owns placement: a deployed app reaches env.workflows
    // only once the manager's placement lane has given it an owner, so the
    // deployed tier needs one. It holds no creator database - it reads the
    // platform metadata under its own login, which the migration creates
    // without a password.
    const managerRole = await postgres.exec(["psql", "-U", "postgres", "-d", "workflow_fixture", "-v", "ON_ERROR_STOP=1", "-c",
      "ALTER ROLE zeroship_workflow WITH PASSWORD 'zeroship_workflow'"]);
    assert.equal(managerRole.exitCode, 0, `Give the manager role a fixture password: ${managerRole.output}`);
    await service("workflow", "zeroship-workflow-server", manager, ["--no-config", "--listen", `127.0.0.1:${manager.number}`], {
      ZEROSHIP_WORKFLOW_DATABASE_URL: `postgres://zeroship_workflow:zeroship_workflow@${authority}`,
      ZEROSHIP_WORKFLOW_CONTROL_URL: control.url,
      ZEROSHIP_WORKFLOW_SERVICE_KEY_FILE: keys.workflow, ZEROSHIP_WORKFLOW_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("workflow manager", () => this.httpReady(`${manager.url}/readyz`));
    await service("worker", "zeroship-worker", worker, ["--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1", "--workflow-advance-unsigned", "--kv-config-file", kv], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_ENROLLER_FILE: enroller, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
      // A workflow host keeps creator journals in the app database and stages
      // payloads in the app object store, so the worker needs both.
      ZEROSHIP_WORKER_WORKFLOW_MANAGER_URL: manager.url,
      ZEROSHIP_WORKER_WORKFLOW_CAPACITY: "8", ZEROSHIP_WORKER_WORKFLOW_SLOTS: "2",
      ZEROSHIP_WORKER_STORAGE_URL: payloads,
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

    console.info("Workflow fixture: create and deploy the app");
    const created = await fetch(`${control.url}/api/apps`, {
      method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" },
      body: JSON.stringify({ name: "probe" }), signal: AbortSignal.any([processes.signal, AbortSignal.timeout(10_000)]),
    });
    assert(created.ok, `Create app: HTTP ${created.status}: ${created.ok ? "" : await created.text()}`);
    const { id } = await created.json();
    assert.equal(typeof id, "string", "Created app must have an id");
    parseTypedId(id, "app");
    const provisionUrl = migrationServer.url + "/v1/apps/" + id + "/workflows/provision";
    const anonymous = await fetch(provisionUrl, { method: "POST" });
    assert.equal(anonymous.status, 401, "Provisioning requires creator authorization");
    await anonymous.arrayBuffer();
    const outsiderId = typedIdFromStableSeed("usr", "workflow-probe-fixture-outsider");
    const outsiderSeed = await postgres.exec(["psql", "-U", "postgres", "-d", "workflow_fixture", "-v", "ON_ERROR_STOP=1", "-c",
      "INSERT INTO zeroship.users (id, email, name, email_verified_at) VALUES ('" + outsiderId + "', 'outsider-" + outsiderId + "@zeroship.test', 'Other creator', NOW())"]);
    assert.equal(outsiderSeed.exitCode, 0, outsiderSeed.output);
    const outsider = identity.bearer(issuerUrl, outsiderId);
    const denied = await fetch(provisionUrl, { method: "POST", headers: { authorization: "Bearer " + outsider } });
    assert.equal(denied.status, 403, "Another creator cannot provision this app");
    await denied.arrayBuffer();
    // Retrying the lifecycle operation must preserve the existing journal.
    for (let attempt = 0; attempt < 2; attempt++) {
      await this.httpReady(provisionUrl, { method: "POST", headers: { authorization: "Bearer " + bearer } });
    }
    // Workflow rollout and plan capabilities are operator-owned.
    const plan = await postgres.exec(["psql", "-U", "postgres", "-d", "workflow_fixture", "-v", "ON_ERROR_STOP=1", "-qtAc",
      "UPDATE zeroship.apps SET workflows_enabled = true, plan_id = (SELECT id FROM zeroship.plans WHERE name = 'unlimited' AND NOT archived) WHERE id = '" + id + "' RETURNING id"]);
    assert.equal(plan.exitCode, 0, plan.output);
    assert.equal(plan.output.trim(), id, "Operator must enable the workflow test plan");
    const rollout = await postgres.exec(["psql", "-U", "postgres", "-d", "workflow_fixture", "-v", "ON_ERROR_STOP=1", "-c",
      "UPDATE zeroship.plans SET workflows_allowed = true; INSERT INTO zeroship.workflow_rollout_config (id, dispatch_paused, ingress_disabled, source_validity_ms, updated_by) VALUES ('global', false, false, 30000, 'workflow-fixture') ON CONFLICT (id) DO UPDATE SET dispatch_paused = false, ingress_disabled = false, source_validity_ms = EXCLUDED.source_validity_ms"]);
    assert.equal(rollout.exitCode, 0, rollout.output);
    await processes.run("deploy", binary("zeroship"), ["deploy", bundle, `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`], work, { HOME: work });

    console.info("Workflow fixture: start local Vite and wait for app dispatch");
    const dev = await this.port();
    const ui = await this.port();
    await dev.release();
    await ui.release();
    processes.start("dev", process.execPath, [vite, "--host", "127.0.0.1", "--port", `${ui.number}`, "--strictPort"], app, {
      WORKFLOW_PROBE_API_PORT: `${dev.number}`, ZEROSHIP_BIN: binary("zeroship"),
    });
    const targets = [
      { name: "local", apiUrl: dev.url, uiUrl: ui.url },
      { name: "deployed", apiUrl: `${gateway.url}/apps/probe`, uiUrl: `http://probe.localhost:${gateway.number}` },
    ];
    for (const target of targets) await this.waitFor(target.name, () => this.httpReady(`${target.apiUrl}/__zeroship/v1/wf.ping`, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: {} }),
    }));
    return targets;
  }

  close(): Promise<void> {
    return this.closing ??= (async () => {
      const errors: unknown[] = [];
      try { await this.processes.close(); } catch (error) { errors.push(error); }
      const results = await Promise.allSettled([
        ...this.ports.map((port) => port.release()),
        ...this.containers.map((container) => container.stop({ timeout: 10_000 })),
      ]);
      for (const result of results) if (result.status === "rejected") errors.push(result.reason);
      try { await rm(this.work, { recursive: true, force: true }); } catch (error) { errors.push(error); }
      if (errors.length) throw new AggregateError(errors, "Failed to clean up workflow fixture");
    })();
  }
}
