import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { generateKeyPairSync, randomBytes } from "node:crypto";
import { createWriteStream, existsSync, rmSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { connect, createServer } from "node:net";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { generate } from "selfsigned";
import { stringify } from "smol-toml";
import { GenericContainer, Wait, type StartedTestContainer } from "testcontainers";
import { isTypedId, typedIdFromStableSeed } from "@zeroship/server/typed-id";
import { assertStaticOnly, readManifest } from "./bundle";
import { EXAMPLES, type AppKind, type StackDescriptor } from "./descriptor";
import { issuer } from "./issuer";
import { Processes } from "./processes";

const suite = fileURLToPath(new URL("../", import.meta.url));
const root = resolve(suite, "../..");

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

/**
 * One request to the gateway addressed by Host, the way a browser reaches
 * `<app>.localhost:<port>`. Node's resolver is not guaranteed to map
 * `*.localhost`, so the socket goes to loopback and the name rides in the
 * header.
 */
function gatewayRequest(port: number, app: string, method: string, path: string, body?: string, signal?: AbortSignal) {
  return new Promise<{ status: number; body: string }>((resolve, reject) => {
    const req = request({
      host: "127.0.0.1", port, method, path, signal,
      headers: { host: `${app}.localhost:${port}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
    }, (res) => {
      const chunks: Buffer[] = [];
      res.on("data", (chunk: Buffer) => chunks.push(chunk));
      res.on("end", () => resolve({ status: res.statusCode ?? 0, body: Buffer.concat(chunks).toString() }));
      res.on("error", reject);
    });
    req.on("error", reject);
    req.end(body);
  });
}

/**
 * The platform the specs address: PostgreSQL with the platform schema, an
 * identity provider, and Control, a worker, the gateway and the CDC relay
 * built from this checkout, with the three render-mode examples built and
 * deployed onto it.
 */
export class Stack {
  readonly processes: Processes;
  private readonly containers: StartedTestContainer[] = [];
  private readonly ports: Awaited<ReturnType<typeof reservePort>>[] = [];
  private closing?: Promise<void>;

  private constructor(readonly work: string, readonly logs: string) {
    this.processes = new Processes(logs);
  }

  static async create(): Promise<Stack> {
    const artifacts = join(suite, ".artifacts");
    await mkdir(artifacts, { recursive: true });
    const logs = await mkdtemp(join(artifacts, "run-"));
    const work = await mkdtemp(join(tmpdir(), "zeroship-browser-stack-"));
    console.info(`Browser stack logs: ${logs}`);
    return new Stack(work, logs);
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

  private async container(name: string, image: GenericContainer): Promise<StartedTestContainer> {
    this.processes.signal.throwIfAborted();
    const log = this.processes.log(name);
    let container: StartedTestContainer;
    try {
      // The suite label tells a census this suite's containers from the other
      // fixtures that start the same images; the run label is what abandon()
      // removes by, which also reaches a container still starting.
      container = await image.withStartupTimeout(120_000).withLabels({
        "ai.zeroship.fixture": "e2e-browser-deployed", "ai.zeroship.fixture.run": basename(this.logs),
      }).withLogConsumer((stream) => stream.pipe(createWriteStream(log))).start();
    } catch (error) {
      throw new Error(`The ${name} container did not start; its output: ${log}`, { cause: error });
    }
    this.containers.push(container);
    // abandon() removes containers from a signal handler, which cannot wait
    // on testcontainers' asynchronous client, so it uses the docker CLI. That
    // holds only while the CLI reaches the daemon testcontainers chose, so
    // each container is looked up through the CLI rather than assumed.
    const seen = spawnSync("docker", ["inspect", "--format", "{{.Id}}", container.getId()], { encoding: "utf8" });
    assert.equal(seen.stdout?.trim(), container.getId(),
      `the docker CLI cannot see the ${name} container testcontainers started, so an interrupted run could not remove it; ` +
      `point both at one daemon: ${seen.error ?? seen.stderr}`);
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

  private async httpReady(url: string) {
    const response = await fetch(url, { signal: AbortSignal.any([this.processes.signal, AbortSignal.timeout(5_000)]) });
    const body = await response.text();
    if (!response.ok) throw new Error(`${url}: HTTP ${response.status}: ${body}`);
    return true;
  }

  /** Build an example into its own `dist/`, from a clean one, and return its bundle. */
  private async buildExample(kind: AppKind): Promise<string> {
    const name = EXAMPLES[kind];
    const directory = join(root, "examples", name);
    const label = `build-${name}`;
    await rm(join(directory, "dist"), { recursive: true, force: true });
    await this.processes.run(label, process.execPath, [join(directory, "node_modules/vite/bin/vite.js"), "build"], directory);
    const where = `examples/${name} (build log: ${this.processes.log(label)})`;
    const bundle = join(directory, "dist/app.zship");
    assert(existsSync(bundle), `${where}: the build wrote no ${bundle}`);
    if (kind === "ssg") assertStaticOnly(await readManifest(bundle), ["/index.html", "/about.html"], where);
    return bundle;
  }

  async start(): Promise<StackDescriptor> {
    const { work, processes } = this;
    const kinds = Object.keys(EXAMPLES) as AppKind[];
    console.info("Browser stack: build platform binaries and the examples");
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
    const bundles = {} as Record<AppKind, string>;
    for (const kind of kinds) bundles[kind] = await this.buildExample(kind);

    console.info("Browser stack: start PostgreSQL and apply platform migrations");
    const postgres = await this.container("postgres", new GenericContainer("postgres:18")
      .withExposedPorts(5432)
      .withEnvironment({ POSTGRES_PASSWORD: "browser-stack-password", POSTGRES_DB: "browser_stack" })
      .withCommand(["postgres", "-c", "wal_level=logical", "-c", "max_slot_wal_keep_size=128MB", "-c", "fsync=off"])
      .withWaitStrategy(Wait.forAll([Wait.forLogMessage("database system is ready to accept connections", 2), Wait.forListeningPorts()])));
    const authority = `${postgres.getHost()}:${postgres.getMappedPort(5432)}/browser_stack`;
    const dsn = `postgres://postgres:browser-stack-password@${authority}`;
    const migration = await this.secret("migrate.toml", stringify({ env: { platform: {
      url: dsn, dir: join(root, "db/migrations-ts"), schema: "zeroship", owner_app: "zeroship_platform",
      registry: join(root, "policies/platform-table-owners.json"), policy: [join(root, "policies/platform.policy.toml")],
    } } }));
    await processes.run("migrate", process.execPath, [join(root, "packages/zero-migrate-cli/dist/cli-bin.js"), "apply", "--config", migration, "--env", "platform", "--approve"], root);

    const identity = issuer();
    const jwks = await this.container("issuer", identity.container);
    const issuerUrl = `http://${jwks.getHost()}:${jwks.getMappedPort(80)}`;
    const owner = typedIdFromStableSeed("usr", "browser-stack-fixture-owner");
    const seeded = await postgres.exec(["psql", "-U", "postgres", "-d", "browser_stack", "-v", "ON_ERROR_STOP=1", "-c",
      `INSERT INTO zeroship.users (id, email, name, email_verified_at) VALUES ('${owner}', 'browser-${owner}@zeroship.test', 'Browser stack owner', NOW())`]);
    assert.equal(seeded.exitCode, 0, `Seed the creator who deploys: ${seeded.output}`);
    const bearer = identity.bearer(issuerUrl, owner);

    const keys: Record<string, string> = {};
    const peerKeys = [];
    for (const service of ["control", "gateway"]) {
      const { publicKey, privateKey } = generateKeyPairSync("ed25519");
      peerKeys.push({ ...publicKey.export({ format: "jwk" }), iss: `spiffe://zeroship.ai/svc/${service}` });
      keys[service] = await this.secret(`${service}.pem`, privateKey.export({ format: "pem", type: "pkcs8" }).toString());
    }
    const peers = await this.secret("peers.json", JSON.stringify({ keys: peerKeys }));
    // The worker holds no service key: it joins with a token a trusted signer
    // minted. Control mints for its own zone, as a single-host deployment
    // configures it, so the signer's credential and the import document
    // Control reads at startup are written here, and the worker is handed the
    // path Control mints the token into.
    const { publicKey: signerPublicKey, privateKey: signerPrivateKey } = generateKeyPairSync("ed25519");
    const signerId = typedIdFromStableSeed("wjs", "browser-stack-fixture-signer");
    const joinSigner = await this.secret("join-signer.json", JSON.stringify({
      signer_id: signerId,
      private_key: signerPrivateKey.export({ format: "pem", type: "pkcs8" }).toString(),
    }));
    const joinSigners = await this.secret("join-signers.json", JSON.stringify({
      signers: [{ id: signerId, zones: ["default"], public_key: signerPublicKey.export({ format: "jwk" }).x }],
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
    // Every worker serves env.db, and a worker's database service subscribes
    // to change data through the CDC relay over TLS, so the relay runs even
    // though these examples reach no database.
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
    const service = async (name: string, executable: string, port: typeof control, args: string[], env: NodeJS.ProcessEnv) => {
      await port.release();
      processes.start(name, binary(executable), args, work, { ...shared, ...env });
    };
    console.info("Browser stack: start the CDC relay, Control, the worker and the gateway");
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
    await service("worker", "zeroship-worker", worker, ["--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1"], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_JOIN_TOKEN_FILE: joinToken, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
    });
    await this.waitFor("worker", () => this.httpReady(`${worker.url}/readyz`));
    await service("gateway", "zeroship-gate", gateway, ["--no-config", "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker], {
      ZEROSHIP_GATEWAY_DATABASE_URL: dsn, ZEROSHIP_GATEWAY_SIGNING_KEY_FILE: keys.gateway,
      ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: masterKey, ZEROSHIP_GATEWAY_SERVICE_KEY_FILE: keys.gateway, ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("gateway", () => this.httpReady(`${gateway.url}/readyz`));

    console.info("Browser stack: create and deploy the examples");
    for (const kind of kinds) {
      const name = EXAMPLES[kind];
      const created = await fetch(`${control.url}/api/apps`, {
        method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" },
        body: JSON.stringify({ name }), signal: AbortSignal.any([processes.signal, AbortSignal.timeout(10_000)]),
      });
      const where = `Create app ${name} (control log: ${processes.log("control")})`;
      assert(created.ok, `${where}: HTTP ${created.status}: ${created.ok ? "" : await created.text()}`);
      const { id } = await created.json();
      assert(isTypedId(id, "app"), `${where}: the created app has no typed app id: ${id}`);
      await processes.run(`deploy-${name}`, binary("zeroship"), ["deploy", bundles[kind], `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`], work, { HOME: work });
    }

    // A deploy is accepted before the gateway's route table and the worker
    // have picked it up. Each example must answer through the gateway, by the
    // Host a browser sends, before any spec measures it: the document for
    // each, and for the SPA, whose document is a static asset, the RPC its
    // page calls, which only the worker can answer.
    const signal = () => AbortSignal.any([processes.signal, AbortSignal.timeout(5_000)]);
    const answered = (response: { status: number; body: string }, what: string) => {
      if (response.status !== 200) throw new Error(`${what}: HTTP ${response.status}: ${response.body}`);
      return true;
    };
    for (const kind of kinds) {
      const name = EXAMPLES[kind];
      await this.waitFor(`${name} document through the gateway`, async () =>
        answered(await gatewayRequest(gateway.number, name, "GET", "/", undefined, signal()), `${name} GET /`));
    }
    await this.waitFor(`${EXAMPLES.csr} RPC through the gateway`, async () =>
      answered(await gatewayRequest(gateway.number, EXAMPLES.csr, "POST", "/__zeroship/v1/listTodos", JSON.stringify({ json: {} }), signal()), `${EXAMPLES.csr} listTodos`));

    return { gatePort: gateway.number, apps: { ...EXAMPLES } };
  }

  /**
   * Tear down synchronously, for a signal that arrives during bring-up: the
   * runner exits without awaiting a setup it interrupted, and the reaper
   * container testcontainers relies on is shared by every testcontainers
   * process of this user, so it does not reap when this process alone exits.
   */
  abandon(): void {
    this.processes.cancel();
    this.containers.splice(0);
    const listed = spawnSync("docker", ["ps", "--all", "--quiet", "--filter", `label=ai.zeroship.fixture.run=${basename(this.logs)}`], { encoding: "utf8" });
    const ids = (listed.stdout ?? "").split("\n").filter(Boolean);
    if (listed.status !== 0) console.error(`Browser stack: could not list this run's containers: ${listed.error ?? listed.stderr}`);
    if (ids.length) {
      const removed = spawnSync("docker", ["rm", "--force", "--volumes", ...ids], { encoding: "utf8" });
      if (removed.status !== 0) console.error(`Browser stack: could not remove containers ${ids.join(" ")}: ${removed.error ?? removed.stderr}`);
    }
    rmSync(this.work, { recursive: true, force: true });
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
      if (errors.length) throw new AggregateError(errors, "Failed to clean up the browser stack");
    })();
  }
}
