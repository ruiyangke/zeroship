#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  ControlError,
  createControlClient,
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
  "set ZEROSHIP_TOKEN (run `zeroship login` or create a PAT)";

const appInput = {
  app: z
    .string()
    .min(1)
    .describe("App UUID or app name. Names are resolved with list_apps."),
};

export const TOOL_NAMES = [
  "list_apps",
  "get_app",
  "create_app",
  "deploy_app",
  "app_logs",
  "delete_app",
] as const;

export interface ZeroshipMcpServerOptions {
  baseUrl?: string;
  token?: string;
}

type ToolResult = {
  content: Array<{ type: "text"; text: string }>;
  isError?: boolean;
};

type AppListItem = Pick<AppRecord, "id" | "name" | "deploy_hash">;
type AppDetails = Pick<
  AppRecord,
  "id" | "name" | "plan_id" | "deploy_hash" | "created_at" | "updated_at"
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
      description: "Get one zeroship app by UUID or name.",
      inputSchema: appInput,
    },
    async ({ app }) =>
      withClient(client, async (control) => {
        const id = await resolveAppId(control, app);
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
        ...appInput,
        zshipPath: z
          .string()
          .min(1)
          .describe("Path to the local .zship artifact."),
      },
    },
    async ({ app, zshipPath }) =>
      withClient(client, async (control) => {
        const artifact = await readFile(zshipPath);
        const { id, created } = await resolveAppIdForDeploy(control, app);
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
        ...appInput,
        limit: z
          .number()
          .int()
          .positive()
          .optional()
          .describe("Maximum log lines to return."),
      },
    },
    async ({ app, limit }) =>
      withClient(client, async (control) => {
        const id = await resolveAppId(control, app);
        const logs = await control.apps.logs(id);
        return jsonResult({
          app_id: id,
          logs: typeof limit === "number" ? logs.slice(-limit) : logs,
        });
      }),
  );

  server.registerTool(
    "delete_app",
    {
      description: "Delete a zeroship app by UUID or name.",
      inputSchema: appInput,
    },
    async ({ app }) =>
      withClient(client, async (control) => {
        const id = await resolveAppId(control, app);
        return jsonResult({
          app_id: id,
          ...(await control.apps.delete(id)),
        });
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

async function resolveAppId(client: ControlClient, app: string): Promise<string> {
  if (isUuid(app)) return app;
  const existing = await findAppByName(client, app);
  if (!existing) {
    throw new Error(`app \`${app}\` not found`);
  }
  return existing.id;
}

async function resolveAppIdForDeploy(
  client: ControlClient,
  app: string,
): Promise<{ id: string; created: AppRecord | null }> {
  if (isUuid(app)) return { id: app, created: null };

  const existing = await findAppByName(client, app);
  if (existing) return { id: existing.id, created: null };

  const created = await client.apps.create({ name: app });
  return { id: created.id, created };
}

async function findAppByName(
  client: ControlClient,
  name: string,
): Promise<AppRecord | null> {
  const apps = await client.apps.list();
  return apps.find((app) => app.name === name) ?? null;
}

function isUuid(value: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
    value,
  );
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
  };
}

function appDetails(app: AppRecord): AppDetails {
  return {
    id: app.id,
    name: app.name,
    plan_id: app.plan_id,
    deploy_hash: app.deploy_hash,
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
