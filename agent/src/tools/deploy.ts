/**
 * deploy_app tool — deploys a JS bundle to the zeroship platform.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";

const ZEROSHIP_URL = process.env.ZEROSHIP_URL || "http://localhost:3333";
const ZEROSHIP_KEY = process.env.ZEROSHIP_MASTER_KEY || "dev-master-key";

/** Create the app if it doesn't exist, then deploy the JS bundle. */
export const deployApp = tool(
  async ({ app_id, server_js }) => {
    // Ensure app exists
    const checkRes = await fetch(`${ZEROSHIP_URL}/api/apps/${app_id}`, {
      headers: { Authorization: `Bearer ${ZEROSHIP_KEY}` },
    });

    if (checkRes.status === 404) {
      // Create app
      const createRes = await fetch(`${ZEROSHIP_URL}/api/apps`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${ZEROSHIP_KEY}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ id: app_id, plan_id: "free" }),
      });
      if (!createRes.ok) {
        return `Failed to create app: ${await createRes.text()}`;
      }
    }

    // Deploy
    const res = await fetch(`${ZEROSHIP_URL}/api/apps/${app_id}/deploy`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${ZEROSHIP_KEY}`,
        "Content-Type": "application/javascript",
      },
      body: server_js,
    });

    if (!res.ok) {
      return `Deploy failed: ${await res.text()}`;
    }

    const data = await res.json();
    return `Deployed app "${app_id}" — version ${data.version}. Live at ${ZEROSHIP_URL}/rpc with header X-App-Id: ${app_id}`;
  },
  {
    name: "deploy_app",
    description:
      "Deploy a JavaScript app to the zeroship platform. The server_js should be an ES module with exported functions (export function methodName(params) { ... }). Each exported function becomes a JSON-RPC endpoint.",
    schema: z.object({
      app_id: z
        .string()
        .describe("Unique app identifier (alphanumeric + hyphens, e.g. 'my-todo-api')"),
      server_js: z
        .string()
        .describe("The complete server.js source code to deploy"),
    }),
  }
);
