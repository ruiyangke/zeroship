import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { appendFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { GenericContainer, Wait } from "testcontainers";
import { stringify } from "smol-toml";
import { PlatformBase, readManifest, prepare, type FixtureSettings } from "./base";
import type { Target, TargetContext } from "../common";

interface SqlResult { exitCode: number; stdout: string; output: string }

/** The LAST non-empty line psql wrote. */
function lastLine(result: SqlResult): string {
  const lines = result.stdout.split("\n").map((line) => line.trim()).filter((line) => line.length > 0);
  assert(lines.length > 0, `psql produced no value: ${result.output}`);
  return lines[lines.length - 1];
}

export interface DatabaseSettings extends FixtureSettings {
  appName: string;
  organization: { name: string; slugPrefix: string };
  /** The plan a creator cannot self-assign, when the app needs one at creation. */
  appPlanId?: string;
  cargoPackages: string[];
  copyExclude: string[];
  postgresImage: string;
  postgresExtra?: string[];
  /** RPC resources this demo exposes anonymously; the bundle must preserve that. */
  manifestResources?: Array<{ id: string; auth: string }>;
  /** The creator table the database's schema carries, and a column of it. */
  table: string;
  column: string;
  /** Whether the deployed app must have written rows before the fence is measured. */
  requireRows: boolean;
  /** Whether a second migrate apply must report zero ops. */
  reapplyCheck: boolean;
  metering: boolean;
  targets?: {
    devEnvVar: string;
    devMigrate?: boolean;
    list: (ctx: TargetContext) => Target[];
    probe: (apiUrl: string) => { path: string; body: unknown };
  };
}

/**
 * A database-first example: Control declares a database, a reconciler provisions
 * it, `zeroship migrate` fills its schema, an explicit binding grants the app
 * access, and the fixture measures the tenant fence on the live cluster.
 */
export class DatabasePlatform extends PlatformBase {
  // The database `zeroship db create` minted, its derived schema, and the
  // binding that lets the deployed app reach it. Read by the acceptance tests:
  // the app's tables live in the DATABASE's schema, which no app id names.
  databaseId = "";
  schema = "";
  bindingId = "";
  appId = "";
  apiUrl = "";
  controlUrl = "";
  bearer = "";
  readyRequests = 0;
  private readonly database: DatabaseSettings;

  protected constructor(readonly settings: DatabaseSettings, work: string, logs: string) {
    super(settings, work, logs);
    this.database = settings;
  }

  static async create(settings: DatabaseSettings): Promise<DatabasePlatform> {
    const { work, logs } = await prepare(settings);
    console.info(`${settings.label} fixture logs: ${logs}`);
    return new DatabasePlatform(settings, work, logs);
  }

  async sql(query: string): Promise<string> {
    const result = await this.psql(["-U", "postgres", "-d", this.settings.database.name], query);
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

  // A statement on the SHARED worker login, optionally narrowed the way the data
  // plane narrows: `SET ROLE` to the binding role the reconciler minted.
  private workerSql(query: string, role?: string): Promise<SqlResult> {
    const statement = role === undefined ? query : `SET ROLE "${role}"; ${query}`;
    return this.psql(["-d", `postgres://zeroship_worker:zeroship_worker@127.0.0.1:5432/${this.settings.database.name}`], statement);
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

  // The one JSON line a `zeroship` subcommand prints on stdout.
  //
  // The command's provenance and advice go to stderr and share this log, so the
  // body is selected rather than the whole output parsed. Refusing on anything
  // but exactly one candidate keeps a changed output shape from being read as an
  // empty result.
  private jsonLine(what: string, output: string): string {
    const lines = output.split("\n").filter((line) => line.startsWith("{"));
    assert.equal(lines.length, 1, `${what} must print exactly one JSON body:\n${output}`);
    return lines[0];
  }

  async start(): Promise<Target[]> {
    const { work, processes, settings } = this;
    console.info(`${settings.label} fixture: build platform binaries and database`);
    await processes.run("packages", "pnpm", ["build"], this.root, process.env);
    await this.buildBinaries(settings.cargoPackages);
    const { app, vite } = await this.copyApp(settings.copyExclude);
    // THE APP IS PACKED LATER, after the database exists. `runtime_descriptor`
    // carries the `dbs_` id, so a bundle built before the create would declare
    // the committed placeholder and be refused for a database nobody owns.

    console.info(`${settings.label} fixture: start backing containers and apply platform migrations`);
    const { authority, dsn } = await this.startPostgres(settings.postgresImage, settings.postgresExtra);
    await this.applyMigrations(dsn);

    let brokers = "";
    let topic = "";
    let config = "";
    if (settings.metering) {
      await this.sql(`
        INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,heap_limit_mb,spend_limit_default_cents)
        VALUES ('pln_db_acceptance','Database acceptance',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000}',256,100000000);
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
      brokers = `127.0.0.1:${kafkaPort.number}`;
      const redpanda = await this.container(new GenericContainer("docker.redpanda.com/redpandadata/redpanda:latest")
        .withExposedPorts({ container: 9092, host: kafkaPort.number })
        .withCommand(["redpanda", "start", "--overprovisioned", "--smp", "1", "--memory", "512M", "--reserve-memory", "0M", "--node-id", "0", "--check=false",
          "--kafka-addr", "internal://0.0.0.0:9093,external://0.0.0.0:9092", "--advertise-kafka-addr", `internal://127.0.0.1:9093,external://${brokers}`, "--set", "redpanda.auto_create_topics_enabled=true"])
        .withHealthCheck({ test: ["CMD", "rpk", "cluster", "health", "--exit-when-healthy"], interval: 1000, timeout: 3000, retries: 60 })
        .withWaitStrategy(Wait.forHealthCheck()));
      topic = "zeroship-usage-db-acceptance";
      const createdTopic = await redpanda.exec(["rpk", "topic", "create", topic, "-X", "brokers=127.0.0.1:9093"]);
      assert.equal(createdTopic.exitCode, 0, `Create the metering topic: ${createdTopic.output}`);
      config = await this.secret("metering.toml", stringify({ metering: { brokers, events_topic: topic } }));
    }

    await this.pipePostgresLogs();
    const { issuerUrl, bearer } = await this.startIdentity();
    this.bearer = bearer;
    const { keys, peers, joinToken, joinSigner, joinSigners, masterKey, broker } = await this.writeServiceIdentity();
    const shared = {
      ZEROSHIP_CONTROL_KEY: randomBytes(16).toString("hex"),
      ZEROSHIP_PAIRWISE_SALT: randomBytes(16).toString("hex"),
      ZEROSHIP_ORIGIN_SCHEME: "http", ZEROSHIP_AUTH_PLATFORM_ISSUER: issuerUrl,
      ZEROSHIP_OBSERVABILITY_LOG_FORMAT: "json",
    };
    const { cert, key } = await this.generateRelayCert();
    const blobs = join(work, "blobs");
    await mkdir(blobs);
    const control = await this.port();
    const worker = await this.port();
    const gateway = await this.port();
    const relay = await this.port();
    const migrationServer = await this.port();
    const service = this.service(shared);
    console.info(`${settings.label} fixture: start platform services`);
    await this.startRelay(shared, authority, cert, key, relay);
    await service("control", "zeroship-control", control, settings.metering
      ? ["--config", config, "--meter-provider", "lite", "--invoicer-provider", "lite", "--allow-unsupported-billing", "--spend-recompute-interval", "2", "--stripe-base-url", "http://127.0.0.1:1", "--port", `${control.number}`, "--blob-store", blobs, "--worker-urls", worker.url]
      : ["--no-config", "--port", `${control.number}`, "--blob-store", blobs, "--worker-urls", worker.url], {
      ...(settings.metering ? { ZEROSHIP_CONTROL_STRIPE_SECRET_KEY: "sk_test_unused" } : {}),
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
    await service("worker", "zeroship-worker", worker, settings.metering
      ? ["--metering-brokers", brokers, "--metering-events-topic", topic, "--metering-outbox-wal-path", join(work, "worker-outbox.redb"), "--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1"]
      : ["--port", `${worker.number}`, "--threads", "1", "--control-url", control.url, "--blob-store", blobs, "--poll-interval", "1"], {
      ZEROSHIP_WORKER_DATABASE_URL: `postgres://zeroship_worker:zeroship_worker@${authority}`,
      ZEROSHIP_WORKER_JOIN_TOKEN_FILE: joinToken, ZEROSHIP_WORKER_SERVICE_PEERS_FILE: peers,
      ZEROSHIP_WORKER_CDC_RELAY_URL: `wss://localhost:${relay.number}/internal/v1/cdc/subscribe`, ZEROSHIP_WORKER_CDC_RELAY_CA_FILE: cert,
    });
    await this.waitFor("worker", () => this.httpReady(`${worker.url}/readyz`));
    await service("gateway", "zeroship-gate", gateway, settings.metering
      ? ["--config", config, "--metering-outbox-wal-path", join(work, "gateway-outbox.redb"), "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker]
      : ["--no-config", "--port", `${gateway.number}`, "--control-url", control.url, "--worker-urls", worker.url, "--blob-store", blobs, "--poll-interval", "1", "--broker-secret-file", broker], {
      ZEROSHIP_GATEWAY_DATABASE_URL: dsn, ZEROSHIP_GATEWAY_SIGNING_KEY_FILE: keys.gateway,
      ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: masterKey, ZEROSHIP_GATEWAY_SERVICE_KEY_FILE: keys.gateway, ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE: peers,
    });
    await this.waitFor("gateway", () => this.httpReady(`${gateway.url}/readyz`));
    this.controlUrl = control.url;

    console.info(`${settings.label} fixture: create the database, through the control plane`);
    const organization = await this.api("POST", `${control.url}/api/organizations`, { name: settings.organization.name, slug: `${settings.organization.slugPrefix}-${randomBytes(6).toString("hex")}` });
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
    const createdDatabase = await processes.run("db-create", this.binary("zeroship"), [
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

    console.info(`${settings.label} fixture: migrate with no app and no binding`);
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
    // result, so there is no recorded-migration file to hand it. `--database` is
    // the `dbs_` id because this working directory holds no zeroship.jsonc, so
    // there are no labels to dereference here.
    const migrateArgs = ["migrate", migrations, `--database=${databaseId}`, `--control=${migrationServer.url}`, `--token=${bearer}`];
    const applied = await processes.run("app-migrate", this.binary("zeroship"), migrateArgs, work);
    assert.match(applied, /Applied [1-9][0-9]* migration op/);
    assert.equal(await this.sql(
      `SELECT count(*) FROM pg_tables WHERE schemaname='${schema}' AND tablename='${settings.table}'`), "1",
      "the apply must land the creator's table in the database's own schema");
    assert.notEqual(await creatorTables(), "0", "the apply must leave creator tables behind");
    if (settings.reapplyCheck) {
      const reapplied = await processes.run("app-migrate-again", this.binary("zeroship"), migrateArgs, work);
      assert.match(reapplied, /Applied 0 migration op/);
    }
    // The apply mints no binding role, so the absence above is still true after
    // it: what an app may touch comes from the capability roles, not from here.
    assert.equal(await this.sql(`SELECT count(*) FROM pg_roles WHERE left(rolname,8)='zs_bind_'`), "0",
      "an apply mints no binding role");

    console.info(`${settings.label} fixture: pack the app against the database it will bind`);
    const configPath = join(app, "zeroship.jsonc");
    const declared = await readFile(configPath, "utf8");
    const placeholders = declared.match(/"id":\s*"dbs_[0-9a-z]+"/g) ?? [];
    assert.equal(placeholders.length, 1, "this app declares exactly one database id to point at the created one");
    await writeFile(configPath, declared.replace(placeholders[0], `"id": "${databaseId}"`));
    await processes.run("app-build", process.execPath, [vite, "build"], app);
    const bundle = join(app, "dist/app.zship");
    const manifest = await readManifest(bundle);
    for (const resource of settings.manifestResources ?? []) {
      assert.equal(manifest.resources?.[resource.id]?.auth, resource.auth, `${resource.id} must be publicly reachable in this demo`);
    }
    assert.equal(typeof manifest.runtime_descriptor?.[0]?.hash, "string");
    assert(manifest.runtime_descriptor[0].hash.length > 0, "Bundle must bind its runtime descriptor");
    assert(manifest.runtime_descriptor[0].primary, "the primary database is the one env.db reaches");
    assert.equal(manifest.runtime_descriptor[0].database_id, databaseId,
      "the bundle must declare the database this fixture created");
    // The SQLite tier keeps its own copy of the same migrations. It reaches no
    // cluster, holds no binding, and is the control the acceptance suite
    // compares the deployed tier against.
    if (settings.targets?.devMigrate) {
      await processes.run("dev-migrate", process.execPath, [join(this.root, "packages/vite-plugin/dist/cli/migrate-dev.js")], app);
    }

    console.info(`${settings.label} fixture: create the app, refuse the unbound deploy, bind, deploy`);
    const created = await this.api("POST", `${control.url}/api/apps`, {
      name: settings.appName,
      ...(settings.appPlanId ? { plan_id: settings.appPlanId } : {}),
      project_id: project.id,
    });
    const id = this.appId = created.id;
    assert.equal(typeof id, "string", "Created app must have an id");
    assert.match(id, /^[a-zA-Z0-9_-]+$/, "Fixture app id must be safe in SQL identifiers and literals");
    this.apiUrl = `http://${settings.appName}.localhost:${gateway.number}`;
    // The nonempty control for "no app existed at migrate time": the same query
    // over the same project now answers one.
    assert.equal(await this.sql(`SELECT count(*) FROM zeroship.apps WHERE project_id='${project.id}'`), "1",
      "the app this fixture created lands in the database's project");

    const deployArgs = ["deploy", bundle, `--app=${id}`, `--control=${control.url}`, `--token=${bearer}`];
    // DECLARING A DATABASE GRANTS NOTHING. The bundle names the database, the
    // schema is already migrated, and the deploy is still refused: a grant is an
    // explicit act, and this is the refusal that says so.
    await assert.rejects(processes.run("deploy-before-bind", this.binary("zeroship"), deployArgs, work), /HTTP 409.*database_not_bound/);
    assert.equal(await this.sql(`SELECT count(*) FROM zeroship.apps WHERE id='${id}' AND deploy_hash IS NOT NULL`), "0",
      "a refused deploy makes nothing live");

    await processes.run("db-bind", this.binary("zeroship"), [
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

    await processes.run("deploy", this.binary("zeroship"), deployArgs, work);
    if (settings.metering) {
      for (const name of ["worker", "gateway"]) {
        const log = await readFile(join(this.logs, `${name}.log`), "utf8");
        assert(log.includes("usage-event outbox started"), `${name} must publish metering events`);
        assert(!log.includes("DISABLED"), `${name} must not disable its usage producer`);
      }
      // Use path routing for Node, which does not resolve wildcard localhost and
      // ignores a fetch Host override. Chromium uses the app hostname directly.
      const probe = `${gateway.url}/apps/${settings.appName}/hit/ready`;
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

    let targets: Target[] = [];
    if (settings.targets) {
      console.info(`${settings.label} fixture: start local Vite and wait for app dispatch`);
      const { dev, ui } = await this.startDev(app, vite, settings.targets.devEnvVar);
      targets = settings.targets.list({ dev, ui, gateway, log: (name) => processes.log(name) });
      for (const target of targets) {
        const probe = settings.targets.probe(target.apiUrl);
        await this.waitForTarget(target, () => this.httpReady(`${target.apiUrl}${probe.path}`, {
          method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: probe.body }),
        }));
      }
    }

    // THE APP REACHES ITS DATA THROUGH THE BINDING, and only through it. Both
    // arms run on the SAME login the worker connects as and differ in one
    // statement: the `SET ROLE` the data plane issues per transaction. Without
    // it PostgreSQL refuses `42501`; with it the deployed app's rows are there
    // to count.
    const owned = await this.sql(`SELECT count(*) FROM "${schema}".${settings.table}`);
    if (settings.requireRows) assert(Number(owned) > 0, "the deployed app must have written rows for this fence to be measured over");
    const unnarrowed = await this.workerSql(`SELECT count(*) FROM "${schema}".${settings.table}`);
    assert.notEqual(unnarrowed.exitCode, 0, `the shared worker login must not reach a tenant schema: ${unnarrowed.output}`);
    assert.match(unnarrowed.output, /42501/, `PostgreSQL must be what refuses it: ${unnarrowed.output}`);
    const narrowed = await this.workerSql(`SELECT count(*) FROM "${schema}".${settings.table}`, bindingRole);
    assert.equal(narrowed.exitCode, 0, `the binding role must reach this database: ${narrowed.output}`);
    assert.equal(lastLine(narrowed), owned, "the binding role reads the table the migration created in this database");
    // WHICH capability the binding carries, as PostgreSQL answers it. The grants
    // an apply emits are column-listed, which is what makes column-level GRANT the
    // masking authority; a readonly binding answers `false` to the second.
    const capability = await this.workerSql(
      `SELECT has_column_privilege('${schema}.${settings.table}', '${settings.column}', 'SELECT')::text,
              has_column_privilege('${schema}.${settings.table}', '${settings.column}', 'INSERT')::text`,
      bindingRole);
    assert.equal(capability.exitCode, 0, capability.output);
    assert.equal(lastLine(capability), "true|true",
      "a readwrite binding must carry the column-listed read AND write grants");
    return targets;
  }
}
