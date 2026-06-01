"use server";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// ─── fakes ───────────────────────────────────────────────────────
//
// The console is a pure creator app: projects live in KV; env + logs
// are files in the project's sandbox. These units drive the REAL
// projects.ts logic (dotenv parse/serialize, read-modify-write, the KV
// registry) against fakes for the two seams it depends on:
//   - `./sandbox.js`  — a fake in-memory sandbox files API.
//   - `@zeroship/kv`  — a fake in-memory KV store.
//   - `zeroship`      — currentUser() for per-creator registry scoping.

const sandboxFiles = new Map<string, string>(); // `${appId}::${path}` -> text
const kvStore = new Map<string, unknown>();

const mocks = vi.hoisted(() => ({
  currentUser: vi.fn(),
}));

vi.mock("zeroship", () => ({
  currentUser: mocks.currentUser,
}));

vi.mock("./sandbox.js", () => ({
  readSandboxFileFor: vi.fn(async (appId: string, path: string) => {
    return sandboxFiles.get(`${appId}::${path}`) ?? null;
  }),
  writeSandboxFileFor: vi.fn(async (appId: string, path: string, content: string) => {
    sandboxFiles.set(`${appId}::${path}`, content);
  }),
}));

vi.mock("@zeroship/kv", () => ({
  kv: {
    get: vi.fn(async (key: string) => ({ data: kvStore.get(key) ?? null, error: null })),
    set: vi.fn(async (key: string, value: unknown) => {
      kvStore.set(key, value);
      return { error: null };
    }),
    delete: vi.fn(async (key: string) => {
      kvStore.delete(key);
      return { error: null };
    }),
  },
}));

import {
  archiveProject,
  createProject,
  deleteEnv,
  deleteProject,
  getEnv,
  getLogs,
  listProjects,
  parseDotenv,
  serializeDotenv,
  setEnv,
  unarchiveProject,
} from "./projects";

const APP_ID = "prj_test_workspace";

beforeEach(() => {
  sandboxFiles.clear();
  kvStore.clear();
  mocks.currentUser.mockReset();
  mocks.currentUser.mockReturnValue({ id: "usr_creator_a" });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe(".env over the sandbox files API", () => {
  it("round-trips set → get → delete via a single .env file", async () => {
    // Fresh project: no `.env` yet → empty.
    await expect(getEnv(APP_ID)).resolves.toEqual({ vars: [] });

    await setEnv({ appId: APP_ID, key: "API_URL", value: "https://api.example" });
    await setEnv({ appId: APP_ID, key: "FEATURE_X", value: "1" });

    // The bytes landed in the project's `.env` (one file, no
    // vars/secrets split).
    const raw = sandboxFiles.get(`${APP_ID}::.env`);
    expect(raw).toBeDefined();
    expect(raw).toContain("API_URL=https://api.example");
    expect(raw).toContain("FEATURE_X=1");

    await expect(getEnv(APP_ID)).resolves.toEqual({
      vars: [
        { key: "API_URL", value: "https://api.example" },
        { key: "FEATURE_X", value: "1" },
      ],
    });

    // Set an existing key → in-place update (no duplicate line).
    await setEnv({ appId: APP_ID, key: "FEATURE_X", value: "0" });
    const afterUpdate = await getEnv(APP_ID);
    expect(afterUpdate.vars.filter((v) => v.key === "FEATURE_X")).toEqual([
      { key: "FEATURE_X", value: "0" },
    ]);

    await deleteEnv({ appId: APP_ID, key: "API_URL" });
    await expect(getEnv(APP_ID)).resolves.toEqual({
      vars: [{ key: "FEATURE_X", value: "0" }],
    });
  });

  it("deleteEnv does not rewrite the file when the key is absent", async () => {
    const sandbox = await import("./sandbox.js");
    await setEnv({ appId: APP_ID, key: "KEEP", value: "yes" });
    (sandbox.writeSandboxFileFor as ReturnType<typeof vi.fn>).mockClear();

    await deleteEnv({ appId: APP_ID, key: "MISSING" });
    expect(sandbox.writeSandboxFileFor).not.toHaveBeenCalled();
  });

  it("parses/serializes quoted values round-trip", () => {
    const vars = [
      { key: "PLAIN", value: "abc" },
      { key: "SPACED", value: "two words" },
      { key: "HASHED", value: "a#b" },
    ];
    const text = serializeDotenv(vars);
    expect(parseDotenv(text)).toEqual(vars);
  });

  it("parseDotenv skips comments and blank lines", () => {
    expect(parseDotenv("# a comment\n\nFOO=bar\n  \nBAZ=qux")).toEqual([
      { key: "FOO", value: "bar" },
      { key: "BAZ", value: "qux" },
    ]);
  });
});

describe("getLogs over .zeroship/dev.log", () => {
  it("returns the dev-server log lines (absent log reads as empty)", async () => {
    // No log yet — preview never started.
    await expect(getLogs(APP_ID)).resolves.toEqual([]);

    sandboxFiles.set(
      `${APP_ID}::.zeroship/dev.log`,
      "vite ready in 312 ms\nGET / 200\n",
    );

    await expect(getLogs(APP_ID)).resolves.toEqual([
      "vite ready in 312 ms",
      "GET / 200",
    ]);

    // Reads `.zeroship/dev.log` specifically, not some other path.
    const sandbox = await import("./sandbox.js");
    expect(sandbox.readSandboxFileFor).toHaveBeenCalledWith(
      APP_ID,
      ".zeroship/dev.log",
    );
  });
});

describe("project registry over KV", () => {
  it("create → list → archive → unarchive against the KV store", async () => {
    await expect(listProjects()).resolves.toEqual([]);

    const created = await createProject({ name: "supper-club" });
    expect(created.id).toMatch(/^prj_/);
    expect(created.name).toBe("supper-club");
    expect(created.archived).toBeUndefined();

    await expect(listProjects()).resolves.toEqual([created]);

    await archiveProject({ appId: created.id });
    const afterArchive = await listProjects();
    expect(afterArchive[0]?.archived).toBe(true);

    await unarchiveProject({ appId: created.id });
    const afterRestore = await listProjects();
    expect(afterRestore[0]?.archived).toBe(false);

    await deleteProject(created.id);
    await expect(listProjects()).resolves.toEqual([]);
  });

  it("scopes the registry per creator", async () => {
    mocks.currentUser.mockReturnValue({ id: "usr_creator_a" });
    await createProject({ name: "a-project" });

    // A different creator sees a different (empty) registry.
    mocks.currentUser.mockReturnValue({ id: "usr_creator_b" });
    await expect(listProjects()).resolves.toEqual([]);

    mocks.currentUser.mockReturnValue({ id: "usr_creator_a" });
    const a = await listProjects();
    expect(a.map((p) => p.name)).toEqual(["a-project"]);
  });
});
