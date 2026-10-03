import assert from "node:assert/strict";
import { createWriteStream, type WriteStream } from "node:fs";
import { generateKeyPairSync, randomBytes } from "node:crypto";
import { cp, mkdir, mkdtemp, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { connect } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { setTimeout as sleep } from "node:timers/promises";
import { zstdDecompressSync } from "node:zlib";
import { generate } from "selfsigned";
import { stringify } from "smol-toml";
import { Parser } from "tar";
import { GenericContainer, Wait, type StartedTestContainer } from "testcontainers";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";
import { Processes } from "../processes";
import { reservePort, type Port } from "../common";
import { issuer } from "../issuer";

/** A backing store a primitive example reaches through a binding primitive. */
export interface Backing {
  /** A redis configuration file the worker reaches a redis backend through. */
  kvConfig?: string;
  /** An object-store URL factory the worker and Control reach S3 through. */
  storeUrl?: (prefix: string) => string;
  /** The raw S3 container and endpoint, for post-deploy inspection. */
  s3?: StartedTestContainer;
  endpoint?: string;
}

/** Every fixture's identity, database and process bookkeeping. */
export interface FixtureSettings {
  /** Absolute path to the example directory whose suite owns this fixture. */
  exampleDir: string;
  /** Console prefix naming the example family, e.g. "KV". */
  label: string;
  /** `artifacts` keeps the work directory beside the suite; a string is a tmpdir prefix. */
  workDir: "artifacts" | string;
  cleanupMessage: string;
  cancelMessage: string;
  database: { name: string; password: string };
  issuer: { kid: string; scope: string };
  owner: { seed: string; emailPrefix: string; name: string };
  /** The join signer id; a stable string so a redeploy reuses one principal. */
  signer: string;
  /** Peer service names whose keys Control trusts, e.g. ["control", "gateway"]. */
  services: string[];
}

/** Resolve the logs and work directories a fixture lifecycle owns. */
export async function prepare(settings: FixtureSettings): Promise<{ work: string; logs: string }> {
  const artifacts = join(settings.exampleDir, "tests/.artifacts");
  await mkdir(artifacts, { recursive: true });
  const logs = await mkdtemp(join(artifacts, "run-"));
  const work = settings.workDir === "artifacts"
    ? await mkdtemp(join(artifacts, "work-"))
    : await mkdtemp(join(tmpdir(), settings.workDir));
  return { work, logs };
}

/** Read the manifest a packed bundle carries, without trusting its ordering. */
export async function readManifest(bundle: string): Promise<any> {
  let first: string | undefined;
  const chunks: Buffer[] = [];
  const parser = new Parser({ onReadEntry(entry) {
    first ??= entry.path;
    if (entry.path === "manifest.json") entry.on("data", (chunk: Buffer) => chunks.push(chunk));
    else entry.resume();
  } });
  await pipeline(Readable.from([zstdDecompressSync(await readFile(bundle))]), parser);
  assert.equal(first, "manifest.json");
  return JSON.parse(Buffer.concat(chunks).toString());
}

export abstract class PlatformBase {
  readonly processes: Processes;
  protected readonly root: string;
  private readonly containers: StartedTestContainer[] = [];
  private readonly ports: Port[] = [];
  protected postgres?: StartedTestContainer;
  private pgLogs?: Readable;
  private pgOutput?: WriteStream;
  private closing?: Promise<void>;
  private binaryResolver?: (name: string) => string;

  protected constructor(readonly settings: FixtureSettings, readonly work: string, readonly logs: string) {
    this.processes = new Processes(logs, settings.cancelMessage);
    this.root = resolve(settings.exampleDir, "../..");
  }

  protected async secret(name: string, contents: string): Promise<string> {
    const path = join(this.work, name);
    await writeFile(path, contents, { mode: 0o600 });
    return path;
  }

  protected async port(): Promise<Port> {
    const port = await reservePort();
    this.ports.push(port);
    return port;
  }

  protected async container(image: GenericContainer): Promise<StartedTestContainer> {
    this.processes.signal.throwIfAborted();
    const container = await image.withStartupTimeout(120_000).start();
    this.containers.push(container);
    this.processes.signal.throwIfAborted();
    return container;
  }

  protected async waitFor(label: string, ready: () => Promise<boolean>): Promise<void> {
    const deadline = Date.now() + 90_000;
    let lastError: unknown;
    do {
      this.processes.assertAlive();
      try { if (await ready()) return; } catch (error) { lastError = error; }
      await sleep(100, undefined, { signal: this.processes.signal });
    } while (Date.now() < deadline);
    throw new Error(`Not ready: ${label}; logs: ${this.logs}`, { cause: lastError });
  }

  protected async httpReady(url: string, init?: RequestInit): Promise<boolean> {
    const response = await fetch(url, { ...init, signal: AbortSignal.any([this.processes.signal, AbortSignal.timeout(5_000)]) });
    const body = await response.text();
    if (!response.ok) throw new Error(`${url}: HTTP ${response.status}: ${body}`);
    return true;
  }

  protected async pipePostgresLogs(): Promise<void> {
    assert(this.postgres, "PostgreSQL must be owned by this fixture");
    this.pgLogs = await this.postgres.logs();
    this.pgOutput = createWriteStream(join(this.logs, "postgres.log"), { mode: 0o600 });
    this.pgLogs.pipe(this.pgOutput);
  }

  protected async buildBinaries(packages: string[]): Promise<void> {
    const artifacts = await this.processes.run("cargo", process.env.CARGO ?? "cargo", [
      "build", "--message-format=json", "--locked", "--bins",
      ...packages.flatMap((name) => ["-p", name]),
    ], this.root, process.env);
    const binaries = new Map<string, string>();
    for (const line of artifacts.split("\n")) {
      if (!line.startsWith("{")) continue;
      const value = JSON.parse(line);
      if (value.reason === "compiler-artifact" && value.executable) binaries.set(value.target.name, value.executable);
    }
    this.binaryResolver = (name: string) => { const path = binaries.get(name); assert(path, `Missing built binary: ${name}`); return path; };
  }

  protected binary(name: string): string {
    assert(this.binaryResolver, "buildBinaries must run before a binary is started");
    return this.binaryResolver(name);
  }

  protected async copyApp(exclude: string[]): Promise<{ app: string; vite: string }> {
    const app = join(this.work, "app");
    await mkdir(app);
    for (const name of await readdir(this.settings.exampleDir)) {
      if (!exclude.includes(name)) await cp(join(this.settings.exampleDir, name), join(app, name), { recursive: true });
    }
    await symlink(join(this.settings.exampleDir, "node_modules"), join(app, "node_modules"), "dir");
    return { app, vite: join(app, "node_modules/vite/bin/vite.js") };
  }

  protected async buildBundle(app: string, vite: string): Promise<string> {
    await this.processes.run("app-build", process.execPath, [vite, "build"], app);
    return join(app, "dist", "app.zship");
  }

  protected async startPostgres(image: string, extra: string[] = []): Promise<{ postgres: StartedTestContainer; authority: string; dsn: string }> {
    const { name, password } = this.settings.database;
    const postgres = await this.container(new GenericContainer(image)
      .withExposedPorts(5432)
      .withEnvironment({ POSTGRES_PASSWORD: password, POSTGRES_DB: name })
      .withCommand(["postgres", "-c", "wal_level=logical", "-c", "max_slot_wal_keep_size=128MB", "-c", "fsync=off", ...extra])
      .withWaitStrategy(Wait.forAll([Wait.forLogMessage("database system is ready to accept connections", 2), Wait.forListeningPorts()])));
    this.postgres = postgres;
    const authority = `${postgres.getHost()}:${postgres.getMappedPort(5432)}/${name}`;
    const dsn = `postgres://postgres:${password}@${authority}`;
    return { postgres, authority, dsn };
  }

  protected async applyMigrations(dsn: string): Promise<void> {
    const migration = await this.secret("migrate.toml", stringify({ env: { platform: {
      url: dsn, dir: join(this.root, "db/migrations-ts"), schema: "zeroship", owner_app: "zeroship_platform",
      registry: join(this.root, "policies/platform-table-owners.json"), policy: [join(this.root, "policies/platform.policy.toml")],
    } } }));
    await this.processes.run("migrate", process.execPath, [join(this.root, "packages/zero-migrate-cli/dist/cli-bin.js"), "apply", "--config", migration, "--env", "platform", "--approve"], this.root);
  }

  protected async startIdentity(): Promise<{ issuerUrl: string; owner: string; bearer: string }> {
    assert(this.postgres, "PostgreSQL must be owned by this fixture");
    const { owner: spec, database, issuer: options } = this.settings;
    const identity = issuer(options);
    const jwks = await this.container(identity.container);
    const issuerUrl = `http://${jwks.getHost()}:${jwks.getMappedPort(80)}`;
    const owner = typedIdFromStableSeed("usr", spec.seed);
    const seeded = await this.postgres.exec(["psql", "-U", "postgres", "-d", database.name, "-v", "ON_ERROR_STOP=1", "-c",
      `INSERT INTO zeroship.users (id, email, name, email_verified_at) VALUES ('${owner}', '${spec.emailPrefix}-${owner}@zeroship.test', '${spec.name}', NOW())`]);
    assert.equal(seeded.exitCode, 0, `Seed authenticated fixture owner: ${seeded.output}`);
    return { issuerUrl, owner, bearer: identity.bearer(issuerUrl, owner) };
  }

  protected async writeServiceIdentity(): Promise<{
    keys: Record<string, string>; peers: string; joinToken: string;
    joinSigner: string; joinSigners: string; masterKey: string; broker: string;
  }> {
    const keys: Record<string, string> = {};
    const peerKeys = [];
    for (const service of this.settings.services) {
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
    const signerId = this.settings.signer;
    const signerPublicKeyX = (signerPublicKey.export({ format: "jwk" }).x) as string;
    const joinSigner = await this.secret("join-signer.json", JSON.stringify({
      signer_id: signerId,
      private_key: signerPrivateKey.export({ format: "pem", type: "pkcs8" }).toString(),
    }));
    const joinSigners = await this.secret("join-signers.json", JSON.stringify({
      signers: [{ id: signerId, zones: ["default"], public_key: signerPublicKeyX }],
    }));
    const joinToken = join(this.work, "join-token");
    const masterKey = randomBytes(32).toString("hex");
    const broker = await this.secret("broker", masterKey);
    return { keys, peers, joinToken, joinSigner, joinSigners, masterKey, broker };
  }

  protected async generateRelayCert(): Promise<{ cert: string; key: string }> {
    const certificate = await generate([{ name: "commonName", value: "localhost" }], {
      keyType: "ec", algorithm: "sha256", notBeforeDate: new Date(Date.now() - 60_000),
    });
    return { cert: await this.secret("relay-cert.pem", certificate.cert), key: await this.secret("relay-key.pem", certificate.private) };
  }

  /** Start the redis backing the KV examples reach through `env.kv`. */
  static async redisBacking(platform: PlatformBase): Promise<Backing> {
    const redis = await platform.container(new GenericContainer("redis:7").withExposedPorts(6379)
      .withWaitStrategy(Wait.forLogMessage("Ready to accept connections")));
    const kvConfig = await platform.secret("kv.toml", stringify({ backend: "redis", redis: { topology: {
      mode: "standalone", endpoint: `${redis.getHost()}:${redis.getMappedPort(6379)}`,
    } } }));
    return { kvConfig };
  }

  /** Start the S3 backing the storage examples reach through `env.storage`. */
  static async s3Backing(platform: PlatformBase): Promise<Backing> {
    const s3 = await platform.container(new GenericContainer("ghcr.io/versity/versitygw:v1.3.0")
      .withExposedPorts(7070)
      .withEntrypoint(["/bin/sh"])
      .withCommand(["-c", "mkdir -p /data/storage-fixture && exec /usr/local/bin/versitygw --port :7070 posix /data"])
      .withEnvironment({ ROOT_ACCESS_KEY_ID: "zeroship-fixture", ROOT_SECRET_ACCESS_KEY: "zeroship-fixture-secret" })
      .withWaitStrategy(Wait.forLogMessage("VersityGW")));
    const endpoint = "http://" + s3.getHost() + ":" + s3.getMappedPort(7070);
    const storeUrl = (prefix: string) => "s3://storage-fixture/" + prefix + "?provider=generic&endpoint=" + endpoint + "&region=us-east-1&style=path&dev_http=true&checksum=none";
    return { s3, endpoint, storeUrl };
  }

  protected async startRelay(shared: NodeJS.ProcessEnv, authority: string, cert: string, key: string, relay: Port): Promise<void> {
    await relay.release();
    this.processes.start("relay", this.binary("zeroship-data-cdc-server"), ["--no-config", "--listen", `127.0.0.1:${relay.number}`, "--tls-cert-file", cert, "--tls-key-file", key], this.work, {
      ...shared,
      ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL: `postgres://zeroship_cdc:zeroship_cdc@${authority}`,
    });
    await this.waitFor("CDC relay", () => new Promise((resolve) => {
      const socket = connect(relay.number, "127.0.0.1");
      const done = (ready: boolean) => { socket.destroy(); resolve(ready); };
      socket.once("connect", () => done(true));
      socket.once("error", () => done(false));
      socket.setTimeout(1000, () => done(false));
    }));
  }

  /** A service starter that releases its reserved port before spawning. */
  protected service(shared: NodeJS.ProcessEnv) {
    return async (name: string, executable: string, port: Port, args: string[], env: NodeJS.ProcessEnv) => {
      await port.release();
      return this.processes.start(name, this.binary(executable), args, this.work, { ...shared, ...env });
    };
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
      if (errors.length) throw new AggregateError(errors, this.settings.cleanupMessage);
    })();
  }
}
