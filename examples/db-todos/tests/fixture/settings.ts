import { fileURLToPath } from "node:url";
import { DatabasePlatform, prepare, type DatabaseSettings } from "@zeroship/example-testkit";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

export const fixtureOwnerId = typedIdFromStableSeed("usr", "db-todos-platform-fixture-owner");

const settings: DatabaseSettings = {
  exampleDir,
  label: "DB",
  workDir: "artifacts",
  cleanupMessage: "Failed to clean up database fixture",
  cancelMessage: "Database fixture cancelled",
  database: { name: "db_fixture", password: "db-fixture-password" },
  issuer: {
    kid: "db-acceptance",
    scope: "organization:create project:create database:read database:write database:migrate apps:read apps:write apps:deploy deployments:read secrets:read",
  },
  owner: { seed: "db-todos-platform-fixture-owner", emailPrefix: "kv", name: "DB fixture owner" },
  signer: "wjs_dbtodosfixturesigner00000",
  services: ["control", "gateway"],
  appName: "dbtodos",
  organization: { name: "DB todos fixture", slugPrefix: "db-todos" },
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server", "zeroship-migrate-server"],
  copyExclude: ["node_modules", "dist", ".zeroship", "tests", "test", "e2e"],
  postgresImage: "postgres:16",
  postgresExtra: ["-c", "log_statement=all"],
  manifestResources: [
    { id: "rpc:todos.create", auth: "anonymous" },
    { id: "rpc:todos.list", auth: "anonymous" },
    { id: "rpc:todos.subscribe", auth: "anonymous" },
    { id: "rpc:users.public", auth: "anonymous" },
  ],
  table: "todos",
  column: "title",
  requireRows: false,
  reapplyCheck: false,
  metering: false,
  targets: {
    devEnvVar: "DB_TODOS_API_PORT",
    devMigrate: true,
    list: ({ dev, ui, gateway, log }) => [
      { name: "sqlite", apiUrl: dev.url, uiUrl: ui.url, log: log("dev") },
      { name: "postgres", apiUrl: `${gateway.url}/apps/dbtodos`, uiUrl: `http://dbtodos.localhost:${gateway.number}`, log: log("worker") },
    ],
    probe: () => ({ path: "/__zeroship/v1/todos.count", body: { userId: "user_doesNotExist0000000" } }),
  },
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
