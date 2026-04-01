/**
 * test_app tool — calls an RPC method on a deployed app to verify it works.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";

const APPBASE_URL = process.env.APPBASE_URL || "http://localhost:3333";

export const testApp = tool(
  async ({ app_id, method, params }) => {
    const res = await fetch(`${APPBASE_URL}/rpc`, {
      method: "POST",
      headers: {
        "X-App-Id": app_id,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method,
        params: params ?? [],
        id: 1,
      }),
    });

    const data = await res.json();

    if (data.error) {
      return `ERROR: ${JSON.stringify(data.error)}`;
    }

    return `OK: ${JSON.stringify(data.result)}`;
  },
  {
    name: "test_app",
    description:
      "Test a deployed app by calling one of its RPC methods. Returns the result or error.",
    schema: z.object({
      app_id: z.string().describe("The app to test"),
      method: z.string().describe("The RPC method name to call"),
      params: z
        .array(z.any())
        .optional()
        .describe("Parameters to pass to the method"),
    }),
  }
);
