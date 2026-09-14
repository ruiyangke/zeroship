import assert from "node:assert/strict";
import { generateKeyPairSync, randomBytes, randomUUID } from "node:crypto";
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
  assert(resources.every(([, resource]) => resource.auth === "anonymous"), "Storage example RPC resources must declare anonymous access");
}

export class Platform {
  readonly processes: Processes;
  private readonly containers: StartedTestContainer[] = [];
  private readonly ports: Awaited<ReturnType<typeof reservePort>>[] = [];
  private closing?: Promise<void>;
  s3?: { endpoint: string; bucket: string; prefix: string; appId: string; workerPid: number };

  private constructor(readonly work: string, readonly logs: string) {
    this.processes = new Processes(logs);
  }

  static async create(): Promise<Platform> {
    const artifacts = join(example, "tests/.artifacts");
    await mkdir(artifacts, { recursive: true });
    const logs = await mkdtemp(join(artifacts, "run-"));
    const work = await mkdtemp(join(tmpdir(), "zeroship-storage-probe-"));
    console.info(`Storage fixture logs: ${logs}`);
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
    console.info("Storage fixture: build platform binaries and storage");
    const artifacts = await processes.run("cargo", process.env.CARGO ?? "cargo", [
      "build", "--message-format=json", "--locked", "--bins",
      ...["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server"].flatMap((name) => ["-p", name]),
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

    console.info("Storage fixture: start backing containers and apply platform migrations");
    const postgres = await this.container(new GenericContainer("postgres:18")
      .withExposedPorts(5432)
      .withEnvironment({ POSTGRES_PASSWORD: "storage-fixture-password", POSTGRES_DB: "storage_fixture" })
      .withCommand(["postgres", "-c", "wal_level=logical", "-c", "max_slot_wal_keep_size=128MB", "-c", "fsync=off"])
      .withWaitStrategy(Wait.forAll([Wait.forLogMessage("database system is ready to accept connections", 2), Wait.forListeningPorts()])));
    const authority = `${postgres.getHost()}:${postgres.getMappedPort(5432)}/storage_fixture`;
    const dsn = `postgres://postgres:storage-fixture-password@${authority}`;
    const migration = await this.secret("migrate.toml", stringify({ env: { platform: {
      url: dsn, dir: join(root, "db/migrations-ts"), schema: "zeroship", owner_app: "zeroship_platform",
      registry: join(root, "policies/platform-table-owners.json"), policy: [join(root, "policies/platform.policy.toml")],
    } } }));
    await processes.run("migrate", process.execPath, [join(root, "packages/zero-migrate-cli/dist/cli-bin.js"), "apply", "--config", migration, "--env", "platform", "--approve"], root);

    const minio = await this.container(new GenericContainer("quay.io/minio/minio:latest")
      .withExposedPorts(9000)
      .withEnvironment({ MINIO_ROOT_USER: "minioadmin", MINIO_ROOT_PASSWORD: "minioadmin" })
      .withCommand(["server", "/data"])
      .withWaitStrategy(Wait.forLogMessage("API:")));
    for (const command of [
      ["mc", "alias", "set", "fixture", "http://127.0.0.1:9000", "minioadmin", "minioadmin"],
      ["mc", "mb", "fixture/storage-fixture"],
    ]) {
      const result = await minio.exec(command);
      assert.equal(result.exitCode, 0, result.output);
    }
    const endpoint = "http://" + minio.getHost() + ":" + minio.getMappedPort(9000);
    const storeUrl = (prefix: string) => "s3://storage-fixture/" + prefix + "?provider=minio&endpoint=" + endpoint + "&region=us-east-1&style=path&dev_http=true&checksum=none";
    const identity = issuer();
    const jwks = await this.container(identity.container);
    const issuerUrl = `http://${jwks.getHost()}:${jwks.getMappedPort(80)}`;
    const owner = randomUUID();
    const seeded = await postgres.exec(["psql", "-U", "postgres", "-d", "storage_fixture", "-v", "ON_ERROR_STOP=1", "-c",
      `INSERT INTO zeroship.users (id, email, name, email_verified_at) VALUES ('${owner}', 'probe-${owner}@zeroship.test', 'Storage fixture owner', NOW())`]);
    assert.equal(seeded.exitCode, 0, `Seed authenticated fixture owner: ${seeded.output}`);
    const bearer = identity.bearer(issuerUrl, owner);

    const keys: Record<string, string> = {};
    const peerKeys = [];
    for (const service of ["control", "worker", "gateway"]) {
      const { publicKey, privateKey } = generateKeyPairSync("ed25519");
      peerKeys.push({ ...publicKey.export({ format: "jwk" }), iss: `spiffe://zeroship.ai/svc/${service}` });
      keys[service] = await this.secret(`${service}.pem`, privateKey.export({ format: "pem", type: "pkcs8" }).toString());
    }
    const peers = await this.secret("peers.json", JSON.stringify({ keys: peerKeys }));
    const masterKey = randomBytes(32).toString("hex");
    const broker = await this.secret("broker", masterKey);
    const shared = {
      AWS_ACCESS_KEY_ID: "minioadmin", AWS_SECRET_ACCESS_KEY: "minioadmin",
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
    const blobs = storeUrl("deploy");
    const control = await this.port();
    const worker = await this.port();
    const gateway = await this.port();
    const relay = await this.port();
    const service = async (name: string, executable: string, port: typeof relay, args: string[], env: NodeJS.ProcessEnv) => {
      await port.release();
      return processes.start(name, binary(executable), args, work, { ...shared, ...env });
    };
    console.info("Storage fixture: start platform services");
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
    await service("control", "zeroship-control", control, ["--no-config", "--port", `${control.number}`, "--blob-store", blobs, "--gateway-url", gateway.url, "--worker-urls", worker.url], {
      ZEROSHIP_CONTROL_DATABASE_URL: dsn, ZEROSHIP_CONTROL_MASTER_KEY: masterKey,
      ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING: "true", ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS: "127.0.0.1/32",
      ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS: `${worker.number}`,
      ZEROSHIP_CONTROL_SERVICE_KEY_FILE: keys.control, ZEROSHIP_CONTROL_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("control", () => this.httpReady(`${control.url}/readyz`));
    const workerProcess = await service("worker", "zeroship-worker", worker, ["--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1", "--storage-url", storeUrl("objects")], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_SERVICE_KEY_FILE: keys.worker, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
    });
    await this.waitFor("worker", () => this.httpReady(`${worker.url}/readyz`));
    await service("gateway", "zeroship-gate", gateway, ["--no-config", "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker], {
      ZEROSHIP_GATEWAY_DATABASE_URL: dsn, ZEROSHIP_GATEWAY_SIGNING_KEY_FILE: keys.gateway,
      ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: masterKey, ZEROSHIP_GATEWAY_SERVICE_KEY_FILE: keys.gateway, ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("gateway", () => this.httpReady(`${gateway.url}/readyz`));

    console.info("Storage fixture: create and deploy the app");
    const created = await fetch(`${control.url}/api/apps`, {
      method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" },
      body: JSON.stringify({ name: "probe" }), signal: AbortSignal.any([processes.signal, AbortSignal.timeout(10_000)]),
    });
    assert(created.ok, `Create app: HTTP ${created.status}: ${created.ok ? "" : await created.text()}`);
    const { id } = await created.json();
    assert.equal(typeof id, "string", "Created app must have an id");
    assert.match(id, /^[0-9a-f-]{36}$/);
    // The fixture is the operator. Creator requests cannot self-assign this tier.
    const plan = await postgres.exec(["psql", "-U", "postgres", "-d", "storage_fixture", "-v", "ON_ERROR_STOP=1", "-qtAc",
      "UPDATE zeroship.apps SET plan_id = (SELECT id FROM zeroship.plans WHERE name = 'unlimited' AND NOT archived) WHERE id = '" + id + "' RETURNING id"]);
    assert.equal(plan.exitCode, 0, plan.output);
    assert.equal(plan.output.trim(), id, "Operator must assign the streaming test plan");
    await processes.run("deploy", binary("zeroship"), ["deploy", bundle, `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`], work, { HOME: work });

    const storedBlobs = await minio.exec(["mc", "ls", "--recursive", "--json", "fixture/storage-fixture/deploy/blobs/"]);
    assert.equal(storedBlobs.exitCode, 0, storedBlobs.output);
    assert(storedBlobs.output.trim(), "Deploy must write blobs into S3");
    assert(workerProcess.child.pid, "Worker process must have a PID");
    this.s3 = { endpoint, bucket: "storage-fixture", prefix: "objects", appId: id, workerPid: workerProcess.child.pid };

    console.info("Storage fixture: start local Vite and wait for app dispatch");
    const dev = await this.port();
    const ui = await this.port();
    await dev.release();
    await ui.release();
    processes.start("dev", process.execPath, [vite, "--host", "127.0.0.1", "--port", `${ui.number}`, "--strictPort"], app, {
      STORAGE_PROBE_API_PORT: `${dev.number}`, ZEROSHIP_BIN: binary("zeroship"),
    });
    const targets = [
      { name: "local", apiUrl: dev.url, uiUrl: ui.url },
      { name: "s3", apiUrl: `${gateway.url}/apps/probe`, uiUrl: `http://probe.localhost:${gateway.number}` },
    ];
    for (const target of targets) await this.waitFor(target.name, () => this.httpReady(`${target.apiUrl}/__zeroship/v1/probe.ping`, {
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
      if (errors.length) throw new AggregateError(errors, "Failed to clean up storage fixture");
    })();
  }
}
