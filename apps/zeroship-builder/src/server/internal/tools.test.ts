"use server";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// The review tool must SHIP NOTHING: it snapshots the sandbox source,
// runs the Reviewer model, and returns the findings. No build, no
// .zship, no artifact download, no control-plane deploy. (The console is
// a pure creator app — see
// docs/superpowers/specs/2026-05-31-console-pure-creator-app-design.md.)
//
// We stub the Reviewer model (the dynamic `@langchain/openai` +
// `@langchain/core/messages` imports) so the unit exercises the tool's
// own control flow, and assert the backend is only ever asked to run the
// snapshot command — never a build.

const reviewerInvoke = vi.fn();

vi.mock("@langchain/openai", () => ({
  ChatOpenAI: class {
    withStructuredOutput() {
      return { invoke: reviewerInvoke };
    }
  },
}));

vi.mock("@langchain/core/messages", () => ({
  SystemMessage: class {
    constructor(public content: unknown) {}
  },
  HumanMessage: class {
    constructor(public content: unknown) {}
  },
}));

import { createReviewTool } from "./tools";

function makeBackend() {
  return {
    execute: vi.fn(async (_command: string) => ({
      output: "## file tree\n./src/app.tsx\n",
      exitCode: 0,
    })),
    // These would only be touched by a deploy/build path. They are NOT
    // on the review tool's backend interface; assert they stay untouched.
    downloadFiles: vi.fn(async () => []),
    write: vi.fn(async () => ({})),
  };
}

beforeEach(() => {
  reviewerInvoke.mockReset();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("createReviewTool — review, ship nothing", () => {
  it("returns the reviewer findings and never builds or deploys", async () => {
    reviewerInvoke.mockResolvedValue({
      approved: true,
      blockers: [
        { kind: "ui", severity: "low", why: "tighten empty state copy" },
      ],
    });

    const backend = makeBackend();
    const tool = createReviewTool({ backend, apiKey: "sk-test" });

    const raw = await tool.invoke({ changes: "added a settings page" });
    const result = JSON.parse(raw as string);

    // Findings come back; approval reflects the reviewer's verdict.
    expect(result.reviewer_approved).toBe(true);
    expect(result.blockers).toEqual([
      { kind: "ui", severity: "low", why: "tighten empty state copy" },
    ]);
    // No deploy fields leak through (url / deploy_hash / blobs_*).
    expect(result).not.toHaveProperty("url");
    expect(result).not.toHaveProperty("deploy_hash");

    // The backend was asked to run EXACTLY the snapshot command — once.
    expect(backend.execute).toHaveBeenCalledTimes(1);
    const command = backend.execute.mock.calls[0]![0] as string;
    expect(command).toContain("## file tree");
    // ...and NEVER a build / .zship path.
    expect(command).not.toContain("pnpm build");
    expect(command).not.toContain("npm run build");
    expect(command).not.toContain("app.zship");

    // No artifact download, no file write — nothing is shipped.
    expect(backend.downloadFiles).not.toHaveBeenCalled();
    expect(backend.write).not.toHaveBeenCalled();
  });

  it("reports approved=false on a hard blocker but still ships nothing", async () => {
    reviewerInvoke.mockResolvedValue({
      approved: true, // the model's raw value is re-derived from severities
      blockers: [
        { kind: "security", severity: "critical", why: "secret in client bundle" },
      ],
    });

    const backend = makeBackend();
    const tool = createReviewTool({ backend, apiKey: "sk-test" });

    const result = JSON.parse((await tool.invoke({ changes: "x" })) as string);

    // normalizeReviewerGate forces approved=false for high/critical.
    expect(result.reviewer_approved).toBe(false);
    expect(result.blockers[0].severity).toBe("critical");

    // Still: snapshot only, no build, no download.
    expect(backend.execute).toHaveBeenCalledTimes(1);
    expect(backend.downloadFiles).not.toHaveBeenCalled();
    expect(backend.write).not.toHaveBeenCalled();
  });
});
