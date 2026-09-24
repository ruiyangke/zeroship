import assert from "node:assert/strict";
import { createWriteStream, type WriteStream } from "node:fs";
import { generateKeyPairSync, randomBytes } from "node:crypto";
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
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";
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

// The bundle must name the database this fixture created, not the id the
// committed `zeroship.jsonc` carries. That is what makes the deploy gate and
// the runtime reach the schema `zeroship migrate` filled, so it is asserted on
// the packed artifact rather than on the file the packer read.
async function checkManifest(bundle: string, databaseId: string) {
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
  assert.equal(typeof manifest.runtime_descriptor?.[0]?.hash, "string");
  assert(manifest.runtime_descriptor[0].hash.length > 0, "Bundle must bind its runtime descriptor");
  assert(manifest.runtime_descriptor[0].primary, "the primary database is the one env.db reaches");
  assert.equal(manifest.runtime_descriptor[0].database_id, databaseId,
    "the bundle must declare the database this fixture created");
}

// One `psql` invocation's result, kept whole so a REFUSAL can be asserted on.
//
// `sql` below throws on a non-zero exit, which is right for every statement
// this fixture expects to succeed. The tenant fence is asserted the other way
// round: the shared worker login must be REFUSED, and a helper that threw
// would make the refusal indistinguishable from a broken fixture.
interface SqlResult { exitCode: number; stdout: string; output: string }

// The LAST non-empty line psql wrote.
//
// A narrowed statement is a role change followed by the query, and psql prints
// the command tag of each, so the value asked for is the last line rather than
// the whole of stdout. Refusing on no line at all keeps an empty result from
// comparing equal to an empty expectation.
function lastLine(result: SqlResult): string {
  const lines = result.stdout.split("\n").map((line) => line.trim()).filter((line) => line.length > 0);
  assert(lines.length > 0, `psql produced no value: ${result.output}`);
  return lines[lines.length - 1];
}

export class Platform {
  readonly processes: Processes;
  private postgres?: StartedTestContainer;
  appId = "";
  apiUrl = "";
  controlUrl = "";
  bearer = "";
  readyRequests = 0;
  // The database `zeroship db create` minted, and the schema derived from it.
  // Both are read by the acceptance test: the app's tables live in the
  // DATABASE's schema, which no app id names.
  databaseId = "";
  schema = "";
  bindingId = "";

  async sql(query: string): Promise<string> {
    const result = await this.psql(["-U", "postgres", "-d", "db_fixture"], query);
    assert.equal(result.exitCode, 0, result.output);
    return result.stdout.trim();
  }

  // One statement through the container's own `psql`, as whichever login the
  // connection arguments name.
  //
  // `VERBOSITY=verbose` is what puts the server's SQLSTATE in the output, so a
  // refusal can be asserted as `42501` rather than as message text.
  private async psql(connection: string[], query: string): Promise<SqlResult> {
    assert(this.postgres, "PostgreSQL must be owned by this fixture");
    const result = await this.postgres.exec([
      "psql", ...connection, "-v", "ON_ERROR_STOP=1", "-v", "VERBOSITY=verbose", "-tA", "-c", query,
    ]);
    return { exitCode: result.exitCode, stdout: result.stdout, output: result.output };
  }

  // A statement on the SHARED worker login, optionally narrowed the way the
  // data plane narrows: `SET ROLE` to the binding role the reconciler minted.
  private workerSql(query: string, role?: string): Promise<SqlResult> {
    const statement = role === undefined ? query : `SET ROLE "${role}"; ${query}`;
    return this.psql(["-d", "postgres://zeroship_worker:zeroship_worker@127.0.0.1:5432/db_fixture"], statement);
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

  // One creator-facing control-plane call, as the fixture owner.
  private async api(method: string, url: string, body?: unknown): Promise<any> {
    const response = await fetch(url, {
      method,
      headers: { authorization: `Bearer ${this.bearer}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.any([this.processes.signal, AbortSignal.timeout(15_000)]),
    });
    const text = await response.text();
    assert(response.ok, `${method} ${url}: HTTP ${response.status}: ${text}`);
    return text.length === 0 ? undefined : JSON.parse(text);
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
    // THE APP IS PACKED LATER, after the database exists. `runtime_descriptor`
    // carries the `dbs_` id, so a bundle built before the create would declare
    // the committed placeholder and be refused for a database nobody owns.

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
    const owner = typedIdFromStableSeed("usr", "db-hitcounter-fixture-owner");
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
    // The worker holds no service key: it joins with a token a trusted signer
    // minted. Control mints for its own zone here, exactly as a single-host
    // deployment configures it, so this fixture writes the signer's
    // credential and the import document Control reads at startup, and hands
    // the worker the path Control mints the token into.
    const { publicKey: signerPublicKey, privateKey: signerPrivateKey } = generateKeyPairSync("ed25519");
    const signerId = "wjs_dbhitcounterfixturesigner";
    const signerPublicKeyX = (signerPublicKey.export({ format: "jwk" }).x) as string;
    const joinSigner = await this.secret("join-signer.json", JSON.stringify({
      signer_id: signerId,
      private_key: signerPrivateKey.export({ format: "pem", type: "pkcs8" }).toString(),
    }));
    const joinSigners = await this.secret("join-signers.json", JSON.stringify({
      signers: [{ id: signerId, zones: ["default"], public_key: signerPublicKeyX }],
    }));
    const joinToken = join(work, "join-token");
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
    await service("control", "zeroship-control", control, ["--config", config, "--meter-provider", "lite", "--invoicer-provider", "lite", "--allow-unsupported-billing", "--spend-recompute-interval", "2", "--stripe-base-url", "http://127.0.0.1:1", "--port", `${control.number}`, "--blob-store", blobs, "--worker-urls", worker.url], {
      ZEROSHIP_CONTROL_STRIPE_SECRET_KEY: "sk_test_unused", ZEROSHIP_CONTROL_DATABASE_URL: dsn, ZEROSHIP_CONTROL_MASTER_KEY: masterKey,
      ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING: "true", ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS: "127.0.0.1/32",
      ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS: `${worker.number}`,
      ZEROSHIP_CONTROL_SERVICE_KEY_FILE: keys.control, ZEROSHIP_CONTROL_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_CONTROL_JOIN_SIGNERS_FILE: joinSigners,
      ZEROSHIP_CONTROL_JOIN_TOKEN_SIGNER_FILE: joinSigner,
      ZEROSHIP_CONTROL_JOIN_TOKEN_FILE: joinToken,
      ZEROSHIP_CONTROL_JOIN_TOKEN_ZONE: "default",
    });
    await this.waitFor("control", () => this.httpReady(`${control.url}/readyz`));
    // THE RECONCILER IS IN THIS PROCESS. Control declares a database and stops;
    // the cluster it names is made to match here, so the interval is what bounds
    // how long `provisioning` lasts. A deployment's default is measured in tens
    // of seconds, which is a wait this fixture has no reason to sit through.
    await service("migrate-server", "zeroship-migrate-server", migrationServer, [
      "--no-config", "--port", `${migrationServer.number}`, "--tmp-dir", join(work, "migrations"), "--mutation-rate-limit-burst", "3",
      "--reconcile-interval-seconds", "1",
    ], {
      ZEROSHIP_MIGRATE_SERVER_DATABASE_URL: dsn, ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL: dsn,
      ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY: randomBytes(32).toString("hex"),
    });
    await this.waitFor("migrate-server", () => this.httpReady(`${migrationServer.url}/readyz`));
    await service("worker", "zeroship-worker", worker, ["--metering-brokers", brokers, "--metering-events-topic", topic, "--metering-outbox-wal-path", join(work, "worker-outbox.redb"), "--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1"], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_JOIN_TOKEN_FILE: joinToken, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
    });
    await this.waitFor("worker", () => this.httpReady(`${worker.url}/readyz`));
    await service("gateway", "zeroship-gate", gateway, ["--config", config, "--metering-outbox-wal-path", join(work, "gateway-outbox.redb"), "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker], {
      ZEROSHIP_GATEWAY_DATABASE_URL: dsn, ZEROSHIP_GATEWAY_SIGNING_KEY_FILE: keys.gateway,
      ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: masterKey, ZEROSHIP_GATEWAY_SERVICE_KEY_FILE: keys.gateway, ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("gateway", () => this.httpReady(`${gateway.url}/readyz`));
    this.controlUrl = control.url;

    console.info("DB fixture: create the database, through the control plane");
    const organization = await this.api("POST", `${control.url}/api/organizations`, { name: "DB hitcounter fixture", slug: `db-hitcounter-${randomBytes(6).toString("hex")}` });
    assert.match(organization.id, /^org_/, `Create organization: ${JSON.stringify(organization)}`);
    const project = await this.api("POST", `${control.url}/api/organizations/${organization.id}/projects`, { name: "acceptance", slug: `acceptance-${randomBytes(6).toString("hex")}` });
    assert.match(project.id, /^prj_/, `Create project: ${JSON.stringify(project)}`);
    // PLACEMENT ADMITS ACTIVE DATASTORES ONLY, so the create below has nowhere
    // to put a database until the reconciler has registered this cluster and
    // bootstrapped it. Waiting on the row it writes is what tells a slow
    // registration apart from a create that would be refused outright.
    const zone = await this.sql(`SELECT execution_zone_id FROM zeroship.projects WHERE id='${project.id}'`);
    assert.match(zone, /^ezn_/, "the project must sit in an execution zone a cluster can serve");
    await this.waitFor("datastore registration", async () =>
      await this.sql(`SELECT count(*) FROM zeroship.datastores WHERE status='active' AND execution_zone_id='${zone}'`) === "1");

    const creatorArgs = [`--control=${control.url}`, `--token=${bearer}`];
    const createdDatabase = await processes.run("db-create", binary("zeroship"), [
      "db", "create", "main", `--project=${project.id}`, ...creatorArgs,
    ], work);
    const databaseRecord = JSON.parse(this.jsonLine("db create", createdDatabase));
    const databaseId = this.databaseId = databaseRecord.id;
    assert.match(databaseId, /^dbs_/, `Create database: ${JSON.stringify(databaseRecord)}`);
    assert.equal(databaseRecord.project_id, project.id, "the database belongs to the project it was created in");
    // CONTROL FOR THE WAIT BELOW. Control declares and stops, so a database is
    // `provisioning` until a cluster has been made to match. A fixture that
    // never saw this value would be waiting for a status that was already there.
    assert.equal(databaseRecord.status, "provisioning", "control declares a database; it does not provision one");
    const schema = this.schema = `db_${databaseId}`;
    const roles = {
      migrator: `zs_db_${databaseId}_mig`,
      readwrite: `zs_db_${databaseId}_rw`,
      readonly: `zs_db_${databaseId}_ro`,
    };

    // THE WAIT READS THE ROW, THE ASSERTION READS THE CREATOR SURFACE. Control's
    // admin quota is per minute and a convergence wait polls far faster than
    // that, so a loop over the HTTP surface would be answered 429 and report a
    // database that never converged.
    await this.waitFor("database convergence", async () =>
      await this.sql(`SELECT status FROM zeroship.databases WHERE id='${databaseId}'`) === "active");
    const listedDatabases = await this.api("GET", `${control.url}/api/projects/${project.id}/databases`);
    const converged = listedDatabases.databases.find((record: { id: string }) => record.id === databaseId);
    assert(converged, `the created database must be listed: ${JSON.stringify(listedDatabases)}`);
    assert.equal(converged.status, "active", "the creator's own view must show the converged database");
    // WHAT `active` MEANS, read off the cluster rather than off the row that
    // claims it: the schema exists, it is owned by the migrator role, and both
    // capability roles are there for a binding to inherit.
    assert.equal(await this.sql(
      `SELECT count(*) FROM pg_namespace n JOIN pg_roles o ON o.oid = n.nspowner
        WHERE n.nspname='${schema}' AND o.rolname='${roles.migrator}'`,
    ), "1", "the reconciler must create the schema and hand it to the migrator role");
    for (const role of Object.values(roles)) {
      assert.equal(await this.sql(`SELECT count(*) FROM pg_roles WHERE rolname='${role}'`), "1",
        `the reconciler must mint ${role}`);
    }

    console.info("DB fixture: migrate with no app and no binding");
    // THE DECOUPLING, ASSERTED. A migration is authorized at the database's own
    // project by a qualifying seat, so nothing below needs an app to exist or a
    // binding to have been granted. Both absences are stated here and both are
    // contradicted later in this same fixture, which is what keeps them from
    // passing over a world where an app or a binding could not have existed.
    assert.equal(await this.sql(`SELECT count(*) FROM zeroship.apps WHERE project_id='${project.id}'`), "0",
      "no app exists when this database is migrated");
    assert.equal(await this.sql(`SELECT count(*) FROM zeroship.database_bindings WHERE database_id='${databaseId}'`), "0",
      "no binding exists when this database is migrated");
    assert.equal(await this.sql(`SELECT count(*) FROM pg_roles WHERE left(rolname,8)='zs_bind_'`), "0",
      "no binding role exists when this database is migrated");
    const creatorTables = () => this.sql(
      `SELECT count(*) FROM pg_tables WHERE schemaname='${schema}' AND left(tablename,10) <> '__zeroship'`);
    assert.equal(await creatorTables(), "0", "the converged schema carries no creator table before the apply");

    const migrations = join(app, "migrations");
    // The DIRECTORY: `zeroship migrate` records the `.ts` itself and posts the
    // result, so there is no recorded-migration file to hand it. `--database`
    // is the `dbs_` id because this working directory holds no zeroship.jsonc,
    // so there are no labels to dereference here.
    const migrateArgs = ["migrate", migrations, `--database=${databaseId}`, `--control=${migrationServer.url}`, `--token=${bearer}`];
    const applied = await processes.run("app-migrate", binary("zeroship"), migrateArgs, work);
    assert.match(applied, /Applied [1-9][0-9]* migration op/);
    assert.equal(await this.sql(
      `SELECT count(*) FROM pg_tables WHERE schemaname='${schema}' AND tablename='hits'`), "1",
      "the apply must land the creator's table in the database's own schema");
    assert.notEqual(await creatorTables(), "0", "the apply must leave creator tables behind");
    const reapplied = await processes.run("app-migrate-again", binary("zeroship"), migrateArgs, work);
    assert.match(reapplied, /Applied 0 migration op/);
    // The apply mints no binding role, so the absence above is still true after
    // it: what an app may touch comes from the capability roles, not from here.
    assert.equal(await this.sql(`SELECT count(*) FROM pg_roles WHERE left(rolname,8)='zs_bind_'`), "0",
      "an apply mints no binding role");

    console.info("DB fixture: pack the app against the database it will bind");
    const configPath = join(app, "zeroship.jsonc");
    const declared = await readFile(configPath, "utf8");
    const placeholders = declared.match(/"id":\s*"dbs_[0-9a-z]+"/g) ?? [];
    assert.equal(placeholders.length, 1, "this app declares exactly one database id to point at the created one");
    await writeFile(configPath, declared.replace(placeholders[0], `"id": "${databaseId}"`));
    await processes.run("app-build", process.execPath, [vite, "build"], app);
    const bundle = join(app, "dist/app.zship");
    await checkManifest(bundle, databaseId);

    console.info("DB fixture: create the app, refuse the unbound deploy, bind, deploy");
    const created = await this.api("POST", `${control.url}/api/apps`, { name: "db-hitcounter", plan_id: "pln_db_acceptance", project_id: project.id });
    const id = created.id;
    assert.equal(typeof id, "string", "Created app must have an id");
    assert.match(id, /^[a-zA-Z0-9_-]+$/, "Fixture app id must be safe in SQL identifiers and literals");
    this.appId = id;
    this.apiUrl = `http://db-hitcounter.localhost:${gateway.number}`;
    // The nonempty control for "no app existed at migrate time": the same query
    // over the same project now answers one.
    assert.equal(await this.sql(`SELECT count(*) FROM zeroship.apps WHERE project_id='${project.id}'`), "1",
      "the app this fixture created lands in the database's project");

    const deployArgs = ["deploy", bundle, `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`];
    // DECLARING A DATABASE GRANTS NOTHING. The bundle names the database, the
    // schema is already migrated, and the deploy is still refused: a grant is an
    // explicit act, and this is the refusal that says so.
    await assert.rejects(processes.run("deploy-before-bind", binary("zeroship"), deployArgs, work), /HTTP 409.*database_not_bound/);
    assert.equal(await this.sql(`SELECT count(*) FROM zeroship.apps WHERE id='${id}' AND deploy_hash IS NOT NULL`), "0",
      "a refused deploy makes nothing live");

    await processes.run("db-bind", binary("zeroship"), [
      "db", "bind", databaseId, `--app=${id}`, "--capability=readwrite", ...creatorArgs,
    ], work);
    // Same split as the database wait: the row is polled, the creator surface is
    // read once. `observed_generation` catching up to `generation` is the cluster
    // saying the two role edges below exist; a deploy is refused until it does.
    await this.waitFor("binding convergence", async () =>
      await this.sql(
        `SELECT count(*) FROM zeroship.database_bindings
          WHERE database_id='${databaseId}' AND app_id='${id}'
            AND status='active' AND observed_generation = generation`) === "1");
    const listedBindings = await this.api("GET", `${control.url}/api/databases/${databaseId}/bindings`);
    const binding = listedBindings.bindings.find((record: { app_id: string }) => record.app_id === id);
    assert(binding, `the declared binding must be listed: ${JSON.stringify(listedBindings)}`);
    const bindingId = this.bindingId = binding.id;
    assert.equal(binding.capability, "readwrite");
    assert.equal(binding.status, "active");
    assert.equal(binding.observed_generation, binding.generation);
    const bindingRole = `zs_bind_${bindingId}`;
    // THE TWO EDGES, each with the grant option that makes it a fence. The
    // binding role INHERITS one capability role and cannot assume it; the shared
    // worker login may ASSUME the binding role and inherits nothing from it.
    assert.equal(await this.sql(
      `SELECT count(*) FROM pg_auth_members m
         JOIN pg_roles granted ON granted.oid = m.roleid
         JOIN pg_roles member ON member.oid = m.member
        WHERE granted.rolname='${roles.readwrite}' AND member.rolname='${bindingRole}'
          AND m.inherit_option AND NOT m.set_option`,
    ), "1", "the binding role must inherit the readwrite capability without being able to assume it");
    assert.equal(await this.sql(
      `SELECT count(*) FROM pg_auth_members m
         JOIN pg_roles granted ON granted.oid = m.roleid
         JOIN pg_roles member ON member.oid = m.member
        WHERE granted.rolname='${bindingRole}' AND member.rolname='zeroship_worker'
          AND m.set_option AND NOT m.inherit_option`,
    ), "1", "the worker login must be able to assume the binding role and inherit nothing from it");
    // The nonempty control for "no binding existed at migrate time".
    assert.equal(await this.sql(`SELECT count(*) FROM zeroship.database_bindings WHERE database_id='${databaseId}'`), "1",
      "the binding this fixture granted is the one the deploy verifies");

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

    // THE APP REACHES ITS DATA THROUGH THE BINDING, and only through it. Both
    // arms run on the SAME login the worker connects as and differ in one
    // statement: the `SET ROLE` the data plane issues per transaction. Without
    // it PostgreSQL refuses `42501`; with it the rows the deployed app just
    // wrote are there to count.
    const written = Number(await this.sql(`SELECT count(*) FROM "${schema}".hits`));
    assert(written > 0, "the deployed app must have written rows for this fence to be measured over");
    const unnarrowed = await this.workerSql(`SELECT count(*) FROM "${schema}".hits`);
    assert.notEqual(unnarrowed.exitCode, 0, `the shared worker login must not reach a tenant schema: ${unnarrowed.output}`);
    assert.match(unnarrowed.output, /42501/, `PostgreSQL must be what refuses it: ${unnarrowed.output}`);
    const narrowed = await this.workerSql(`SELECT count(*) FROM "${schema}".hits`, bindingRole);
    assert.equal(narrowed.exitCode, 0, `the binding role must reach this database: ${narrowed.output}`);
    assert.equal(lastLine(narrowed), String(written),
      "the rows the deployed app wrote are the rows the binding role reads");
    // WHICH capability the binding carries, as PostgreSQL answers it. The grants
    // an apply emits are column-listed, which is what makes column-level GRANT the
    // masking authority; a readonly binding answers `false` to the second.
    const capability = await this.workerSql(
      `SELECT has_column_privilege('${schema}.hits', 'path', 'SELECT')::text,
              has_column_privilege('${schema}.hits', 'path', 'INSERT')::text`,
      bindingRole);
    assert.equal(capability.exitCode, 0, capability.output);
    assert.equal(lastLine(capability), "true|true",
      "a readwrite binding must carry the column-listed read AND write grants");
  }

  // The one JSON line a `zeroship` subcommand prints on stdout.
  //
  // The command's provenance and advice go to stderr and share this log, so the
  // body is selected rather than the whole output parsed. Refusing on anything
  // but exactly one candidate keeps a changed output shape from being read as
  // an empty result.
  private jsonLine(what: string, output: string): string {
    const lines = output.split("\n").filter((line) => line.startsWith("{"));
    assert.equal(lines.length, 1, `${what} must print exactly one JSON body:\n${output}`);
    return lines[0];
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
