import { fileURLToPath } from "node:url";
import { DatabasePlatform, prepare, type DatabaseSettings } from "@zeroship/example-testkit";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: DatabaseSettings = {
  exampleDir,
  label: "DB",
  workDir: "artifacts",
  cleanupMessage: "Failed to clean up database fixture",
  cancelMessage: "Database fixture cancelled",
  database: { name: "db_fixture", password: "db-fixture-password" },
  issuer: {
    kid: "db-acceptance",
    scope: "organization:create project:create database:read database:write database:migrate apps:read apps:write apps:deploy deployments:read secrets:read billing:read billing:write",
  },
  owner: { seed: "db-hitcounter-fixture-owner", emailPrefix: "db", name: "DB fixture owner" },
  signer: "wjs_dbhitcounterfixturesigner",
  services: ["control", "gateway"],
  appName: "db-hitcounter",
  organization: { name: "DB hitcounter fixture", slugPrefix: "db-hitcounter" },
  appPlanId: "pln_db_acceptance",
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server", "zeroship-migrate-server"],
  copyExclude: ["node_modules", "dist", ".zeroship", "tests", "test", "e2e"],
  postgresImage: "postgres:16",
  postgresExtra: ["-c", "log_statement=all"],
  table: "hits",
  column: "path",
  requireRows: true,
  reapplyCheck: true,
  metering: true,
};

export class Platform extends DatabasePlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`DB fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
