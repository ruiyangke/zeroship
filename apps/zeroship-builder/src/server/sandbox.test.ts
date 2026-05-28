"use server";

import { describe, expect, it, vi } from "vitest";

import { jsonOrThrow } from "./sandbox";
import { UpstreamServiceError } from "./internal/upstream-error";

describe("sandbox server client", () => {
  it("does not expose controller bodies in thrown messages", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const res = new Response(
      "{\"error\":\"backend_create_failed\",\"message\":\"host=10.0.0.7 path=/var/lib/nomad\"}",
      { status: 502 },
    );

    const resForMessage = res.clone();

    await expect(jsonOrThrow(res, "list files")).rejects.toMatchObject({
      name: "UpstreamServiceError",
      status: 502,
      message: "sandbox request failed",
    } satisfies Partial<UpstreamServiceError>);

    await expect(jsonOrThrow(resForMessage, "list files")).rejects.not.toThrow("/var/lib/nomad");
  });
});
