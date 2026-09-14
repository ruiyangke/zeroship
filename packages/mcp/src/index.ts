#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  ControlError,
  createControlClient,
  isAppId,
  type AppId,
  type AppRecord,
  type ControlClient,
} from "@zeroship/control";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const DEFAULT_CONTROL_URL = "http://localhost:9090";
const PACKAGE_NAME = "@zeroship/mcp";
const PACKAGE_VERSION = "0.1.0";
const MISSING_TOKEN_MESSAGE =
  "set ZEROSHIP_TOKEN (run `zeroship login`)";

const appTargetSchema = z.discriminatedUnion("kind", [
  z
    .object({
      kind: z.literal("id"),
      appId: z.string().refine(isAppId, "appId must be a canonical AppId"),
    })
    .strict(),
  z
    .object({
      kind: z.literal("name"),
      appName: z.string().min(1),
    })
    .strict(),
]);

type AppTarget = z.infer<typeof appTargetSchema>;

const appTargetInput = {
  target: appTargetSchema.describe(
    "Choose an app by canonical AppId or by its exact name.",
  ),
};

export const TOOL_NAMES = [
  "list_apps",
  "get_app",
  "create_app",
  "deploy_app",
  "app_logs",
  "archive_app",
  "restore_app",
] as const;

export interface ZeroshipMcpServerOptions {
  baseUrl?: string;
  token?: string;
}

type ToolResult = {
  content: Array<{ type: "text"; text: string }>;
  isError?: boolean;
};

type AppListItem = Pick<
  AppRecord,
  "id" | "name" | "deploy_hash" | "archived_at"
>;
type AppDetails = Pick<
  AppRecord,
  | "id"
  | "name"
  | "plan_id"
  | "deploy_hash"
  | "archived_at"
  | "created_at"
  | "updated_at"
>;

export function createZeroshipMcpServer(
  options: ZeroshipMcpServerOptions = {},
): McpServer {
  const baseUrl =
    options.baseUrl ?? process.env.ZEROSHIP_CONTROL_URL ?? DEFAULT_CONTROL_URL;
  const token = options.token ?? process.env.ZEROSHIP_TOKEN;
  const client = token
    ? createControlClient({
        baseUrl,
        auth: token,
      })
    : null;

  const server = new McpServer({
    name: PACKAGE_NAME,
    version: PACKAGE_VERSION,
  });

  server.registerTool(
    "list_apps",
    {
      description: "List zeroship apps available to the current token.",
      inputSchema: {},
    },
    async () =>
      withClient(client, async (control) => {
        const apps = await control.apps.list();
        return jsonResult(apps.map(appListItem));
      }),
  );

  server.registerTool(
    "get_app",
    {
      description: "Get one zeroship app by an explicit typed-ID or name target.",
      inputSchema: appTargetInput,
    },
    async ({ target }) =>
      withClient(client, async (control) => {
        const id = await resolveAppTarget(control, target);
        return jsonResult(appDetails(await control.apps.get(id)));
      }),
  );

  server.registerTool(
    "create_app",
    {
      description: "Create a zeroship app.",
      inputSchema: {
        name: z.string().min(1).describe("App name."),
        plan_id: z
          .string()
          .min(1)
          .optional()
          .describe("Plan id. Defaults to free."),
      },
    },
    async ({ name, plan_id }) =>
      withClient(client, async (control) =>
        jsonResult(appDetails(await control.apps.create({ name, plan_id }))),
      ),
  );

  server.registerTool(
    "deploy_app",
    {
      description: "Deploy a local .zship to an app, creating missing named apps.",
      inputSchema: {
        ...appTargetInput,
        zshipPath: z
          .string()
          .min(1)
          .describe("Path to the local .zship artifact."),
      },
    },
    async ({ target, zshipPath }) =>
      withClient(client, async (control) => {
        const artifact = await readFile(zshipPath);
        const { id, created } = await resolveAppTargetForDeploy(control, target);
        const deploy = await control.apps.deploy(id, artifact);
        return jsonResult({
          app_id: id,
          created: created ? appDetails(created) : null,
          deploy_hash: deploy.deploy_hash,
          blobs_uploaded: deploy.blobs_uploaded,
          blobs_deduped: deploy.blobs_deduped,
        });
      }),
  );

  server.registerTool(
    "app_logs",
    {
      description: "Read recent worker logs for a zeroship app.",
      inputSchema: {
        ...appTargetInput,
        limit: z
          .number()
          .int()
          .positive()
          .optional()
          .describe("Maximum log lines to return."),
      },
    },
    async ({ target, limit }) =>
      withClient(client, async (control) => {
        const id = await resolveAppTarget(control, target);
        const logs = await control.apps.logs(id);
        return jsonResult({
          app_id: id,
          logs: typeof limit === "number" ? logs.slice(-limit) : logs,
        });
      }),
  );

  server.registerTool(
    "archive_app",
    {
      description: "Archive a zeroship app by an explicit typed-ID or name target.",
      inputSchema: appTargetInput,
    },
    async ({ target }) =>
      withClient(client, async (control) => {
        const id = await resolveAppTarget(control, target);
        return jsonResult(appDetails(await control.apps.archive(id)));
      }),
  );

  server.registerTool(
    "restore_app",
    {
      description: "Restore an archived zeroship app by an explicit typed-ID or name target.",
      inputSchema: appTargetInput,
    },
    async ({ target }) =>
      withClient(client, async (control) => {
        const id = await resolveAppTarget(control, target);
        return jsonResult(appDetails(await control.apps.unarchive(id)));
      }),
  );

  return server;
}

export async function runStdioServer(): Promise<void> {
  const server = createZeroshipMcpServer();
  const transport = new StdioServerTransport();
  await server.connect(transport);
}

async function withClient(
  client: ControlClient | null,
  fn: (client: ControlClient) => Promise<ToolResult>,
): Promise<ToolResult> {
  if (!client) {
    return errorResult(MISSING_TOKEN_MESSAGE);
  }
  try {
    return await fn(client);
  } catch (error) {
    return errorResult(formatError(error));
  }
}

async function resolveAppTarget(
  client: ControlClient,
  target: AppTarget,
): Promise<AppId> {
  if (target.kind === "id") return target.appId;
  const existing = await findAppByName(client, target.appName);
  if (!existing) {
    throw new Error(`app \`${target.appName}\` not found`);
  }
  return existing.id;
}

async function resolveAppTargetForDeploy(
  client: ControlClient,
  target: AppTarget,
): Promise<{ id: AppId; created: AppRecord | null }> {
  if (target.kind === "id") return { id: target.appId, created: null };

  const existing = await findAppByName(client, target.appName);
  if (existing) return { id: existing.id, created: null };

  const created = await client.apps.create({ name: target.appName });
  return { id: created.id, created };
}

async function findAppByName(
  client: ControlClient,
  name: string,
): Promise<AppRecord | null> {
  const apps = await client.apps.list();
  return apps.find((app) => app.name === name) ?? null;
}

function jsonResult(value: unknown): ToolResult {
  return {
    content: [
      {
        type: "text",
        text: JSON.stringify(value, null, 2),
      },
    ],
  };
}

function appListItem(app: AppRecord): AppListItem {
  return {
    id: app.id,
    name: app.name,
    deploy_hash: app.deploy_hash,
    archived_at: app.archived_at,
  };
}

function appDetails(app: AppRecord): AppDetails {
  return {
    id: app.id,
    name: app.name,
    plan_id: app.plan_id,
    deploy_hash: app.deploy_hash,
    archived_at: app.archived_at,
    created_at: app.created_at,
    updated_at: app.updated_at,
  };
}

function errorResult(message: string): ToolResult {
  return {
    isError: true,
    content: [
      {
        type: "text",
        text: `Error: ${message}`,
      },
    ],
  };
}

function formatError(error: unknown): string {
  if (error instanceof ControlError) {
    return `control plane returned HTTP ${error.status}: ${error.message}`;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

function printToolNames(): void {
  for (const name of TOOL_NAMES) {
    console.log(name);
  }
}

function isMainModule(): boolean {
  return (
    process.argv[1] !== undefined &&
    pathToFileURL(resolve(process.argv[1])).href === import.meta.url
  );
}

if (isMainModule()) {
  if (process.argv.includes("--list-tools")) {
    printToolNames();
  } else {
    runStdioServer().catch((error: unknown) => {
      console.error(formatError(error));
      process.exitCode = 1;
    });
  }
}
