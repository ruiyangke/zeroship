import assert from "node:assert/strict";
import { createWriteStream, type WriteStream } from "node:fs";
import { generateKeyPairSync, randomBytes, randomUUID } from "node:crypto";
import { appendFile, cp, mkdir, mkdtemp, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { connect, createServer } from "node:net";
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
  assert.equal(typeof manifest.runtime_descriptor?.hash, "string");
  assert(manifest.runtime_descriptor.hash.length > 0, "Bundle must bind its runtime descriptor");
}

export class Platform {
  readonly processes: Processes;
  private postgres?: StartedTestContainer;
  appId = "";
  apiUrl = "";
  controlUrl = "";
  bearer = "";
  readyRequests = 0;

  async sql(query: string): Promise<string> {
    assert(this.postgres, "PostgreSQL must be owned by this fixture");
    const result = await this.postgres.exec(["psql", "-U", "postgres", "-d", "db_fixture", "-v", "ON_ERROR_STOP=1", "-tA", "-c", query]);
    assert.equal(result.exitCode, 0, result.output);
    return result.stdout.trim();
  }
  private readonly containers: StartedTestContainer[] = [];
  private readonly ports: Awaited<ReturnType<typeof reservePort>>[] = [];
  private closing?: Promise<void>;
  private pgLogs?: Readable;
  private pgOutput?: WriteStream;

  private constructor(readonly work: string, readonly logs: string) {
    this.processes = new Processes(logs);
  }

  static async create(): Promise<Platform> {
    const artifacts = join(example, "tests/.artifacts");
    await mkdir(artifacts, { recursive: true });
    const logs = await mkdtemp(join(artifacts, "run-"));
    const work = await mkdtemp(join(artifacts, "work-"));
    console.info(`DB fixture logs: ${logs}`);
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
    await response.arrayBuffer();
    return response.ok;
  }

  async start(): Promise<void> {
    const { work, processes } = this;
    console.info("DB fixture: build platform binaries and database");
    await processes.run("packages", "pnpm", ["build"], root, process.env);
    const artifacts = await processes.run("cargo", process.env.CARGO ?? "cargo", [
      "build", "--message-format=json", "--locked", "--bins",
      ...["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server", "zeroship-migrate-server"].flatMap((name) => ["-p", name]),
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
      if (!["node_modules", "dist", ".zeroship", "tests", "test", "e2e"].includes(name)) await cp(join(example, name), join(app, name), { recursive: true });
    }
    await symlink(join(example, "node_modules"), join(app, "node_modules"), "dir");
    const vite = join(app, "node_modules/vite/bin/vite.js");
    await processes.run("app-build", process.execPath, [vite, "build"], app);
    const bundle = join(app, "dist/app.zship");
    await checkManifest(bundle);

    console.info("DB fixture: start backing containers and apply platform migrations");
    const postgres = await this.container(new GenericContainer("postgres:16")
      .withExposedPorts(5432)
      .withEnvironment({ POSTGRES_PASSWORD: "db-fixture-password", POSTGRES_DB: "db_fixture" })
      .withCommand(["postgres", "-c", "wal_level=logical", "-c", "max_slot_wal_keep_size=128MB", "-c", "fsync=off", "-c", "log_statement=all"])
      .withWaitStrategy(Wait.forAll([Wait.forLogMessage("database system is ready to accept connections", 2), Wait.forListeningPorts()])));
    this.postgres = postgres;
    const authority = `${postgres.getHost()}:${postgres.getMappedPort(5432)}/db_fixture`;
    const dsn = `postgres://postgres:db-fixture-password@${authority}`;
    const migration = await this.secret("migrate.toml", stringify({ env: { platform: {
      url: dsn, dir: join(root, "db/migrations-ts"), schema: "zeroship", owner_app: "zeroship_platform",
      registry: join(root, "policies/platform-table-owners.json"), policy: [join(root, "policies/platform.policy.toml")],
    } } }));
    await processes.run("migrate", process.execPath, [join(root, "packages/zero-migrate-cli/dist/cli-bin.js"), "apply", "--config", migration, "--env", "platform", "--approve"], root);

    await this.sql(`
      INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
      VALUES ('pln_db_acceptance','Database acceptance',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000);
      INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000)
      ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
      INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES
        ('requests','platform','op'),('cpu_us','platform','us'),('wall_us','platform','us'),
        ('ingress_bytes','platform','byte'),('egress_bytes','platform','byte'),
        ('db_reads','primitive','op'),('db_writes','primitive','op'),('db_rows_written','primitive','row')
      ON CONFLICT (metric) DO UPDATE SET kind=EXCLUDED.kind,unit=EXCLUDED.unit;
      INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES
        ('requests',1,1),('cpu_us',0,1),('wall_us',0,1),('ingress_bytes',0,1),('egress_bytes',0,1),
        ('db_reads',1,1),('db_writes',1,1),('db_rows_written',0,1)
      ON CONFLICT (metric) DO UPDATE SET units_per_op=EXCLUDED.units_per_op,per_units=EXCLUDED.per_units;
    `);
    const kafkaPort = await this.port();
    await kafkaPort.release();
    const brokers = `127.0.0.1:${kafkaPort.number}`;
    const redpanda = await this.container(new GenericContainer("docker.redpanda.com/redpandadata/redpanda:latest")
      .withExposedPorts({ container: 9092, host: kafkaPort.number })
      .withCommand(["redpanda", "start", "--overprovisioned", "--smp", "1", "--memory", "512M", "--reserve-memory", "0M", "--node-id", "0", "--check=false",
        "--kafka-addr", "internal://0.0.0.0:9093,external://0.0.0.0:9092", "--advertise-kafka-addr", `internal://127.0.0.1:9093,external://${brokers}`, "--set", "redpanda.auto_create_topics_enabled=true"])
      .withHealthCheck({ test: ["CMD", "rpk", "cluster", "health", "--exit-when-healthy"], interval: 1000, timeout: 3000, retries: 60 })
      .withWaitStrategy(Wait.forHealthCheck()));
    const topic = "zeroship-usage-db-acceptance";
    const createdTopic = await redpanda.exec(["rpk", "topic", "create", topic, "-X", "brokers=127.0.0.1:9093"]);
    assert.equal(createdTopic.exitCode, 0, `Create the metering topic: ${createdTopic.output}`);
    const config = await this.secret("metering.toml", stringify({ metering: { brokers, events_topic: topic } }));

    this.pgLogs = await postgres.logs();
    this.pgOutput = createWriteStream(join(this.logs, "postgres.log"), { mode: 0o600 });
    this.pgLogs.pipe(this.pgOutput);
    const identity = issuer();
    const jwks = await this.container(identity.container);
    const issuerUrl = `http://${jwks.getHost()}:${jwks.getMappedPort(80)}`;
    const owner = randomUUID();
    const seeded = await postgres.exec(["psql", "-U", "postgres", "-d", "db_fixture", "-v", "ON_ERROR_STOP=1", "-c",
      `INSERT INTO zeroship.users (id, email, name, email_verified_at) VALUES ('${owner}', 'db-${owner}@zeroship.test', 'DB fixture owner', NOW())`]);
    assert.equal(seeded.exitCode, 0, `Seed authenticated fixture owner: ${seeded.output}`);
    const bearer = this.bearer = identity.bearer(issuerUrl, owner);

    const keys: Record<string, string> = {};
    const peerKeys = [];
    for (const service of ["control", "gateway"]) {
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
    await mkdir(blobs);
    const control = await this.port();
    const worker = await this.port();
    const gateway = await this.port();
    const relay = await this.port();
    const migrationServer = await this.port();
    const service = async (name: string, executable: string, port: typeof relay, args: string[], env: NodeJS.ProcessEnv) => {
      await port.release();
      processes.start(name, binary(executable), args, work, { ...shared, ...env });
    };
    console.info("DB fixture: start platform services");
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
    await service("control", "zeroship-control", control, ["--config", config, "--meter-provider", "lite", "--invoicer-provider", "lite", "--allow-unsupported-billing", "--spend-recompute-interval", "2", "--stripe-base-url", "http://127.0.0.1:1", "--port", `${control.number}`, "--blob-store", blobs, "--gateway-url", gateway.url, "--worker-urls", worker.url], {
      ZEROSHIP_CONTROL_STRIPE_SECRET_KEY: "sk_test_unused", ZEROSHIP_CONTROL_DATABASE_URL: dsn, ZEROSHIP_CONTROL_MASTER_KEY: masterKey,
      ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING: "true", ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS: "127.0.0.1/32",
      ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS: `${worker.number}`,
      ZEROSHIP_CONTROL_SERVICE_KEY_FILE: keys.control, ZEROSHIP_CONTROL_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_CONTROL_WORKER_ENROLLERS_FILE: enrollers,
    });
    await this.waitFor("control", () => this.httpReady(`${control.url}/readyz`));
    await service("migrate-server", "zeroship-migrate-server", migrationServer, [
      "--no-config", "--port", `${migrationServer.number}`, "--tmp-dir", join(work, "migrations"), "--mutation-rate-limit-burst", "3",
    ], {
      ZEROSHIP_MIGRATE_SERVER_DATABASE_URL: dsn, ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL: dsn,
      ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY: randomBytes(32).toString("hex"),
    });
    await this.waitFor("migrate-server", () => this.httpReady(`${migrationServer.url}/readyz`));
    await service("worker", "zeroship-worker", worker, ["--metering-brokers", brokers, "--metering-events-topic", topic, "--metering-outbox-wal-path", join(work, "worker-outbox.redb"), "--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1"], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_ENROLLER_FILE: enroller, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
    });
    await this.waitFor("worker", () => this.httpReady(`${worker.url}/readyz`));
    await service("gateway", "zeroship-gate", gateway, ["--config", config, "--metering-outbox-wal-path", join(work, "gateway-outbox.redb"), "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker], {
      ZEROSHIP_GATEWAY_DATABASE_URL: dsn, ZEROSHIP_GATEWAY_SIGNING_KEY_FILE: keys.gateway,
      ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: masterKey, ZEROSHIP_GATEWAY_SERVICE_KEY_FILE: keys.gateway, ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("gateway", () => this.httpReady(`${gateway.url}/readyz`));

    console.info("DB fixture: create and deploy the app");
    const created = await fetch(`${control.url}/api/apps`, {
      method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" },
      body: JSON.stringify({ name: "db-hitcounter", plan_id: "pln_db_acceptance" }), signal: AbortSignal.any([processes.signal, AbortSignal.timeout(10_000)]),
    });
    assert(created.ok, `Create app: HTTP ${created.status}: ${created.ok ? "" : await created.text()}`);
    const { id } = await created.json();
    assert.equal(typeof id, "string", "Created app must have an id");
    assert.match(id, /^[a-zA-Z0-9_-]+$/, "Fixture app id must be safe in SQL identifiers and literals");
    this.appId = id;
    this.controlUrl = control.url;
    this.apiUrl = `http://db-hitcounter.localhost:${gateway.number}`;
    const deployArgs = ["deploy", bundle, `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`];
    await assert.rejects(processes.run("deploy-before-migrate", binary("zeroship"), deployArgs, work), /HTTP 409.*schema_not_applied/);
    const role = `app_${id}_role`;
    assert.equal(await this.sql(`SELECT count(*) FROM pg_roles WHERE rolname='${role}'`), "0", "Deployment must not provision an app role");
    const database = await fetch(`${migrationServer.url}/v1/databases/${id}`, {
      method: "POST", headers: { authorization: `Bearer ${bearer}` }, signal: AbortSignal.timeout(30_000),
    });
    assert(database.ok, `Create database: ${database.status}: ${await database.text()}`);
    const ir = join(app, "generated/zeroship/migrations.ir.json");
    const applied = await processes.run("app-migrate", binary("zeroship"), [
      "migrate", ir, `--app=${id}`, `--control=${migrationServer.url}`, `--token=${bearer}`,
    ], work);
    assert.match(applied, /Applied [1-9][0-9]* migration op/);
    const reapplied = await processes.run("app-migrate-again", binary("zeroship"), [
      "migrate", ir, `--app=${id}`, `--control=${migrationServer.url}`, `--token=${bearer}`,
    ], work);
    assert.match(reapplied, /Applied 0 migration op/);
    assert.equal(await this.sql(`SELECT count(*) FROM pg_roles WHERE rolname='${role}'`), "1", "Migration must provision the app role");
    await processes.run("deploy", binary("zeroship"), deployArgs, work);
    for (const name of ["worker", "gateway"]) {
      const log = await readFile(join(this.logs, `${name}.log`), "utf8");
      assert(log.includes("usage-event outbox started"), `${name} must publish metering events`);
      assert(!log.includes("DISABLED"), `${name} must not disable its usage producer`);
    }
    // Use path routing for Node, which does not resolve wildcard localhost and
    // ignores a fetch Host override. Chromium uses the app hostname directly.
    const probe = `${gateway.url}/apps/db-hitcounter/hit/ready`;
    await this.waitFor("deployed database", async () => {
      const response = await fetch(probe, { signal: AbortSignal.timeout(5000) });
      const value = await response.json();
      await appendFile(join(this.logs, "readiness.jsonl"), `${JSON.stringify({ status: response.status, value })}\n`);
      // Gateway routing warmups have no app execution to meter. Count every
      // response from the counter itself, including unsuccessful database ops.
      if (response.ok && typeof value.wrote === "boolean" && value.path === "/hit/ready") this.readyRequests++;
      assert(response.ok && value.wrote === true && value.readBack > 0, `Database readiness: HTTP ${response.status}: ${JSON.stringify(value)}`);
      return true;
    });
  }

  close(): Promise<void> {
    return this.closing ??= (async () => {
      const errors: unknown[] = [];
      try { await this.processes.close(); } catch (error) { errors.push(error); }
      this.pgLogs?.destroy();
      this.pgOutput?.end();
      const results = await Promise.allSettled([
        ...this.ports.map((port) => port.release()),
        ...this.containers.map((container) => container.stop({ timeout: 10_000 })),
      ]);
      for (const result of results) if (result.status === "rejected") errors.push(result.reason);
      try { await rm(this.work, { recursive: true, force: true }); } catch (error) { errors.push(error); }
      if (errors.length) throw new AggregateError(errors, "Failed to clean up database fixture");
    })();
  }
}
