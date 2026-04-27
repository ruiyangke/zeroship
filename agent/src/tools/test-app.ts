/**
 * test_app tool — fetch a path on the deployed app via the gateway.
 *
 * In the new fetch-handler model, apps are normal HTTP services —
 * we just GET / POST a path through the gateway and look at the
 * response. Used to verify deploys are alive (smoke check) and to
 * confirm specific endpoints work as expected.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";
import { GATEWAY_URL } from "../env.js";

export const testApp = tool(
  async ({ app_name, path, method, body }) => {
    const url = `${GATEWAY_URL}/apps/${app_name}${path.startsWith("/") ? path : "/" + path}`;
    const init: RequestInit = {
      method: method ?? "GET",
      headers: body ? { "Content-Type": "application/json" } : {},
    };
    if (body) init.body = typeof body === "string" ? body : JSON.stringify(body);

    let res: Response;
    try {
      res = await fetch(url, init);
    } catch (e: any) {
      return JSON.stringify({
        ok: false,
        error: `Network error: ${e?.message ?? String(e)}`,
        url,
      });
    }

    const text = await res.text();
    const truncated = text.length > 2000 ? text.slice(0, 2000) + "\n…(truncated)" : text;

    return JSON.stringify({
      ok: res.ok,
      status: res.status,
      url,
      content_type: res.headers.get("content-type") ?? "",
      body: truncated,
    });
  },
  {
    name: "test_app",
    description:
      "Make an HTTP request to a deployed app via the gateway. Use to smoke-test the app after deploy: GET / for the homepage, POST /api/foo for an API endpoint, etc. Pass `app_name` (slug) NOT the UUID.",
    schema: z.object({
      app_name: z
        .string()
        .describe("App slug (the `name` field, e.g. 'recipe-app'), NOT the UUID."),
      path: z.string().default("/").describe("URL path including query, e.g. '/api/recipes'."),
      method: z
        .enum(["GET", "POST", "PUT", "DELETE", "PATCH"])
        .optional()
        .describe("HTTP method; defaults to GET."),
      body: z
        .union([z.string(), z.record(z.string(), z.any())])
        .optional()
        .describe("Optional request body. Object → JSON; string → raw."),
    }),
  },
);
